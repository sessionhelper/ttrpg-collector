use serenity::all::*;
use tracing::{error, info};

use crate::consent::SessionState;
use crate::db;
use crate::state::AppState;

/// Edit the consent embed and all ephemeral license followups to show session complete.
async fn cleanup_session_ui(ctx: &Context, state: &AppState, guild_id: u64) {
    let (consent_msg, license_followups) = {
        let consent_mgr = state.consent.lock().await;
        match consent_mgr.get_session(guild_id) {
            Some(s) => (s.consent_message, s.license_followups.clone()),
            None => (None, vec![]),
        }
    };

    if let Some((channel_id, message_id)) = consent_msg {
        let edit = EditMessage::new()
            .embed(CreateEmbed::new()
                .title("Session complete")
                .description("Recording has ended. Thank you for contributing!")
                .color(0x8b949e))
            .components(vec![]);
        let _ = ctx.http.edit_message(channel_id, message_id, &edit, vec![]).await;
    }

    for (token, msg_id) in license_followups {
        let edit = CreateInteractionResponseFollowup::new()
            .content("License preferences saved. Change them anytime on the participant portal.")
            .components(vec![]);
        let _ = ctx.http.edit_followup_message(&token, msg_id, &edit, vec![]).await;
    }
}

/// Flush remaining audio buffers to S3 and upload metadata.
/// Shared by both /stop and auto-stop.
async fn finalize_session(
    ctx: &Context,
    state: &AppState,
    guild_id: u64,
    session_id: &str,
) {
    // Shut down audio capture — closes channel, buffer task flushes and uploads final chunks
    {
        let handle = {
            let mut ah = state.audio_handles.lock().await;
            ah.remove(&guild_id)
        };
        if let Some(h) = handle {
            h.shutdown().await;
        }
    }

    // Upload meta.json and consent.json to S3
    {
        let consent_mgr = state.consent.lock().await;
        let mut bundles = state.bundles.lock().await;
        if let (Some(consent), Some(bundle)) = (
            consent_mgr.get_session(guild_id),
            bundles.get_mut(&guild_id),
        ) {
            bundle.ended_at = Some(chrono::Utc::now());
            let meta_json = bundle.meta_json(consent);
            let consent_json = bundle.consent_json(consent);

            let meta_key = format!("sessions/{}/{}/meta.json", guild_id, session_id);
            let consent_key = format!("sessions/{}/{}/consent.json", guild_id, session_id);

            if let Err(e) = state.s3.upload_bytes(&meta_key, meta_json).await {
                error!(error = %e, "meta_upload_failed");
            }
            if let Err(e) = state.s3.upload_bytes(&consent_key, consent_json).await {
                error!(error = %e, "consent_upload_failed");
            }
        }
    }

    info!(session_id = %session_id, "session_finalized");

    // Write Point 4: Finalize session in Postgres
    if let Ok(sid) = uuid::Uuid::parse_str(session_id) {
        let participant_count = {
            let consent_mgr = state.consent.lock().await;
            consent_mgr
                .get_session(guild_id)
                .map(|s| s.participants.len() as i32)
                .unwrap_or(0)
        };
        let s3_prefix_full = format!("sessions/{}/{}", guild_id, session_id);
        let finalized = db::FinalizedSession {
            session_id: sid,
            ended_at: chrono::Utc::now(),
            duration_seconds: 0.0,
            participant_count,
            s3_prefix: Some(s3_prefix_full),
        };
        if let Err(e) = db::finalize_session(&state.db, &finalized).await {
            error!("DB write failed (finalize_session): {e}");
        }
    }
}

/// Clean up all in-memory state for a guild's session.
async fn cleanup_state(state: &AppState, guild_id: u64) {
    {
        let mut consent_mgr = state.consent.lock().await;
        consent_mgr.remove_session(guild_id);
    }
    {
        let mut bundles = state.bundles.lock().await;
        bundles.remove(&guild_id);
    }
    {
        let mut maps = state.ssrc_maps.lock().await;
        maps.remove(&guild_id);
    }
    {
        let mut ah = state.audio_handles.lock().await;
        ah.remove(&guild_id);
    }
    state.abort_license_cleanups(guild_id).await;
}

pub async fn handle_stop(
    ctx: &Context,
    command: &CommandInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    let guild_id = command.guild_id.unwrap();

    // Check for active recording
    let session_id = {
        let manager = state.consent.lock().await;
        match manager.get_session(guild_id.get()) {
            Some(s) if s.state == SessionState::Recording => {
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
                s.session_id.clone()
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

    // Leave voice channel — stops audio capture
    let manager = songbird::get(ctx).await.unwrap();
    let _ = manager.leave(guild_id).await;

    // Clean up Discord UI (consent embed, license followups)
    cleanup_session_ui(ctx, state, guild_id.get()).await;

    // Flush audio buffers to S3, upload metadata, update Postgres
    finalize_session(ctx, state, guild_id.get(), &session_id).await;

    command
        .channel_id
        .say(&ctx.http, "Thanks for contributing!")
        .await?;

    cleanup_state(state, guild_id.get()).await;

    Ok(())
}

/// Auto-stop when channel empties. No command interaction needed.
pub async fn auto_stop(ctx: &Context, guild_id: u64, state: &AppState) {
    cleanup_session_ui(ctx, state, guild_id).await;

    let session_id = {
        let manager = state.consent.lock().await;
        match manager.get_session(guild_id) {
            Some(s) => s.session_id.clone(),
            None => return,
        }
    };

    // Flush audio buffers to S3, upload metadata, update Postgres
    finalize_session(ctx, state, guild_id, &session_id).await;

    // Notify in text channel
    let text_channel = {
        let consent_mgr = state.consent.lock().await;
        consent_mgr
            .get_session(guild_id)
            .map(|s| ChannelId::new(s.text_channel_id))
    };
    if let Some(ch) = text_channel {
        let _ = ch.say(&ctx.http, "Channel empty — thanks for contributing!").await;
    }

    cleanup_state(state, guild_id).await;
}
