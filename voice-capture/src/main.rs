mod commands;
mod config;
mod consent;
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
use tracing::{error, info};

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
        "consent_decline_audio" => ConsentScope::DeclineAudio,
        "consent_decline" => ConsentScope::Decline,
        _ => return Ok(()),
    };

    let (should_start, embed, session_id, channel_id) = {
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

        session.record_consent(user_id, scope);
        info!(user_id = %user_id, scope = ?scope, "consent_recorded");

        let embed = build_consent_embed(session);
        let should_start = session.all_responded() && session.evaluate_quorum();
        let session_id = session.session_id.clone();
        let channel_id = ChannelId::new(session.channel_id);

        (should_start, embed, session_id, channel_id)
    };

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

                info!(output_dir = %output_dir.display(), "registering_audio_receiver");

                let mut handler = call.lock().await;
                let ssrc_map = AudioReceiver::register(&mut handler, output_dir);

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
                .add_directive("songbird=warn".parse().unwrap())
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
