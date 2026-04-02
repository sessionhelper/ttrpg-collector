mod commands;
mod config;
mod consent;
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
                            .description("Start recording this voice channel for the TTRPG dataset")
                            .add_option(
                                CreateCommandOption::new(
                                    CommandOptionType::String,
                                    "system",
                                    "Game system (e.g., D&D 5e)",
                                )
                                .required(false),
                            )
                            .add_option(
                                CreateCommandOption::new(
                                    CommandOptionType::String,
                                    "campaign",
                                    "Campaign name",
                                )
                                .required(false),
                            ),
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
                let display_name = guild
                    .members
                    .get(&user_id)
                    .map(|m| m.display_name().to_string())
                    .unwrap_or_else(|| format!("User {}", user_id));

                // Add as mid-session joiner
                {
                    let mut consent = self.state.consent.lock().await;
                    if let Some(session) = consent.get_session_mut(guild_id.get()) {
                        session.add_participant(user_id, display_name.clone(), true);
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

    // Mid-session joiner accepted — add to consented_users so their audio is captured
    if is_mid_session_accept {
        let cu = state.consented_users.lock().await;
        if let Some(set) = cu.get(&guild_id) {
            let mut set = set.lock().await;
            set.insert(user_id.get());
            info!(user_id = %user_id, "mid_session_consent_granted — audio capture enabled");
        }
    }

    // Update the embed
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

                info!(output_dir = %output_dir.display(), "registering_audio_receiver");

                let mut handler = call.lock().await;
                let ssrc_map = AudioReceiver::register(&mut handler, output_dir, consented_set);

                {
                    let mut maps = state.ssrc_maps.lock().await;
                    maps.insert(guild_id, ssrc_map);
                }

                {
                    let mut consent_mgr = state.consent.lock().await;
                    if let Some(s) = consent_mgr.get_session_mut(guild_id) {
                        s.state = SessionState::Recording;
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

    let state = Arc::new(AppState::new(config));

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
