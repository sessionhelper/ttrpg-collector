//! `/stop` slash command and auto-stop handler.

use serenity::all::*;
use tracing::{error, info};

use crate::db;
use crate::session::{Phase, Session};
use crate::state::AppState;

/// Edit the consent embed and all ephemeral license followups to show session complete.
/// Takes a reference to the Session to read consent_message and license_followups.
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

/// Flush remaining audio buffers to S3 and upload metadata.
/// Transitions the session through Finalizing -> uploading metadata.
/// Shared by both /stop and auto-stop.
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

    // Upload meta.json and consent.json to S3 (read-only lock on session data)
    let (meta_json, consent_json, session_id, s3_prefix_base, participant_count) = {
        let sessions = state.sessions.lock().await;
        match sessions.get(guild_id) {
            Some(session) => {
                let s3_base = format!("sessions/{}/{}", guild_id, session.id);
                (
                    session.meta_json(),
                    session.consent_json(),
                    session.id.clone(),
                    s3_base,
                    session.participants.len() as i32,
                )
            }
            None => return,
        }
    };

    let meta_key = format!("{}/meta.json", s3_prefix_base);
    let consent_key = format!("{}/consent.json", s3_prefix_base);

    if let Err(e) = state.s3.upload_bytes(&meta_key, meta_json).await {
        error!(error = %e, "meta_upload_failed");
    }
    if let Err(e) = state.s3.upload_bytes(&consent_key, consent_json).await {
        error!(error = %e, "consent_upload_failed");
    }

    info!(session_id = %session_id, "session_finalized");

    // Finalize session in Postgres
    if let Ok(sid) = uuid::Uuid::parse_str(&session_id) {
        let finalized = db::FinalizedSession {
            session_id: sid,
            ended_at: chrono::Utc::now(),
            participant_count,
            s3_prefix: Some(s3_prefix_base),
        };
        if let Err(e) = db::finalize_session(&state.db, &finalized).await {
            error!("DB write failed (finalize_session): {e}");
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

    // Leave voice channel — stops audio capture at the Songbird level
    let manager = songbird::get(ctx).await.unwrap();
    let _ = manager.leave(guild_id).await;

    // Clean up Discord UI (consent embed, license followups) before removing session
    {
        let sessions = state.sessions.lock().await;
        if let Some(session) = sessions.get(guild_id.get()) {
            cleanup_session_ui(ctx, session).await;
        }
    }

    // Flush audio buffers to S3, upload metadata, update Postgres, remove session
    finalize_session(ctx, state, guild_id.get()).await;

    command
        .channel_id
        .say(&ctx.http, "Thanks for contributing!")
        .await?;

    Ok(())
}

/// Auto-stop when channel empties. No command interaction — called from voice_state_update.
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
