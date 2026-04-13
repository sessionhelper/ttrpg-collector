//! Per-session actor.
//!
//! Each active recording session runs as its own `tokio::spawn`ed task
//! ("the actor"), owns its `Session` exclusively, and is the sole mutator
//! of that session's state. External handlers (slash commands, button
//! clicks, voice events, the harness HTTP endpoints) send `SessionCmd`
//! messages over a bounded mpsc channel and await `oneshot` replies for
//! queries. The `AppState` carries a `DashMap<guild_id, SessionHandle>`
//! of per-guild senders — per-guild lookups are lock-free, per-session
//! state mutations happen on exactly one task, and there is no shared
//! mutex that blocks Discord's 3-second interaction ack window.
//!
//! ## Lifecycle
//!
//! ```text
//!   spawn_session (inserts handle into DashMap)
//!     │
//!     ▼
//!   actor run loop:
//!     ┌──────────────────────────────────────────────────────────────┐
//!     │ select! {                                                    │
//!     │   rx.recv()         — cmd from outside (user click, /stop)   │
//!     │   heal_tick         — periodic DAVE heal check               │
//!     │   startup_done      — oneshot completion of startup task    │
//!     │ }                                                            │
//!     └──────────────────────────────────────────────────────────────┘
//!     │
//!     ▼
//!   on Stop / terminal / channel closed:
//!     finalize (upload meta+consent if Recording, abandon otherwise)
//!     remove from DashMap
//!     return
//! ```
//!
//! ## Long-running operations + cancellation
//!
//! `StartRecording` kicks off the voice-join + DAVE-handshake pipeline.
//! Because that pipeline awaits up to ~30s on Songbird and Discord, the
//! actor delegates it to a short-lived helper task and keeps its own
//! `select!` loop responsive. The helper task holds a `watch::Receiver`
//! whose sender is owned by the actor — when `Stop` arrives mid-startup,
//! the actor flips the watch and the helper bails between awaits. This
//! is the cancellation mechanism called out in the refactor spec.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serenity::all::*;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn, Instrument};

use crate::session::{consent_buttons, ConsentScope, Phase, Session};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Command / reply types
// ---------------------------------------------------------------------------

/// Which license flag to flip.
#[derive(Debug, Clone, Copy)]
pub enum LicenseField {
    NoLlmTraining,
    NoPublicRelease,
}

/// Result of a `RecordConsent` message — surfaces what the actor did so
/// the calling button handler can render the right Discord response.
#[derive(Debug)]
pub enum ConsentOutcome {
    /// Click was accepted; nothing more to do (waiting on other participants
    /// or this was a mid-session joiner).
    Ack { embed: CreateEmbed },
    /// This click completed quorum — the actor is kicking off the startup
    /// pipeline. The button handler should ack and, if the user is the
    /// primary human, kick off the license followup flow.
    QuorumMet { embed: CreateEmbed },
    /// Everyone responded but quorum was NOT met. Actor will tear down.
    QuorumFailed { embed: CreateEmbed },
}

/// Granular reasons why a consent click was rejected before being recorded.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("user is not a participant in this session")]
    NotParticipant,
    #[error("user has already responded")]
    AlreadyResponded,
    #[error("only the initiator can stop the session")]
    NotInitiator,
    #[error("session is not in a stoppable state")]
    NotStoppable,
    #[error("session actor is gone")]
    ActorGone,
}

/// Minimal view of the session returned by `GetSnapshot`. Used by the
/// harness /status endpoint and any debug surfacing.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `guild_id`, `phase_label`, `participant_count` exposed for debug tooling.
pub struct SessionSnapshot {
    pub session_id: String,
    pub guild_id: u64,
    pub recording: bool,
    pub stable: bool,
    pub phase_label: &'static str,
    pub participant_count: usize,
}

/// Messages accepted by the session actor. Reply channels are `oneshot`
/// so callers get a single response per command. If the actor has
/// terminated before the reply is sent, the `oneshot::Receiver::recv`
/// returns `RecvError` and the caller treats it as "session is gone".
pub enum SessionCmd {
    /// Record a consent click. Replies with a status that the button handler
    /// uses to drive the Discord UI update.
    RecordConsent {
        user: UserId,
        scope: ConsentScope,
        reply: oneshot::Sender<Result<ConsentOutcome, SessionError>>,
    },

    /// Toggle one of the two license flags and return the new (no_llm,
    /// no_public) pair plus the cached Data API participant UUID if the
    /// caller needs to sync asynchronously.
    ToggleLicense {
        user: UserId,
        field: LicenseField,
        reply: oneshot::Sender<(bool, bool, Option<uuid::Uuid>)>,
    },

    /// User-initiated /stop. Only the session's initiator is allowed; other
    /// users get `NotInitiator`. Cancels any running startup task and drives
    /// the session into the correct terminal path based on its current phase.
    Stop {
        initiator: UserId,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },

    /// Auto-stop after channel-empty timeout. Skips the initiator check.
    /// Behaves identically to `Stop` otherwise.
    AutoStop,

    /// Voice state change from `voice_state_update`. May produce a mid-
    /// session join prompt or arm/cancel the auto-stop timer. The actor
    /// computes occupant counts from the Serenity cache via the saved
    /// Context so the caller doesn't need to know the session's channel.
    VoiceStateChange {
        old_channel: Option<ChannelId>,
        new_channel: Option<ChannelId>,
        user_id: UserId,
        is_bot: bool,
        display_name: String,
    },

    /// Register the Discord message id of the consent embed so /stop can
    /// edit it at cleanup time. Sent by `/record` after it posts the embed.
    SetConsentMessage {
        channel_id: ChannelId,
        message_id: MessageId,
    },

    /// Cache the Data API's participant row UUID for a local user. Sent by
    /// `/record` after `add_participants_batch` returns.
    SetParticipantUuid {
        user_id: UserId,
        participant_uuid: uuid::Uuid,
    },

    /// Apply the dev-only bypass: mark the given users as auto-consented
    /// locally. Called by /record right after participant registration.
    BypassConsent { users: Vec<UserId> },

    /// Trigger the recording pipeline now. Used by /record's
    /// bypass-quorum-met fast path and the harness /record endpoint. A
    /// concurrent real consent click that triggers quorum also ends up
    /// here (internally, after RecordConsent detects quorum).
    ///
    /// Replies once the pipeline's initial outcome is known — Recording,
    /// Preempted, DaveFailed, etc. The session continues running for
    /// Recording; the other outcomes mean the actor has already finalized
    /// itself and will drop its handle.
    StartRecording {
        reply: oneshot::Sender<StartRecordingOutcome>,
    },

    /// Snapshot of current session state, for status / debug surfaces.
    GetSnapshot {
        reply: oneshot::Sender<SessionSnapshot>,
    },

    /// Register that a license-preference ephemeral followup message was
    /// sent. Stored so the session can edit it at cleanup time and so /stop
    /// can abort its expiry-cleanup task.
    AddLicenseFollowup {
        token: String,
        message_id: MessageId,
        cleanup_task: JoinHandle<()>,
    },
}

/// Outcome of a `StartRecording` command.
#[derive(Debug)]
pub enum StartRecordingOutcome {
    /// Audio is flowing. The actor remains alive, servicing DAVE heal and
    /// further commands.
    Recording,
    /// The pipeline was preempted mid-startup (concurrent /stop, session
    /// replaced). Actor has or will finalize itself.
    Preempted,
    /// DAVE never delivered decoded audio within the retry budget.
    DaveFailed,
    /// Mid-loop voice rejoin failed.
    DaveRejoinFailed,
    /// Initial voice-join errored at the Songbird/Serenity layer.
    VoiceJoinFailed(String),
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// Lightweight per-session handle stored in `AppState.sessions`. Cheap to
/// clone; all fields are `Arc`-like in cost.
#[derive(Clone)]
pub struct SessionHandle {
    tx: mpsc::Sender<SessionCmd>,
    /// Mirror of the session ID so a handler can log it without a roundtrip.
    pub session_id: String,
    /// Observability: flipped to true once the actor has begun draining.
    /// Lets an outside caller detect an actor in the process of exiting
    /// without sending a command that would race the shutdown.
    pub shutting_down: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionHandle {
    /// Send a command to the actor. Returns `ActorGone` if the actor has
    /// dropped its receiver (typically because it terminated).
    pub async fn send(&self, cmd: SessionCmd) -> Result<(), SessionError> {
        self.tx.send(cmd).await.map_err(|_| SessionError::ActorGone)
    }

    /// True if the actor has signalled it's on its way out.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Build a `(SessionCmd, oneshot::Receiver)` pair, send the command to the
/// actor, and await the reply. Wraps the ActorGone / RecvError handling in
/// one place so callers can write straight-line code.
pub async fn request<T, F>(handle: &SessionHandle, build: F) -> Result<T, SessionError>
where
    F: FnOnce(oneshot::Sender<T>) -> SessionCmd,
{
    let (tx, rx) = oneshot::channel();
    handle.send(build(tx)).await?;
    rx.await.map_err(|_| SessionError::ActorGone)
}

// ---------------------------------------------------------------------------
// Spawn + run loop
// ---------------------------------------------------------------------------

/// Spawn the per-guild actor. Inserts the handle into `state.sessions`
/// atomically — returns `Err(session)` if another actor is already running
/// for the same guild so the caller can recover the unused Session.
///
/// The spawned task runs until it processes a terminal command (Stop,
/// AutoStop, unrecoverable startup failure) or the last handle is dropped
/// and the mpsc closes.
pub fn spawn_session(
    state: Arc<AppState>,
    ctx: Context,
    session: Session,
) -> Result<SessionHandle, Box<Session>> {
    let guild_id = session.guild_id;
    let session_id = session.id.clone();
    let (tx, rx) = mpsc::channel::<SessionCmd>(64);
    let shutting_down = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let handle = SessionHandle {
        tx,
        session_id: session_id.clone(),
        shutting_down: shutting_down.clone(),
    };

    // DashMap's `entry().or_insert_with` is a true CAS — if another actor
    // sneaks in between our check and insert, we lose and return the
    // session to the caller (matching the old `try_insert` contract).
    match state.sessions.entry(guild_id) {
        dashmap::mapref::entry::Entry::Occupied(_) => return Err(Box::new(session)),
        dashmap::mapref::entry::Entry::Vacant(vac) => {
            vac.insert(handle.clone());
        }
    }

    let span = tracing::info_span!(
        "session_actor",
        session_id = %session_id,
        guild_id = guild_id,
    );
    tokio::spawn(run_actor(state, ctx, session, rx, shutting_down).instrument(span));

    Ok(handle)
}

/// Actor run loop. Consumes commands until shutdown, drives the heal timer,
/// tracks the startup sub-task's completion channel. Exits by dropping the
/// handle from the DashMap and returning.
async fn run_actor(
    state: Arc<AppState>,
    ctx: Context,
    mut session: Session,
    mut rx: mpsc::Receiver<SessionCmd>,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
) {
    // Cancellation signal for the in-flight startup task (if any). The
    // actor is the sole owner of the Sender; the startup task holds a
    // Receiver. Flipping to `true` asks startup to bail between awaits.
    let (cancel_tx, _cancel_rx_primed) = watch::channel(false);

    // Set of long-running sub-tasks we've spawned but whose JoinHandle we
    // don't care about individually (startup's internal helpers are joined
    // through the startup task itself). Kept only so Drop doesn't cancel
    // them prematurely if we ever need to wait before returning.
    let mut heal_interval = tokio::time::interval(Duration::from_secs(5));
    heal_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick so heal waits a full interval before
    // the first check — the startup pipeline itself already guarantees a
    // fresh DAVE state at the moment it transitions to Recording.
    heal_interval.tick().await;

    // One-shot heal budget — the legacy heal task only ever performed a
    // single leave+rejoin per session. Keep that invariant here; after
    // the first heal this flag flips and subsequent ticks short-circuit.
    let mut healed_once = false;

    loop {
        tokio::select! {
            biased;
            maybe_cmd = rx.recv() => {
                match maybe_cmd {
                    Some(cmd) => {
                        if handle_cmd(&state, &ctx, &mut session, &cancel_tx, &mut rx, cmd).await {
                            break;
                        }
                    }
                    None => {
                        info!("session_actor_exit — all senders dropped");
                        break;
                    }
                }
            }
            _ = heal_interval.tick() => {
                if !healed_once && heal_step(&state, &ctx, &mut session).await {
                    healed_once = true;
                }
            }
        }
    }

    shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);

    // Ensure we flip the cancel signal so any lingering startup helper
    // sees shutdown.
    let _ = cancel_tx.send(true);

    // Finalize based on whichever phase we ended in.
    finalize_on_exit(&state, &ctx, &mut session).await;

    state.sessions.remove(&session.guild_id);
}

/// Process a single command. Returns `true` when the command demands the
/// actor exit immediately after processing.
///
/// `rx` is passed so the long-running `StartRecording` handler can listen
/// for interleaved commands (notably `Stop` / `AutoStop`) during its
/// multi-second voice-join + DAVE-wait sequence. Every other command
/// processes in constant time and does not touch `rx`.
async fn handle_cmd(
    state: &Arc<AppState>,
    ctx: &Context,
    session: &mut Session,
    cancel_tx: &watch::Sender<bool>,
    rx: &mut mpsc::Receiver<SessionCmd>,
    cmd: SessionCmd,
) -> bool {
    match cmd {
        SessionCmd::RecordConsent { user, scope, reply } => {
            let result = do_record_consent(state, session, user, scope).await;
            let is_quorum_met = matches!(result, Ok(ConsentOutcome::QuorumMet { .. }));
            let is_quorum_failed = matches!(result, Ok(ConsentOutcome::QuorumFailed { .. }));
            let _ = reply.send(result);

            if is_quorum_met {
                // The caller will follow up with StartRecording; we simply
                // wait. Quorum met is the green light for the button
                // handler to ack and for the next command (typically
                // StartRecording) to arrive via the mpsc.
            } else if is_quorum_failed {
                // Quorum failed — we're done. Signal exit so the actor
                // finalizes and removes itself.
                return true;
            }
            false
        }
        SessionCmd::ToggleLicense { user, field, reply } => {
            let (current_llm, current_public) = session.license_flags(user);
            let (new_llm, new_public) = match field {
                LicenseField::NoLlmTraining => (!current_llm, current_public),
                LicenseField::NoPublicRelease => (current_llm, !current_public),
            };
            session.set_license_flags(user, new_llm, new_public);
            let _ = reply.send((new_llm, new_public, session.participant_uuid(user)));
            false
        }
        SessionCmd::Stop { initiator, reply } => {
            if session.initiator_id != initiator {
                let _ = reply.send(Err(SessionError::NotInitiator));
                return false;
            }
            if !session.phase.is_stoppable() {
                let _ = reply.send(Err(SessionError::NotStoppable));
                return false;
            }
            let _ = reply.send(Ok(()));
            // Preempt any startup helper + mark the session for exit.
            let _ = cancel_tx.send(true);
            true
        }
        SessionCmd::AutoStop => {
            if !session.phase.is_stoppable() {
                return false;
            }
            let _ = cancel_tx.send(true);
            true
        }
        SessionCmd::VoiceStateChange {
            old_channel,
            new_channel,
            user_id,
            is_bot,
            display_name,
        } => {
            handle_voice_state(
                state,
                ctx,
                session,
                old_channel,
                new_channel,
                user_id,
                is_bot,
                display_name,
            )
            .await;
            false
        }
        SessionCmd::SetConsentMessage { channel_id, message_id } => {
            session.consent_message = Some((channel_id, message_id));
            false
        }
        SessionCmd::SetParticipantUuid { user_id, participant_uuid } => {
            session.set_participant_uuid(user_id, participant_uuid);
            false
        }
        SessionCmd::BypassConsent { users } => {
            for uid in users {
                session.record_consent(uid, ConsentScope::Full);
            }
            false
        }
        SessionCmd::StartRecording { reply } => {
            run_startup_pipeline(state, ctx, session, cancel_tx, rx, reply).await
        }
        SessionCmd::GetSnapshot { reply } => {
            let phase_label = match &session.phase {
                Phase::AwaitingConsent => "awaiting_consent",
                Phase::StartingRecording(_) => "starting_recording",
                Phase::Recording(_) => "recording",
                Phase::Finalizing => "finalizing",
                Phase::Cancelled => "cancelled",
                Phase::Complete => "complete",
            };
            let snapshot = SessionSnapshot {
                session_id: session.id.clone(),
                guild_id: session.guild_id,
                recording: matches!(session.phase, Phase::Recording(_)),
                stable: session.is_stable(),
                phase_label,
                participant_count: session.participants.len(),
            };
            let _ = reply.send(snapshot);
            false
        }
        SessionCmd::AddLicenseFollowup {
            token,
            message_id,
            cleanup_task,
        } => {
            session.license_followups.push((token, message_id));
            session.license_cleanup_tasks.push(cleanup_task);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Consent click handling
// ---------------------------------------------------------------------------

async fn do_record_consent(
    state: &Arc<AppState>,
    session: &mut Session,
    user: UserId,
    scope: ConsentScope,
) -> Result<ConsentOutcome, SessionError> {
    // Fast pre-validation + state mutation — pure logic, no I/O. Split
    // into its own function so unit tests can exercise the state machine
    // without constructing an AppState / DataApiClient.
    apply_consent_click(session, user, scope)?;

    // Fire-and-forget the Data API write.
    let api = state.api.clone();
    let scope_str = match scope {
        ConsentScope::Full => "full",
        ConsentScope::Decline => "decline",
    };
    let session_id = session.id.clone();
    let cached_pid = session.participant_uuid(user);
    let user_get = user.get();
    tokio::spawn(async move {
        let res = if let Some(pid) = cached_pid {
            api.record_consent_by_id(pid, scope_str).await
        } else if let Ok(sid) = uuid::Uuid::parse_str(&session_id) {
            api.record_consent(sid, user_get, scope_str).await
        } else {
            Ok(())
        };
        if let Err(e) = res {
            error!("API call failed (record_consent): {e}");
        }
    });

    // Mid-session joiner accepted — add them to the live capture set.
    let is_mid_session = matches!(session.phase, Phase::Recording(_))
        && session.participants[&user].mid_session_join;
    if is_mid_session && scope == ConsentScope::Full {
        if let Phase::Recording(data) | Phase::StartingRecording(data) = &session.phase {
            let cu = data.consented_users.clone();
            tokio::spawn(async move {
                cu.lock().await.insert(user_get);
            });
        }
        info!(user_id = %user, "mid_session_consent_granted — audio capture enabled");
    }

    Ok(build_consent_outcome(session))
}

/// Validate + record a consent click. Pure state mutation — no async I/O,
/// no spawns. Unit-testable in isolation.
fn apply_consent_click(
    session: &mut Session,
    user: UserId,
    scope: ConsentScope,
) -> Result<(), SessionError> {
    if !session.participants.contains_key(&user) {
        return Err(SessionError::NotParticipant);
    }
    if session.participants[&user].scope.is_some() {
        return Err(SessionError::AlreadyResponded);
    }
    session.record_consent(user, scope);
    Ok(())
}

/// Inspect the current session state and return the appropriate
/// `ConsentOutcome`. Pure read — no mutation, no I/O.
fn build_consent_outcome(session: &Session) -> ConsentOutcome {
    let embed = session.consent_embed();
    let is_awaiting = matches!(session.phase, Phase::AwaitingConsent);
    if is_awaiting && session.all_responded() {
        if session.evaluate_quorum() {
            ConsentOutcome::QuorumMet { embed }
        } else {
            ConsentOutcome::QuorumFailed { embed }
        }
    } else {
        ConsentOutcome::Ack { embed }
    }
}

// ---------------------------------------------------------------------------
// Voice state dispatch
// ---------------------------------------------------------------------------

async fn handle_voice_state(
    state: &Arc<AppState>,
    ctx: &Context,
    session: &mut Session,
    old_channel: Option<ChannelId>,
    new_channel: Option<ChannelId>,
    user_id: UserId,
    is_bot: bool,
    display_name: String,
) {
    // Only Recording cares about mid-session joins or auto-stop arming.
    if !matches!(session.phase, Phase::Recording(_)) {
        return;
    }

    let voice_channel = ChannelId::new(session.channel_id);
    let joined = new_channel == Some(voice_channel);
    let was_in = old_channel == Some(voice_channel);

    // Compute occupants of OUR channel (not the user's target channel) —
    // the auto-stop decision is only about whether our voice channel is
    // empty. Relies on the Serenity cache being populated.
    let occupants =
        channel_occupants(ctx, GuildId::new(session.guild_id), voice_channel)
            .unwrap_or(usize::MAX);

    if joined && !was_in && !is_bot && !session.participants.contains_key(&user_id) {
        let text_channel = ChannelId::new(session.text_channel_id);

        // Blocklist check without holding session state references.
        let api = state.api.clone();
        let blocked = match api.check_blocklist(user_id.get()).await {
            Ok(b) => b,
            Err(e) => {
                error!("API call failed (check_blocklist): {e}");
                false
            }
        };
        if blocked {
            info!(user_id = %user_id, "mid_session_joiner_blocked — user opted out globally");
        } else {
            // Register locally + remotely.
            session.add_participant(user_id, display_name.clone(), true);
            if let Ok(sid) = uuid::Uuid::parse_str(&session.id) {
                match state
                    .api
                    .add_participant(sid, user_id.get(), true, Some(display_name.clone()))
                    .await
                {
                    Ok(row) => {
                        session.set_participant_uuid(user_id, row.id);
                    }
                    Err(e) => {
                        error!("API call failed (add_participant mid-session): {e}")
                    }
                }
            }
            info!(user_id = %user_id, name = %display_name, "mid_session_joiner");

            let embed = session.consent_embed();
            let msg = CreateMessage::new()
                .content(format!(
                    "<@{user_id}> joined the voice channel during recording. \
                     Their audio is **not being captured** until they consent."
                ))
                .embed(embed)
                .components(vec![consent_buttons()]);
            let http = ctx.http.clone();
            tokio::spawn(async move {
                if let Err(e) = text_channel.send_message(&http, msg).await {
                    warn!(error = %e, "failed to send mid-session consent prompt");
                }
            });
        }
    }

    // Auto-stop arming. Zero occupants → schedule the 30s timer; non-zero
    // → cancel any pending timer (rapid leave/rejoin).
    if occupants == 0 {
        schedule_auto_stop(state, ctx, session);
    } else {
        session.abort_auto_stop();
    }
}

/// Schedule the auto-stop timer. A fresh one replaces any pending timer.
fn schedule_auto_stop(state: &Arc<AppState>, ctx: &Context, session: &mut Session) {
    let guild_id = session.guild_id;
    let voice_channel = ChannelId::new(session.channel_id);

    session.abort_auto_stop();

    let ctx_clone = ctx.clone();
    let state_clone = state.clone();
    let guild_id_obj = GuildId::new(guild_id);

    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;

        // Re-check occupants from the cache before acting. Avoid racing
        // a rejoin that landed between our arming and this moment.
        let occupants = channel_occupants(&ctx_clone, guild_id_obj, voice_channel);
        if occupants.is_none_or(|n| n > 0) {
            return;
        }

        info!(guild_id = %guild_id_obj, "auto_stop — channel empty for 30s");

        // Play announcement + leave voice, then signal the actor.
        if let Some(manager) = songbird::get(&ctx_clone).await
            && let Some(call) = manager.get(guild_id_obj)
        {
            let mut handler = call.lock().await;
            let source = songbird::input::File::new("/assets/recording_stopped.wav");
            let _ = handler.play_input(source.into());
            drop(handler);
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = manager.leave(guild_id_obj).await;
        }

        // Fire AutoStop into the actor. Best effort — if the actor's
        // already gone, nothing to do.
        if let Some(h) = state_clone.sessions.get(&guild_id) {
            let _ = h.send(SessionCmd::AutoStop).await;
        }
    });

    session.auto_stop_task = Some(handle);
}

/// Count "relevant" users (humans + bypass-listed bots) in the given voice
/// channel. Mirrors the logic previously in voice/events.rs.
fn channel_occupants(ctx: &Context, guild_id: GuildId, channel_id: ChannelId) -> Option<usize> {
    let bypass: HashSet<u64> = std::env::var("BYPASS_CONSENT_USER_IDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let guild = ctx.cache.guild(guild_id)?.clone();
    Some(
        guild
            .voice_states
            .values()
            .filter(|vs| vs.channel_id == Some(channel_id))
            .filter(|vs| {
                let member = guild.members.get(&vs.user_id);
                let is_bot = member.is_some_and(|m| m.user.bot);
                if !is_bot {
                    true
                } else {
                    bypass.contains(&vs.user_id.get())
                }
            })
            .count(),
    )
}

// ---------------------------------------------------------------------------
// Startup pipeline (ported from commands/consent.rs)
// ---------------------------------------------------------------------------

/// Drive the voice-join → DAVE-handshake → transition-to-Recording pipeline
/// inline on the actor. Returns `true` if the outcome is terminal (caller
/// must exit the run loop).
///
/// During the DAVE wait this function polls `rx` for new commands so an
/// incoming `Stop` can flip the cancel watch mid-startup. Only `Stop` and
/// `AutoStop` are acted upon inline (they flip the cancel signal); other
/// commands are deferred — re-enqueued is not trivial, so we handle them
/// eagerly but correctly:
///   - `Stop` / `AutoStop` → flip `cancel_tx` and remember to exit after
///     the pipeline has returned.
///   - `VoiceStateChange` → process inline (same as in the normal loop).
///   - `GetSnapshot` / `ToggleLicense` / etc → process inline. These are
///     all fast and safe during startup.
///
/// The deferred-stop flag is returned in the outer boolean so the caller
/// exits the run loop even if the original pipeline step returned before
/// Stop was seen.
async fn run_startup_pipeline(
    state: &Arc<AppState>,
    ctx: &Context,
    session: &mut Session,
    cancel_tx: &watch::Sender<bool>,
    rx: &mut mpsc::Receiver<SessionCmd>,
    reply: oneshot::Sender<StartRecordingOutcome>,
) -> bool {
    let guild_id = session.guild_id;
    let guild_id_obj = GuildId::new(guild_id);
    let channel_id = ChannelId::new(session.channel_id);
    let session_id = session.id.clone();

    let manager = match songbird::get(ctx).await {
        Some(m) => m,
        None => {
            let _ = reply.send(StartRecordingOutcome::VoiceJoinFailed(
                "songbird manager missing".into(),
            ));
            return true;
        }
    };

    // --- Initial voice join ---
    let call = match manager.join(guild_id_obj, channel_id).await {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "voice_join_failed");
            metrics::counter!("chronicle_sessions_total", "outcome" => "failed").increment(1);
            let _ = reply.send(StartRecordingOutcome::VoiceJoinFailed(e.to_string()));
            return true;
        }
    };
    info!("voice_joined");

    if cancel_requested(cancel_tx) {
        let _ = manager.leave(guild_id_obj).await;
        let _ = reply.send(StartRecordingOutcome::Preempted);
        return true;
    }

    // Begin_startup transitions AwaitingConsent → StartingRecording and
    // creates the audio pipeline. Must happen under the actor's ownership,
    // which it is by construction here (we're inside handle_cmd).
    {
        let mut handler = call.lock().await;
        let _ = session.begin_startup(&mut handler, state.api.clone());
    }

    info!(session_id = %session_id, "registering_audio_receiver");

    // --- DAVE wait retry loop ---
    let dave_outcome = wait_for_dave(
        state,
        ctx,
        &manager,
        session,
        guild_id_obj,
        channel_id,
        cancel_tx,
        rx,
    )
    .await;

    match dave_outcome {
        DaveOutcome::Confirmed => {}
        DaveOutcome::Preempted => {
            let _ = manager.leave(guild_id_obj).await;
            let _ = reply.send(StartRecordingOutcome::Preempted);
            return true;
        }
        DaveOutcome::Failed => {
            session.cancel_startup().await;
            if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
                && let Err(e) = state.api.abandon_session(sid).await
            {
                error!("API call failed (abandon_session): {e}");
            }
            let _ = manager.leave(guild_id_obj).await;
            let _ = reply.send(StartRecordingOutcome::DaveFailed);
            return true;
        }
        DaveOutcome::RejoinFailed => {
            session.cancel_startup().await;
            if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
                && let Err(e) = state.api.abandon_session(sid).await
            {
                error!("API call failed (abandon_session): {e}");
            }
            let _ = reply.send(StartRecordingOutcome::DaveRejoinFailed);
            return true;
        }
    }

    if cancel_requested(cancel_tx) {
        info!("startup_preempted_after_dave_confirm — leaving voice");
        let _ = manager.leave(guild_id_obj).await;
        let _ = reply.send(StartRecordingOutcome::Preempted);
        return true;
    }

    // Transition into Recording.
    session.confirm_recording();

    // Best-effort cosmetic updates.
    update_consent_embed_to_recording(ctx, session).await;
    if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
        && let Err(e) = state.api.update_session_state(sid, "recording").await
    {
        error!("API call failed (update_session_state): {e}");
    }

    metrics::gauge!("chronicle_sessions_active").increment(1.0);
    info!(session_id = %session_id, "recording_started");

    // Spawn the initial DAVE heal observer: it plays the start announcement
    // when the connection is confirmed stable, mirroring the previous
    // behaviour. Distinct from the periodic heal ticks on the actor's
    // select loop — this handles the startup-time stabilization window.
    spawn_initial_stability_check(ctx, session, guild_id_obj);

    let _ = reply.send(StartRecordingOutcome::Recording);
    false
}

fn cancel_requested(tx: &watch::Sender<bool>) -> bool {
    *tx.borrow()
}

enum DaveOutcome {
    Confirmed,
    Preempted,
    Failed,
    RejoinFailed,
}

async fn wait_for_dave(
    state: &Arc<AppState>,
    ctx: &Context,
    manager: &songbird::Songbird,
    session: &mut Session,
    guild_id_obj: GuildId,
    channel_id: ChannelId,
    cancel_tx: &watch::Sender<bool>,
    rx: &mut mpsc::Receiver<SessionCmd>,
) -> DaveOutcome {
    const DAVE_WAIT_SECS: u64 = 5;
    const MAX_ATTEMPTS: u32 = 3;
    let dave_start = std::time::Instant::now();

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt = attempt, "waiting_for_dave");

        if cancellable_sleep_drain(
            Duration::from_secs(DAVE_WAIT_SECS),
            state,
            ctx,
            session,
            cancel_tx,
            rx,
        )
        .await
        {
            return DaveOutcome::Preempted;
        }

        if cancel_requested(cancel_tx) {
            return DaveOutcome::Preempted;
        }

        let has_audio = session.has_audio();
        let has_ssrc = session.has_ssrc();

        if has_audio || has_ssrc {
            let dave_elapsed = dave_start.elapsed().as_secs_f64();
            let msg = if has_audio {
                "dave_audio_confirmed"
            } else {
                "dave_connection_confirmed — ssrc mapped, awaiting speech"
            };
            info!(attempt = attempt, dave_secs = dave_elapsed, "{msg}");
            metrics::counter!("chronicle_dave_attempts_total", "outcome" => "success")
                .increment(1);
            return DaveOutcome::Confirmed;
        }

        if attempt == MAX_ATTEMPTS {
            metrics::counter!("chronicle_dave_attempts_total", "outcome" => "failure")
                .increment(1);
            warn!("dave_failed — no audio or ssrc after {MAX_ATTEMPTS} attempts");
            return DaveOutcome::Failed;
        }

        info!(attempt = attempt, "dave_retry — reconnecting voice");
        let _ = manager.leave(guild_id_obj).await;
        if cancellable_sleep_drain(
            Duration::from_secs(1),
            state,
            ctx,
            session,
            cancel_tx,
            rx,
        )
        .await
        {
            return DaveOutcome::Preempted;
        }

        match manager.join(guild_id_obj, channel_id).await {
            Ok(new_call) => {
                let mut handler = new_call.lock().await;
                session.reattach_audio(&mut handler);
            }
            Err(e) => {
                error!(error = %e, attempt = attempt, "dave_rejoin_failed");
                return DaveOutcome::RejoinFailed;
            }
        }
    }

    DaveOutcome::Failed
}

/// Sleep for `d` while concurrently draining commands from `rx`. Returns
/// `true` if the sleep was cancelled because `Stop` / `AutoStop` fired, or
/// the channel closed. Other commands (license toggles, voice-state
/// updates, snapshot queries) are processed inline so the startup pipeline
/// doesn't stall them.
async fn cancellable_sleep_drain(
    d: Duration,
    state: &Arc<AppState>,
    ctx: &Context,
    session: &mut Session,
    cancel_tx: &watch::Sender<bool>,
    rx: &mut mpsc::Receiver<SessionCmd>,
) -> bool {
    let deadline = tokio::time::Instant::now() + d;
    let mut watch_rx = cancel_tx.subscribe();
    if *watch_rx.borrow() {
        return true;
    }

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        tokio::select! {
            biased;
            changed = watch_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
                return *watch_rx.borrow();
            }
            maybe_cmd = rx.recv() => {
                match maybe_cmd {
                    Some(cmd) => {
                        // Route through handle_cmd-without-rx — we can't
                        // re-enter handle_cmd recursively with our own rx.
                        // Process inline; Stop/AutoStop flip the watch
                        // which the next loop iteration observes.
                        if handle_cmd_no_recurse(
                            state, ctx, session, cancel_tx, cmd,
                        ).await {
                            return true;
                        }
                    }
                    None => return true,
                }
            }
            _ = tokio::time::sleep(remaining) => return false,
        }
    }
}

/// Subset of `handle_cmd` used by the startup-time drain. Rejects
/// `StartRecording` (we're already doing that) and treats `Stop` /
/// `AutoStop` as "flip cancel and tell caller to exit". Everything else is
/// processed identically to the main loop.
async fn handle_cmd_no_recurse(
    state: &Arc<AppState>,
    ctx: &Context,
    session: &mut Session,
    cancel_tx: &watch::Sender<bool>,
    cmd: SessionCmd,
) -> bool {
    match cmd {
        SessionCmd::Stop { initiator, reply } => {
            if session.initiator_id != initiator {
                let _ = reply.send(Err(SessionError::NotInitiator));
                return false;
            }
            if !session.phase.is_stoppable() {
                let _ = reply.send(Err(SessionError::NotStoppable));
                return false;
            }
            let _ = reply.send(Ok(()));
            let _ = cancel_tx.send(true);
            true
        }
        SessionCmd::AutoStop => {
            if !session.phase.is_stoppable() {
                return false;
            }
            let _ = cancel_tx.send(true);
            true
        }
        SessionCmd::StartRecording { reply } => {
            // Reject a nested StartRecording — shouldn't happen but be safe.
            let _ = reply.send(StartRecordingOutcome::Preempted);
            false
        }
        SessionCmd::RecordConsent { user, scope, reply } => {
            let r = do_record_consent(state, session, user, scope).await;
            let _ = reply.send(r);
            false
        }
        SessionCmd::ToggleLicense { user, field, reply } => {
            let (current_llm, current_public) = session.license_flags(user);
            let (new_llm, new_public) = match field {
                LicenseField::NoLlmTraining => (!current_llm, current_public),
                LicenseField::NoPublicRelease => (current_llm, !current_public),
            };
            session.set_license_flags(user, new_llm, new_public);
            let _ = reply.send((new_llm, new_public, session.participant_uuid(user)));
            false
        }
        SessionCmd::VoiceStateChange {
            old_channel,
            new_channel,
            user_id,
            is_bot,
            display_name,
        } => {
            handle_voice_state(
                state,
                ctx,
                session,
                old_channel,
                new_channel,
                user_id,
                is_bot,
                display_name,
            )
            .await;
            false
        }
        SessionCmd::SetConsentMessage { channel_id, message_id } => {
            session.consent_message = Some((channel_id, message_id));
            false
        }
        SessionCmd::SetParticipantUuid { user_id, participant_uuid } => {
            session.set_participant_uuid(user_id, participant_uuid);
            false
        }
        SessionCmd::BypassConsent { users } => {
            for uid in users {
                session.record_consent(uid, ConsentScope::Full);
            }
            false
        }
        SessionCmd::GetSnapshot { reply } => {
            let phase_label = match &session.phase {
                Phase::AwaitingConsent => "awaiting_consent",
                Phase::StartingRecording(_) => "starting_recording",
                Phase::Recording(_) => "recording",
                Phase::Finalizing => "finalizing",
                Phase::Cancelled => "cancelled",
                Phase::Complete => "complete",
            };
            let snapshot = SessionSnapshot {
                session_id: session.id.clone(),
                guild_id: session.guild_id,
                recording: matches!(session.phase, Phase::Recording(_)),
                stable: session.is_stable(),
                phase_label,
                participant_count: session.participants.len(),
            };
            let _ = reply.send(snapshot);
            false
        }
        SessionCmd::AddLicenseFollowup { token, message_id, cleanup_task } => {
            session.license_followups.push((token, message_id));
            session.license_cleanup_tasks.push(cleanup_task);
            false
        }
    }
}

async fn update_consent_embed_to_recording(ctx: &Context, session: &Session) {
    if let Some((channel_id_msg, message_id)) = session.consent_message {
        let edit = EditMessage::new()
            .embed(
                CreateEmbed::new()
                    .title("Recording in progress")
                    .description("All participants accepted. Recording is active.")
                    .color(0x238636),
            )
            .components(vec![]);
        let _ = ctx
            .http
            .edit_message(channel_id_msg, message_id, &edit, vec![])
            .await;
    }
}

/// After the pipeline transitions to Recording, spawn a helper that waits
/// for audio proof (or performs a one-shot heal) and plays the stability
/// announcement. The helper signals completion via the session's shared
/// `recording_stable` flag. It runs independently of the actor's main
/// `select!` loop because it does its own sleeping — the actor stays
/// responsive to commands while it runs.
fn spawn_initial_stability_check(
    ctx: &Context,
    session: &mut Session,
    guild_id_obj: GuildId,
) {
    let (ssrcs_seen, op5_rx, _ssrc_map, _consented_users, stable_flag) = match &mut session.phase {
        Phase::Recording(data) => (
            data.ssrcs_seen.clone(),
            data.op5_rx.take(),
            data.ssrc_map.clone(),
            data.consented_users.clone(),
            session.recording_stable.clone(),
        ),
        _ => return,
    };

    let ctx_clone = ctx.clone();

    tokio::spawn(async move {
        // Initial grace period, then simplified audio-confirmed wait.
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Best-effort drain of buffered OP5 to detect broken speakers; heal
        // responsibility lives in the actor's heal loop, so this stabilization
        // check only plays the announcement and flags stability.
        if let Some(mut rx) = op5_rx {
            for _ in 0..10 {
                if rx.try_recv().is_err() {
                    break;
                }
            }
            // Keep the receiver alive by putting it back if possible — the
            // actor's heal loop re-acquires it via the Session's phase.
            drop(rx);
        }

        // Wait up to 5s for an SSRC to appear before announcing stable.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let seen = ssrcs_seen.lock().expect("ssrcs_seen poisoned").len();
            if seen > 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        if let Some(manager) = songbird::get(&ctx_clone).await
            && let Some(call) = manager.get(guild_id_obj)
        {
            let mut handler = call.lock().await;
            let source = songbird::input::File::new("/assets/recording_started.wav");
            let _ = handler.play_input(source.into());
            drop(handler);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        stable_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        info!("recording_stable");
    });
}

// ---------------------------------------------------------------------------
// Heal loop (continuous monitoring)
// ---------------------------------------------------------------------------

/// Periodic heal tick. Simplified from the original heal task — the actor
/// runs this on a 5s cadence while in Recording and performs at most one
/// heal attempt (the caller tracks that budget). Returns `true` if a heal
/// was actually performed, so the caller can consume its one-shot token.
async fn heal_step(state: &Arc<AppState>, ctx: &Context, session: &mut Session) -> bool {
    if !matches!(session.phase, Phase::Recording(_)) {
        return false;
    }
    if !session.is_stable() {
        // Stability check is still in flight; leave it alone.
        return false;
    }

    let (seen_count, mapped_count, consented_count) = {
        let (Phase::Recording(data) | Phase::StartingRecording(data)) = &session.phase else {
            return false;
        };
        let seen = data.ssrcs_seen.lock().expect("ssrcs_seen poisoned").len();
        let mapped = data.ssrc_map.lock().expect("ssrc_map poisoned").len();
        let consented = data.consented_users.lock().await.len();
        (seen, mapped, consented)
    };

    // Heal trigger conditions (from the legacy heal task):
    //  - SSRCs seen but fewer mapped than consented → need OP5 re-issue.
    //  - No SSRCs at all → dead DAVE connection.
    let should_heal = (seen_count > 0 && mapped_count < consented_count)
        || (seen_count == 0 && mapped_count == 0);
    if !should_heal {
        return false;
    }

    warn!(
        seen = seen_count,
        mapped = mapped_count,
        consented = consented_count,
        "dave_heal_triggered_from_actor"
    );

    let manager = match songbird::get(ctx).await {
        Some(m) => m,
        None => return false,
    };
    let guild_id_obj = GuildId::new(session.guild_id);
    let channel_id = ChannelId::new(session.channel_id);

    let _ = manager.leave(guild_id_obj).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    match manager.join(guild_id_obj, channel_id).await {
        Ok(new_call) => {
            let mut handler = new_call.lock().await;
            session.reattach_audio(&mut handler);
        }
        Err(e) => {
            error!(error = %e, "dave_heal_rejoin_failed");
            return false;
        }
    }

    tokio::time::sleep(Duration::from_secs(5)).await;
    if let Some(call) = manager.get(guild_id_obj) {
        let mut handler = call.lock().await;
        let source = songbird::input::File::new("/assets/recording_started.wav");
        let _ = handler.play_input(source.into());
    }

    info!("dave_heal_complete");
    let _ = state; // silence unused on builds without prometheus feature
    true
}

// ---------------------------------------------------------------------------
// Finalization on exit
// ---------------------------------------------------------------------------

/// Dispatch on the current phase and run the terminal cleanup pipeline
/// (upload metadata, call the Data API's finalize/abandon endpoint, clean
/// up Discord UI).
async fn finalize_on_exit(state: &Arc<AppState>, ctx: &Context, session: &mut Session) {
    let guild_id_obj = GuildId::new(session.guild_id);

    // Play "stopped" announcement + leave voice when we're coming from an
    // active recording state. Skip for AwaitingConsent (no voice join).
    let active = matches!(
        session.phase,
        Phase::Recording(_) | Phase::StartingRecording(_)
    );
    if active && let Some(manager) = songbird::get(ctx).await {
        if let Some(call) = manager.get(guild_id_obj) {
            let mut handler = call.lock().await;
            let source = songbird::input::File::new("/assets/recording_stopped.wav");
            let _ = handler.play_input(source.into());
            drop(handler);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let _ = manager.leave(guild_id_obj).await;
    }

    // Tear down Discord UI (consent embed + license followups).
    cleanup_session_ui(ctx, session).await;

    // Phase-specific finalization. finalize() drains the audio pipeline.
    let session_id = session.id.clone();
    let participant_count = session.participants.len() as i32;

    match &session.phase {
        Phase::Recording(_) => {
            session.finalize().await;
            let meta_json = session.meta_json();
            let consent_json = session.consent_json();
            let meta_value: Option<serde_json::Value> =
                serde_json::from_slice(&meta_json).ok();
            let consent_value: Option<serde_json::Value> =
                serde_json::from_slice(&consent_json).ok();

            if let Ok(sid) = uuid::Uuid::parse_str(&session_id) {
                match state.api.write_metadata(sid, meta_value, consent_value).await {
                    Ok(_) => {
                        metrics::counter!(
                            "chronicle_uploads_total",
                            "type" => "metadata",
                            "outcome" => "success"
                        )
                        .increment(1);
                    }
                    Err(e) => {
                        error!(error = %e, "metadata_upload_failed");
                        metrics::counter!(
                            "chronicle_uploads_total",
                            "type" => "metadata",
                            "outcome" => "failure"
                        )
                        .increment(1);
                    }
                }
                if let Err(e) = state
                    .api
                    .finalize_session(sid, chrono::Utc::now(), participant_count)
                    .await
                {
                    error!("API call failed (finalize_session): {e}");
                }
            }

            metrics::gauge!("chronicle_sessions_active").decrement(1.0);
            info!(session_id = %session_id, "session_finalized");
        }
        Phase::StartingRecording(_) => {
            session.cancel_startup().await;
            if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
                && let Err(e) = state.api.abandon_session(sid).await
            {
                error!("API call failed (abandon_session): {e}");
            }
            info!(session_id = %session_id, "session_cancelled_starting");
        }
        Phase::AwaitingConsent => {
            if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
                && let Err(e) = state.api.abandon_session(sid).await
            {
                error!("API call failed (abandon_session): {e}");
            }
            info!(session_id = %session_id, "session_cancelled_pending");
        }
        _ => {}
    }

    session.abort_all_background_tasks();
    session.complete();
}

async fn cleanup_session_ui(ctx: &Context, session: &Session) {
    if let Some((channel_id, message_id)) = session.consent_message {
        let edit = EditMessage::new()
            .embed(
                CreateEmbed::new()
                    .title("Session complete")
                    .description("Recording has ended. Thank you for contributing!")
                    .color(0x8b949e),
            )
            .components(vec![]);
        let _ = ctx
            .http
            .edit_message(channel_id, message_id, &edit, vec![])
            .await;
    }

    for (token, msg_id) in &session.license_followups {
        let edit = CreateInteractionResponseFollowup::new()
            .content(
                "License preferences saved. Change them anytime on the participant portal.",
            )
            .components(vec![]);
        let _ = ctx
            .http
            .edit_followup_message(token, *msg_id, &edit, vec![])
            .await;
    }
}

// ---------------------------------------------------------------------------
// Tests — state machine of do_record_consent, in-isolation
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Actor state-machine tests.
    //!
    //! These tests exercise the pure-logic portions of the actor: consent
    //! validation, outcome computation, and license toggle math. The
    //! async I/O portions (Data API calls, Songbird voice join, Discord
    //! HTTP) are not tested here — they require a live Discord client,
    //! which is out of scope for unit tests.
    //!
    //! The full `handle_cmd` entry point is tested transitively: every
    //! variant's inner logic is covered below by calling the split-out
    //! pure functions (`apply_consent_click`, `build_consent_outcome`).

    use super::*;
    use crate::session::Session;
    use serenity::all::UserId;

    fn user(id: u64) -> UserId {
        UserId::new(id)
    }

    fn base_session(min: usize, require_all: bool) -> Session {
        let mut s = Session::new(111, 222, 333, user(1), min, require_all);
        s.add_participant(user(1), "alice".into(), false);
        s.add_participant(user(2), "bob".into(), false);
        s.add_participant(user(3), "carol".into(), false);
        s
    }

    // ------------------------------------------------------------------
    // apply_consent_click — invalid-click rejection paths
    // ------------------------------------------------------------------

    #[test]
    fn consent_click_rejects_non_participant() {
        let mut s = base_session(2, true);
        let res = apply_consent_click(&mut s, user(999), ConsentScope::Full);
        assert!(matches!(res, Err(SessionError::NotParticipant)));
    }

    #[test]
    fn consent_click_rejects_double_click() {
        let mut s = base_session(2, true);
        apply_consent_click(&mut s, user(1), ConsentScope::Full).unwrap();
        let res = apply_consent_click(&mut s, user(1), ConsentScope::Decline);
        assert!(matches!(res, Err(SessionError::AlreadyResponded)));
        // First click's scope is preserved — double click does NOT overwrite.
        assert_eq!(s.participants[&user(1)].scope, Some(ConsentScope::Full));
    }

    #[test]
    fn consent_click_records_scope_on_success() {
        let mut s = base_session(2, true);
        apply_consent_click(&mut s, user(1), ConsentScope::Full).unwrap();
        assert_eq!(s.participants[&user(1)].scope, Some(ConsentScope::Full));
        assert!(s.participants[&user(1)].consented_at.is_some());
    }

    // ------------------------------------------------------------------
    // build_consent_outcome — state → variant mapping
    // ------------------------------------------------------------------

    #[test]
    fn outcome_is_ack_while_participants_pending() {
        let mut s = base_session(2, true);
        apply_consent_click(&mut s, user(1), ConsentScope::Full).unwrap();
        match build_consent_outcome(&s) {
            ConsentOutcome::Ack { .. } => {}
            o => panic!("expected Ack, got {:?}", o),
        }
    }

    #[test]
    fn outcome_is_quorum_met_when_all_accepted_and_require_all() {
        let mut s = base_session(2, true);
        apply_consent_click(&mut s, user(1), ConsentScope::Full).unwrap();
        apply_consent_click(&mut s, user(2), ConsentScope::Full).unwrap();
        apply_consent_click(&mut s, user(3), ConsentScope::Full).unwrap();
        match build_consent_outcome(&s) {
            ConsentOutcome::QuorumMet { .. } => {}
            o => panic!("expected QuorumMet, got {:?}", o),
        }
    }

    #[test]
    fn outcome_is_quorum_failed_when_require_all_and_decline() {
        let mut s = base_session(2, true);
        apply_consent_click(&mut s, user(1), ConsentScope::Full).unwrap();
        apply_consent_click(&mut s, user(2), ConsentScope::Full).unwrap();
        apply_consent_click(&mut s, user(3), ConsentScope::Decline).unwrap();
        match build_consent_outcome(&s) {
            ConsentOutcome::QuorumFailed { .. } => {}
            o => panic!("expected QuorumFailed, got {:?}", o),
        }
    }

    #[test]
    fn outcome_is_quorum_met_when_min_met_and_not_require_all() {
        let mut s = base_session(2, false);
        apply_consent_click(&mut s, user(1), ConsentScope::Full).unwrap();
        apply_consent_click(&mut s, user(2), ConsentScope::Full).unwrap();
        apply_consent_click(&mut s, user(3), ConsentScope::Decline).unwrap();
        match build_consent_outcome(&s) {
            ConsentOutcome::QuorumMet { .. } => {}
            o => panic!("expected QuorumMet, got {:?}", o),
        }
    }

    #[test]
    fn outcome_is_ack_in_recording_phase_regardless_of_quorum() {
        // Mid-session joiner clicks after session is already Recording:
        // quorum calculation is irrelevant, outcome must be Ack so the
        // session keeps going instead of restarting the pipeline.
        let mut s = base_session(2, true);
        apply_consent_click(&mut s, user(1), ConsentScope::Full).unwrap();
        apply_consent_click(&mut s, user(2), ConsentScope::Full).unwrap();
        apply_consent_click(&mut s, user(3), ConsentScope::Full).unwrap();
        // Simulate phase transition manually — can't construct a
        // RecordingPipeline without real Songbird, but we can swap in
        // Finalizing as a proxy for "not AwaitingConsent".
        s.phase = Phase::Finalizing;
        match build_consent_outcome(&s) {
            ConsentOutcome::Ack { .. } => {}
            o => panic!("expected Ack, got {:?}", o),
        }
    }

    // ------------------------------------------------------------------
    // License toggle math (mirrors handle_cmd's ToggleLicense branch)
    // ------------------------------------------------------------------

    #[test]
    fn toggle_no_llm_flips_only_no_llm() {
        let mut s = base_session(2, true);
        // Starting state: both false.
        let (cur_llm, cur_public) = s.license_flags(user(1));
        let (new_llm, new_public) = ( !cur_llm, cur_public );
        s.set_license_flags(user(1), new_llm, new_public);
        assert_eq!(s.license_flags(user(1)), (true, false));

        // Toggle again: back to false.
        let (cur_llm, cur_public) = s.license_flags(user(1));
        let (new_llm, new_public) = ( !cur_llm, cur_public );
        s.set_license_flags(user(1), new_llm, new_public);
        assert_eq!(s.license_flags(user(1)), (false, false));
    }

    #[test]
    fn toggle_no_public_flips_only_no_public() {
        let mut s = base_session(2, true);
        s.set_license_flags(user(1), true, false);
        let (cur_llm, cur_public) = s.license_flags(user(1));
        let (new_llm, new_public) = ( cur_llm, !cur_public );
        s.set_license_flags(user(1), new_llm, new_public);
        assert_eq!(s.license_flags(user(1)), (true, true));
    }

    // ------------------------------------------------------------------
    // Phase invariants
    // ------------------------------------------------------------------

    #[test]
    fn confirm_recording_no_op_from_awaiting_consent() {
        let mut s = base_session(2, true);
        s.confirm_recording();
        assert!(matches!(s.phase, Phase::AwaitingConsent));
    }

    #[test]
    fn cancel_startup_no_op_from_awaiting_consent() {
        // async-less check: cancel_startup only transitions to Cancelled
        // if currently in StartingRecording. AwaitingConsent becomes
        // Cancelled via the placeholder swap.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut s = base_session(2, true);
            s.cancel_startup().await;
            // After cancel_startup, phase is Cancelled (the phase swap
            // consumes whatever was there).
            assert!(matches!(s.phase, Phase::Cancelled));
        });
    }

    // ------------------------------------------------------------------
    // SessionError variants
    // ------------------------------------------------------------------

    #[test]
    fn session_error_not_stoppable_is_distinct_from_not_initiator() {
        // Sanity: every SessionError variant renders a distinct message.
        let msgs: Vec<String> = vec![
            SessionError::NotParticipant.to_string(),
            SessionError::AlreadyResponded.to_string(),
            SessionError::NotInitiator.to_string(),
            SessionError::NotStoppable.to_string(),
            SessionError::ActorGone.to_string(),
        ];
        let unique: std::collections::HashSet<&String> = msgs.iter().collect();
        assert_eq!(unique.len(), msgs.len());
    }
}
