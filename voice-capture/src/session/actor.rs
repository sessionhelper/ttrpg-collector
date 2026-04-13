//! Per-session actor.
//!
//! Runs as its own `tokio::spawn`ed task, owns its `Session` exclusively.
//! External handlers send [`SessionCmd`]s over an mpsc and await oneshot
//! replies.
//!
//! The phase machine lives in [`super::phases`]; the gate logic in
//! [`super::stabilization`]; the heal loop in [`super::heal`]; the catastrophic
//! restart in [`super::restart`]; per-participant capture in
//! [`super::participant`] + the per-task code below.
//!
//! ## Cancellation
//!
//! The actor holds a `tokio_util::sync::CancellationToken`. Long-running
//! in-actor work (stabilization wait, heal reconnect, sleeps) polls the
//! token via `tokio::select!` and exits cleanly. Terminal commands (Stop,
//! AutoStop) trip the token.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use chrono::Utc;
use dashmap::DashMap;
use serenity::all::*;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn, Instrument};

use crate::api_client::DataApiClient;
use crate::session::participant::ParticipantState;
use crate::session::phases::Phase;
use crate::session::restart::carry_forward;
use crate::session::stabilization::{self, GateInputs, StreakStatus, StreakTracker};
use crate::session::{consent_buttons, ConsentScope, Session};
use crate::state::AppState;
use crate::storage::pseudonymize;
use crate::voice::buffer::{self, BufferRoot, FlushError, ParticipantCache};
use crate::voice::{AudioHandle, AudioObservables, AudioPacket, AudioReceiver, PacketSink};

// ---------------------------------------------------------------------------
// Errors + command types
// ---------------------------------------------------------------------------

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

/// License flag the harness (or portal) can flip.
#[derive(Debug, Clone, Copy)]
pub enum LicenseField {
    NoLlmTraining,
    NoPublicRelease,
}

/// Outcome of a consent click.
#[derive(Debug)]
pub enum ConsentOutcome {
    Ack { embed: CreateEmbed },
    QuorumFailed { embed: CreateEmbed },
}

/// Snapshot for debug / harness status.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub guild_id: u64,
    pub recording: bool,
    pub stable: bool,
    pub phase_label: &'static str,
    pub participant_count: usize,
}

#[allow(dead_code)]
pub enum SessionCmd {
    RecordConsent {
        user: UserId,
        scope: ConsentScope,
        reply: oneshot::Sender<Result<ConsentOutcome, SessionError>>,
    },
    SetLicense {
        user: UserId,
        field: LicenseField,
        value: bool,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    Stop {
        initiator: UserId,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    AutoStop,
    VoiceStateChange {
        old_channel: Option<ChannelId>,
        new_channel: Option<ChannelId>,
        user_id: UserId,
        is_bot: bool,
        display_name: String,
    },
    SetConsentMessage {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    SetParticipantUuid {
        user_id: UserId,
        participant_uuid: uuid::Uuid,
    },
    /// Harness `POST /enrol`: add a participant after session spawn.
    Enrol {
        user_id: UserId,
        display_name: String,
        is_bot: bool,
        reply: oneshot::Sender<Result<uuid::Uuid, SessionError>>,
    },
    GetSnapshot {
        reply: oneshot::Sender<SessionSnapshot>,
    },
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[allow(dead_code)]
pub struct SessionHandle {
    tx: mpsc::Sender<SessionCmd>,
    pub session_id: String,
    pub shutting_down: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionHandle {
    pub async fn send(&self, cmd: SessionCmd) -> Result<(), SessionError> {
        self.tx.send(cmd).await.map_err(|_| SessionError::ActorGone)
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Request/reply helper — collapses ActorGone + RecvError into one Result.
pub async fn request<T, F>(handle: &SessionHandle, build: F) -> Result<T, SessionError>
where
    F: FnOnce(oneshot::Sender<T>) -> SessionCmd,
{
    let (tx, rx) = oneshot::channel();
    handle.send(build(tx)).await?;
    rx.await.map_err(|_| SessionError::ActorGone)
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

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
    let cancel = CancellationToken::new();
    tokio::spawn(run_actor(state, ctx, session, rx, shutting_down, cancel).instrument(span));
    Ok(handle)
}

// ---------------------------------------------------------------------------
// Per-participant capture task
// ---------------------------------------------------------------------------

enum ParticipantCmd {
    Packet(AudioPacket),
    Accept,
    Decline,
}

const CHUNK_SIZE_BYTES: usize = 2 * 1024 * 1024;

enum Mode {
    CachingToDisk,
    DirectUpload,
    Dropping,
}

struct ParticipantChannel {
    tx: mpsc::Sender<ParticipantCmd>,
    join: JoinHandle<()>,
    #[allow(dead_code)]
    pseudo_id: String,
}

#[allow(clippy::too_many_arguments)]
fn spawn_participant_task(
    session_id: String,
    session_uuid: uuid::Uuid,
    user_id: UserId,
    pseudo_id: String,
    buffer_root: BufferRoot,
    api: Arc<DataApiClient>,
    cancel: CancellationToken,
    initial_state: ParticipantState,
) -> ParticipantChannel {
    let (tx, mut rx) = mpsc::channel::<ParticipantCmd>(1024);
    let pseudo_for_task = pseudo_id.clone();
    let sid_for_task = session_id.clone();
    let span = tracing::info_span!(
        "participant_capture",
        pseudo_id = %pseudo_id,
        user_id = %user_id,
    );
    let join: JoinHandle<()> = tokio::spawn(
        async move {
            let mut cache = match ParticipantCache::open(&buffer_root, &sid_for_task, &pseudo_for_task) {
                Ok(c) => Some(c),
                Err(e) => {
                    error!(error = %e, "participant_cache_open_failed");
                    None
                }
            };
            let mut mode = match initial_state {
                ParticipantState::Pending => Mode::CachingToDisk,
                ParticipantState::Accepted => Mode::DirectUpload,
                ParticipantState::Declined => Mode::Dropping,
            };
            let mut accum: Vec<u8> = Vec::with_capacity(CHUNK_SIZE_BYTES);

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        // Persist the last partial chunk if we were caching.
                        if let (Mode::CachingToDisk, Some(c)) = (&mode, cache.as_mut())
                            && !accum.is_empty()
                            && let Err(e) = c.append_chunk(&accum) {
                            warn!(error = %e, "final_cache_flush_failed");
                        }
                        break;
                    }
                    maybe = rx.recv() => {
                        let Some(cmd) = maybe else { break };
                        match cmd {
                            ParticipantCmd::Packet(p) => {
                                let bytes = samples_to_bytes(p.samples);
                                accum.extend_from_slice(&bytes);
                                if accum.len() < CHUNK_SIZE_BYTES {
                                    continue;
                                }
                                let payload = std::mem::take(&mut accum);
                                match mode {
                                    Mode::CachingToDisk => {
                                        if let Some(c) = cache.as_mut()
                                            && let Err(e) = c.append_chunk(&payload) {
                                            warn!(error = %e, "chunk_cache_write_failed");
                                        }
                                    }
                                    Mode::DirectUpload => {
                                        upload_chunk(&api, session_uuid, &pseudo_for_task, payload).await;
                                    }
                                    Mode::Dropping => {}
                                }
                            }
                            ParticipantCmd::Accept => {
                                info!("participant_accept — flushing cache to api");
                                if let Some(c) = cache.as_ref() {
                                    let dir = c.dir().to_path_buf();
                                    match buffer::flush_and_delete(api.clone(), session_uuid, &pseudo_for_task, dir).await {
                                        Ok(n) => info!(uploaded = n, "participant_cache_flushed"),
                                        Err(e) => error!(error = %e, "participant_cache_flush_failed"),
                                    }
                                }
                                cache = None;
                                mode = Mode::DirectUpload;
                            }
                            ParticipantCmd::Decline => {
                                info!("participant_decline — deleting cache");
                                if let Some(c) = cache.as_ref()
                                    && let Err(e) = c.delete() {
                                    warn!(error = %e, "participant_cache_delete_failed");
                                }
                                cache = None;
                                mode = Mode::Dropping;
                            }
                        }
                    }
                }
            }
            if let Mode::DirectUpload = mode
                && !accum.is_empty() {
                upload_chunk(&api, session_uuid, &pseudo_for_task, accum).await;
            }
        }
        .instrument(span),
    );
    ParticipantChannel {
        tx,
        join,
        pseudo_id,
    }
}

fn samples_to_bytes(samples: Vec<i16>) -> Vec<u8> {
    let mut v = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

async fn upload_chunk(api: &DataApiClient, session: uuid::Uuid, pseudo: &str, bytes: Vec<u8>) {
    let size = bytes.len();
    let start = std::time::Instant::now();
    match api.upload_chunk_with_retry(session, pseudo, bytes).await {
        Ok(_) => {
            metrics::histogram!("chronicle_audio_chunk_upload_seconds")
                .record(start.elapsed().as_secs_f64());
            metrics::counter!("chronicle_audio_chunks_uploaded").increment(1);
            metrics::counter!("chronicle_uploads_total", "type" => "chunk", "outcome" => "success").increment(1);
            info!(size, "chunk_uploaded");
        }
        Err(e) => {
            metrics::counter!("chronicle_uploads_total", "type" => "chunk", "outcome" => "failure").increment(1);
            error!(error = %e, size, "chunk_upload_lost");
        }
    }
}

// ---------------------------------------------------------------------------
// Actor run loop
// ---------------------------------------------------------------------------

struct ActorEnv {
    state: Arc<AppState>,
    ctx: Context,
    cancel: CancellationToken,
    buffer_root: BufferRoot,
    obs: AudioObservables,
    participants: HashMap<UserId, ParticipantChannel>,
    expected_user_ids: Arc<StdMutex<HashSet<u64>>>,
    #[allow(dead_code)]
    op5_rx: Option<mpsc::UnboundedReceiver<crate::voice::Op5Event>>,
    audio_handle: Option<AudioHandle>,
    pending_flush: JoinSet<Result<(), FlushError>>,
}

struct Outcome {
    restart: bool,
}

#[allow(clippy::too_many_arguments)]
async fn run_actor(
    state: Arc<AppState>,
    ctx: Context,
    mut session: Session,
    rx: mpsc::Receiver<SessionCmd>,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    cancel: CancellationToken,
) {
    let buffer_root = BufferRoot::new(
        state.config.local_buffer_dir(),
        state.config.local_buffer_max_secs,
    );
    let expected = {
        let mut s = HashSet::new();
        for (uid, p) in &session.participants {
            if p.state != ParticipantState::Declined {
                s.insert(uid.get());
            }
        }
        Arc::new(StdMutex::new(s))
    };
    let mut env = ActorEnv {
        state: state.clone(),
        ctx: ctx.clone(),
        cancel: cancel.clone(),
        buffer_root,
        obs: AudioObservables::new(),
        participants: HashMap::new(),
        expected_user_ids: expected,
        op5_rx: None,
        audio_handle: None,
        pending_flush: JoinSet::new(),
    };

    let outcome = match join_voice_and_attach(&mut env, &mut session).await {
        Ok(()) => drive_session(&mut env, &mut session, rx).await,
        Err(e) => {
            error!(error = %e, "voice_join_failed");
            metrics::counter!("chronicle_sessions_total", "outcome" => "failed").increment(1);
            Outcome { restart: false }
        }
    };

    shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    env.cancel.cancel();

    if outcome.restart {
        finalize_before_restart(&mut env, &session).await;
        spawn_restart(state.clone(), ctx.clone(), &session);
    } else {
        finalize_normal(&mut env, &mut session).await;
    }

    if let Some(h) = &env.audio_handle {
        h.shutdown();
    }
    state.sessions.remove(&session.guild_id);
    info!(phase = session.phase.label(), "session_actor_exit");
}

async fn join_voice_and_attach(env: &mut ActorEnv, session: &mut Session) -> Result<(), String> {
    let manager = songbird::get(&env.ctx).await.ok_or("songbird manager missing")?;
    let guild_id = GuildId::new(session.guild_id);
    let channel_id = ChannelId::new(session.channel_id);
    let call = manager
        .join(guild_id, channel_id)
        .await
        .map_err(|e| e.to_string())?;

    let session_uuid = uuid::Uuid::parse_str(&session.id).map_err(|e| e.to_string())?;
    for (uid, p) in &session.participants {
        if p.state == ParticipantState::Declined {
            continue;
        }
        let pseudo = pseudonymize(uid.get());
        let ch = spawn_participant_task(
            session.id.clone(),
            session_uuid,
            *uid,
            pseudo,
            env.buffer_root.clone(),
            env.state.api.clone(),
            env.cancel.clone(),
            p.state,
        );
        env.participants.insert(*uid, ch);
    }

    let mut call_lock = call.lock().await;
    let (sink, op5_rx) = build_sink_and_rx(&env.participants);
    let (op5_tx, op5_rx_new) = mpsc::unbounded_channel();
    drop(op5_rx);
    let audio_handle = AudioReceiver::attach(&mut call_lock, sink, env.obs.clone(), op5_tx);
    drop(call_lock);

    env.audio_handle = Some(audio_handle);
    env.op5_rx = Some(op5_rx_new);
    Ok(())
}

/// Construct a `PacketSink` that routes each packet to the right
/// per-participant channel, plus an op5 rx placeholder consumed by the caller.
fn build_sink_and_rx(
    participants: &HashMap<UserId, ParticipantChannel>,
) -> (PacketSink, mpsc::UnboundedReceiver<crate::voice::Op5Event>) {
    let routes: Arc<DashMap<u64, mpsc::Sender<ParticipantCmd>>> = Arc::new(DashMap::new());
    for (uid, ch) in participants {
        routes.insert(uid.get(), ch.tx.clone());
    }
    let sink: PacketSink = {
        let routes = routes.clone();
        Arc::new(move |pkt: AudioPacket| {
            if let Some(sender) = routes.get(&pkt.user_id) {
                let _ = sender.try_send(ParticipantCmd::Packet(pkt));
            }
        })
    };
    let (_tx, rx) = mpsc::unbounded_channel();
    (sink, rx)
}

async fn drive_session(
    env: &mut ActorEnv,
    session: &mut Session,
    mut rx: mpsc::Receiver<SessionCmd>,
) -> Outcome {
    // --- Stabilization gate ---
    let gate_inputs = GateInputs {
        ssrcs_seen: env.obs.ssrcs_seen.clone(),
        ssrc_map: env.obs.ssrc_map.clone(),
        expected_user_ids: env.expected_user_ids.clone(),
    };
    let gate_required = Duration::from_secs(env.state.config.stabilization_gate_secs);
    match wait_for_gate(env, session, &mut rx, &gate_inputs, gate_required).await {
        GateResult::Opened => {
            session.phase = Phase::Recording {
                gate_opened_at: Utc::now(),
            };
            play_announcement(&env.ctx, session.guild_id, "recording_started").await;
            info!("recording_started");
            metrics::gauge!("chronicle_sessions_active").increment(1.0);
            send_consent_embed(&env.ctx, session).await;
        }
        GateResult::Stopped => {
            session.phase = Phase::Cancelled;
            return Outcome { restart: false };
        }
        GateResult::TimedOut => {
            warn!("stabilization_timeout — cancelling session");
            session.phase = Phase::Cancelled;
            return Outcome { restart: false };
        }
    }

    // --- Recording loop ---
    let mut heal_tick = tokio::time::interval(crate::session::heal::HEAL_TICK);
    heal_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heal_tick.tick().await;
    let mut heal_failures: u32 = 0;
    let restart_budget = env.state.restart_budget.clone();

    loop {
        tokio::select! {
            biased;
            _ = env.cancel.cancelled() => {
                info!("actor_cancelled");
                return Outcome { restart: false };
            }
            maybe_cmd = rx.recv() => {
                let Some(cmd) = maybe_cmd else {
                    return Outcome { restart: false };
                };
                if handle_cmd(env, session, cmd).await {
                    return Outcome { restart: false };
                }
            }
            _ = heal_tick.tick() => {
                let Some(manager) = songbird::get(&env.ctx).await else { continue };
                let verdict = crate::session::heal::check_and_heal(
                    &manager,
                    GuildId::new(session.guild_id),
                    ChannelId::new(session.channel_id),
                    &GateInputs {
                        ssrcs_seen: env.obs.ssrcs_seen.clone(),
                        ssrc_map: env.obs.ssrc_map.clone(),
                        expected_user_ids: env.expected_user_ids.clone(),
                    },
                    &mut heal_failures,
                ).await;
                match verdict {
                    crate::session::heal::HealVerdict::BudgetExhausted => {
                        if restart_budget.try_consume(session.guild_id) {
                            session.phase = Phase::Restarting;
                            return Outcome { restart: true };
                        } else {
                            send_unrecoverable(&env.ctx, session).await;
                            return Outcome { restart: false };
                        }
                    }
                    crate::session::heal::HealVerdict::Healed => {
                        env.obs.reset();
                        reattach_audio(env, session).await;
                    }
                    crate::session::heal::HealVerdict::Healthy => {}
                }
            }
            Some(joined) = env.pending_flush.join_next() => {
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!(error = %e, "pending_flush_error"),
                    Err(e) => warn!(error = %e, "pending_flush_join_error"),
                }
            }
        }
    }
}

enum GateResult {
    Opened,
    Stopped,
    TimedOut,
}

async fn wait_for_gate(
    env: &mut ActorEnv,
    session: &mut Session,
    rx: &mut mpsc::Receiver<SessionCmd>,
    gate_inputs: &GateInputs,
    required: Duration,
) -> GateResult {
    let mut tracker = StreakTracker::new(required);
    let mut poll = tokio::time::interval(Duration::from_millis(250));
    let started = std::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = env.cancel.cancelled() => {
                return GateResult::Stopped;
            }
            maybe_cmd = rx.recv() => {
                let Some(cmd) = maybe_cmd else { return GateResult::Stopped };
                if handle_cmd(env, session, cmd).await {
                    return GateResult::Stopped;
                }
            }
            _ = poll.tick() => {
                let verdict = stabilization::evaluate(gate_inputs);
                match tracker.observe(verdict) {
                    StreakStatus::GateOpens { total_secs } => {
                        metrics::histogram!("chronicle_stabilization_gate_secs")
                            .record(total_secs.as_secs_f64());
                        return GateResult::Opened;
                    }
                    StreakStatus::TimedOut => {
                        metrics::histogram!("chronicle_stabilization_gate_secs")
                            .record(started.elapsed().as_secs_f64());
                        return GateResult::TimedOut;
                    }
                    StreakStatus::Streaking | StreakStatus::Unhealthy => {}
                }
            }
        }
    }
}

async fn reattach_audio(env: &mut ActorEnv, session: &Session) {
    let Some(manager) = songbird::get(&env.ctx).await else { return };
    let Some(call) = manager.get(GuildId::new(session.guild_id)) else { return };
    let (sink, _drop_rx) = build_sink_and_rx(&env.participants);
    let (op5_tx, op5_rx) = mpsc::unbounded_channel();
    let mut call_lock = call.lock().await;
    let new_handle = AudioReceiver::attach(&mut call_lock, sink, env.obs.clone(), op5_tx);
    drop(call_lock);
    env.audio_handle = Some(new_handle);
    env.op5_rx = Some(op5_rx);
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

async fn handle_cmd(env: &mut ActorEnv, session: &mut Session, cmd: SessionCmd) -> bool {
    match cmd {
        SessionCmd::RecordConsent { user, scope, reply } => {
            let result = apply_consent(env, session, user, scope).await;
            let failed = matches!(result, Ok(ConsentOutcome::QuorumFailed { .. }));
            let _ = reply.send(result);
            if failed {
                env.cancel.cancel();
            }
            failed
        }
        SessionCmd::SetLicense { user, field, value, reply } => {
            let result = apply_license(&env.state, session, user, field, value).await;
            let _ = reply.send(result);
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
            env.cancel.cancel();
            true
        }
        SessionCmd::AutoStop => {
            if !session.phase.is_stoppable() {
                return false;
            }
            env.cancel.cancel();
            true
        }
        SessionCmd::VoiceStateChange { .. } => {
            // Retained in the command surface; auto-stop timer is outside the
            // scope of this refactor pass and would be re-introduced as a
            // follow-up in a later feature entry.
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
        SessionCmd::Enrol { user_id, display_name, is_bot: _, reply } => {
            let result = apply_enrol(env, session, user_id, display_name).await;
            let _ = reply.send(result);
            false
        }
        SessionCmd::GetSnapshot { reply } => {
            let snap = SessionSnapshot {
                session_id: session.id.clone(),
                guild_id: session.guild_id,
                recording: session.phase.is_recording(),
                stable: session.phase.is_recording(),
                phase_label: session.phase.label(),
                participant_count: session.participants.len(),
            };
            let _ = reply.send(snap);
            false
        }
    }
}

async fn apply_consent(
    env: &mut ActorEnv,
    session: &mut Session,
    user: UserId,
    scope: ConsentScope,
) -> Result<ConsentOutcome, SessionError> {
    if !session.participants.contains_key(&user) {
        return Err(SessionError::NotParticipant);
    }
    if session.participants[&user].scope.is_some() {
        return Err(SessionError::AlreadyResponded);
    }
    session.record_consent(user, scope);

    let session_uuid = uuid::Uuid::parse_str(&session.id).ok();
    let cached_pid = session.participant_uuid(user);

    // Consent PATCH — fire-and-forget.
    let api = env.state.api.clone();
    let scope_str = match scope {
        ConsentScope::Full => "full",
        ConsentScope::Decline => "decline",
    };
    let scope_owned = scope_str.to_string();
    tokio::spawn(async move {
        let res = if let Some(pid) = cached_pid {
            api.record_consent_by_id(pid, &scope_owned).await
        } else if let Some(sid) = session_uuid {
            api.record_consent(sid, user.get(), &scope_owned).await
        } else {
            Ok(())
        };
        if let Err(e) = res {
            error!("record_consent api call failed: {e}");
        }
    });

    // Drive the participant task: Accept → flush + direct mode, Decline → drop.
    if let Some(ch) = env.participants.get(&user) {
        let cmd = match scope {
            ConsentScope::Full => ParticipantCmd::Accept,
            ConsentScope::Decline => ParticipantCmd::Decline,
        };
        let _ = ch.tx.send(cmd).await;
    }

    // F4: on Accept, immediately PATCH default license flags.
    if scope == ConsentScope::Full
        && let Some(pid) = cached_pid {
        let api = env.state.api.clone();
        tokio::spawn(async move {
            if let Err(e) = api
                .set_license_flags_by_id(pid, Some(false), Some(false))
                .await
            {
                error!("set_license_flags (F4 defaults) failed: {e}");
            }
        });
    }

    // Declined users are no longer expected to produce audio.
    if scope == ConsentScope::Decline
        && let Ok(mut g) = env.expected_user_ids.lock() {
        g.remove(&user.get());
    }

    let embed = session.consent_embed();
    let outcome = if session.all_responded() && !session.evaluate_quorum() {
        ConsentOutcome::QuorumFailed { embed }
    } else {
        ConsentOutcome::Ack { embed }
    };
    Ok(outcome)
}

async fn apply_license(
    state: &Arc<AppState>,
    session: &Session,
    user: UserId,
    field: LicenseField,
    value: bool,
) -> Result<(), SessionError> {
    let pid = session
        .participant_uuid(user)
        .ok_or(SessionError::NotParticipant)?;
    let (no_llm, no_public) = match field {
        LicenseField::NoLlmTraining => (Some(value), None),
        LicenseField::NoPublicRelease => (None, Some(value)),
    };
    state
        .api
        .set_license_flags_by_id(pid, no_llm, no_public)
        .await
        .map_err(|_| SessionError::NotParticipant)?;
    Ok(())
}

async fn apply_enrol(
    env: &mut ActorEnv,
    session: &mut Session,
    user_id: UserId,
    display_name: String,
) -> Result<uuid::Uuid, SessionError> {
    session.add_participant(user_id, display_name.clone(), false);
    let session_uuid =
        uuid::Uuid::parse_str(&session.id).map_err(|_| SessionError::NotParticipant)?;
    let row = env
        .state
        .api
        .add_participant(session_uuid, user_id.get(), false, Some(display_name))
        .await
        .map_err(|_| SessionError::NotParticipant)?;
    session.set_participant_uuid(user_id, row.id);

    if let Ok(mut g) = env.expected_user_ids.lock() {
        g.insert(user_id.get());
    }

    let pseudo = pseudonymize(user_id.get());
    let ch = spawn_participant_task(
        session.id.clone(),
        session_uuid,
        user_id,
        pseudo,
        env.buffer_root.clone(),
        env.state.api.clone(),
        env.cancel.clone(),
        ParticipantState::Pending,
    );
    env.participants.insert(user_id, ch);
    Ok(row.id)
}

// ---------------------------------------------------------------------------
// Finalization
// ---------------------------------------------------------------------------

async fn finalize_normal(env: &mut ActorEnv, session: &mut Session) {
    let was_recording = session.phase.is_recording();
    session.phase = Phase::Finalizing;
    session.ended_at = Some(Utc::now());

    if was_recording {
        play_announcement(&env.ctx, session.guild_id, "recording_stopped").await;
    }

    if let Some(manager) = songbird::get(&env.ctx).await {
        let _ = manager.leave(GuildId::new(session.guild_id)).await;
    }

    // Drain per-participant tasks concurrently.
    let mut drains = JoinSet::new();
    for (_, ch) in env.participants.drain() {
        drains.spawn(async move {
            let _ = ch.join.await;
        });
    }
    while drains.join_next().await.is_some() {}

    if was_recording {
        if let Ok(sid) = uuid::Uuid::parse_str(&session.id) {
            let meta_bytes = session.meta_json();
            let consent_bytes = session.consent_json();
            let meta_val: Option<serde_json::Value> = serde_json::from_slice(&meta_bytes).ok();
            let consent_val: Option<serde_json::Value> = serde_json::from_slice(&consent_bytes).ok();
            let api = env.state.api.clone();
            let participant_count = session.participants.len() as i32;
            let (meta_res, final_res) = tokio::join!(
                api.write_metadata(sid, meta_val, consent_val),
                async {
                    let api = env.state.api.clone();
                    api.finalize_session(sid, Utc::now(), participant_count).await
                }
            );
            if let Err(e) = meta_res {
                error!("write_metadata failed: {e}");
            }
            if let Err(e) = final_res {
                error!("finalize_session failed: {e}");
            }
            metrics::gauge!("chronicle_sessions_active").decrement(1.0);
        }
    } else {
        // Never reached Recording → abandoned.
        if let Ok(sid) = uuid::Uuid::parse_str(&session.id)
            && let Err(e) = env.state.api.abandon_session(sid).await {
            error!("abandon_session failed: {e}");
        }
    }

    if let Err(e) = buffer::delete_session(&env.buffer_root, &session.id) {
        warn!(error = %e, "buffer_dir_cleanup_failed");
    }
    cleanup_consent_embed(&env.ctx, session).await;
}

async fn finalize_before_restart(env: &mut ActorEnv, session: &Session) {
    if let Some(manager) = songbird::get(&env.ctx).await {
        let _ = manager.leave(GuildId::new(session.guild_id)).await;
    }
    if let Ok(sid) = uuid::Uuid::parse_str(&session.id)
        && let Err(e) = env.state.api.abandon_session(sid).await
    {
        error!("abandon_session (restart) failed: {e}");
    }
    if let Err(e) = buffer::delete_session(&env.buffer_root, &session.id) {
        warn!(error = %e, "buffer_dir_cleanup_failed (restart)");
    }
    ChannelId::new(session.text_channel_id)
        .say(
            &env.ctx.http,
            "Something went catastrophically wrong, restarting session.",
        )
        .await
        .ok();
}

fn spawn_restart(state: Arc<AppState>, ctx: Context, failed: &Session) {
    let carried = carry_forward(&failed.participants);
    let next = Session::new_carry_forward(
        failed.guild_id,
        failed.channel_id,
        failed.text_channel_id,
        failed.initiator_id,
        state.config.min_participants,
        state.config.require_all_consent,
        carried,
    );
    match spawn_session(state, ctx, next) {
        Ok(_) => info!("catastrophic_restart_spawned"),
        Err(_) => {
            warn!("catastrophic_restart_insert_conflict — guild already had an active session")
        }
    }
}

async fn send_unrecoverable(ctx: &Context, session: &Session) {
    ChannelId::new(session.text_channel_id)
        .say(&ctx.http, "Session unrecoverable — please `/record` again.")
        .await
        .ok();
}

async fn play_announcement(ctx: &Context, guild_id: u64, label: &str) {
    let Some(manager) = songbird::get(ctx).await else { return };
    let Some(call) = manager.get(GuildId::new(guild_id)) else { return };
    let path = match label {
        "recording_started" => "/assets/recording_started.wav",
        "recording_stopped" => "/assets/recording_stopped.wav",
        _ => return,
    };
    let mut handler = call.lock().await;
    let source = songbird::input::File::new(path);
    let _ = handler.play_input(source.into());
    drop(handler);
    tokio::time::sleep(Duration::from_secs(2)).await;
}

async fn send_consent_embed(ctx: &Context, session: &Session) {
    if session.consent_message.is_some() {
        return;
    }
    let embed = session.consent_embed();
    let msg = serenity::all::CreateMessage::new()
        .embed(embed)
        .components(vec![consent_buttons()]);
    let _ = ChannelId::new(session.text_channel_id)
        .send_message(&ctx.http, msg)
        .await;
}

async fn cleanup_consent_embed(ctx: &Context, session: &Session) {
    if let Some((channel_id, msg_id)) = session.consent_message {
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
            .edit_message(channel_id, msg_id, &edit, vec![])
            .await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_error_variants_have_distinct_messages() {
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
