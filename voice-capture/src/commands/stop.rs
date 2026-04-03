use serenity::all::*;
use tracing::{error, info};

use crate::consent::SessionState;
use crate::db;
use crate::state::AppState;
use crate::storage::pseudonymize;

/// Edit the consent embed and all ephemeral license followups to show session complete.
async fn cleanup_session_ui(ctx: &Context, state: &AppState, guild_id: u64) {
    let (consent_msg, license_followups) = {
        let consent_mgr = state.consent.lock().await;
        match consent_mgr.get_session(guild_id) {
            Some(s) => (s.consent_message, s.license_followups.clone()),
            None => (None, vec![]),
        }
    };

    // Clean up the main consent embed
    if let Some((channel_id, message_id)) = consent_msg {
        let edit = EditMessage::new()
            .embed(CreateEmbed::new()
                .title("Session complete")
                .description("Recording has ended. Thank you for contributing!")
                .color(0x8b949e))
            .components(vec![]);
        let _ = ctx.http.edit_message(channel_id, message_id, &edit, vec![]).await;
    }

    // Clean up all ephemeral license followup messages
    for (token, msg_id) in license_followups {
        let edit = CreateInteractionResponseFollowup::new()
            .content("License preferences saved. Change them anytime on the participant portal.")
            .components(vec![]);
        let _ = ctx.http.edit_followup_message(&token, msg_id, &edit, vec![]).await;
    }
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
                                    .content(
                                        "Only the person who started the recording can stop it.",
                                    )
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

    // Leave voice channel
    let manager = songbird::get(ctx).await.unwrap();
    let _ = manager.leave(guild_id).await;

    // Clean up consent embed — remove buttons, show "Session complete"
    cleanup_session_ui(ctx, state, guild_id.get()).await;

    // Finalize session
    {
        let mut consent_mgr = state.consent.lock().await;
        if let Some(s) = consent_mgr.get_session_mut(guild_id.get()) {
            s.state = SessionState::Finalizing;
        }
    }

    // Get session dir and process files
    let (session_dir, guild_id_u64) = {
        let consent_mgr = state.consent.lock().await;
        let mut bundles = state.bundles.lock().await;

        let (consent, bundle) = match (
            consent_mgr.get_session(guild_id.get()),
            bundles.get_mut(&guild_id.get()),
        ) {
            (Some(c), Some(b)) => (c, b),
            _ => {
                error!("no session or bundle found for finalization");
                return Ok(());
            }
        };

        bundle.ended_at = Some(chrono::Utc::now());

        // Rename PCM files from SSRC to pseudo user ID
        let mut renamed = false;
        let ssrc_map = state.ssrc_maps.lock().await;
        if let Some(map) = ssrc_map.get(&guild_id.get()) {
            let map = map.lock().await;
            for (ssrc, user_id) in map.iter() {
                let pseudo = pseudonymize(*user_id);
                let src = bundle.pcm_dir().join(format!("{}.pcm", ssrc));
                let dst = bundle.pcm_dir().join(format!("{}.pcm", pseudo));
                if src.exists() {
                    let _ = std::fs::rename(&src, &dst);
                    info!(ssrc = ssrc, pseudo = %pseudo, "track_renamed");
                    renamed = true;
                }
            }
        }

        // Fallback: match PCM files to consented users by order
        if !renamed {
            let consented = consent.consented_user_ids();
            let mut pcm_files: Vec<_> = std::fs::read_dir(bundle.pcm_dir())
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "pcm"))
                .collect();
            pcm_files.sort_by_key(|e| e.file_name());

            for (file, uid) in pcm_files.iter().zip(consented.iter()) {
                let pseudo = pseudonymize(uid.get());
                let dst = bundle.pcm_dir().join(format!("{}.pcm", pseudo));
                let _ = std::fs::rename(file.path(), &dst);
                info!(
                    from = %file.file_name().to_string_lossy(),
                    pseudo = %pseudo,
                    "track_renamed_fallback"
                );
            }
        }

        // Convert PCM to FLAC
        let pcm_dir = bundle.pcm_dir();
        let audio_dir = bundle.audio_dir();
        if let Ok(entries) = std::fs::read_dir(&pcm_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "pcm") {
                    let stem = path.file_stem().unwrap().to_string_lossy();
                    let flac_path = audio_dir.join(format!("{}.flac", stem));
                    let status = std::process::Command::new("ffmpeg")
                        .args([
                            "-y",
                            "-f", "s16le",
                            "-ar", "48000",
                            "-ac", "2",
                            "-i", &path.to_string_lossy(),
                            "-ac", "1",
                            &flac_path.to_string_lossy(),
                        ])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();

                    match status {
                        Ok(s) if s.success() => {
                            let size = std::fs::metadata(&flac_path)
                                .map(|m| m.len())
                                .unwrap_or(0);
                            info!(flac = %flac_path.display(), size = size, "flac_converted");
                        }
                        _ => error!(pcm = %path.display(), "flac_conversion_failed"),
                    }
                }
            }
        }

        // Write metadata
        bundle.write_meta(consent);
        bundle.write_consent(consent);

        info!(session_id = %session_id, "session_finalized");

        (bundle.session_dir.clone(), guild_id.get())
    };

    // Upload to S3
    let s3_prefix = format!("{}/{}", guild_id_u64, session_id);
    match state
        .s3
        .upload_session(&session_dir, guild_id_u64, &session_id)
        .await
    {
        Ok(count) => {
            info!(count = count, session_id = %session_id, "upload_complete");
        }
        Err(e) => {
            error!(error = %e, "upload_failed");
        }
    }

    // Write Point 4: Finalize session in Postgres
    if let Ok(sid) = uuid::Uuid::parse_str(&session_id) {
        let participant_count = {
            let consent_mgr = state.consent.lock().await;
            consent_mgr
                .get_session(guild_id.get())
                .map(|s| s.participants.len() as i32)
                .unwrap_or(0)
        };
        let ended_at = chrono::Utc::now();
        let finalized = db::FinalizedSession {
            session_id: sid,
            ended_at,
            duration_seconds: 0.0, // duration computed from bundle timestamps
            participant_count,
            s3_prefix: Some(s3_prefix),
        };
        if let Err(e) = db::finalize_session(&state.db, &finalized).await {
            error!("DB write failed (finalize_session): {e}");
        }
    }

    command
        .channel_id
        .say(&ctx.http, "Recording saved. Thanks for contributing!")
        .await?;

    // Cleanup state
    {
        let mut consent_mgr = state.consent.lock().await;
        consent_mgr.remove_session(guild_id.get());
    }
    {
        let mut bundles = state.bundles.lock().await;
        bundles.remove(&guild_id.get());
    }
    {
        let mut maps = state.ssrc_maps.lock().await;
        maps.remove(&guild_id.get());
    }
    // Abort any pending license button cleanup tasks
    state.abort_license_cleanups(guild_id.get()).await;

    Ok(())
}

/// Auto-stop when channel empties. No command interaction needed.
pub async fn auto_stop(ctx: &Context, guild_id: u64, state: &AppState) {
    // Clean up consent embed before anything else
    cleanup_session_ui(ctx, state, guild_id).await;

    let session_id = {
        let manager = state.consent.lock().await;
        match manager.get_session(guild_id) {
            Some(s) => s.session_id.clone(),
            None => return,
        }
    };

    {
        let mut consent_mgr = state.consent.lock().await;
        if let Some(s) = consent_mgr.get_session_mut(guild_id) {
            s.state = crate::consent::SessionState::Finalizing;
        }
    }

    let session_dir = {
        let consent_mgr = state.consent.lock().await;
        let mut bundles = state.bundles.lock().await;

        if let (Some(consent), Some(bundle)) = (
            consent_mgr.get_session(guild_id),
            bundles.get_mut(&guild_id),
        ) {
            bundle.ended_at = Some(chrono::Utc::now());

            // Rename PCM files
            let mut renamed = false;
            let ssrc_map = state.ssrc_maps.lock().await;
            if let Some(map) = ssrc_map.get(&guild_id) {
                let map = map.lock().await;
                for (ssrc, user_id) in map.iter() {
                    let pseudo = pseudonymize(*user_id);
                    let src = bundle.pcm_dir().join(format!("{}.pcm", ssrc));
                    let dst = bundle.pcm_dir().join(format!("{}.pcm", pseudo));
                    if src.exists() {
                        let _ = std::fs::rename(&src, &dst);
                        renamed = true;
                    }
                }
            }

            if !renamed {
                let consented = consent.consented_user_ids();
                let mut pcm_files: Vec<_> = std::fs::read_dir(bundle.pcm_dir())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "pcm"))
                    .collect();
                pcm_files.sort_by_key(|e| e.file_name());
                for (file, uid) in pcm_files.iter().zip(consented.iter()) {
                    let pseudo = pseudonymize(uid.get());
                    let dst = bundle.pcm_dir().join(format!("{}.pcm", pseudo));
                    let _ = std::fs::rename(file.path(), &dst);
                }
            }

            // Convert PCM to FLAC
            if let Ok(entries) = std::fs::read_dir(bundle.pcm_dir()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "pcm") {
                        let stem = path.file_stem().unwrap().to_string_lossy();
                        let flac_path = bundle.audio_dir().join(format!("{}.flac", stem));
                        let _ = std::process::Command::new("ffmpeg")
                            .args([
                                "-y", "-f", "s16le", "-ar", "48000", "-ac", "2",
                                "-i", &path.to_string_lossy(), "-ac", "1",
                                &flac_path.to_string_lossy(),
                            ])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                    }
                }
            }

            bundle.write_meta(consent);
            bundle.write_consent(consent);
            info!(session_id = %session_id, "auto_session_finalized");
            Some((bundle.session_dir.clone(), guild_id))
        } else {
            None
        }
    };

    // Upload
    if let Some((dir, gid)) = session_dir {
        let s3_prefix = format!("{}/{}", gid, session_id);
        let _ = state.s3.upload_session(&dir, gid, &session_id).await;

        // Write Point 4: Finalize session in Postgres (auto-stop)
        if let Ok(sid) = uuid::Uuid::parse_str(&session_id) {
            let participant_count = {
                let consent_mgr = state.consent.lock().await;
                consent_mgr
                    .get_session(guild_id)
                    .map(|s| s.participants.len() as i32)
                    .unwrap_or(0)
            };
            let finalized = db::FinalizedSession {
                session_id: sid,
                ended_at: chrono::Utc::now(),
                duration_seconds: 0.0,
                participant_count,
                s3_prefix: Some(s3_prefix),
            };
            if let Err(e) = db::finalize_session(&state.db, &finalized).await {
                error!("DB write failed (finalize_session auto-stop): {e}");
            }
        }

        // Notify in text channel
        let text_channel = {
            let consent_mgr = state.consent.lock().await;
            consent_mgr
                .get_session(guild_id)
                .map(|s| ChannelId::new(s.text_channel_id))
        };
        if let Some(ch) = text_channel {
            let _ = ch
                .say(&ctx.http, "Channel empty — recording saved. Thanks!")
                .await;
        }
    }

    // Cleanup
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
    // Abort any pending license button cleanup tasks
    state.abort_license_cleanups(guild_id).await;
}
