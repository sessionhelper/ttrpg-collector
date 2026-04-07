//! Consent button handler (`/record`'s Accept / Decline flow).
//!
//! This module owns the long-running "consent click → voice join → DAVE
//! handshake → recording_started" pipeline. The top-level entry point is
//! [`handle_consent_button`], which dispatches into smaller functions so
//! the control flow is readable:
//!
//! ```text
//!   handle_consent_button
//!     ├── license button? → commands::license::handle_license_button
//!     ├── record_click                 (validate, ack, record via API,
//!     │                                 handle mid-session joiner)
//!     └── start recording? ──┬── start_recording_pipeline
//!                            │     ├── join_voice_and_begin_startup
//!                            │     ├── wait_for_dave
//!                            │     ├── transition_to_recording
//!                            │     └── send_license_followup
//!                            └── quorum failed? → handle_quorum_failure
//! ```
//!
//! Each sub-function has a single job and a single return-value shape, so
//! tracing the flow top-to-bottom is straightforward. The previous
//! monolithic handler was ~470 lines in one function at indentation depth
//! 7; the split brings every function under ~100 lines.

use std::sync::Arc;
use std::time::Duration;

use serenity::all::*;
use tracing::{error, info, warn};

use crate::commands::license;
use crate::session::{consent_buttons, ConsentScope, Phase};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Handle a consent Accept/Decline click, or delegate to the license-button
/// handler if the click was actually on a license preference followup.
#[tracing::instrument(
    skip_all,
    fields(
        guild_id = component.guild_id.map(|g| g.get()),
        user_id = %component.user.id,
    )
)]
pub async fn handle_consent_button(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    // License buttons share the same `Interaction::Component` dispatch, so
    // we branch on custom_id up-front.
    let scope = match component.data.custom_id.as_str() {
        "consent_accept" => ConsentScope::Full,
        "consent_decline" => ConsentScope::Decline,
        "license_no_llm" | "license_no_public" => {
            return license::handle_license_button(ctx, component, state).await;
        }
        _ => return Ok(()),
    };

    let scope_label = match scope {
        ConsentScope::Full => "full",
        ConsentScope::Decline => "decline",
    };
    metrics::counter!("ttrpg_consent_responses_total", "scope" => scope_label).increment(1);

    let outcome = match record_click(ctx, component, state, scope).await? {
        Some(o) => o,
        // None = validation failed, an ephemeral reply was already sent.
        None => return Ok(()),
    };

    let guild_id = component.guild_id.unwrap().get();
    match outcome {
        ClickOutcome::StartRecording {
            session_id,
            channel_id,
        } => {
            start_recording_pipeline(
                ctx,
                component,
                state,
                scope,
                guild_id,
                session_id,
                channel_id,
            )
            .await?;
        }
        ClickOutcome::QuorumFailed => {
            handle_quorum_failure(ctx, component, state, guild_id).await?;
        }
        ClickOutcome::Ack => {}
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Click handling: validate, record locally + via API, decide next action
// ---------------------------------------------------------------------------

/// What happens after a consent click is recorded.
enum ClickOutcome {
    /// Quorum was met by this click. Run the startup pipeline.
    StartRecording {
        session_id: String,
        channel_id: ChannelId,
    },
    /// Everyone responded but quorum wasn't met (someone declined with
    /// `REQUIRE_ALL_CONSENT=true`, or not enough Full accepts). Cancel.
    QuorumFailed,
    /// Nothing further to do — the click was recorded and ack'd (e.g. first
    /// of several participants, or mid-session joiner).
    Ack,
}

/// Record the user's consent click: validate participant eligibility,
/// update the Session in place, ack the Discord interaction, push the
/// decision to the Data API, and (if mid-session) add the user to the
/// live audio capture set.
///
/// Returns `Ok(Some(outcome))` on success, or `Ok(None)` if validation
/// rejected the click and an ephemeral reply was already sent to the user.
async fn record_click(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
    scope: ConsentScope,
) -> Result<Option<ClickOutcome>, serenity::Error> {
    let guild_id = component.guild_id.unwrap().get();
    let user_id = component.user.id;

    // Fast path: lock once, validate eligibility, record consent, decide
    // whether this click triggered quorum. Also snapshot the cached
    // participant UUID (if any) so the Data API call below can hit the
    // fast path.
    let (should_start, is_mid_session_accept, embed, session_id, channel_id, cached_pid) = {
        let mut sessions = state.sessions.lock().await;
        let session = match sessions.get_mut(guild_id) {
            Some(s) => s,
            None => return Ok(None),
        };

        if !session.participants.contains_key(&user_id) {
            component
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("You're not in the voice channel for this session.")
                            .ephemeral(true),
                    ),
                )
                .await?;
            return Ok(None);
        }

        if session.participants[&user_id].scope.is_some() {
            component
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("You've already responded.")
                            .ephemeral(true),
                    ),
                )
                .await?;
            return Ok(None);
        }

        let is_mid_session = matches!(session.phase, Phase::Recording(_))
            && session.participants[&user_id].mid_session_join;

        session.record_consent(user_id, scope);
        info!(user_id = %user_id, scope = ?scope, "consent_recorded");

        let embed = session.consent_embed();
        let should_start = matches!(session.phase, Phase::AwaitingConsent)
            && session.all_responded()
            && session.evaluate_quorum();
        let is_mid_session_accept = is_mid_session && scope == ConsentScope::Full;
        let session_id = session.id.clone();
        let channel_id = ChannelId::new(session.channel_id);
        let cached_pid = session.participant_uuid(user_id);

        (
            should_start,
            is_mid_session_accept,
            embed,
            session_id,
            channel_id,
            cached_pid,
        )
    };

    // Ack the interaction IMMEDIATELY — inside Discord's 3-second window.
    // From here on everything is on the 15-minute followup clock.
    component
        .create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .embed(embed)
                    .components(vec![consent_buttons()]),
            ),
        )
        .await?;

    // Push the decision to the Data API BEFORE any startup path branches.
    // If the pipeline later aborts (DAVE failed, /stop preempts), we still
    // want the consent_audit_log to reflect the user's choice. Best effort
    // — logged on failure, doesn't block the UI.
    //
    // Fast path: if we cached the participant UUID when they were added
    // (the common case, populated by add_participants_batch during
    // /record), skip the 3-hop find_participant dance. Cold cache — e.g.
    // bot restart mid-session — falls through to the discord-id lookup.
    let scope_str = match scope {
        ConsentScope::Full => "full",
        ConsentScope::Decline => "decline",
    };
    let api_result = if let Some(pid) = cached_pid {
        state.api.record_consent_by_id(pid, scope_str).await
    } else if let Ok(sid) = uuid::Uuid::parse_str(&session_id) {
        state.api.record_consent(sid, user_id.get(), scope_str).await
    } else {
        Ok(())
    };
    if let Err(e) = api_result {
        error!("API call failed (record_consent): {e}");
    }

    // Mid-session joiner accepted — add them to the live capture set so
    // their audio starts flowing.
    if is_mid_session_accept {
        add_mid_session_consent(state, guild_id, user_id).await;
    }

    Ok(Some(if should_start {
        ClickOutcome::StartRecording {
            session_id,
            channel_id,
        }
    } else {
        // Check for quorum failure as a separate state. all_responded
        // implies the session is terminal one way or the other.
        let quorum_failed = {
            let sessions = state.sessions.lock().await;
            sessions
                .get(guild_id)
                .is_some_and(|s| s.all_responded() && !s.evaluate_quorum())
        };
        if quorum_failed {
            ClickOutcome::QuorumFailed
        } else {
            ClickOutcome::Ack
        }
    }))
}

/// Add a mid-session joiner to the live `consented_users` set. Valid for
/// both `StartingRecording` and `Recording` phases since the set lives on
/// `RecordingPipeline`.
async fn add_mid_session_consent(state: &AppState, guild_id: u64, user_id: UserId) {
    let consented_users = {
        let sessions = state.sessions.lock().await;
        sessions.get(guild_id).and_then(|s| match &s.phase {
            Phase::StartingRecording(data) | Phase::Recording(data) => {
                Some(data.consented_users.clone())
            }
            _ => None,
        })
    };
    if let Some(cu) = consented_users {
        let mut set = cu.lock().await;
        set.insert(user_id.get());
        info!(user_id = %user_id, "mid_session_consent_granted — audio capture enabled");
    }
}

// ---------------------------------------------------------------------------
// Startup pipeline: voice join → DAVE wait → transition to Recording
// ---------------------------------------------------------------------------

/// Outcome of the headless recording startup path, used by the E2E harness.
///
/// Mirrors the set of terminal states that `start_recording_pipeline`
/// handles internally, but surfaces them to the caller so the harness
/// endpoint can return meaningful HTTP statuses instead of swallowing the
/// failure.
#[derive(Debug)]
pub(crate) enum HeadlessStartOutcome {
    /// Recording is live.
    Recording,
    /// The pipeline was cancelled mid-startup (e.g. by a concurrent /stop).
    Preempted,
    /// DAVE never delivered audio after MAX_ATTEMPTS retries.
    DaveFailed,
    /// A mid-loop voice rejoin itself failed.
    DaveRejoinFailed,
    /// The initial voice-join call errored (serenity/songbird).
    VoiceJoinFailed(String),
}

/// Headless variant of `start_recording_pipeline` for the dev-only E2E
/// harness endpoint in `crate::harness`.
///
/// Mirrors the slash-command path's voice-join → DAVE-wait → transition-to-
/// recording sequence but strips the four `component.channel_id.say()`
/// user-facing messages and the license followup (there's no human in the
/// channel for the harness to address). Returns a `HeadlessStartOutcome`
/// so the caller can translate it into an HTTP response.
pub(crate) async fn start_recording_headless(
    ctx: &Context,
    state: &AppState,
    guild_id: u64,
    guild_id_obj: GuildId,
    session_id: String,
    channel_id: ChannelId,
) -> HeadlessStartOutcome {
    info!("quorum_met — joining voice channel (headless)");
    let manager = songbird::get(ctx).await.unwrap();

    let _call = match join_voice_and_begin_startup(
        state,
        &manager,
        guild_id,
        guild_id_obj,
        channel_id,
        &session_id,
    )
    .await
    {
        StartupStep::Proceed(call) => call,
        StartupStep::Aborted => return HeadlessStartOutcome::Preempted,
        StartupStep::VoiceJoinFailed(e) => {
            error!(error = %e, "voice_join_failed (headless)");
            metrics::counter!("ttrpg_sessions_total", "outcome" => "failed").increment(1);
            let mut sessions = state.sessions.lock().await;
            sessions.remove(guild_id);
            return HeadlessStartOutcome::VoiceJoinFailed(e.to_string());
        }
    };

    info!(session_id = %session_id, "registering_audio_receiver (headless)");

    match wait_for_dave(
        state,
        &manager,
        guild_id,
        guild_id_obj,
        channel_id,
        &session_id,
    )
    .await
    {
        DaveOutcome::Confirmed => {}
        DaveOutcome::Preempted => {
            let _ = manager.leave(guild_id_obj).await;
            return HeadlessStartOutcome::Preempted;
        }
        DaveOutcome::Failed => {
            cancel_and_abandon(state, guild_id, &session_id).await;
            let _ = manager.leave(guild_id_obj).await;
            return HeadlessStartOutcome::DaveFailed;
        }
        DaveOutcome::RejoinFailed => {
            cancel_and_abandon(state, guild_id, &session_id).await;
            return HeadlessStartOutcome::DaveRejoinFailed;
        }
    }

    if pipeline_aborted(state, guild_id, &session_id).await {
        info!("startup_preempted_after_dave_confirm — leaving voice (headless)");
        let _ = manager.leave(guild_id_obj).await;
        return HeadlessStartOutcome::Preempted;
    }
    transition_to_recording(state, guild_id, &session_id).await;

    // Don't play the start announcement yet — the DAVE heal task will
    // play it once the initial check passes or heal completes. This is
    // the contract: announcement = recording is stable, players can talk.
    if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
        && let Err(e) = state.api.update_session_state(sid, "recording").await
    {
        error!("API call failed (update_session_state, headless): {e}");
    }

    metrics::gauge!("ttrpg_sessions_active").increment(1.0);
    info!(session_id = %session_id, "recording_started (headless)");

    // Spawn DAVE heal monitor — runs for the lifetime of the session.
    // Plays the start announcement once stable.
    spawn_dave_heal_task(
        state.sessions.clone(),
        &manager,
        guild_id,
        guild_id_obj,
        channel_id,
        session_id.clone(),
    );

    HeadlessStartOutcome::Recording
}

/// Top-level orchestrator for the full recording startup sequence. Runs
/// after quorum is met and the consent interaction has been ack'd.
async fn start_recording_pipeline(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
    scope: ConsentScope,
    guild_id: u64,
    session_id: String,
    channel_id: ChannelId,
) -> Result<(), serenity::Error> {
    info!("quorum_met — joining voice channel");
    let guild_id_obj = component.guild_id.unwrap();
    let manager = songbird::get(ctx).await.unwrap();

    let call = match join_voice_and_begin_startup(
        state,
        &manager,
        guild_id,
        guild_id_obj,
        channel_id,
        &session_id,
    )
    .await
    {
        StartupStep::Proceed(call) => call,
        StartupStep::Aborted => return Ok(()),
        StartupStep::VoiceJoinFailed(e) => {
            error!(error = %e, "voice_join_failed");
            metrics::counter!("ttrpg_sessions_total", "outcome" => "failed").increment(1);
            component
                .channel_id
                .say(&ctx.http, "Failed to join voice channel.")
                .await?;
            let mut sessions = state.sessions.lock().await;
            sessions.remove(guild_id);
            return Ok(());
        }
    };

    info!(session_id = %session_id, "registering_audio_receiver");

    match wait_for_dave(
        state,
        &manager,
        guild_id,
        guild_id_obj,
        channel_id,
        &session_id,
    )
    .await
    {
        DaveOutcome::Confirmed => {}
        DaveOutcome::Preempted => {
            let _ = manager.leave(guild_id_obj).await;
            return Ok(());
        }
        DaveOutcome::Failed => {
            cancel_and_abandon(state, guild_id, &session_id).await;
            let _ = manager.leave(guild_id_obj).await;
            component
                .channel_id
                .say(&ctx.http, "Unable to receive audio. Try `/record` again.")
                .await?;
            return Ok(());
        }
        DaveOutcome::RejoinFailed => {
            cancel_and_abandon(state, guild_id, &session_id).await;
            component
                .channel_id
                .say(&ctx.http, "Failed to reconnect to voice. Try `/record` again.")
                .await?;
            return Ok(());
        }
    }

    // DAVE confirmed. Transition the session into Recording and do the
    // "you're now live" cosmetic work — UI update, announcement, data-api
    // state push, license followup.
    if pipeline_aborted(state, guild_id, &session_id).await {
        info!("startup_preempted_after_dave_confirm — leaving voice");
        let _ = manager.leave(guild_id_obj).await;
        return Ok(());
    }
    transition_to_recording(state, guild_id, &session_id).await;

    // Don't play the start announcement yet — the DAVE heal task will
    // play it once the initial check passes or heal completes.
    update_consent_embed_to_recording(ctx, state, guild_id).await;
    if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
        && let Err(e) = state.api.update_session_state(sid, "recording").await
    {
        error!("API call failed (update_session_state): {e}");
    }

    metrics::gauge!("ttrpg_sessions_active").increment(1.0);
    info!(session_id = %session_id, "recording_started");

    // Spawn DAVE heal monitor — plays the start announcement once stable.
    spawn_dave_heal_task(
        state.sessions.clone(),
        &manager,
        guild_id,
        guild_id_obj,
        channel_id,
        session_id.clone(),
    );

    component
        .channel_id
        .say(&ctx.http, "Recording. Use `/stop` when done.")
        .await?;

    if scope == ConsentScope::Full {
        send_license_followup(ctx, component, state, guild_id).await;
    }

    let _ = call; // keep the call handle alive until here to be explicit
    Ok(())
}

/// Outcome of the initial voice-join + begin_startup step.
enum StartupStep {
    Proceed(Arc<tokio::sync::Mutex<songbird::Call>>),
    Aborted,
    VoiceJoinFailed(songbird::error::JoinError),
}

/// Join the voice channel and transition the session to
/// `StartingRecording`, creating the audio pipeline. Phase-guarded against
/// a concurrently-replaced session.
async fn join_voice_and_begin_startup(
    state: &AppState,
    manager: &songbird::Songbird,
    guild_id: u64,
    guild_id_obj: GuildId,
    channel_id: ChannelId,
    expected_id: &str,
) -> StartupStep {
    let call = match manager.join(guild_id_obj, channel_id).await {
        Ok(c) => c,
        Err(e) => return StartupStep::VoiceJoinFailed(e),
    };
    info!("voice_joined");

    if pipeline_aborted(state, guild_id, expected_id).await {
        info!("startup_preempted_after_voice_join — leaving voice");
        let _ = manager.leave(guild_id_obj).await;
        return StartupStep::Aborted;
    }

    {
        let mut sessions = state.sessions.lock().await;
        match sessions.get_mut(guild_id) {
            Some(s) if s.id == expected_id => {
                let mut handler = call.lock().await;
                let _ = s.begin_startup(&mut handler, state.api.clone());
            }
            _ => {
                info!("session_replaced_before_begin_startup");
                drop(sessions);
                let _ = manager.leave(guild_id_obj).await;
                return StartupStep::Aborted;
            }
        }
    }

    StartupStep::Proceed(call)
}

/// Outcome of the DAVE-wait retry loop.
enum DaveOutcome {
    /// DAVE delivered audio or mapped an SSRC within the retry budget.
    Confirmed,
    /// /stop fired during the wait (phase is no longer StartingRecording).
    Preempted,
    /// Max retries exhausted without audio or SSRC.
    Failed,
    /// A mid-loop voice rejoin itself failed.
    RejoinFailed,
}

/// Poll for DAVE handshake completion, re-joining voice up to MAX_ATTEMPTS
/// times. This is the messiest part of the pipeline because DAVE's initial
/// key exchange can hang and the only known workaround is leave+rejoin.
async fn wait_for_dave(
    state: &AppState,
    manager: &songbird::Songbird,
    guild_id: u64,
    guild_id_obj: GuildId,
    channel_id: ChannelId,
    expected_id: &str,
) -> DaveOutcome {
    const DAVE_WAIT_SECS: u64 = 5;
    const MAX_ATTEMPTS: u32 = 3;
    let dave_start = std::time::Instant::now();

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt = attempt, "waiting_for_dave");
        tokio::time::sleep(std::time::Duration::from_secs(DAVE_WAIT_SECS)).await;

        if pipeline_aborted(state, guild_id, expected_id).await {
            info!("startup_preempted_during_dave_wait");
            return DaveOutcome::Preempted;
        }

        let (has_audio, has_ssrc) = {
            let sessions = state.sessions.lock().await;
            match sessions.get(guild_id) {
                Some(s) if s.id == expected_id => (s.has_audio(), s.has_ssrc()),
                _ => {
                    info!("session_replaced_during_dave_wait");
                    return DaveOutcome::Preempted;
                }
            }
        };

        if has_audio || has_ssrc {
            let dave_elapsed = dave_start.elapsed().as_secs_f64();
            let msg = if has_audio {
                "dave_audio_confirmed"
            } else {
                "dave_connection_confirmed — ssrc mapped, awaiting speech"
            };
            info!(attempt = attempt, dave_secs = dave_elapsed, "{msg}");
            metrics::counter!("ttrpg_dave_attempts_total", "outcome" => "success").increment(1);
            return DaveOutcome::Confirmed;
        }

        if attempt == MAX_ATTEMPTS {
            metrics::counter!("ttrpg_dave_attempts_total", "outcome" => "failure").increment(1);
            warn!("dave_failed — no audio or ssrc after {MAX_ATTEMPTS} attempts");
            return DaveOutcome::Failed;
        }

        // Leave + rejoin to re-negotiate the DAVE key exchange.
        info!(attempt = attempt, "dave_retry — reconnecting voice");
        let _ = manager.leave(guild_id_obj).await;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        if pipeline_aborted(state, guild_id, expected_id).await {
            info!("startup_preempted_between_dave_retries");
            return DaveOutcome::Preempted;
        }

        match manager.join(guild_id_obj, channel_id).await {
            Ok(new_call) => {
                let mut sessions = state.sessions.lock().await;
                match sessions.get_mut(guild_id) {
                    Some(s) if s.id == expected_id => {
                        let mut handler = new_call.lock().await;
                        s.reattach_audio(&mut handler);
                    }
                    _ => {
                        info!("session_replaced_during_dave_rejoin");
                        return DaveOutcome::Preempted;
                    }
                }
            }
            Err(e) => {
                error!(error = %e, attempt = attempt, "dave_rejoin_failed");
                return DaveOutcome::RejoinFailed;
            }
        }
    }

    // Unreachable: the loop either returns or continues via `continue`. But
    // an explicit fallback keeps the compiler happy without an `unreachable!()`.
    DaveOutcome::Failed
}

/// Tear down an in-progress startup: transition the session to Cancelled,
/// mark the Data API row abandoned, abort license cleanup tasks, remove
/// from the manager. Used by both the dave_failed and dave_rejoin_failed
/// paths.
async fn cancel_and_abandon(state: &AppState, guild_id: u64, session_id: &str) {
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(s) = sessions.get_mut(guild_id) {
            s.cancel_startup().await;
        }
    }
    if let Ok(sid) = uuid::Uuid::parse_str(session_id)
        && let Err(e) = state.api.abandon_session(sid).await
    {
        error!("API call failed (abandon_session): {e}");
    }
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(s) = sessions.get_mut(guild_id) {
            s.abort_all_background_tasks();
        }
        sessions.remove(guild_id);
    }
}

/// StartingRecording → Recording. Idempotent and phase-guarded.
async fn transition_to_recording(state: &AppState, guild_id: u64, expected_id: &str) {
    let mut sessions = state.sessions.lock().await;
    if let Some(s) = sessions.get_mut(guild_id)
        && s.id == expected_id
    {
        s.confirm_recording();
    }
}

async fn play_start_announcement(manager: &songbird::Songbird, guild_id: GuildId) {
    if let Some(call) = manager.get(guild_id) {
        let mut handler = call.lock().await;
        let source = songbird::input::File::new("/assets/recording_started.wav");
        let _ = handler.play_input(source.into());
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
}

async fn update_consent_embed_to_recording(
    ctx: &Context,
    state: &AppState,
    guild_id: u64,
) {
    let consent_msg = {
        let sessions = state.sessions.lock().await;
        sessions.get(guild_id).and_then(|s| s.consent_message)
    };
    if let Some((channel_id_msg, message_id)) = consent_msg {
        let edit = EditMessage::new()
            .embed(
                CreateEmbed::new()
                    .title("Recording in progress")
                    .description("All participants accepted. Recording is active.")
                    .color(0x238636),
            )
            .components(vec![]);
        let _ = ctx.http.edit_message(channel_id_msg, message_id, &edit, vec![]).await;
    }
}

/// Send the ephemeral license-preference followup (No LLM Training /
/// No Public Release buttons) and spawn a background task to clear the
/// buttons 14 minutes later (Discord's followup token expires at 15m).
async fn send_license_followup(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
    guild_id: u64,
) {
    let followup = CreateInteractionResponseFollowup::new()
        .content(
            "Your audio defaults to **public dataset + LLM training**. \
             Toggle restrictions below:",
        )
        .ephemeral(true)
        .components(vec![CreateActionRow::Buttons(vec![
            CreateButton::new("license_no_llm")
                .label("No LLM Training")
                .style(ButtonStyle::Secondary),
            CreateButton::new("license_no_public")
                .label("No Public Release")
                .style(ButtonStyle::Secondary),
        ])]);

    let msg = match component.create_followup(&ctx.http, followup).await {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "license_followup_failed");
            return;
        }
    };

    {
        let mut sessions = state.sessions.lock().await;
        if let Some(session) = sessions.get_mut(guild_id) {
            session
                .license_followups
                .push((component.token.clone(), msg.id));
        }
    }

    // Spawn a cleanup task to disable the buttons before the interaction
    // token expires. Stored on the session so /stop can abort it.
    let http = ctx.http.clone();
    let interaction_token = component.token.clone();
    let msg_id = msg.id;
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(14 * 60)).await;
        let edit = EditInteractionResponse::new().components(vec![]);
        let _ = http
            .edit_followup_message(&interaction_token, msg_id, &edit, vec![])
            .await;
    });
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(session) = sessions.get_mut(guild_id) {
            session.license_cleanup_tasks.push(handle);
        }
    }
}

// ---------------------------------------------------------------------------
// DAVE heal: detect speakers with broken decryptors and reconnect once
// ---------------------------------------------------------------------------

/// Spawn a DAVE health monitor that runs for the lifetime of the session.
///
/// **Initial check (startup):** After a 5s grace period, checks whether
/// consented speakers are healthy via OP5 + SSRC presence + fallback.
/// If broken, performs a one-shot heal (leave → rejoin). Sets the
/// `recording_stable` flag once the check passes or heal completes.
///
/// **Continuous monitoring:** After the initial check, keeps consuming
/// OP5 events. If at any point during the session an OP5 fires and the
/// SSRC doesn't appear in VoiceTick within 2s, triggers a heal. The
/// heal is still one-shot — only one reconnect per session.
///
/// Exits when the `op5_rx` channel closes (session finalized).
fn spawn_dave_heal_task(
    sessions: Arc<tokio::sync::Mutex<crate::session::SessionManager>>,
    manager: &Arc<songbird::Songbird>,
    guild_id: u64,
    guild_id_obj: GuildId,
    channel_id: ChannelId,
    session_id: String,
) {
    let heal_sessions = sessions;
    let heal_manager = manager.clone();

    tokio::spawn(async move {
        // Take the op5_rx and shared state from the session.
        let (mut op5_rx, ssrcs_seen, ssrc_map, consented_count, stable_flag) = {
            let mut sessions = heal_sessions.lock().await;
            let Some(s) = sessions.get_mut(guild_id) else {
                return;
            };
            let stable = s.recording_stable.clone();
            match &mut s.phase {
                Phase::Recording(data) | Phase::StartingRecording(data) => {
                    let rx = data.op5_rx.take();
                    let seen = data.ssrcs_seen.clone();
                    let map = data.ssrc_map.clone();
                    let count = data.consented_users.lock().await.len();
                    (rx, seen, map, count, stable)
                }
                _ => return,
            }
        };

        let Some(ref mut op5_rx) = op5_rx else {
            warn!(session_id = %session_id, "dave_heal — no op5_rx available");
            return;
        };

        let mut healed = false;

        // --- Initial startup check ---
        // Grace period: let MLS group stabilize after collector joins.
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Drain buffered OP5 events, check each against ssrcs_seen
        let mut broken_ssrcs: Vec<(u32, u64)> = Vec::new();
        while let Ok(evt) = op5_rx.try_recv() {
            let seen = ssrcs_seen.lock().expect("ssrcs_seen poisoned");
            if !seen.contains(&evt.ssrc) {
                broken_ssrcs.push((evt.ssrc, evt.user_id));
            }
        }

        // Give broken speakers 2s more to appear
        if !broken_ssrcs.is_empty() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let seen = ssrcs_seen.lock().expect("ssrcs_seen poisoned");
            broken_ssrcs.retain(|(ssrc, _)| !seen.contains(ssrc));
        }

        // Wait up to 10s for late OP5 arrivals
        if broken_ssrcs.is_empty() {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, op5_rx.recv()).await {
                    Ok(Some(evt)) => {
                        tokio::time::sleep(Duration::from_secs(2).min(
                            deadline.saturating_duration_since(tokio::time::Instant::now()),
                        )).await;
                        let seen = ssrcs_seen.lock().expect("ssrcs_seen poisoned");
                        if !seen.contains(&evt.ssrc) {
                            broken_ssrcs.push((evt.ssrc, evt.user_id));
                            break;
                        }
                    }
                    Ok(None) => return, // Session ended
                    Err(_) => break,    // Deadline — all good
                }
            }
        }

        // Fallback: OP5 is edge-triggered. If speakers were already
        // transmitting when the collector joined, OP5 never fires and the
        // check above passes vacuously. Detect this by comparing ssrc_map
        // against ssrcs_seen: if VoiceTick is delivering SSRCs that OP5
        // never mapped, the edge-trigger was missed and DAVE is likely broken.
        //
        // If seen=0 AND mapped=0, nobody is transmitting — that's normal
        // (e.g. feeders waiting for the stable signal, or players not
        // talking yet). Do NOT heal in this case.
        let needs_heal = if !broken_ssrcs.is_empty() {
            true
        } else {
            let mapped = ssrc_map.lock().expect("ssrc_map poisoned").len();
            let seen = ssrcs_seen.lock().expect("ssrcs_seen poisoned").len();
            if seen > 0 && mapped < consented_count {
                // SSRCs in VoiceTick but not all mapped via OP5 — edge-trigger
                // missed, DAVE decryption likely broken for unmapped speakers.
                warn!(
                    session_id = %session_id,
                    mapped, seen, consented = consented_count,
                    "dave_heal_triggered — SSRCs seen but only {} of {} mapped",
                    mapped, consented_count,
                );
                true
            } else {
                info!(
                    session_id = %session_id,
                    mapped, seen,
                    "dave_heal_initial_check_passed"
                );
                false
            }
        };

        if needs_heal {
            if !broken_ssrcs.is_empty() {
                warn!(
                    session_id = %session_id,
                    broken = ?broken_ssrcs.iter().map(|(s, u)| format!("{}(uid={})", s, u)).collect::<Vec<_>>(),
                    "dave_heal_triggered — OP5 fired but SSRC missing from VoiceTick"
                );
            }
            healed = do_heal(
                &heal_sessions, &heal_manager, guild_id, guild_id_obj,
                channel_id, &session_id,
            ).await;
        }

        // Signal stable — either check passed or heal completed/failed.
        // Play the start announcement NOW. This is the contract: the
        // announcement means "recording is stable, you can talk."
        if !healed {
            // If we healed, do_heal already played the announcement.
            play_start_announcement(&heal_manager, guild_id_obj).await;
        }
        stable_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        info!(session_id = %session_id, healed, "recording_stable");

        // --- Continuous monitoring ---
        // Two detection paths run in parallel:
        // 1. OP5 events: if a new OP5 fires and the SSRC doesn't appear
        //    in VoiceTick within 2s, DAVE is broken.
        // 2. Periodic fallback (every 10s): if ssrcs_seen has entries that
        //    ssrc_map doesn't, OP5 was edge-triggered away and we missed it.
        //    This catches the case where feeders were already in "speaking"
        //    state before the collector joined.
        let mut fallback_interval = tokio::time::interval(Duration::from_secs(10));
        fallback_interval.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                evt = op5_rx.recv() => {
                    let Some(evt) = evt else {
                        info!(session_id = %session_id, "dave_monitor_exit — session ended");
                        return;
                    };

                    // Already seen this SSRC? Skip.
                    {
                        let seen = ssrcs_seen.lock().expect("ssrcs_seen poisoned");
                        if seen.contains(&evt.ssrc) {
                            continue;
                        }
                    }

                    // New SSRC from OP5 — give 2s for VoiceTick to confirm
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    {
                        let seen = ssrcs_seen.lock().expect("ssrcs_seen poisoned");
                        if seen.contains(&evt.ssrc) {
                            continue;
                        }
                    }

                    if healed {
                        warn!(
                            session_id = %session_id,
                            ssrc = evt.ssrc, user_id = evt.user_id,
                            "dave_broken_mid_session — already healed once, cannot retry"
                        );
                        continue;
                    }

                    warn!(
                        session_id = %session_id,
                        ssrc = evt.ssrc, user_id = evt.user_id,
                        "dave_heal_triggered_mid_session — OP5 fired but SSRC missing"
                    );
                    healed = do_heal(
                        &heal_sessions, &heal_manager, guild_id, guild_id_obj,
                        channel_id, &session_id,
                    ).await;

                    // After heal, take the new op5_rx from the fresh Call
                    let new_rx = {
                        let mut sessions = heal_sessions.lock().await;
                        sessions.get_mut(guild_id).and_then(|s| match &mut s.phase {
                            Phase::Recording(data) => data.op5_rx.take(),
                            _ => None,
                        })
                    };
                    match new_rx {
                        Some(rx) => *op5_rx = rx,
                        None => {
                            info!(session_id = %session_id, "dave_monitor_exit — no new op5_rx after heal");
                            return;
                        }
                    }
                }

                _ = fallback_interval.tick() => {
                    if healed { continue; }

                    let seen_count = ssrcs_seen.lock().expect("ssrcs_seen poisoned").len();
                    let mapped_count = ssrc_map.lock().expect("ssrc_map poisoned").len();

                    if seen_count > 0 && mapped_count < consented_count {
                        warn!(
                            session_id = %session_id,
                            seen = seen_count, mapped = mapped_count, consented = consented_count,
                            "dave_heal_triggered — SSRCs in VoiceTick but unmapped (OP5 edge-trigger missed)"
                        );
                        healed = do_heal(
                            &heal_sessions, &heal_manager, guild_id, guild_id_obj,
                            channel_id, &session_id,
                        ).await;

                        let new_rx = {
                            let mut sessions = heal_sessions.lock().await;
                            sessions.get_mut(guild_id).and_then(|s| match &mut s.phase {
                                Phase::Recording(data) => data.op5_rx.take(),
                                _ => None,
                            })
                        };
                        match new_rx {
                            Some(rx) => *op5_rx = rx,
                            None => {
                                info!(session_id = %session_id, "dave_monitor_exit — no new op5_rx after heal");
                                return;
                            }
                        }
                    }
                }
            }
        }
    });
}

/// Execute a single heal: leave voice, wait, rejoin, reattach, play announcement.
/// Returns true if the rejoin succeeded.
async fn do_heal(
    sessions: &Arc<tokio::sync::Mutex<crate::session::SessionManager>>,
    manager: &Arc<songbird::Songbird>,
    guild_id: u64,
    guild_id_obj: GuildId,
    channel_id: ChannelId,
    session_id: &str,
) -> bool {
    // Check session still active
    {
        let s = sessions.lock().await;
        match s.get(guild_id) {
            Some(s) if s.id == session_id && !s.phase.is_terminal() => {}
            _ => {
                info!(session_id = %session_id, "dave_heal_aborted — session ended");
                return false;
            }
        }
    }

    let _ = manager.leave(guild_id_obj).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Re-check after disconnect
    {
        let s = sessions.lock().await;
        match s.get(guild_id) {
            Some(s) if s.id == session_id && !s.phase.is_terminal() => {}
            _ => {
                info!(session_id = %session_id, "dave_heal_aborted — session ended during reconnect");
                return false;
            }
        }
    }

    match manager.join(guild_id_obj, channel_id).await {
        Ok(new_call) => {
            let mut sessions = sessions.lock().await;
            if let Some(s) = sessions.get_mut(guild_id) {
                let mut handler = new_call.lock().await;
                s.reattach_audio(&mut handler);
            }
        }
        Err(e) => {
            error!(session_id = %session_id, error = %e, "dave_heal_rejoin_failed");
            return false;
        }
    }

    tokio::time::sleep(Duration::from_secs(5)).await;
    play_start_announcement(manager, guild_id_obj).await;
    info!(session_id = %session_id, "dave_heal_complete");
    true
}

// ---------------------------------------------------------------------------
// Quorum failure + pipeline abort check
// ---------------------------------------------------------------------------

async fn handle_quorum_failure(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
    guild_id: u64,
) -> Result<(), serenity::Error> {
    metrics::counter!("ttrpg_sessions_total", "outcome" => "cancelled").increment(1);
    {
        let mut sessions = state.sessions.lock().await;
        sessions.remove(guild_id);
    }
    component
        .channel_id
        .say(
            &ctx.http,
            "Recording cancelled — consent requirements not met.",
        )
        .await?;
    Ok(())
}

/// Check if the recording-startup pipeline should abort.
///
/// Returns `true` when the session has dropped into a terminal phase
/// (`Cancelled` / `Finalizing` / `Complete`) OR been removed from the
/// manager OR replaced by a newer session with the same guild id. Any
/// non-terminal phase (`AwaitingConsent`, `StartingRecording`,
/// `Recording`) is "keep going":
///
///   - `AwaitingConsent` is expected BEFORE `begin_startup` is called
///     (i.e. between quorum-met and the voice-join completing). An early
///     bug checked only for `StartingRecording` and aborted every run
///     because the first check fired before the transition.
///   - `StartingRecording` is the primary "in progress" state.
///   - `Recording` is briefly passed through in the pipeline's tail
///     (announcement, UI update, license followup) after
///     `confirm_recording`. The pipeline is allowed to finish those
///     cosmetic steps even though `/stop` now owns the session.
///
/// A concurrent `/stop` transitions `Starting → Cancelled` or
/// `Recording → Finalizing`, both terminal, which is what this function
/// detects.
async fn pipeline_aborted(state: &AppState, guild_id: u64, expected_session_id: &str) -> bool {
    let sessions = state.sessions.lock().await;
    match sessions.get(guild_id) {
        Some(s) if s.id == expected_session_id => s.phase.is_terminal(),
        _ => true,
    }
}
