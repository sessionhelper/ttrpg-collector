//! Feeder bot — part of the E2E test harness.
//!
//! A minimal Discord bot that joins a voice channel and plays a pre-recorded
//! WAV file on demand. Used in E2E tests to simulate real participants talking
//! into a voice channel while the collector records them.
//!
//! Not part of the production binary: gated behind the `e2e-harness` cargo
//! feature so prod builds never include it.
//!
//! Control surface: a tiny axum server bound to 127.0.0.1 exposes /health,
//! /join, /play, /stop, /leave. An external test runner drives these to
//! orchestrate multi-bot scenarios.
//!
//! Env:
//!   DISCORD_TOKEN  — bot token
//!   FEEDER_NAME    — short name for logs (e.g. "moe")
//!   AUDIO_FILE     — absolute path to the WAV to play on /play
//!   CONTROL_PORT   — loopback port for the control server (default 8003)

#![cfg(feature = "e2e-harness")]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serenity::all::*;
use serenity::async_trait;
use songbird::driver::{DecodeConfig, DecodeMode};
use songbird::input::File as SongbirdFile;
use songbird::serenity::register_from_config;
use songbird::tracks::TrackHandle;
use songbird::{Call, Config as SongbirdConfig, Songbird};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Shared state between the serenity client and the axum control server.
/// The serenity client populates `songbird` + `self_user_id` on ready; the
/// axum routes read them to drive voice operations.
struct AppState {
    name: String,
    audio_file: PathBuf,
    songbird: Mutex<Option<Arc<Songbird>>>,
    self_user_id: Mutex<Option<u64>>,
    // Current Call handle and most recent TrackHandle, so /stop + /leave can
    // tear down cleanly. We hold the call Arc (not a lock on it) so we can
    // re-acquire the songbird mutex on each control call.
    current_call: Mutex<Option<Arc<tokio::sync::Mutex<Call>>>>,
    current_track: Mutex<Option<TrackHandle>>,
    current_guild: Mutex<Option<GuildId>>,
}

impl AppState {
    fn new(name: String, audio_file: PathBuf) -> Self {
        Self {
            name,
            audio_file,
            songbird: Mutex::new(None),
            self_user_id: Mutex::new(None),
            current_call: Mutex::new(None),
            current_track: Mutex::new(None),
            current_guild: Mutex::new(None),
        }
    }
}

struct Handler {
    state: Arc<AppState>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(
            feeder = %self.state.name,
            user = %ready.user.name,
            user_id = %ready.user.id,
            "bot_ready"
        );
        *self.state.self_user_id.lock().await = Some(ready.user.id.get());
        // Pull the Songbird manager out of the TypeMap once at ready time.
        let manager = songbird::get(&ctx)
            .await
            .expect("songbird not registered on client");
        *self.state.songbird.lock().await = Some(manager);
    }
}

#[derive(Deserialize)]
struct JoinReq {
    guild_id: u64,
    channel_id: u64,
}

#[derive(Serialize)]
struct HealthResp {
    name: String,
    user_id: Option<u64>,
    in_voice: bool,
    playing: bool,
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResp> {
    let user_id = *state.self_user_id.lock().await;
    let in_voice = state.current_call.lock().await.is_some();
    let playing = state.current_track.lock().await.is_some();
    Json(HealthResp {
        name: state.name.clone(),
        user_id,
        in_voice,
        playing,
    })
}

/// Turn any error into a 500 with a JSON message. The harness is local-only
/// so leaking error strings to the caller is fine and useful for debugging.
fn err500<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn join(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JoinReq>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let manager = state
        .songbird
        .lock()
        .await
        .clone()
        .ok_or_else(|| err500("songbird not ready"))?;
    let guild = GuildId::new(req.guild_id);
    let channel = ChannelId::new(req.channel_id);
    let call = manager.join(guild, channel).await.map_err(err500)?;
    *state.current_call.lock().await = Some(call);
    *state.current_guild.lock().await = Some(guild);
    info!(feeder = %state.name, guild_id = req.guild_id, channel_id = req.channel_id, "joined_voice");
    Ok(StatusCode::OK)
}

async fn play(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let call = state
        .current_call
        .lock()
        .await
        .clone()
        .ok_or_else(|| err500("not in a voice channel"))?;
    if !state.audio_file.exists() {
        return Err(err500(format!(
            "audio file missing: {}",
            state.audio_file.display()
        )));
    }
    let input = SongbirdFile::new(state.audio_file.clone()).into();
    let track = {
        let mut handler = call.lock().await;
        handler.play_input(input)
    };
    *state.current_track.lock().await = Some(track);
    info!(feeder = %state.name, file = %state.audio_file.display(), "playing");
    Ok(StatusCode::OK)
}

async fn stop(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(track) = state.current_track.lock().await.take() {
        if let Err(e) = track.stop() {
            warn!(feeder = %state.name, error = %e, "track_stop_failed");
        }
    }
    info!(feeder = %state.name, "stopped");
    StatusCode::OK
}

async fn leave(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Clear track first — the voice connection is about to go away.
    *state.current_track.lock().await = None;
    let manager = state
        .songbird
        .lock()
        .await
        .clone()
        .ok_or_else(|| err500("songbird not ready"))?;
    let guild = state
        .current_guild
        .lock()
        .await
        .take()
        .ok_or_else(|| err500("not in a guild"))?;
    manager.leave(guild).await.map_err(err500)?;
    *state.current_call.lock().await = None;
    info!(feeder = %state.name, guild_id = %guild, "left_voice");
    Ok(StatusCode::OK)
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,serenity=warn,songbird=warn")),
        )
        .init();

    let token = std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let name = std::env::var("FEEDER_NAME").unwrap_or_else(|_| "feeder".to_string());
    let audio_file = PathBuf::from(
        std::env::var("AUDIO_FILE").expect("AUDIO_FILE not set — path to WAV to play"),
    );
    if !audio_file.exists() {
        panic!("AUDIO_FILE does not exist: {}", audio_file.display());
    }
    let control_port: u16 = std::env::var("CONTROL_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8003);

    let state = Arc::new(AppState::new(name.clone(), audio_file));

    let intents = GatewayIntents::GUILD_VOICE_STATES;
    let songbird_config =
        SongbirdConfig::default().decode_mode(DecodeMode::Decode(DecodeConfig::default()));

    let handler = Handler {
        state: state.clone(),
    };
    let client_builder = Client::builder(&token, intents).event_handler(handler);
    let mut client = register_from_config(client_builder, songbird_config)
        .await
        .expect("failed to build serenity client");

    let app = Router::new()
        .route("/health", get(health))
        .route("/join", post(join))
        .route("/play", post(play))
        .route("/stop", post(stop))
        .route("/leave", post(leave))
        .with_state(state.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], control_port));
    info!(feeder = %name, %addr, "control_server_listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind control port failed");

    // Run serenity and axum concurrently. If either exits, the process dies
    // so the container restarts — keeps the test harness honest.
    tokio::select! {
        res = client.start() => {
            if let Err(e) = res {
                error!(error = %e, "serenity_client_exited");
            }
        }
        res = axum::serve(listener, app) => {
            if let Err(e) = res {
                error!(error = %e, "axum_server_exited");
            }
        }
    }
}
