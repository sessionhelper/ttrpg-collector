mod commands;
mod config;
mod consent;
mod db;
mod state;
mod storage;
mod voice;

use std::collections::HashSet;
use std::sync::Arc;

use clap::Parser;
use serenity::all::*;
use serenity::async_trait;
use songbird::driver::{DecodeConfig, DecodeMode};
use songbird::serenity::register_from_config;
use songbird::Config as SongbirdConfig;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::consent::*;
use crate::consent::embeds::consent_buttons;
use crate::state::AppState;
use crate::voice::AudioReceiver;

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
        match interaction {
            Interaction::Command(command) => {
                let result = match command.data.name.as_str() {
                    "record" => {
                        commands::record::handle_record(&ctx, &command, &self.state).await
                    }
                    "stop" => commands::stop::handle_stop(&ctx, &command, &self.state).await,
                    _ => Ok(()),
                };

                if let Err(e) = result {
                    error!(error = %e, "command_error");
                }
            }
            Interaction::Component(component) => {
                if let Err(e) = handle_consent_button(&ctx, &component, &self.state).await {
                    error!(error = %e, "consent_button_error");
                    // Clean up stale session on interaction failure
                    if let Some(gid) = component.guild_id {
                        let mut manager = self.state.consent.lock().await;
                        if let Some(session) = manager.get_session(gid.get()) {
                            if session.state == SessionState::AwaitingConsent {
                                info!(guild_id = %gid, "cleaning_up_stale_session");
                                manager.remove_session(gid.get());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn voice_state_update(&self, ctx: Context, _old: Option<VoiceState>, _new: VoiceState) {
        let guild_id = match _new.guild_id {
            Some(id) => id,
            None => return,
        };

        let (channel_id, text_channel_id, is_recording) = {
            let consent = self.state.consent.lock().await;
            match consent.get_session(guild_id.get()) {
                Some(s) if s.state == SessionState::Recording => {
                    (ChannelId::new(s.channel_id), ChannelId::new(s.text_channel_id), true)
                }
                _ => return,
            }
        };

        if !is_recording {
            return;
        }

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

            // Check if already a participant
            let already_participant = {
                let consent = self.state.consent.lock().await;
                if let Some(session) = consent.get_session(guild_id.get()) {
                    session.participants.contains_key(&user_id)
                } else {
                    return;
                }
            };

            if !already_participant {
                // Write Point 5: Check blocklist before adding mid-session joiner
                match db::check_blocklist(&self.state.db, user_id.get()).await {
                    Ok(true) => {
                        info!(user_id = %user_id, "mid_session_joiner_blocked — user opted out globally");
                        return;
                    }
                    Err(e) => {
                        tracing::error!("DB read failed (check_blocklist): {e}");
                        // Continue — allow join if DB is down
                    }
                    _ => {}
                }

                let display_name = guild
                    .members
                    .get(&user_id)
                    .map(|m| m.display_name().to_string())
                    .unwrap_or_else(|| format!("User {}", user_id));

                // Get session_id for DB write
                let session_id_str = {
                    let consent = self.state.consent.lock().await;
                    consent.get_session(guild_id.get()).map(|s| s.session_id.clone())
                };

                // Add as mid-session joiner
                {
                    let mut consent = self.state.consent.lock().await;
                    if let Some(session) = consent.get_session_mut(guild_id.get()) {
                        session.add_participant(user_id, display_name.clone(), true);
                    }
                }

                // Write Point 5: Add mid-session participant to Postgres
                if let Some(sid_str) = &session_id_str {
                    if let Ok(sid) = uuid::Uuid::parse_str(sid_str) {
                        if let Err(e) = db::add_participant(&self.state.db, &db::NewParticipant {
                            session_id: sid,
                            discord_user_id: user_id.get(),
                            mid_session_join: true,
                        }).await {
                            tracing::error!("DB write failed (add_participant mid-session): {e}");
                        }
                    }
                }

                info!(user_id = %user_id, name = %display_name, "mid_session_joiner");

                // Send consent prompt in text channel
                let embed = {
                    let consent = self.state.consent.lock().await;
                    if let Some(session) = consent.get_session(guild_id.get()) {
                        build_consent_embed(session)
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

        // Auto-stop: check if bot is alone
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

                // Re-check after 30s
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
                    let _ = manager.leave(guild_id).await;

                    commands::stop::auto_stop(&ctx_clone, guild_id.get(), &state).await;
                }
            });
        }
    }
}

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

    let (should_start, is_mid_session_accept, embed, session_id, channel_id) = {
        let mut manager = state.consent.lock().await;
        let session = match manager.get_session_mut(guild_id) {
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

        let is_mid_session = session.state == SessionState::Recording
            && session.participants[&user_id].mid_session_join;

        session.record_consent(user_id, scope);
        info!(user_id = %user_id, scope = ?scope, "consent_recorded");

        let embed = build_consent_embed(session);
        let should_start = session.state == SessionState::AwaitingConsent
            && session.all_responded()
            && session.evaluate_quorum();
        let is_mid_session_accept = is_mid_session && scope == ConsentScope::Full;
        let session_id = session.session_id.clone();
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
        let cu = state.consented_users.lock().await;
        if let Some(set) = cu.get(&guild_id) {
            let mut set = set.lock().await;
            set.insert(user_id.get());
            info!(user_id = %user_id, "mid_session_consent_granted — audio capture enabled");
        }
    }

    if should_start {
        info!("quorum_met — joining voice channel");
        let guild_id_obj = component.guild_id.unwrap();
        let manager = songbird::get(ctx).await.unwrap();

        match manager.join(guild_id_obj, channel_id).await {
            Ok(call) => {
                info!("voice_joined");

                // Get output dir from bundle
                let output_dir = {
                    let bundles = state.bundles.lock().await;
                    match bundles.get(&guild_id) {
                        Some(b) => b.pcm_dir(),
                        None => {
                            error!("no bundle found for guild {}", guild_id);
                            return Ok(());
                        }
                    }
                };

                // Build consented users set
                let consented_set = {
                    let consent_mgr = state.consent.lock().await;
                    let session = consent_mgr.get_session(guild_id).unwrap();
                    let set: HashSet<u64> = session
                        .consented_user_ids()
                        .into_iter()
                        .map(|uid| uid.get())
                        .collect();
                    Arc::new(tokio::sync::Mutex::new(set))
                };

                {
                    let mut cu = state.consented_users.lock().await;
                    cu.insert(guild_id, consented_set.clone());
                }

                // Create audio pipeline ONCE — channel + buffer task
                let audio_received = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let s3_prefix = format!("sessions/{}/{}/audio", guild_id, session_id);
                let (audio_tx, audio_handle) = AudioReceiver::create_pipeline(
                    state.s3.clone(), s3_prefix.clone(),
                );

                info!(s3_prefix = %s3_prefix, "registering_audio_receiver");

                // Attach to the Songbird Call (wires VoiceTick → channel)
                {
                    let mut handler = call.lock().await;
                    let ssrc_map = AudioReceiver::attach(
                        &mut handler, audio_tx.clone(), consented_set.clone(), audio_received.clone(),
                    );
                    let mut maps = state.ssrc_maps.lock().await;
                    maps.insert(guild_id, ssrc_map);
                    let mut ah = state.audio_handles.lock().await;
                    ah.insert(guild_id, audio_handle);
                }

                // Wait for DAVE to deliver decoded audio — retry voice join if needed
                const DAVE_WAIT_SECS: u64 = 5;
                const MAX_ATTEMPTS: u32 = 3;

                for attempt in 1..=MAX_ATTEMPTS {
                    info!(attempt = attempt, "waiting_for_dave");
                    tokio::time::sleep(std::time::Duration::from_secs(DAVE_WAIT_SECS)).await;

                    // Check if decoded audio arrived (someone was speaking)
                    if audio_received.load(std::sync::atomic::Ordering::Relaxed) {
                        info!(attempt = attempt, "dave_audio_confirmed");
                        break;
                    }

                    // Also check if SSRC mapping arrived — DAVE is working,
                    // just nobody spoke during the check window
                    let has_ssrc = {
                        let maps = state.ssrc_maps.lock().await;
                        if let Some(m) = maps.get(&guild_id) {
                            let inner = m.lock().await;
                            !inner.is_empty()
                        } else {
                            false
                        }
                    };
                    if has_ssrc {
                        info!(attempt = attempt, "dave_connection_confirmed — ssrc mapped, awaiting speech");
                        break;
                    }

                    if attempt == MAX_ATTEMPTS {
                        warn!("dave_failed — no audio or ssrc after {MAX_ATTEMPTS} attempts");
                        // Clean up: shut down buffer task, leave voice, remove state
                        {
                            let handle = {
                                let mut ah = state.audio_handles.lock().await;
                                ah.remove(&guild_id)
                            };
                            if let Some(h) = handle {
                                h.shutdown().await;
                            }
                        }
                        let _ = manager.leave(guild_id_obj).await;
                        {
                            let mut consent_mgr = state.consent.lock().await;
                            consent_mgr.remove_session(guild_id);
                        }
                        {
                            let mut bundles = state.bundles.lock().await;
                            bundles.remove(&guild_id);
                        }
                        component
                            .channel_id
                            .say(&ctx.http, "⚠️ Unable to receive audio. Try `/record` again.")
                            .await?;
                        return Ok(());
                    }

                    // Leave and rejoin to re-negotiate DAVE encryption
                    info!(attempt = attempt, "dave_retry — reconnecting voice");
                    let _ = manager.leave(guild_id_obj).await;
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                    match manager.join(guild_id_obj, channel_id).await {
                        Ok(new_call) => {
                            // Re-attach to the new Call — same channel, same buffer task
                            let mut handler = new_call.lock().await;
                            let ssrc_map = AudioReceiver::attach(
                                &mut handler, audio_tx.clone(), consented_set.clone(), audio_received.clone(),
                            );
                            drop(handler);

                            let mut maps = state.ssrc_maps.lock().await;
                            maps.insert(guild_id, ssrc_map);
                        }
                        Err(e) => {
                            error!(error = %e, attempt = attempt, "dave_rejoin_failed");
                            // Clean up everything on rejoin failure
                            {
                                let handle = {
                                    let mut ah = state.audio_handles.lock().await;
                                    ah.remove(&guild_id)
                                };
                                if let Some(h) = handle {
                                    h.shutdown().await;
                                }
                            }
                            {
                                let mut consent_mgr = state.consent.lock().await;
                                consent_mgr.remove_session(guild_id);
                            }
                            {
                                let mut bundles = state.bundles.lock().await;
                                bundles.remove(&guild_id);
                            }
                            component
                                .channel_id
                                .say(&ctx.http, "⚠️ Failed to reconnect to voice. Try `/record` again.")
                                .await?;
                            return Ok(());
                        }
                    }
                }

                // Update consent embed: remove buttons, show recording status
                let consent_msg = {
                    let mut consent_mgr = state.consent.lock().await;
                    if let Some(s) = consent_mgr.get_session_mut(guild_id) {
                        s.state = SessionState::Recording;
                    }
                    consent_mgr.get_session(guild_id).and_then(|s| s.consent_message)
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

                // Write Point 3: Update session state to recording in Postgres
                if let Ok(sid) = uuid::Uuid::parse_str(&session_id) {
                    if let Err(e) = db::update_session_state(&state.db, sid, "recording").await {
                        tracing::error!("DB write failed (update_session_state): {e}");
                    }
                }

                info!(session_id = %session_id, "recording_started");

                component
                    .channel_id
                    .say(&ctx.http, "Recording. Use `/stop` when done.")
                    .await?;
            }
            Err(e) => {
                error!(error = %e, "voice_join_failed");
                component
                    .channel_id
                    .say(&ctx.http, "Failed to join voice channel.")
                    .await?;

                let mut consent_mgr = state.consent.lock().await;
                consent_mgr.remove_session(guild_id);
            }
        }
    } else {
        // Check if quorum failed
        let manager = state.consent.lock().await;
        if let Some(session) = manager.get_session(guild_id) {
            if session.all_responded() && !session.evaluate_quorum() {
                drop(manager);
                let mut manager = state.consent.lock().await;
                manager.remove_session(guild_id);

                component
                    .channel_id
                    .say(
                        &ctx.http,
                        "Recording cancelled — consent requirements not met.",
                    )
                    .await?;
            }
        }
    }

    // Write Point 2: Record consent in Postgres (after voice join, non-critical path)
    {
        let session_uuid = uuid::Uuid::parse_str(&session_id).ok();
        if let Some(sid) = session_uuid {
            let scope_str = match scope {
                ConsentScope::Full => "full",
                ConsentScope::Decline => "decline",
                ConsentScope::DeclineAudio => "decline_audio",
            };
            if let Err(e) = db::record_consent(&state.db, sid, user_id.get(), scope_str).await {
                tracing::error!("DB write failed (record_consent): {e}");
            }
        }
    }

    // If scope is Full, send ephemeral license follow-up (non-blocking step 2)
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
                // Store followup info so we can edit it on session end
                {
                    let mut consent_mgr = state.consent.lock().await;
                    if let Some(session) = consent_mgr.get_session_mut(guild_id) {
                        session.license_followups.push((component.token.clone(), msg.id));
                    }
                }
                // Remove buttons before the 15-minute interaction token expires
                let http = ctx.http.clone();
                let interaction_token = component.token.clone();
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(14 * 60)).await;
                    let edit = CreateInteractionResponseFollowup::new()
                        .content("License preferences saved. Change them anytime on the participant portal.")
                        .components(vec![]);
                    let _ = http.edit_followup_message(&interaction_token, msg.id, &edit, vec![]).await;
                });
                // Store handle so it can be aborted on session end
                let mut tasks = state.license_cleanup_tasks.lock().await;
                tasks.entry(guild_id).or_default().push(handle);
            }
            Err(e) => {
                warn!(error = %e, "failed to send license preference followup");
            }
        }
    }

    Ok(())
}

/// Write Point 2b: Handle license preference button clicks (toggles).
async fn handle_license_button(
    ctx: &Context,
    component: &ComponentInteraction,
    state: &AppState,
) -> Result<(), serenity::Error> {
    let guild_id = component.guild_id.unwrap().get();
    let user_id = component.user.id;

    let field = match component.data.custom_id.as_str() {
        "license_no_llm" => "no_llm_training",
        "license_no_public" => "no_public_release",
        _ => return Ok(()),
    };

    let session_id_str = {
        let consent = state.consent.lock().await;
        consent.get_session(guild_id).map(|s| s.session_id.clone())
    };

    // Toggle the flag and read back current state
    let (no_llm, no_public) = if let Some(sid_str) = &session_id_str {
        if let Ok(sid) = uuid::Uuid::parse_str(sid_str) {
            if let Err(e) = db::toggle_license_flag(&state.db, sid, user_id.get(), field).await {
                tracing::error!("DB write failed (toggle_license_flag): {e}");
            }
            // Read back current flags
            db::get_license_flags(&state.db, sid, user_id.get())
                .await
                .unwrap_or((false, false))
        } else {
            (false, false)
        }
    } else {
        (false, false)
    };

    // Re-render buttons only — state expressed through button style, no text clutter
    let llm_style = if no_llm { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let public_style = if no_public { ButtonStyle::Danger } else { ButtonStyle::Secondary };
    let llm_label = if no_llm { "No LLM Training ✓" } else { "No LLM Training" };
    let public_label = if no_public { "No Public Release ✓" } else { "No Public Release" };

    component
        .create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content("Toggle restrictions on your audio:")
                    .components(vec![CreateActionRow::Buttons(vec![
                        CreateButton::new("license_no_llm")
                            .label(llm_label)
                            .style(llm_style),
                        CreateButton::new("license_no_public")
                            .label(public_label)
                            .style(public_style),
                    ])]),
            ),
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

    let config = Config::parse();
    let token = config.token.clone();

    info!("starting_bot");

    let pool = sqlx::PgPool::connect(&config.database_url)
        .await
        .expect("Failed to connect to Postgres");
    sqlx::migrate!()
        .run(&pool)
        .await
        .expect("Failed to run database migrations");

    // Clean up any sessions left in non-terminal states from a previous crash
    let abandoned = sqlx::query_scalar::<_, i64>(
        "UPDATE sessions SET status = 'abandoned' WHERE status IN ('awaiting_consent', 'recording', 'finalizing') RETURNING 1"
    )
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    if !abandoned.is_empty() {
        info!(count = abandoned.len(), "cleaned_up_abandoned_sessions");
    }

    let state = Arc::new(AppState::new(config, pool));

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
