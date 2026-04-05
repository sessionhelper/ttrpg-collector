//! Dev-only E2E test harness HTTP endpoint.
//!
//! Lets an external test runner (curl, an ssh script, a future CI binary)
//! drive the collector end-to-end without a Discord client — no slash
//! commands, no consent-button clicks, no human in the loop. Pairs with
//! the `ttrpg-collector-feeder` fleet: the runner sends the feeders to a
//! voice channel via their control API, POSTs here to start recording,
//! drives the feeders to play their WAVs, then POSTs here again to stop.
//!
//! # Routes
//!
//! - `GET  /health` — liveness probe; also reports whether the collector
//!   has received its first `ready` event (i.e. the serenity `Context`
//!   is available).
//! - `POST /record` with `{"guild_id": u64, "channel_id": u64}` — start
//!   a recording session with the given voice channel's current members,
//!   auto-consenting every member in `BYPASS_CONSENT_USER_IDS`. Returns
//!   `{"session_id": "..."}` on success or an error envelope on failure.
//! - `POST /stop` with `{"guild_id": u64}` — finalize the active session
//!   for the guild. Returns `{"ok": true}` on success.
//!
//! # Safety
//!
//! This endpoint **must never be enabled in production.** It bypasses
//! the consent UI entirely and starts recording anyone in the voice
//! channel whose Discord user ID is in `BYPASS_CONSENT_USER_IDS`. Prod
//! leaves `HARNESS_ENABLED=false` and the axum server is never spawned;
//! the dev compose file sets `HARNESS_ENABLED=true` and binds the port
//! loopback-only via `127.0.0.1:8010:8010` so it's unreachable from the
//! public internet even on the dev VPS.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serenity::all::*;
use tracing::{error, info, warn};

use crate::commands::consent::{start_recording_headless, HeadlessStartOutcome};
use crate::commands::stop::finalize_session;
use crate::session::{ConsentScope, Session};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct RecordRequest {
    guild_id: u64,
    channel_id: u64,
}

#[derive(Serialize, Debug)]
struct RecordResponse {
    session_id: String,
}

#[derive(Deserialize, Debug)]
struct StopRequest {
    guild_id: u64,
}

#[derive(Serialize, Debug)]
struct OkResponse {
    ok: bool,
}

#[derive(Serialize, Debug)]
struct HealthResponse {
    /// True once `Handler::ready` has populated the shared `Context` in
    /// `AppState::ctx`. Before then, /record and /stop return 503.
    ready: bool,
    harness_enabled: bool,
    bypass_user_count: usize,
}

#[derive(Serialize, Debug)]
struct ErrorResponse {
    error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ready: state.ctx.get().is_some(),
        harness_enabled: state.config.harness_enabled,
        bypass_user_count: state.config.bypass_consent_user_ids().len(),
    })
}

/// Start a recording session headlessly.
///
/// Mirrors the middle of `commands::record::handle_record` (enumerate voice
/// channel members → create session → data-api create_session →
/// add_participants_batch → bypass-consent every member), then jumps
/// straight to the "quorum met" path via `start_recording_headless`.
///
/// Only users in `BYPASS_CONSENT_USER_IDS` are admitted — if the voice
/// channel contains any human user (bot=false), the harness flow would
/// stall waiting for their consent, which is exactly the failure mode
/// we're trying to avoid. So this endpoint hard-fails on human presence.
async fn record(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RecordRequest>,
) -> Result<Json<RecordResponse>, (StatusCode, Json<ErrorResponse>)> {
    let ctx = state
        .ctx
        .get()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "collector not ready yet"))?
        .clone();

    let guild_id_obj = GuildId::new(req.guild_id);
    let channel_id = ChannelId::new(req.channel_id);

    // Enumerate voice channel members from cache. Mirrors the logic in
    // commands::record::handle_record, but without the "user's voice
    // channel" lookup (the harness caller tells us which channel).
    let bypass_ids = state.config.bypass_consent_user_ids();
    if bypass_ids.is_empty() {
        return Err(err(
            StatusCode::PRECONDITION_FAILED,
            "BYPASS_CONSENT_USER_IDS is empty; harness can't admit anyone",
        ));
    }

    let (members, human_present) = {
        let guild = ctx
            .cache
            .guild(guild_id_obj)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "guild not in cache"))?
            .clone();

        let mut admitted: Vec<(UserId, String)> = Vec::new();
        let mut human_present = false;
        for (uid, vs) in guild.voice_states.iter() {
            if vs.channel_id != Some(channel_id) {
                continue;
            }
            let Some(member) = guild.members.get(uid) else {
                continue;
            };
            if member.user.bot {
                if bypass_ids.contains(&uid.get()) {
                    admitted.push((*uid, member.display_name().to_string()));
                }
                // non-bypass bots silently skipped (same as prod record flow)
            } else {
                human_present = true;
            }
        }
        (admitted, human_present)
    };

    if human_present {
        return Err(err(
            StatusCode::CONFLICT,
            "human user present in voice channel; harness requires bot-only channel",
        ));
    }
    if members.is_empty() {
        return Err(err(
            StatusCode::PRECONDITION_FAILED,
            "no bypass-eligible members in voice channel",
        ));
    }

    // Build Session locally (uses collector bot's own user id as initiator,
    // since there's no human command.user.id in the headless path).
    let initiator_id = ctx.cache.current_user().id;
    let mut session = Session::new(
        req.guild_id,
        req.channel_id,
        req.channel_id, // text channel same as voice for harness; unused in headless
        initiator_id,
        state.config.min_participants,
        state.config.require_all_consent,
    );
    for (uid, name) in &members {
        session.add_participant(*uid, name.clone(), false);
    }
    let session_id = session.id.clone();

    // Atomic reserve.
    {
        let mut sessions = state.sessions.lock().await;
        if sessions.try_insert(session).is_err() {
            return Err(err(
                StatusCode::CONFLICT,
                "a recording session is already active in this guild",
            ));
        }
    }

    // Data API: create_session + add_participants_batch.
    let session_uuid = uuid::Uuid::parse_str(&session_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("bad session id: {e}")))?;
    let s3_prefix = format!("sessions/{}/{}", req.guild_id, session_id);
    if let Err(e) = state
        .api
        .create_session(
            session_uuid,
            req.guild_id as i64,
            chrono::Utc::now(),
            None,
            None,
            Some(s3_prefix),
        )
        .await
    {
        error!(error = %e, "harness create_session failed");
        let mut sessions = state.sessions.lock().await;
        sessions.remove(req.guild_id);
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!("data-api create_session: {e}"),
        ));
    }

    let batch_input: Vec<(u64, bool)> = members.iter().map(|(uid, _)| (uid.get(), false)).collect();
    match state.api.add_participants_batch(session_uuid, &batch_input).await {
        Ok(rows) => {
            let mut sessions = state.sessions.lock().await;
            if let Some(s) = sessions.get_mut(req.guild_id) {
                for ((user_id, _), row) in members.iter().zip(rows.iter()) {
                    s.set_participant_uuid(*user_id, row.id);
                }
            }
        }
        Err(e) => {
            error!(error = %e, "harness add_participants_batch failed");
            let _ = state.api.abandon_session(session_uuid).await;
            let mut sessions = state.sessions.lock().await;
            sessions.remove(req.guild_id);
            return Err(err(
                StatusCode::BAD_GATEWAY,
                format!("data-api add_participants_batch: {e}"),
            ));
        }
    }

    // Bypass-consent every admitted member locally + remotely. Same pattern
    // as the handle_record bypass path; duplicated here to avoid touching
    // that code path.
    let to_bypass: Vec<(UserId, uuid::Uuid)> = {
        let sessions = state.sessions.lock().await;
        match sessions.get(req.guild_id) {
            Some(s) => members
                .iter()
                .filter_map(|(uid, _)| s.participant_uuid(*uid).map(|pid| (*uid, pid)))
                .collect(),
            None => Vec::new(),
        }
    };

    {
        let mut sessions = state.sessions.lock().await;
        if let Some(s) = sessions.get_mut(req.guild_id) {
            for (uid, _) in &to_bypass {
                s.record_consent(*uid, ConsentScope::Full);
            }
        }
    }

    for (uid, pid) in &to_bypass {
        if let Err(e) = state.api.record_consent_by_id(*pid, "full").await {
            warn!(
                user_id = %uid,
                participant_id = %pid,
                error = %e,
                "harness record_consent_by_id failed (non-fatal)",
            );
        }
    }

    info!(
        session_id = %session_id,
        participants = members.len(),
        "harness_record — starting recording pipeline",
    );

    // Jump straight to the quorum-met path. This does the voice join, DAVE
    // wait, transition to Recording state, and sets status=recording on
    // the data-api row.
    match start_recording_headless(
        &ctx,
        &state,
        req.guild_id,
        guild_id_obj,
        session_id.clone(),
        channel_id,
    )
    .await
    {
        HeadlessStartOutcome::Recording => Ok(Json(RecordResponse { session_id })),
        HeadlessStartOutcome::Preempted => Err(err(
            StatusCode::GONE,
            "session preempted during startup (concurrent /stop?)",
        )),
        HeadlessStartOutcome::DaveFailed => Err(err(
            StatusCode::GATEWAY_TIMEOUT,
            "DAVE handshake failed after retries — no audio",
        )),
        HeadlessStartOutcome::DaveRejoinFailed => Err(err(
            StatusCode::BAD_GATEWAY,
            "mid-loop voice rejoin failed",
        )),
        HeadlessStartOutcome::VoiceJoinFailed(msg) => Err(err(
            StatusCode::BAD_GATEWAY,
            format!("voice join failed: {msg}"),
        )),
    }
}

/// Finalize the active session for a guild. Thin wrapper over the existing
/// `commands::stop::finalize_session` which already handles every phase
/// (AwaitingConsent, StartingRecording, Recording) and sets status=uploaded
/// at the end via the same code path as the /stop slash command.
async fn stop(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StopRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ErrorResponse>)> {
    let ctx = state
        .ctx
        .get()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "collector not ready yet"))?
        .clone();

    // finalize_session internally no-ops if there's no active session for
    // the guild. We check first so the harness caller gets a 404 instead of
    // a misleading 200.
    {
        let sessions = state.sessions.lock().await;
        if sessions.get(req.guild_id).is_none() {
            return Err(err(
                StatusCode::NOT_FOUND,
                "no active recording session in guild",
            ));
        }
    }

    finalize_session(&ctx, &state, req.guild_id).await;
    info!(guild_id = req.guild_id, "harness_stop — finalized");
    Ok(Json(OkResponse { ok: true }))
}

// ---------------------------------------------------------------------------
// Server bootstrap
// ---------------------------------------------------------------------------

/// Build the axum router. Separated from the listener bind so tests can
/// exercise the routes without opening a TCP socket.
fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/record", post(record))
        .route("/stop", post(stop))
        .with_state(state)
}

/// Spawn the harness HTTP server. Call this once from `main` after
/// `AppState` is constructed. Binds `0.0.0.0:HARNESS_PORT` inside the
/// container — host-side safety comes from the compose port mapping
/// (`127.0.0.1:HARNESS_PORT:HARNESS_PORT`), never from the in-container
/// bind address.
pub async fn spawn(state: Arc<AppState>) {
    if !state.config.harness_enabled {
        return;
    }
    let port = state.config.harness_port;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let app = router(state);

    tokio::spawn(async move {
        info!(%addr, "harness_http_listening");
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, "harness_bind_failed");
                return;
            }
        };
        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "harness_server_exited");
        }
    });
}
