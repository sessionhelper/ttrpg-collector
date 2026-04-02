use serenity::all::*;
use tracing::{error, info};

use crate::consent::SessionState;
use crate::state::AppState;
use crate::storage::pseudonymize;

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
            CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
        )
        .await?;

    // Leave voice channel
    let manager = songbird::get(ctx).await.unwrap();
    let _ = manager.leave(guild_id).await;

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

    Ok(())
}
