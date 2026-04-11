//! TTRPG session audio collector Discord bot — entry point.
//!
//! This file is intentionally thin. It owns only:
//!   - Module tree (`mod foo;` declarations)
//!   - Serenity `EventHandler` impl (thin dispatch into `commands::*` and
//!     `voice::events`)
//!   - `main()` — TLS init, tracing, config, Data API auth, heartbeat task,
//!     Songbird setup, client start
//!
//! All session/consent logic lives in `commands::*`. Voice state change
//! handling lives in `voice::events`. The audio hot path lives in
//! `voice::receiver`. The Data API client lives in `api_client`. This
//! module is a coordinator, not a kitchen sink.

mod api_client;
mod commands;
mod config;
mod harness;
mod session;
mod state;
mod storage;
mod telemetry;
mod voice;

use std::sync::Arc;

use clap::Parser;
use serenity::all::*;
use serenity::async_trait;
use songbird::driver::{DecodeConfig, DecodeMode};
use songbird::serenity::register_from_config;
use songbird::Config as SongbirdConfig;
use tracing::{error, info};

use crate::api_client::DataApiClient;
use crate::config::Config;
use crate::state::AppState;

/// Serenity event handler. Every arm spawns into a detached tokio task so
/// the gateway event loop can keep pumping while slow work (DAVE retries,
/// S3 uploads, session finalization) runs in the background. Blocking the
/// event handler would queue subsequent interactions and let their 3-second
/// response tokens expire before the handler starts running them.
struct Handler {
    state: Arc<AppState>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "bot_ready");

        // Publish the Context into AppState so the dev-only E2E harness
        // endpoint can access the cache + songbird manager. First caller
        // wins — subsequent ready events (e.g. after a reconnect) are
        // ignored because the OnceCell is already populated. That's fine:
        // the cache inside Context is Arc'd and shared across reconnects.
        let _ = self.state.ctx.set(ctx.clone());

        for guild in &ready.guilds {
            let cmds = vec![
                CreateCommand::new("record").description("Start recording this voice channel"),
                CreateCommand::new("stop").description("Stop the current recording"),
            ];
            if let Err(e) = guild.id.set_commands(&ctx.http, cmds).await {
                error!(guild_id = %guild.id, error = %e, "guild_command_registration_failed");
            }
        }
        info!(guild_id = %ready.guilds.first().map(|g| g.id.get()).unwrap_or(0), "guild_commands_registered");
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => {
                let state = self.state.clone();
                tokio::spawn(async move {
                    let result = match command.data.name.as_str() {
                        "record" => commands::record::handle_record(&ctx, &command, &state).await,
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
                    if let Err(e) =
                        commands::consent::handle_consent_button(&ctx, &component, &state).await
                    {
                        error!(error = %e, "consent_button_error");
                        // If the handler errored before ack'ing and the
                        // session is still in AwaitingConsent, clear it so
                        // the user can /record again without hitting the
                        // "already active" guard.
                        if let Some(gid) = component.guild_id {
                            let mut sessions = state.sessions.lock().await;
                            if let Some(session) = sessions.get(gid.get())
                                && matches!(session.phase, session::Phase::AwaitingConsent)
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

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        voice::events::handle_voice_state_update(ctx, old, new, self.state.clone()).await;
    }
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("chronicle_bot=info".parse().unwrap())
                .add_directive("songbird=error".parse().unwrap())
                .add_directive("songbird::driver::tasks::udp_rx=off".parse().unwrap())
                .add_directive("serenity=warn".parse().unwrap()),
        )
        .init();

    #[cfg(feature = "prometheus")]
    {
        metrics_exporter_prometheus::PrometheusBuilder::new()
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

    let api = DataApiClient::authenticate(
        &config.data_api_url,
        &config.data_api_shared_secret,
        "bot",
    )
    .await
    .expect("Failed to authenticate with Data API");

    info!("data_api_authenticated");

    let state = Arc::new(AppState::new(config, api));

    // Dev-only E2E harness HTTP server. No-op unless HARNESS_ENABLED=true.
    // Spawned before the serenity client starts so it's ready to serve
    // /health probes during startup; /record and /stop 503 until the
    // `ready` event populates AppState.ctx.
    harness::spawn(state.clone()).await;

    // Heartbeat task — keeps the Data API service session alive (reaped
    // server-side after 90 seconds of inactivity).
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

    let songbird_config =
        SongbirdConfig::default().decode_mode(DecodeMode::Decode(DecodeConfig::default()));

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
