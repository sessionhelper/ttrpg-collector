//! `/stop` slash command and auto-stop handler.

use serenity::all::*;
use tracing::{error, info};

use crate::session::{Phase, Session};
use crate::state::AppState;

/// Edit the consent embed and all ephemeral license followups to show session complete.
/// Takes a reference to the Session to read consent_message and license_followups.
#[tracing::instrument(skip_all, fields(session_id = %session.id))]
async fn cleanup_session_ui(ctx: &Context, session: &Session) {
    if let Some((channel_id, message_id)) = session.consent_message {
        let edit = EditMessage::new()
            .embed(CreateEmbed::new()
                .title("Session complete")
                .description("Recording has ended. Thank you for contributing!")
                .color(0x8b949e))
            .components(vec![]);
        let _ = ctx.http.edit_message(channel_id, message_id, &edit, vec![]).await;
    }

    for (token, msg_id) in &session.license_followups {
        let edit = CreateInteractionResponseFollowup::new()
            .content("License preferences saved. Change them anytime on the participant portal.")
            .components(vec![]);
        let _ = ctx.http.edit_followup_message(token, *msg_id, &edit, vec![]).await;
    }
}

/// Action to take when stopping a session, dispatched on current phase.
enum StopAction {
    /// Phase::Recording — flush audio, upload metadata, mark complete
    Finalize,
    /// Phase::StartingRecording — shut down pipeline, mark abandoned (no
    /// metadata to upload, no audio chunks to flush)
    CancelStartup,
    /// Phase::AwaitingConsent — no pipeline to tear down, just mark
    /// abandoned in the Data API and remove
    RemovePending,
    /// Already terminal or session gone — no-op
    NoOp,
}

/// Stop a session by dispatching on its current phase.
///
/// This is the only correct path for /stop and auto_stop. It replaces the
/// old finalize_session that unconditionally ran the full metadata-upload
/// path regardless of whether the session had actually recorded anything —
/// an assumption that was wrong for StartingRecording and AwaitingConsent,
/// and manifested as: orphan Data API rows, double-decrement of the
/// sessions_active gauge, and metadata uploads with zero audio.
///
/// `pub(crate)` so the dev-only E2E harness endpoint in `crate::harness`
/// can finalize sessions without a Discord `/stop` interaction. The
/// `_ctx` parameter is unused today but kept in the signature for future
/// code paths that need to send Discord messages (e.g. auto-stop
/// announcements in the voice channel).
#[tracing::instrument(skip_all, fields(guild_id = guild_id))]
pub(crate) async fn finalize_session(
    _ctx: &Context,
    state: &AppState,
    guild_id: u64,
) {
    // Snapshot phase + identity under one lock, then release before any
    // network work.
    let (action, session_id, participant_count) = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id) {
            Some(s) => {
                let action = match &s.phase {
                    Phase::Recording(_) => StopAction::Finalize,
                    Phase::StartingRecording(_) => StopAction::CancelStartup,
                    Phase::AwaitingConsent => StopAction::RemovePending,
                    _ => StopAction::NoOp,
                };
                (action, s.id.clone(), s.participants.len() as i32)
            }
            None => return,
        }
    };

    match action {
        StopAction::NoOp => {}
        StopAction::RemovePending => stop_pending(state, guild_id, &session_id).await,
        StopAction::CancelStartup => stop_starting(state, guild_id, &session_id).await,
        StopAction::Finalize => {
            stop_recording(state, guild_id, &session_id, participant_count).await
        }
    }
}

/// Stop path for Phase::AwaitingConsent: no pipeline running yet, just mark
/// the Data API session row abandoned and drop the local state.
async fn stop_pending(state: &AppState, guild_id: u64, session_id: &str) {
    if let Ok(sid) = uuid::Uuid::parse_str(session_id)
        && let Err(e) = state.api.abandon_session(sid).await
    {
        error!("API call failed (abandon_session): {e}");
    }
    info!(session_id = %session_id, "session_cancelled_pending");
    let mut sessions = state.sessions.lock().await;
    if let Some(s) = sessions.get_mut(guild_id) {
        s.abort_all_background_tasks();
    }
    sessions.remove(guild_id);
}

/// Stop path for Phase::StartingRecording: audio pipeline is spun up but
/// DAVE never confirmed, so there is nothing to upload. Drain and shut down
/// the pipeline, mark the session abandoned in the Data API, remove.
async fn stop_starting(state: &AppState, guild_id: u64, session_id: &str) {
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

    info!(session_id = %session_id, "session_cancelled_starting");
    let mut sessions = state.sessions.lock().await;
    if let Some(s) = sessions.get_mut(guild_id) {
        s.abort_all_background_tasks();
        s.complete();
    }
    sessions.remove(guild_id);
}

/// Stop path for Phase::Recording: the full "normal /stop" flow. Flush audio
/// buffers, upload meta.json + consent.json, mark status=complete in the
/// Data API, remove.
async fn stop_recording(
    state: &AppState,
    guild_id: u64,
    session_id: &str,
    participant_count: i32,
) {
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(s) = sessions.get_mut(guild_id) {
            s.finalize().await;
        }
    }

    let (meta_json, consent_json) = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id) {
            Some(s) => (s.meta_json(), s.consent_json()),
            None => return,
        }
    };

    let meta_value: Option<serde_json::Value> = serde_json::from_slice(&meta_json).ok();
    let consent_value: Option<serde_json::Value> = serde_json::from_slice(&consent_json).ok();

    if let Ok(sid) = uuid::Uuid::parse_str(session_id) {
        match state.api.write_metadata(sid, meta_value, consent_value).await {
            Ok(_) => {
                metrics::counter!("chronicle_uploads_total", "type" => "metadata", "outcome" => "success").increment(1);
            }
            Err(e) => {
                error!(error = %e, "metadata_upload_failed");
                metrics::counter!("chronicle_uploads_total", "type" => "metadata", "outcome" => "failure").increment(1);
            }
        }
    }

    metrics::gauge!("chronicle_sessions_active").decrement(1.0);
    info!(session_id = %session_id, "session_finalized");

    if let Ok(sid) = uuid::Uuid::parse_str(session_id)
        && let Err(e) = state
            .api
            .finalize_session(sid, chrono::Utc::now(), participant_count)
            .await
    {
        error!("API call failed (finalize_session): {e}");
    }

    let mut sessions = state.sessions.lock().await;
    if let Some(s) = sessions.get_mut(guild_id) {
        s.abort_all_background_tasks();
        s.complete();
    }
    sessions.remove(guild_id);
}

/// Handle the /stop slash command.
/// Only the initiator can stop the session. Transitions through
/// Finalizing, uploads metadata, then removes the session.
#[tracing::instrument(skip(ctx, command, state), fields(guild_id = %command.guild_id.unwrap()))]
pub async fn handle_stop(
    ctx: &Context,
    command: &CommandInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    let guild_id = command.guild_id.unwrap();

    // Defer IMMEDIATELY — before any lock acquisition or other async work.
    // Discord's interaction-response window is 3 seconds; deferring extends
    // it to 15 minutes and lets us take our time on session-lock contention,
    // audio flushing, S3 uploads, etc.
    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(
                CreateInteractionResponseMessage::new().ephemeral(true),
            ),
        )
        .await?;

    // Verify there's a stoppable session for this guild. is_stoppable
    // matches AwaitingConsent | StartingRecording | Recording — the three
    // phases where the user might visibly expect /stop to do something.
    let _session_id = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id.get()) {
            Some(s) if s.phase.is_stoppable() => {
                if s.initiator_id != command.user.id {
                    command
                        .edit_response(
                            &ctx.http,
                            EditInteractionResponse::new()
                                .content("Only the person who started the recording can stop it."),
                        )
                        .await?;
                    return Ok(());
                }
                s.id.clone()
            }
            _ => {
                command
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .content("No active recording in this server."),
                    )
                    .await?;
                return Ok(());
            }
        }
    };

    command
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new().content("Wrapping up..."),
        )
        .await?;

    // Play "Recording complete" announcement before leaving voice
    let manager = songbird::get(ctx).await.unwrap();
    if let Some(call) = manager.get(guild_id) {
        let mut handler = call.lock().await;
        let source = songbird::input::File::new("/assets/recording_stopped.wav");
        let _ = handler.play_input(source.into());
        drop(handler);
        // Wait for clip to play (~2 seconds)
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    // Leave voice channel — stops audio capture at the Songbird level
    let _ = manager.leave(guild_id).await;

    // Clean up Discord UI (consent embed, license followups) before removing session
    {
        let sessions = state.sessions.lock().await;
        if let Some(session) = sessions.get(guild_id.get()) {
            cleanup_session_ui(ctx, session).await;
        }
    }

    // Flush audio buffers, upload metadata, update Data API, remove session
    finalize_session(ctx, state, guild_id.get()).await;

    command
        .channel_id
        .say(&ctx.http, "Thanks for contributing!")
        .await?;

    Ok(())
}

/// Auto-stop when channel empties. No command interaction — called from voice_state_update.
#[tracing::instrument(skip_all, fields(guild_id = guild_id))]
pub async fn auto_stop(ctx: &Context, guild_id: u64, state: &AppState) {
    // Clean up Discord UI before finalizing
    let text_channel = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id) {
            Some(session) => {
                cleanup_session_ui(ctx, session).await;
                Some(ChannelId::new(session.text_channel_id))
            }
            None => return,
        }
    };

    // Flush audio, upload metadata, remove session
    finalize_session(ctx, state, guild_id).await;

    // Notify in text channel
    if let Some(ch) = text_channel {
        let _ = ch.say(&ctx.http, "Channel empty — thanks for contributing!").await;
    }
}
