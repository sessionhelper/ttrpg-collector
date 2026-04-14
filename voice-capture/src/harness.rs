//! Dev-only E2E test harness HTTP surface.
//!
//! Every endpoint flows through the same `SessionCmd` actor surface as the
//! Discord handlers. There is no "bypass" — the harness IS the bypass, and
//! it exercises the real code path for enrolment, consent and license
//! mutation rather than short-circuiting it.
//!
//! Endpoints:
//!   - `GET  /health`         → `{ ready, harness_enabled }`
//!   - `GET  /status?guild`   → current session snapshot
//!   - `POST /record`         → spawn session
//!   - `POST /enrol`          → add a participant
//!   - `POST /consent`        → record consent
//!   - `POST /license`        → PATCH license flag
//!   - `POST /stop`           → stop + wait for finalize

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serenity::all::*;
use tracing::{error, info};

use crate::session::actor::{
    request, spawn_session, LicenseField, SessionCmd, SessionSnapshot,
};
use crate::session::{ConsentScope, Session};
use crate::state::AppState;

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

#[derive(Deserialize, Debug)]
struct EnrolRequest {
    guild_id: u64,
    user_id: u64,
    display_name: String,
    is_bot: bool,
}

#[derive(Serialize, Debug)]
struct EnrolResponse {
    participant_id: uuid::Uuid,
}

#[derive(Deserialize, Debug)]
struct ConsentRequest {
    guild_id: u64,
    user_id: u64,
    scope: String,
}

#[derive(Deserialize, Debug)]
struct LicenseRequest {
    guild_id: u64,
    user_id: u64,
    field: String,
    value: bool,
}

#[derive(Serialize, Debug)]
struct OkResponse {
    ok: bool,
}

#[derive(Serialize, Debug)]
#[cfg_attr(test, derive(Deserialize))]
struct HealthResponse {
    ready: bool,
    harness_enabled: bool,
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
    phase: Option<String>,
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
    })
}

async fn status(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StatusQuery>,
) -> Json<StatusResponse> {
    let Some(handle) = state.sessions.get(&q.guild_id).map(|e| e.clone()) else {
        return Json(StatusResponse {
            recording: false,
            stable: false,
            session_id: None,
            phase: None,
        });
    };
    match request(&handle, |reply| SessionCmd::GetSnapshot { reply }).await {
        Ok(SessionSnapshot { recording, stable, session_id, phase_label, .. }) => {
            Json(StatusResponse {
                recording,
                stable,
                session_id: Some(session_id),
                phase: Some(phase_label.to_string()),
            })
        }
        Err(_) => Json(StatusResponse {
            recording: false,
            stable: false,
            session_id: None,
            phase: None,
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
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "bot not ready"))?
        .clone();

    // No bypass list, no pre-gate data-api calls. The actor creates the
    // session row at gate-open.
    let session = Session::new(
        req.guild_id,
        req.channel_id,
        req.channel_id,
        ctx.cache.current_user().id,
        state.config.min_participants,
        state.config.require_all_consent,
    );
    let session_id = session.id.clone();

    spawn_session(state.clone(), ctx.clone(), session)
        .map_err(|_| err(StatusCode::CONFLICT, "session already active"))?;

    info!(%session_id, "harness_record — session spawned");
    Ok(Json(RecordResponse { session_id }))
}

async fn enrol(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EnrolRequest>,
) -> Result<Json<EnrolResponse>, (StatusCode, Json<ErrorResponse>)> {
    let handle = state
        .sessions
        .get(&req.guild_id)
        .map(|e| e.clone())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no active session in guild"))?;

    let result = request(&handle, |reply| SessionCmd::Enrol {
        user_id: UserId::new(req.user_id),
        display_name: req.display_name,
        is_bot: req.is_bot,
        reply,
    })
    .await;

    match result {
        Ok(Ok(participant_id)) => Ok(Json(EnrolResponse { participant_id })),
        Ok(Err(e)) => Err(err(StatusCode::CONFLICT, e.to_string())),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn consent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ConsentRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ErrorResponse>)> {
    let scope = match req.scope.as_str() {
        "full" => ConsentScope::Full,
        "decline" => ConsentScope::Decline,
        _ => return Err(err(StatusCode::BAD_REQUEST, "scope must be full or decline")),
    };
    let handle = state
        .sessions
        .get(&req.guild_id)
        .map(|e| e.clone())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no active session in guild"))?;

    let result = request(&handle, |reply| SessionCmd::RecordConsent {
        user: UserId::new(req.user_id),
        scope,
        reply,
    })
    .await;

    match result {
        Ok(Ok(_)) => Ok(Json(OkResponse { ok: true })),
        Ok(Err(e)) => Err(err(StatusCode::CONFLICT, e.to_string())),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn license(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LicenseRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ErrorResponse>)> {
    let field = match req.field.as_str() {
        "no_llm_training" => LicenseField::NoLlmTraining,
        "no_public_release" => LicenseField::NoPublicRelease,
        _ => return Err(err(StatusCode::BAD_REQUEST, "unknown license field")),
    };
    let handle = state
        .sessions
        .get(&req.guild_id)
        .map(|e| e.clone())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no active session in guild"))?;
    let result = request(&handle, |reply| SessionCmd::SetLicense {
        user: UserId::new(req.user_id),
        field,
        value: req.value,
        reply,
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(Json(OkResponse { ok: true })),
        Ok(Err(e)) => Err(err(StatusCode::CONFLICT, e.to_string())),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn stop(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StopRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ErrorResponse>)> {
    let handle = state
        .sessions
        .get(&req.guild_id)
        .map(|e| e.clone())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no active session in guild"))?;

    let _ = handle.send(SessionCmd::AutoStop).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while tokio::time::Instant::now() < deadline {
        if !state.sessions.contains_key(&req.guild_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(Json(OkResponse { ok: true }))
}

pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/record", post(record))
        .route("/enrol", post(enrol))
        .route("/consent", post(consent))
        .route("/license", post(license))
        .route("/stop", post(stop))
        .with_state(state)
}

pub async fn spawn(state: Arc<AppState>) {
    if !state.config.harness_enabled {
        return;
    }
    let port = state.config.harness_port;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::DataApiClient;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn cfg(harness_enabled: bool) -> Config {
        Config {
            token: "t".into(),
            data_api_url: "http://127.0.0.1:1".into(),
            data_api_shared_secret: "s".into(),
            local_buffer_dir_raw: "".into(),
            local_buffer_max_secs: 7200,
            stabilization_gate_secs: 3,
            min_participants: 1,
            require_all_consent: true,
            harness_enabled,
            harness_port: 8010,
        }
    }

    /// Build an AppState with a never-called DataApiClient by injecting a
    /// hand-rolled instance via unsafe transmute is a bad idea — instead use
    /// a HTTP client that won't be called: every handler we test ends in
    /// "bot not ready" or "no session" before it ever touches the api.
    fn app_state_sans_ctx() -> Arc<AppState> {
        // We can't actually build a DataApiClient without a running server;
        // for these tests the harness endpoints either 503 (not ready) or
        // 404 (no session) BEFORE touching the api, so constructing a
        // dummy client that panics on use is fine.
        let api = DataApiClient::test_stub();
        Arc::new(AppState::new(cfg(true), api))
    }

    #[tokio::test]
    async fn health_endpoint_reports_state() {
        let state = app_state_sans_ctx();
        let app = router(state);
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let health: HealthResponse = serde_json::from_slice(&body).unwrap();
        assert!(!health.ready, "ctx not set → ready=false");
        assert!(health.harness_enabled);
    }

    #[tokio::test]
    async fn enrol_404s_when_no_session() {
        let state = app_state_sans_ctx();
        let app = router(state);
        let body = serde_json::json!({
            "guild_id": 42u64,
            "user_id": 100u64,
            "display_name": "alice",
            "is_bot": false,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/enrol")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn consent_404s_when_no_session() {
        let state = app_state_sans_ctx();
        let app = router(state);
        let body = serde_json::json!({
            "guild_id": 42u64,
            "user_id": 100u64,
            "scope": "full",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/consent")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn license_404s_when_no_session() {
        let state = app_state_sans_ctx();
        let app = router(state);
        let body = serde_json::json!({
            "guild_id": 42u64,
            "user_id": 100u64,
            "field": "no_llm_training",
            "value": true,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/license")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn consent_rejects_invalid_scope() {
        let state = app_state_sans_ctx();
        let app = router(state);
        let body = serde_json::json!({
            "guild_id": 42u64,
            "user_id": 100u64,
            "scope": "not-a-scope",
        });
        let req = Request::builder()
            .method("POST")
            .uri("/consent")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn license_rejects_unknown_field() {
        let state = app_state_sans_ctx();
        let app = router(state);
        let body = serde_json::json!({
            "guild_id": 42u64,
            "user_id": 100u64,
            "field": "not-a-field",
            "value": true,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/license")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
