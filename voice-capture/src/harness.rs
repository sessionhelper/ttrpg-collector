//! Dev-only E2E test harness HTTP endpoint.
//!
//! Lets an external test runner (curl, an ssh script, a future CI binary)
//! drive the collector end-to-end without a Discord client — no slash
//! commands, no consent-button clicks, no human in the loop. Pairs with
//! the `ttrpg-collector-feeder` fleet: the runner sends the feeders to a
//! voice channel via their control API, POSTs here to start recording,
//! drives the feeders to play their WAVs, then POSTs here again to stop.
//!
//! # DAVE/MLS staggering requirement
//!
//! Feeders MUST join voice one at a time with a ~5 second delay between
//! each join. Discord's DAVE protocol uses MLS for key exchange, and each
//! join triggers a proposal -> commit -> acknowledge cycle. If two feeders
//! join before the first cycle completes, the MLS commit pipeline collides:
//! the pending commit for feeder A is cleared when feeder B's proposal
//! arrives, causing A's add to be lost from the MLS group. The collector
//! then has no decryptor for A (NoDecryptorForUser), and A's audio is
//! silently dropped as PLC silence.
//!
//! Use `scripts/e2e-run.sh` which enforces this staggering automatically.
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

use axum::extract::{Query, State};
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

#[derive(Deserialize, Debug)]
struct StatusQuery {
    guild_id: u64,
}

#[derive(Serialize, Debug)]
struct StatusResponse {
    recording: bool,
    stable: bool,
    session_id: Option<String>,
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

/// Check recording status for a guild. The E2E test runner polls this
/// after /record to know when the DAVE heal check has settled and feeders
/// can start transmitting.
async fn status(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StatusQuery>,
) -> Json<StatusResponse> {
    let sessions = state.sessions.lock().await;
    match sessions.get(q.guild_id) {
        Some(s) => Json(StatusResponse {
            recording: matches!(s.phase, crate::session::Phase::Recording(_)),
            stable: s.is_stable(),
            session_id: Some(s.id.clone()),
        }),
        None => Json(StatusResponse {
            recording: false,
            stable: false,
            session_id: None,
        }),
    }
}

/// Start a recording session headlessly.
///
/// Mirrors the middle of `commands::record::handle_record` (enumerate voice
/// channel members → create session → data-api create_session →
/// add_participants_batch → bypass-consent every member), then jumps
/// straight to the "quorum met" path via `start_recording_headless`.
///
/// Admission policy: **only users in `BYPASS_CONSENT_USER_IDS` are
/// admitted to the session.** Everyone else in the voice channel — humans,
/// non-bypass bots, visiting music bots, anyone — is silently skipped.
/// Their audio never lands in S3 because they're not in the session's
/// `consented_users` set, so the voice receiver filters their packets at
/// the hot path regardless. This keeps humans safe-by-default even if
/// they share the channel during a harness run, and it means the harness
/// can run without requiring the channel to be scrubbed first.
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

    let bypass_ids = state.config.bypass_consent_user_ids();
    if bypass_ids.is_empty() {
        return Err(err(
            StatusCode::PRECONDITION_FAILED,
            "BYPASS_CONSENT_USER_IDS is empty; harness can't admit anyone",
        ));
    }

    // DAVE/MLS timing: feeders should join voice at least 10 seconds
    // before this endpoint is called, to give the MLS group time to
    // stabilize. If feeders join too close to the collector's join,
    // the MLS commit pipeline races and some speakers' decryptors
    // won't be established. The collector's DAVE heal task (spawned
    // after recording_started) will detect and reconnect if this
    // happens, but the 10-second buffer avoids it entirely.
    //
    // Build participant list directly from the bypass list — don't
    // enumerate voice channel members. The feeders join AFTER recording
    // starts (to get clean per-feeder DAVE key exchanges), so they won't
    // be in the voice channel when /record fires. We know exactly who
    // the participants will be from BYPASS_CONSENT_USER_IDS.
    //
    // Display names are looked up from the guild cache if available,
    // otherwise a fallback is generated from the user ID. The feeders
    // don't need to be in voice for this — they just need to be guild
    // members (which they are, since they were invited).
    let members: Vec<(UserId, String)> = {
        let guild = ctx
            .cache
            .guild(guild_id_obj)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "guild not in cache"))?
            .clone();

        bypass_ids
            .iter()
            .map(|&uid| {
                let user_id = UserId::new(uid);
                let name = guild
                    .members
                    .get(&user_id)
                    .map(|m| m.display_name().to_string())
                    .unwrap_or_else(|| format!("bot_{}", uid));
                (user_id, name)
            })
            .collect()
    };

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
        .route("/status", get(status))
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
