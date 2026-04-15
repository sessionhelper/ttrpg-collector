//! chronicle-bot entry point.
//!
//! Thin coordinator:
//!   - module tree
//!   - EventHandler dispatch (spawn-and-return per §8)
//!   - main(): TLS + tracing + config + data-api auth + startup buffer sweep +
//!     metrics + periodic cache-size sampler + Songbird setup + client start
//!
//! No session logic lives here. See `commands::*` and `session::*`.

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

/// Ring buffer of recently-seen interaction IDs.
///
/// Protects against serenity gateway RESUME replaying an interaction we
/// already processed — Discord would then return 404 `Unknown interaction`
/// on our second ack attempt. We can't stop the replay (it lives inside
/// serenity), but we can recognise duplicates by ID and skip them.
///
/// Snowflake IDs are globally unique across Discord, so a seen-before ID
/// is unambiguously a replay; no risk of dropping legitimate work.
///
/// Capacity 128 ≈ 5 s of peak interaction traffic for a small guild.
const RECENT_INTERACTION_CAPACITY: usize = 128;

#[derive(Default)]
struct RecentInteractions {
    ids: std::sync::Mutex<std::collections::VecDeque<u64>>,
}

impl RecentInteractions {
    /// Record `id`. Returns `true` if this is the first time we've seen it
    /// (and the caller should proceed); `false` if it's a duplicate replay.
    fn record(&self, id: u64) -> bool {
        let Ok(mut q) = self.ids.lock() else {
            return true;
        };
        if q.contains(&id) {
            return false;
        }
        if q.len() >= RECENT_INTERACTION_CAPACITY {
            q.pop_front();
        }
        q.push_back(id);
        true
    }
}

struct Handler {
    state: Arc<AppState>,
    recent: Arc<RecentInteractions>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "bot_ready");
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
        info!(
            guild_id = %ready.guilds.first().map(|g| g.id.get()).unwrap_or(0),
            "guild_commands_registered"
        );
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let received_at = std::time::Instant::now();
        match interaction {
            Interaction::Command(command) => {
                let state = self.state.clone();
                let cmd_name = command.data.name.clone();
                let interaction_id = command.id.get();
                if !self.recent.record(interaction_id) {
                    info!(
                        interaction_id,
                        command = %cmd_name,
                        "skipping duplicate interaction (gateway resume replay)"
                    );
                    metrics::counter!("chronicle_interactions_deduped_total").increment(1);
                    return;
                }
                // Discord snowflake → created-at lag tells us how stale the
                // interaction was by the time our gateway delivered it.
                // If this is > 2800ms we're racing the 3s ack window.
                let discord_created_ms = (interaction_id >> 22) + 1420070400000;
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let gateway_lag_ms = now_ms.saturating_sub(discord_created_ms);
                let token_prefix: String = command.token.chars().take(8).collect();
                tokio::spawn(async move {
                    let spawn_delay_us = received_at.elapsed().as_micros();
                    tracing::info!(
                        interaction_id,
                        command = %cmd_name,
                        spawn_delay_us,
                        gateway_lag_ms,
                        token_prefix = %token_prefix,
                        "interaction_handler_entered"
                    );
                    let result = match cmd_name.as_str() {
                        "record" => commands::record::handle_record(&ctx, &command, &state).await,
                        "stop" => commands::stop::handle_stop(&ctx, &command, &state).await,
                        _ => Ok(()),
                    };
                    if let Err(e) = result {
                        error!(error = ?e, interaction_id, gateway_lag_ms, "command_error");
                    }
                });
            }
            Interaction::Component(component) => {
                let state = self.state.clone();
                let component_id = component.id.get();
                if !self.recent.record(component_id) {
                    info!(
                        interaction_id = component_id,
                        custom_id = %component.data.custom_id,
                        "skipping duplicate component click (gateway resume replay)"
                    );
                    metrics::counter!("chronicle_interactions_deduped_total").increment(1);
                    return;
                }
                tokio::spawn(async move {
                    if let Err(e) =
                        commands::consent::handle_consent_button(&ctx, &component, &state).await
                    {
                        error!(error = %e, "consent_button_error");
                    }
                });
            }
            _ => {}
        }
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let state = self.state.clone();
        tokio::spawn(async move {
            voice::events::handle_voice_state_update(ctx, old, new, state).await;
        });
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

    // Startup buffer sweep — remove any stale session subdirs from a crashed
    // prior run so we don't accidentally stream them at next Accept.
    let buffer_root = config.local_buffer_dir();
    voice::buffer::sweep_on_startup(&buffer_root);

    let api = DataApiClient::authenticate(
        &config.data_api_url,
        &config.data_api_shared_secret,
        "bot",
    )
    .await
    .expect("Failed to authenticate with Data API");
    info!("data_api_authenticated");

    let state = Arc::new(AppState::new(config, api));

    harness::spawn(state.clone()).await;

    // Data-API heartbeat daemon.
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

    // Periodic cache-size gauge. Walk the buffer dir on a blocking thread
    // so it doesn't starve the tokio worker pool.
    {
        let state_gauge = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                let root = state_gauge.config.local_buffer_dir();
                let total = tokio::task::spawn_blocking(move || {
                    voice::buffer::walk_total_bytes(&root).unwrap_or(0)
                })
                .await
                .unwrap_or(0);
                metrics::gauge!("chronicle_prerolled_chunks_cached_bytes").set(total as f64);
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
        recent: Arc::new(RecentInteractions::default()),
    };

    let client_builder = Client::builder(&token, intents).event_handler(handler);
    let mut client = register_from_config(client_builder, songbird_config)
        .await
        .expect("Error creating client");

    if let Err(e) = client.start().await {
        error!(error = %e, "client_error");
    }
}

#[cfg(test)]
mod dedupe_tests {
    use super::*;

    #[test]
    fn first_record_returns_true() {
        let r = RecentInteractions::default();
        assert!(r.record(42));
    }

    #[test]
    fn duplicate_record_returns_false() {
        let r = RecentInteractions::default();
        assert!(r.record(42));
        assert!(!r.record(42), "replay should be deduped");
        assert!(!r.record(42), "second replay still deduped");
    }

    #[test]
    fn distinct_ids_all_accepted() {
        let r = RecentInteractions::default();
        for i in 0..50 {
            assert!(r.record(i), "id {i} first-seen");
        }
    }

    #[test]
    fn eviction_past_capacity() {
        let r = RecentInteractions::default();
        for i in 0..(RECENT_INTERACTION_CAPACITY as u64) {
            r.record(i);
        }
        // id 0 is still in the window.
        assert!(!r.record(0));
        // Push one more, evicting 0.
        assert!(r.record(999));
        // Now 0 is outside the window; a replay is accepted again.
        assert!(
            r.record(0),
            "once evicted, id 0 is treated as fresh — acceptable trade-off for bounded memory"
        );
    }
}
