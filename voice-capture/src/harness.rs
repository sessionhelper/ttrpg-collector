//! Dev-only E2E test harness HTTP endpoint.
//!
//! Post-refactor: the actor drives the recording lifecycle. The harness
//! still exposes its HTTP surface but routes every action through
//! `SessionCmd::*` on a `SessionHandle`. No `state.sessions.lock()` here.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serenity::all::*;
use tracing::{error, info, warn};

use crate::session::actor::{
    request, spawn_session, SessionCmd, StartRecordingOutcome,
};
use crate::session::Session;
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

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ready: state.ctx.get().is_some(),
        harness_enabled: state.config.harness_enabled,
        bypass_user_count: state.config.bypass_consent_user_ids().len(),
    })
}

async fn status(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StatusQuery>,
) -> Json<StatusResponse> {
    let handle = match state.sessions.get(&q.guild_id) {
        Some(h) => h.clone(),
        None => {
            return Json(StatusResponse {
                recording: false,
                stable: false,
                session_id: None,
            });
        }
    };
    let snapshot = request(&handle, |reply| SessionCmd::GetSnapshot { reply }).await;
    match snapshot {
        Ok(s) => Json(StatusResponse {
            recording: s.recording,
            stable: s.stable,
            session_id: Some(s.session_id),
        }),
        Err(_) => Json(StatusResponse {
            recording: false,
            stable: false,
            session_id: None,
        }),
    }
}

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
    let _channel_id = ChannelId::new(req.channel_id);

    let bypass_ids = state.config.bypass_consent_user_ids();
    if bypass_ids.is_empty() {
        return Err(err(
            StatusCode::PRECONDITION_FAILED,
            "BYPASS_CONSENT_USER_IDS is empty; harness can't admit anyone",
        ));
    }

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

    let initiator_id = ctx.cache.current_user().id;
    let mut session = Session::new(
        req.guild_id,
        req.channel_id,
        req.channel_id,
        initiator_id,
        state.config.min_participants,
        state.config.require_all_consent,
    );
    for (uid, name) in &members {
        session.add_participant(*uid, name.clone(), false);
    }
    let session_id = session.id.clone();

    // Spawn actor.
    let handle = match spawn_session(state.clone(), ctx.clone(), session) {
        Ok(h) => h,
        Err(_) => {
            return Err(err(
                StatusCode::CONFLICT,
                "a recording session is already active in this guild",
            ));
        }
    };

    // Data API: create_session + batch add participants.
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
        let _ = handle.send(SessionCmd::AutoStop).await;
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!("data-api create_session: {e}"),
        ));
    }

    let batch_input: Vec<(u64, bool, Option<String>)> = members
        .iter()
        .map(|(uid, name)| (uid.get(), false, Some(name.clone())))
        .collect();
    match state.api.add_participants_batch(session_uuid, &batch_input).await {
        Ok(rows) => {
            for ((user_id, _), row) in members.iter().zip(rows.iter()) {
                let _ = handle
                    .send(SessionCmd::SetParticipantUuid {
                        user_id: *user_id,
                        participant_uuid: row.id,
                    })
                    .await;
            }
        }
        Err(e) => {
            error!(error = %e, "harness add_participants_batch failed");
            let _ = state.api.abandon_session(session_uuid).await;
            let _ = handle.send(SessionCmd::AutoStop).await;
            return Err(err(
                StatusCode::BAD_GATEWAY,
                format!("data-api add_participants_batch: {e}"),
            ));
        }
    }

    // Bypass consent for every admitted member.
    let _ = handle
        .send(SessionCmd::BypassConsent {
            users: members.iter().map(|(uid, _)| *uid).collect(),
        })
        .await;
    for (uid, _) in &members {
        if let Err(e) = state.api.record_consent(session_uuid, uid.get(), "full").await {
            warn!(
                user_id = %uid,
                error = %e,
                "harness record_consent failed (non-fatal)",
            );
        }
    }

    info!(
        session_id = %session_id,
        participants = members.len(),
        "harness_record — starting recording pipeline",
    );

    // Fire StartRecording and await the outcome.
    let outcome = request(&handle, |reply| SessionCmd::StartRecording { reply }).await;
    match outcome {
        Ok(StartRecordingOutcome::Recording) => Ok(Json(RecordResponse { session_id })),
        Ok(StartRecordingOutcome::Preempted) => Err(err(
            StatusCode::GONE,
            "session preempted during startup (concurrent /stop?)",
        )),
        Ok(StartRecordingOutcome::DaveFailed) => Err(err(
            StatusCode::GATEWAY_TIMEOUT,
            "DAVE handshake failed after retries — no audio",
        )),
        Ok(StartRecordingOutcome::DaveRejoinFailed) => Err(err(
            StatusCode::BAD_GATEWAY,
            "mid-loop voice rejoin failed",
        )),
        Ok(StartRecordingOutcome::VoiceJoinFailed(msg)) => Err(err(
            StatusCode::BAD_GATEWAY,
            format!("voice join failed: {msg}"),
        )),
        Err(e) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("actor gone: {e}"),
        )),
    }
}

async fn stop(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StopRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _ctx = state
        .ctx
        .get()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "collector not ready yet"))?
        .clone();

    let handle = match state.sessions.get(&req.guild_id) {
        Some(h) => h.clone(),
        None => {
            return Err(err(
                StatusCode::NOT_FOUND,
                "no active recording session in guild",
            ));
        }
    };

    // Send AutoStop to preempt the session. The actor exits its run loop,
    // which finalizes the session and removes its DashMap entry. Poll
    // briefly for the DashMap removal so the harness caller sees "stop
    // complete" only after metadata upload has kicked off — matching the
    // pre-refactor await-to-completion semantics.
    let _ = handle.send(SessionCmd::AutoStop).await;

    let guild_id = req.guild_id;
    let state_poll = state.clone();
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while tokio::time::Instant::now() < deadline {
        if !state_poll.sessions.contains_key(&guild_id) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    info!(guild_id = req.guild_id, "harness_stop — finalized");
    Ok(Json(OkResponse { ok: true }))
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/record", post(record))
        .route("/stop", post(stop))
        .with_state(state)
}

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
