//! TTRPG session audio collector Discord bot.
//!
//! Entry point, serenity event handlers, and consent button logic.

mod api_client;
mod commands;
mod config;
mod telemetry;
mod session;
mod state;
mod storage;
mod voice;

use std::sync::Arc;

use clap::Parser;
use serenity::all::*;
use serenity::async_trait;
use songbird::driver::{DecodeConfig, DecodeMode};
use songbird::serenity::register_from_config;
use songbird::Config as SongbirdConfig;
use tracing::{error, info, warn};

use crate::api_client::DataApiClient;
use crate::config::Config;
use crate::session::{consent_buttons, ConsentScope, Phase};
use crate::state::AppState;

/// Serenity event handler. Dispatches slash commands, button clicks, and voice state changes.
struct Handler {
    state: Arc<AppState>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "bot_ready");

        // Register guild commands (instant) for all connected guilds
        for guild in &ready.guilds {
            let guild_id = guild.id;
            guild_id
                .set_commands(
                    &ctx.http,
                    vec![
                        CreateCommand::new("record")
                            .description("Start recording this voice channel for the Open Voice Project"),
                        CreateCommand::new("stop")
                            .description("Stop the current recording session"),
                    ],
                )
                .await
                .expect("Failed to register guild commands");
            info!(guild_id = %guild_id, "guild_commands_registered");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        // Spawn every handler into a detached task so the serenity gateway can
        // keep pumping other interactions while slow work (DAVE retries,
        // session finalization, S3 uploads) runs in the background. If a
        // handler blocks inline here, subsequent /stop / button clicks sit
        // in the gateway queue and their interaction tokens expire before
        // the handler even calls defer().
        match interaction {
            Interaction::Command(command) => {
                let state = self.state.clone();
                tokio::spawn(async move {
                    let result = match command.data.name.as_str() {
                        "record" => {
                            commands::record::handle_record(&ctx, &command, &state).await
                        }
                        "stop" => commands::stop::handle_stop(&ctx, &command, &state).await,
                        _ => Ok(()),
                    };

                    if let Err(e) = result {
                        error!(error = %e, "command_error");
                    }
                });
            }
            Interaction::Component(component) => {
                let state = self.state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_consent_button(&ctx, &component, &state).await {
                        error!(error = %e, "consent_button_error");
                        // Clean up stale session on interaction failure — only if still awaiting consent
                        if let Some(gid) = component.guild_id {
                            let mut sessions = state.sessions.lock().await;
                            if let Some(session) = sessions.get(gid.get())
                                && matches!(session.phase, Phase::AwaitingConsent)
                            {
                                info!(guild_id = %gid, "cleaning_up_stale_session");
                                sessions.remove(gid.get());
                            }
                        }
                    }
                });
            }
            _ => {}
        }
    }

    /// Handle voice state changes: mid-session joins and auto-stop when channel empties.
    async fn voice_state_update(&self, ctx: Context, _old: Option<VoiceState>, _new: VoiceState) {
        let guild_id = match _new.guild_id {
            Some(id) => id,
            None => return,
        };

        // Only proceed if there's an active recording session for this guild
        let (channel_id, text_channel_id) = {
            let sessions = self.state.sessions.lock().await;
            match sessions.get(guild_id.get()) {
                Some(s) if matches!(s.phase, Phase::Recording { .. }) => {
                    (ChannelId::new(s.channel_id), ChannelId::new(s.text_channel_id))
                }
                _ => return,
            }
        };

        // Check if this is a new user joining the recording channel
        let joined_recording_channel = _new.channel_id == Some(channel_id);
        let was_in_channel = _old.as_ref().is_some_and(|o| o.channel_id == Some(channel_id));

        if joined_recording_channel && !was_in_channel {
            let user_id = _new.user_id;

            // Skip bots
            let guild = match ctx.cache.guild(guild_id) {
                Some(g) => g.clone(),
                None => return,
            };
            if guild.members.get(&user_id).is_some_and(|m| m.user.bot) {
                return;
            }

            // Check if already a participant — single lock
            let already_participant = {
                let sessions = self.state.sessions.lock().await;
                sessions
                    .get(guild_id.get())
                    .is_some_and(|s| s.participants.contains_key(&user_id))
            };

            if !already_participant {
                // Check blocklist before adding mid-session joiner
                match self.state.api.check_blocklist(user_id.get()).await {
                    Ok(true) => {
                        info!(user_id = %user_id, "mid_session_joiner_blocked — user opted out globally");
                        return;
                    }
                    Err(e) => {
                        tracing::error!("API call failed (check_blocklist): {e}");
                        // Continue — allow join if API is down
                    }
                    _ => {}
                }

                let display_name = guild
                    .members
                    .get(&user_id)
                    .map(|m| m.display_name().to_string())
                    .unwrap_or_else(|| format!("User {}", user_id));

                // Add as mid-session joiner and get session ID for API call
                let session_id_str = {
                    let mut sessions = self.state.sessions.lock().await;
                    if let Some(session) = sessions.get_mut(guild_id.get()) {
                        session.add_participant(user_id, display_name.clone(), true);
                        Some(session.id.clone())
                    } else {
                        None
                    }
                };

                // Persist mid-session participant via Data API
                if let Some(sid_str) = &session_id_str
                    && let Ok(sid) = uuid::Uuid::parse_str(sid_str)
                    && let Err(e) = self.state.api.add_participant(sid, user_id.get(), true).await
                {
                    tracing::error!("API call failed (add_participant mid-session): {e}");
                }

                info!(user_id = %user_id, name = %display_name, "mid_session_joiner");

                // Build consent embed from session state
                let embed = {
                    let sessions = self.state.sessions.lock().await;
                    if let Some(session) = sessions.get(guild_id.get()) {
                        session.consent_embed()
                    } else {
                        return;
                    }
                };

                let msg = CreateMessage::new()
                    .content(format!(
                        "<@{}> joined the voice channel during recording. Their audio is **not being captured** until they consent.",
                        user_id
                    ))
                    .embed(embed)
                    .components(vec![consent_buttons()]);

                if let Err(e) = text_channel_id.send_message(&ctx.http, msg).await {
                    warn!(error = %e, "failed to send mid-session consent prompt");
                }
            }
        }

        // Auto-stop: check if bot is alone in the voice channel
        let guild = match ctx.cache.guild(guild_id) {
            Some(g) => g.clone(),
            None => return,
        };

        let humans_in_channel = guild
            .voice_states
            .values()
            .filter(|vs| vs.channel_id == Some(channel_id))
            .filter(|vs| {
                guild
                    .members
                    .get(&vs.user_id)
                    .is_none_or(|m| !m.user.bot)
            })
            .count();

        if humans_in_channel == 0 {
            let state = self.state.clone();
            let ctx_clone = ctx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;

                // Re-check after 30s to avoid false triggers from brief disconnects
                let guild = match ctx_clone.cache.guild(guild_id) {
                    Some(g) => g.clone(),
                    None => return,
                };
                let still_empty = guild
                    .voice_states
                    .values()
                    .filter(|vs| vs.channel_id == Some(channel_id))
                    .filter(|vs| {
                        guild
                            .members
                            .get(&vs.user_id)
                            .is_none_or(|m| !m.user.bot)
                    })
                    .count()
                    == 0;

                if still_empty {
                    info!(guild_id = %guild_id, "auto_stop — channel empty for 30s");
                    let manager = songbird::get(&ctx_clone).await.unwrap();

                    // Play "Recording complete" announcement before leaving
                    if let Some(call) = manager.get(guild_id) {
                        let mut handler = call.lock().await;
                        let source = songbird::input::File::new("/assets/recording_stopped.wav");
                        let _ = handler.play_input(source.into());
                        drop(handler);
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }

                    let _ = manager.leave(guild_id).await;

                    commands::stop::auto_stop(&ctx_clone, guild_id.get(), &state).await;
                }
            });
        }
    }
}

/// Check if the recording-startup pipeline should abort. Returns true if
/// the `cancel` flag has been set (by /stop or auto_stop via finalize_session)
/// OR if the session has vanished from the manager OR if it's been replaced
/// by a newer one (different session id). The pipeline captures its cancel
/// flag and expected session id up-front so the check is stable even if the
/// session is concurrently removed.
async fn pipeline_aborted(
    state: &AppState,
    guild_id: u64,
    expected_session_id: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> bool {
    use std::sync::atomic::Ordering;
    if cancel.load(Ordering::Relaxed) {
        return true;
    }
    let sessions = state.sessions.lock().await;
    match sessions.get(guild_id) {
        Some(s) => s.id != expected_session_id,
        None => true,
    }
}

/// Handle consent Accept/Decline button clicks.
/// Uses one lock on `state.sessions` to read and mutate session state.
#[tracing::instrument(skip_all, fields(guild_id = component.guild_id.map(|g| g.get()), user_id = %component.user.id))]
async fn handle_consent_button(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    let guild_id = component.guild_id.unwrap().get();
    let user_id = component.user.id;

    let scope = match component.data.custom_id.as_str() {
        "consent_accept" => ConsentScope::Full,
        "consent_decline" => ConsentScope::Decline,
        "license_no_llm" | "license_no_public" => {
            return handle_license_button(ctx, component, state).await;
        }
        _ => return Ok(()),
    };

    let scope_label = match scope {
        ConsentScope::Full => "full",
        ConsentScope::Decline => "decline",
        ConsentScope::DeclineAudio => "decline_audio",
    };
    metrics::counter!("ttrpg_consent_responses_total", "scope" => scope_label).increment(1);

    // Lock sessions once, record consent, and extract everything we need
    let (should_start, is_mid_session_accept, embed, session_id, channel_id) = {
        let mut sessions = state.sessions.lock().await;
        let session = match sessions.get_mut(guild_id) {
            Some(s) => s,
            None => return Ok(()),
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
            return Ok(());
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
            return Ok(());
        }

        // Check if this is a mid-session joiner accepting during recording
        let is_mid_session = matches!(session.phase, Phase::Recording { .. })
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

        (should_start, is_mid_session_accept, embed, session_id, channel_id)
    };

    // Acknowledge the interaction FIRST (Discord 3-second deadline)
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

    // Mid-session joiner accepted — add to consented_users so their audio is captured
    if is_mid_session_accept {
        let sessions = state.sessions.lock().await;
        if let Some(session) = sessions.get(guild_id)
            && let Phase::Recording { consented_users, .. } = &session.phase
        {
            let mut set = consented_users.lock().await;
            set.insert(user_id.get());
            info!(user_id = %user_id, "mid_session_consent_granted — audio capture enabled");
        }
    }

    if should_start {
        info!("quorum_met — joining voice channel");
        let guild_id_obj = component.guild_id.unwrap();
        let manager = songbird::get(ctx).await.unwrap();

        // Capture the cancellation flag before we start any long-running
        // work. This Arc outlives the session itself — if /stop fires and
        // removes the session from the manager, our captured flag still
        // carries the cancellation signal and we see it at the next await
        // point instead of panicking on sessions.get(...).unwrap().
        let cancel = {
            let sessions = state.sessions.lock().await;
            match sessions.get(guild_id) {
                Some(s) => s.startup_cancelled.clone(),
                None => {
                    info!("session_vanished_before_pipeline_start");
                    return Ok(());
                }
            }
        };
        let expected_id = session_id.clone();

        match manager.join(guild_id_obj, channel_id).await {
            Ok(call) => {
                info!("voice_joined");

                // Abort if /stop fired while we were joining voice.
                if pipeline_aborted(state, guild_id, &expected_id, &cancel).await {
                    info!("startup_cancelled_after_voice_join — leaving voice");
                    let _ = manager.leave(guild_id_obj).await;
                    return Ok(());
                }

                // Start the audio pipeline via the session state machine.
                // Phase-guard the mutation so a concurrently-replaced session
                // doesn't get its pipeline hijacked.
                {
                    let mut sessions = state.sessions.lock().await;
                    match sessions.get_mut(guild_id) {
                        Some(s) if s.id == expected_id => {
                            let mut handler = call.lock().await;
                            let _audio_tx = s.start_recording(&mut handler, state.api.clone());
                        }
                        _ => {
                            info!("session_replaced_before_start_recording");
                            drop(sessions);
                            let _ = manager.leave(guild_id_obj).await;
                            return Ok(());
                        }
                    }
                }

                info!(session_id = %session_id, "registering_audio_receiver");

                // Wait for DAVE to deliver decoded audio — retry voice join if needed
                const DAVE_WAIT_SECS: u64 = 5;
                const MAX_ATTEMPTS: u32 = 3;
                let dave_start = std::time::Instant::now();

                for attempt in 1..=MAX_ATTEMPTS {
                    info!(attempt = attempt, "waiting_for_dave");
                    tokio::time::sleep(std::time::Duration::from_secs(DAVE_WAIT_SECS)).await;

                    if pipeline_aborted(state, guild_id, &expected_id, &cancel).await {
                        info!("startup_cancelled_during_dave_wait — leaving voice");
                        let _ = manager.leave(guild_id_obj).await;
                        return Ok(());
                    }

                    // Check session state for DAVE confirmation. Phase-guarded.
                    let has_audio = {
                        let sessions = state.sessions.lock().await;
                        match sessions.get(guild_id) {
                            Some(s) if s.id == expected_id => s.has_audio(),
                            _ => {
                                info!("session_replaced_during_dave_wait");
                                drop(sessions);
                                let _ = manager.leave(guild_id_obj).await;
                                return Ok(());
                            }
                        }
                    };

                    if has_audio {
                        let dave_elapsed = dave_start.elapsed().as_secs_f64();
                        info!(attempt = attempt, dave_secs = dave_elapsed, "dave_audio_confirmed");
                        metrics::counter!("ttrpg_dave_attempts_total", "outcome" => "success").increment(1);
                        break;
                    }

                    // Check SSRC separately (needs .await inside the session). Phase-guarded.
                    let has_ssrc = {
                        let sessions = state.sessions.lock().await;
                        match sessions.get(guild_id) {
                            Some(s) if s.id == expected_id => s.has_ssrc().await,
                            _ => {
                                info!("session_replaced_during_ssrc_check");
                                drop(sessions);
                                let _ = manager.leave(guild_id_obj).await;
                                return Ok(());
                            }
                        }
                    };
                    if has_ssrc {
                        let dave_elapsed = dave_start.elapsed().as_secs_f64();
                        info!(attempt = attempt, dave_secs = dave_elapsed, "dave_connection_confirmed — ssrc mapped, awaiting speech");
                        metrics::counter!("ttrpg_dave_attempts_total", "outcome" => "success").increment(1);
                        break;
                    }

                    if attempt == MAX_ATTEMPTS {
                        metrics::counter!("ttrpg_dave_attempts_total", "outcome" => "failure").increment(1);
                        warn!("dave_failed — no audio or ssrc after {MAX_ATTEMPTS} attempts");
                        // Tell the Data API before we drop the local state, so the
                        // sessions row doesn't stay stuck in awaiting_consent forever.
                        if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
                            && let Err(e) = state.api.update_session_state(sid, "abandoned").await
                        {
                            error!("API call failed (update_session_state=abandoned): {e}");
                        }
                        // Clean up: shut down audio pipeline, leave voice, remove session
                        {
                            let mut sessions = state.sessions.lock().await;
                            if let Some(session) = sessions.get_mut(guild_id) {
                                session.cleanup().await;
                            }
                            sessions.remove(guild_id);
                        }
                        let _ = manager.leave(guild_id_obj).await;
                        component
                            .channel_id
                            .say(&ctx.http, "Unable to receive audio. Try `/record` again.")
                            .await?;
                        return Ok(());
                    }

                    // Leave and rejoin to re-negotiate DAVE encryption
                    info!(attempt = attempt, "dave_retry — reconnecting voice");
                    let _ = manager.leave(guild_id_obj).await;
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                    if pipeline_aborted(state, guild_id, &expected_id, &cancel).await {
                        info!("startup_cancelled_between_dave_retries");
                        return Ok(());
                    }

                    match manager.join(guild_id_obj, channel_id).await {
                        Ok(new_call) => {
                            // Re-attach to the new Call — phase-guarded reattach.
                            let mut sessions = state.sessions.lock().await;
                            match sessions.get_mut(guild_id) {
                                Some(s) if s.id == expected_id => {
                                    let mut handler = new_call.lock().await;
                                    s.reattach_audio(&mut handler);
                                }
                                _ => {
                                    info!("session_replaced_during_dave_rejoin");
                                    drop(sessions);
                                    let _ = manager.leave(guild_id_obj).await;
                                    return Ok(());
                                }
                            }
                        }
                        Err(e) => {
                            error!(error = %e, attempt = attempt, "dave_rejoin_failed");
                            // Mark the session abandoned in the Data API before clearing
                            // local state so the row doesn't dangle.
                            if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
                                && let Err(e) = state.api.update_session_state(sid, "abandoned").await
                            {
                                error!("API call failed (update_session_state=abandoned): {e}");
                            }
                            // Clean up everything on rejoin failure
                            {
                                let mut sessions = state.sessions.lock().await;
                                if let Some(session) = sessions.get_mut(guild_id) {
                                    session.cleanup().await;
                                }
                                sessions.remove(guild_id);
                            }
                            component
                                .channel_id
                                .say(&ctx.http, "Failed to reconnect to voice. Try `/record` again.")
                                .await?;
                            return Ok(());
                        }
                    }
                }

                // DAVE confirmed. Abort-check once more before playing the
                // announcement — /stop can fire between DAVE confirm and here.
                if pipeline_aborted(state, guild_id, &expected_id, &cancel).await {
                    info!("startup_cancelled_after_dave_confirm — leaving voice");
                    let _ = manager.leave(guild_id_obj).await;
                    return Ok(());
                }

                // Play "Recording has begun" announcement
                if let Some(call) = manager.get(guild_id_obj) {
                    let mut handler = call.lock().await;
                    let source = songbird::input::File::new("/assets/recording_started.wav");
                    let _ = handler.play_input(source.into());
                }
                // Brief pause to let the clip play before we start capturing
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                if pipeline_aborted(state, guild_id, &expected_id, &cancel).await {
                    info!("startup_cancelled_during_announcement");
                    return Ok(());
                }

                // Update consent embed: remove buttons, show recording status
                let consent_msg = {
                    let sessions = state.sessions.lock().await;
                    sessions.get(guild_id).and_then(|s| s.consent_message)
                };
                if let Some((channel_id_msg, message_id)) = consent_msg {
                    let edit = serenity::all::EditMessage::new()
                        .embed(CreateEmbed::new()
                            .title("Recording in progress")
                            .description("All participants accepted. Recording is active.")
                            .color(0x238636))
                        .components(vec![]);
                    let _ = ctx.http.edit_message(channel_id_msg, message_id, &edit, vec![]).await;
                }

                // Update session state via Data API
                if let Ok(sid) = uuid::Uuid::parse_str(&session_id)
                    && let Err(e) = state.api.update_session_state(sid, "recording").await
                {
                    tracing::error!("API call failed (update_session_state): {e}");
                }

                metrics::gauge!("ttrpg_sessions_active").increment(1.0);
                info!(session_id = %session_id, "recording_started");

                component
                    .channel_id
                    .say(&ctx.http, "Recording. Use `/stop` when done.")
                    .await?;

                // Send license preference buttons now that recording is active.
                // Uses the original interaction's followup window (15 min from consent ack).
                if scope == ConsentScope::Full {
                    let followup = CreateInteractionResponseFollowup::new()
                        .content("Your audio defaults to **public dataset + LLM training**. Toggle restrictions below:")
                        .ephemeral(true)
                        .components(vec![CreateActionRow::Buttons(vec![
                            CreateButton::new("license_no_llm")
                                .label("No LLM Training")
                                .style(ButtonStyle::Secondary),
                            CreateButton::new("license_no_public")
                                .label("No Public Release")
                                .style(ButtonStyle::Secondary),
                        ])]);
                    match component.create_followup(&ctx.http, followup).await {
                        Ok(msg) => {
                            {
                                let mut sessions = state.sessions.lock().await;
                                if let Some(session) = sessions.get_mut(guild_id) {
                                    session.license_followups.push((component.token.clone(), msg.id));
                                }
                            }
                            let http = ctx.http.clone();
                            let interaction_token = component.token.clone();
                            let handle = tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(14 * 60)).await;
                                let edit = serenity::all::EditInteractionResponse::new()
                                    .components(vec![]);
                                let _ = http
                                    .edit_followup_message(&interaction_token, msg.id, &edit, vec![])
                                    .await;
                            });
                            {
                                let mut sessions = state.sessions.lock().await;
                                if let Some(session) = sessions.get_mut(guild_id) {
                                    session.license_cleanup_tasks.push(handle);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "license_followup_failed");
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "voice_join_failed");
                metrics::counter!("ttrpg_sessions_total", "outcome" => "failed").increment(1);
                component
                    .channel_id
                    .say(&ctx.http, "Failed to join voice channel.")
                    .await?;

                // Remove the session since we couldn't join voice
                let mut sessions = state.sessions.lock().await;
                sessions.remove(guild_id);
            }
        }
    } else {
        // Check if quorum failed — all responded but not enough accepted
        let quorum_failed = {
            let sessions = state.sessions.lock().await;
            sessions.get(guild_id).is_some_and(|s| {
                s.all_responded() && !s.evaluate_quorum()
            })
        };

        if quorum_failed {
            metrics::counter!("ttrpg_sessions_total", "outcome" => "cancelled").increment(1);
            let mut sessions = state.sessions.lock().await;
            sessions.remove(guild_id);

            component
                .channel_id
                .say(
                    &ctx.http,
                    "Recording cancelled — consent requirements not met.",
                )
                .await?;
        }
    }

    // Record consent via Data API (after voice join, non-critical path)
    {
        let session_uuid = uuid::Uuid::parse_str(&session_id).ok();
        if let Some(sid) = session_uuid {
            let scope_str = match scope {
                ConsentScope::Full => "full",
                ConsentScope::Decline => "decline",
                ConsentScope::DeclineAudio => "decline_audio",
            };
            if let Err(e) = state.api.record_consent(sid, user_id.get(), scope_str).await {
                tracing::error!("API call failed (record_consent): {e}");
            }
        }
    }

    Ok(())
}

/// Handle license preference button clicks (toggles no-LLM / no-public-release).
#[tracing::instrument(skip_all, fields(guild_id = component.guild_id.map(|g| g.get()), user_id = %component.user.id))]
async fn handle_license_button(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    // Acknowledge the component interaction immediately — the Data API
    // round-trips below (toggle_license_flag + get_license_flags) can
    // exceed Discord's 3-second interaction response window. Acknowledge
    // is the component equivalent of Defer: no visible "thinking" state,
    // and we can still edit the original button message via edit_response
    // once the API calls complete.
    component
        .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
        .await?;

    let guild_id = component.guild_id.unwrap().get();
    let user_id = component.user.id;

    let field = match component.data.custom_id.as_str() {
        "license_no_llm" => "no_llm_training",
        "license_no_public" => "no_public_release",
        _ => return Ok(()),
    };

    let session_id_str = {
        let sessions = state.sessions.lock().await;
        sessions.get(guild_id).map(|s| s.id.clone())
    };

    // Toggle the flag via Data API and read back current state
    let (no_llm, no_public) = if let Some(sid_str) = &session_id_str {
        if let Ok(sid) = uuid::Uuid::parse_str(sid_str) {
            if let Err(e) = state.api.toggle_license_flag(sid, user_id.get(), field).await {
                tracing::error!("API call failed (toggle_license_flag): {e}");
            }
            state
                .api
                .get_license_flags(sid, user_id.get())
                .await
                .unwrap_or((false, false))
        } else {
            (false, false)
        }
    } else {
        (false, false)
    };

    // Re-render buttons — state expressed through button style, no text clutter
    let llm_style = if no_llm { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let public_style = if no_public { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let llm_label = if no_llm { "No LLM Training ✓" } else { "No LLM Training" };
    let public_label = if no_public { "No Public Release ✓" } else { "No Public Release" };

    component
        .edit_response(
            &ctx.http,
            EditInteractionResponse::new()
                .content("Toggle restrictions on your audio:")
                .components(vec![CreateActionRow::Buttons(vec![
                    CreateButton::new("license_no_llm")
                        .label(llm_label)
                        .style(llm_style),
                    CreateButton::new("license_no_public")
                        .label(public_label)
                        .style(public_style),
                ])]),
        )
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() {
    // Install rustls crypto provider before anything uses TLS
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ttrpg_collector=info".parse().unwrap())
                .add_directive("songbird=error".parse().unwrap())
                .add_directive("songbird::driver::tasks::udp_rx=off".parse().unwrap())
                .add_directive("serenity=warn".parse().unwrap()),
        )
        .init();

    // Metrics are recorded via the metrics crate facade.
    // A Prometheus exporter can be enabled with the `prometheus` feature.
    #[cfg(feature = "prometheus")]
    {
        let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
        builder
            .install()
            .expect("Failed to install Prometheus exporter");
    }

    telemetry::describe_metrics();

    let config = Config::parse();
    let token = config.token.clone();

    info!(
        version = option_env!("BUILD_VERSION").unwrap_or("dev"),
        "starting_bot"
    );

    // Authenticate with the Data API
    let api = DataApiClient::authenticate(&config.data_api_url, &config.data_api_shared_secret, "bot")
        .await
        .expect("Failed to authenticate with Data API");

    info!("data_api_authenticated");

    let state = Arc::new(AppState::new(config, api));

    // Spawn heartbeat task — keeps the service session alive
    {
        let state_hb = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if let Err(e) = state_hb.api.heartbeat().await {
                    tracing::warn!(error = %e, "heartbeat_failed");
                }
            }
        });
    }

    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_MEMBERS;

    let songbird_config = SongbirdConfig::default()
        .decode_mode(DecodeMode::Decode(DecodeConfig::default()));

    let handler = Handler {
        state: state.clone(),
    };

    let client_builder = Client::builder(&token, intents).event_handler(handler);
    let mut client = register_from_config(client_builder, songbird_config)
        .await
        .expect("Error creating client");

    if let Err(e) = client.start().await {
        error!(error = %e, "client_error");
    }
}
