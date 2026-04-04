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

/// Flush remaining audio buffers and upload metadata via the Data API.
/// Transitions the session through Finalizing -> uploading metadata.
/// Shared by both /stop and auto-stop.
#[tracing::instrument(skip_all, fields(guild_id = guild_id))]
async fn finalize_session(
    _ctx: &Context,
    state: &AppState,
    guild_id: u64,
) {
    // Finalize: shut down audio pipeline, set ended_at
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(session) = sessions.get_mut(guild_id) {
            session.finalize().await;
        }
    }

    // Upload meta.json and consent.json via Data API (read-only lock on session data)
    let (meta_json, consent_json, session_id, participant_count) = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id) {
            Some(session) => {
                (
                    session.meta_json(),
                    session.consent_json(),
                    session.id.clone(),
                    session.participants.len() as i32,
                )
            }
            None => return,
        }
    };

    // Parse metadata bytes into JSON values for the API
    let meta_value: Option<serde_json::Value> =
        serde_json::from_slice(&meta_json).ok();
    let consent_value: Option<serde_json::Value> =
        serde_json::from_slice(&consent_json).ok();

    if let Ok(sid) = uuid::Uuid::parse_str(&session_id) {
        match state.api.write_metadata(sid, meta_value, consent_value).await {
            Ok(_) => {
                metrics::counter!("ttrpg_uploads_total", "type" => "metadata", "outcome" => "success").increment(1);
            }
            Err(e) => {
                error!(error = %e, "metadata_upload_failed");
                metrics::counter!("ttrpg_uploads_total", "type" => "metadata", "outcome" => "failure").increment(1);
            }
        }
    }

    metrics::gauge!("ttrpg_sessions_active").decrement(1.0);
    info!(session_id = %session_id, "session_finalized");

    // Finalize session in Data API
    if let Ok(sid) = uuid::Uuid::parse_str(&session_id) {
        if let Err(e) = state
            .api
            .finalize_session(sid, chrono::Utc::now(), participant_count)
            .await
        {
            error!("API call failed (finalize_session): {e}");
        }
    }

    // Mark session complete and remove from manager
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(session) = sessions.get_mut(guild_id) {
            session.abort_license_cleanups();
            session.complete();
        }
        sessions.remove(guild_id);
    }
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

    // Verify there's an active recording and the caller is the initiator
    let _session_id = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id.get()) {
            Some(s) if matches!(s.phase, Phase::Recording { .. }) => {
                if s.initiator_id != command.user.id {
                    command
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .content("Only the person who started the recording can stop it.")
                                    .ephemeral(true),
                            ),
                        )
                        .await?;
                    return Ok(());
                }
                s.id.clone()
            }
            _ => {
                command
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content("No active recording in this server.")
                                .ephemeral(true),
                        ),
                    )
                    .await?;
                return Ok(());
            }
        }
    };

    command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("Wrapping up...")
                    .ephemeral(true),
            ),
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
