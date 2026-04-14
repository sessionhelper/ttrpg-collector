//! HTTP client for the Data API.
//!
//! Replaces direct Postgres + S3 access. Every persistence operation
//! goes through the Data API as an HTTP call.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::storage::pseudonymize;

// ---------------------------------------------------------------------------
// Response types (match Data API's JSON responses)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SessionResponse {
    pub id: Uuid,
    pub guild_id: i64,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub game_system: Option<String>,
    pub campaign_name: Option<String>,
    pub participant_count: Option<i32>,
    pub s3_prefix: Option<String>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ParticipantResponse {
    pub id: Uuid,
    pub session_id: Uuid,
    pub pseudo_id: String,
    pub consent_scope: Option<String>,
    pub consented_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub data_wiped_at: Option<DateTime<Utc>>,
    pub mid_session_join: bool,
    pub no_llm_training: bool,
    pub no_public_release: bool,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct UserResponse {
    pub pseudo_id: String,
    #[serde(default)]
    pub is_admin: bool,
    #[serde(default)]
    pub data_wiped_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct AuthResponse {
    pub session_token: String,
}


// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CreateSessionRequest {
    id: Uuid,
    guild_id: i64,
    started_at: DateTime<Utc>,
    game_system: Option<String>,
    campaign_name: Option<String>,
    s3_prefix: Option<String>,
}

#[derive(Serialize)]
struct UpdateSessionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    participant_count: Option<i32>,
}

#[derive(Serialize)]
struct CreateUserRequest {
    pseudo_id: String,
}

#[derive(Serialize)]
struct DisplayNameRequest {
    display_name: String,
    source: &'static str,
}

#[derive(Serialize)]
struct AddParticipantRequest {
    pseudo_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mid_session_join: Option<bool>,
}

#[derive(Serialize)]
struct BatchAddParticipantsRequest {
    participants: Vec<AddParticipantRequest>,
}

#[derive(Serialize)]
struct UpdateConsentRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    consent_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    consented_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct UpdateLicenseRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    no_llm_training: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_public_release: Option<bool>,
}

#[derive(Serialize)]
struct AuthRequest {
    shared_secret: String,
    service_name: String,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// HTTP client for the Data API. Thread-safe, meant to be wrapped in Arc.
pub struct DataApiClient {
    client: reqwest::Client,
    base_url: String,
    /// Bearer token; wrapped in RwLock so `re_authenticate` can swap it
    /// out atomically while in-flight requests on other tasks read it.
    session_token: RwLock<String>,
    /// Stored for re-authentication on 401. The collector holds the
    /// shared secret in memory for the life of the process; re-auth
    /// happens lazily when an upload returns 401 (token expired/reaped).
    shared_secret: String,
    /// Service name used when reauthenticating. Matches the original
    /// value passed to `authenticate()`.
    service_name: String,
}

/// Errors from Data API calls.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API returned {status}: {body}")]
    Status { status: u16, body: String },
}

/// Convert a reqwest response into an `ApiError::Status` if its HTTP status
/// is not a success code. Returns the response unchanged on success so you
/// can chain `.json()` / `.bytes()` / etc. Removes ~12 copies of the same
/// if-!is_success block from the client methods below.
async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(ApiError::Status { status, body })
    }
}

impl DataApiClient {
    /// Build a stub client for tests that never actually talk to the API.
    /// Calls will fail with connection errors — callers must ensure their
    /// test path doesn't reach the network. Dev-only; hidden behind
    /// `#[cfg(test)]` so it can't ship.
    #[cfg(test)]
    pub fn test_stub() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: "http://127.0.0.1:0".to_string(),
            session_token: tokio::sync::RwLock::new(String::new()),
            shared_secret: String::new(),
            service_name: "test".to_string(),
        }
    }

    /// Authenticate with the Data API using a shared secret.
    /// Returns a client ready to make authenticated requests.
    pub async fn authenticate(
        base_url: &str,
        shared_secret: &str,
        service_name: &str,
    ) -> Result<Self, ApiError> {
        let client = reqwest::Client::new();
        let token = Self::do_auth(&client, base_url, shared_secret, service_name).await?;

        Ok(Self {
            client,
            base_url: base_url.to_string(),
            session_token: RwLock::new(token),
            shared_secret: shared_secret.to_string(),
            service_name: service_name.to_string(),
        })
    }

    /// Perform the auth handshake and return the session token. Shared
    /// between `authenticate()` (initial login) and `re_authenticate()`
    /// (token refresh after a 401).
    async fn do_auth(
        client: &reqwest::Client,
        base_url: &str,
        shared_secret: &str,
        service_name: &str,
    ) -> Result<String, ApiError> {
        let resp = client
            .post(format!("{base_url}/internal/auth"))
            .json(&AuthRequest {
                shared_secret: shared_secret.to_string(),
                service_name: service_name.to_string(),
            })
            .send()
            .await?;

        let auth: AuthResponse = check_status(resp).await?.json().await?;
        Ok(auth.session_token)
    }

    /// Re-authenticate with the Data API, replacing the stored session
    /// token. Called by `upload_chunk_with_retry` when an upload returns
    /// 401 (token expired or reaped after >90s heartbeat silence).
    pub async fn re_authenticate(&self) -> Result<(), ApiError> {
        let token =
            Self::do_auth(&self.client, &self.base_url, &self.shared_secret, &self.service_name)
                .await?;
        *self.session_token.write().await = token;
        info!(service = %self.service_name, "re_authenticated_with_data_api");
        Ok(())
    }

    async fn auth_header(&self) -> String {
        format!("Bearer {}", self.session_token.read().await)
    }

    // --- Heartbeat ---

    /// Send a heartbeat to keep the service session alive.
    pub async fn heartbeat(&self) -> Result<(), ApiError> {
        let resp = self
            .client
            .post(format!("{}/internal/heartbeat", self.base_url))
            .header("authorization", self.auth_header().await)
            .send()
            .await?;
        check_status(resp).await?;
        Ok(())
    }

    // --- Sessions ---

    /// Create a new session (replaces db::create_session).
    pub async fn create_session(
        &self,
        id: Uuid,
        guild_id: i64,
        started_at: DateTime<Utc>,
        game_system: Option<String>,
        campaign_name: Option<String>,
        s3_prefix: Option<String>,
    ) -> Result<SessionResponse, ApiError> {
        let resp = self
            .client
            .post(format!("{}/internal/sessions", self.base_url))
            .header("authorization", self.auth_header().await)
            .json(&CreateSessionRequest {
                id,
                guild_id,
                started_at,
                game_system,
                campaign_name,
                s3_prefix,
            })
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
    }

    /// Update session status (replaces db::update_session_state).
    #[allow(dead_code)]
    pub async fn update_session_state(
        &self,
        session_id: Uuid,
        status: &str,
    ) -> Result<SessionResponse, ApiError> {
        let resp = self
            .client
            .patch(format!("{}/internal/sessions/{}", self.base_url, session_id))
            .header("authorization", self.auth_header().await)
            .json(&UpdateSessionRequest {
                status: Some(status.to_string()),
                ended_at: None,
                participant_count: None,
            })
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
    }

    /// Mark a session abandoned: PATCH both status="abandoned" and
    /// ended_at=now() in a single call. Atomic from the server's POV —
    /// avoids the bug where an abandoned row has status=abandoned but
    /// ended_at=NULL, which made "sessions ended today" queries miss
    /// abandoned runs. Prefer this over update_session_state("abandoned")
    /// so the intent is explicit at every call site.
    pub async fn abandon_session(&self, session_id: Uuid) -> Result<SessionResponse, ApiError> {
        let resp = self
            .client
            .patch(format!("{}/internal/sessions/{}", self.base_url, session_id))
            .header("authorization", self.auth_header().await)
            .json(&UpdateSessionRequest {
                status: Some("abandoned".to_string()),
                ended_at: Some(Utc::now()),
                participant_count: None,
            })
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
    }

    /// Finalize a session: set ended_at, participant_count, and
    /// status="uploaded" — meaning the collector has finished uploading
    /// all per-speaker audio chunks and the session is ready for
    /// transcription. The `chronicle-worker` polls `?status=uploaded` to find
    /// work to pick up; from there the lifecycle continues
    /// `uploaded → transcribing → transcribed`, owned by the worker.
    ///
    /// This used to write `status="complete"`, which was a slight misnomer
    /// (the session isn't actually complete until transcription lands).
    /// Renamed to `uploaded` in v0.5.3 so the state machine reads honestly
    /// across the whole family of services. The collector now owns exactly
    /// three DB statuses: `awaiting_consent → recording → uploaded`.
    pub async fn finalize_session(
        &self,
        session_id: Uuid,
        ended_at: DateTime<Utc>,
        participant_count: i32,
    ) -> Result<SessionResponse, ApiError> {
        let resp = self
            .client
            .patch(format!("{}/internal/sessions/{}", self.base_url, session_id))
            .header("authorization", self.auth_header().await)
            .json(&UpdateSessionRequest {
                ended_at: Some(ended_at),
                participant_count: Some(participant_count),
                status: Some("uploaded".to_string()),
            })
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
    }

    // --- Users ---

    /// Upsert a user row for a discord_user_id and return the UserResponse.
    /// Private helper — centralizes the "pseudonymize → POST /internal/users"
    /// dance that four public methods were previously open-coding.
    async fn upsert_user(&self, discord_user_id: u64) -> Result<UserResponse, ApiError> {
        let pseudo_id = pseudonymize(discord_user_id);
        let resp = self
            .client
            .post(format!("{}/internal/users", self.base_url))
            .header("authorization", self.auth_header().await)
            .json(&CreateUserRequest { pseudo_id })
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
    }

    /// Record a display name for a pseudo_id. Fire-and-forget error
    /// handling — display names are an affordance, not a correctness
    /// requirement, so a failure here doesn't block enrolment.
    async fn record_display_name(
        &self,
        pseudo_id: &str,
        display_name: &str,
    ) -> Result<(), ApiError> {
        let resp = self
            .client
            .post(format!(
                "{}/internal/users/{}/display_names",
                self.base_url, pseudo_id
            ))
            .header("authorization", self.auth_header().await)
            .json(&DisplayNameRequest {
                display_name: display_name.to_string(),
                source: "bot",
            })
            .send()
            .await?;
        check_status(resp).await?;
        Ok(())
    }

    /// Look up a participant row by (session_id, discord_user_id). This is
    /// the client-side join the Data API forces us into: resolve discord →
    /// user UUID, list all participants for the session, find the match.
    /// Three public methods (record_consent, toggle_license_flag,
    /// get_license_flags) previously duplicated this logic.
    ///
    /// Future: push this into a dedicated Data API endpoint so we do one
    /// round-trip instead of two.
    async fn find_participant(
        &self,
        session_id: Uuid,
        discord_user_id: u64,
    ) -> Result<ParticipantResponse, ApiError> {
        let user = self.upsert_user(discord_user_id).await?;
        let participants = self.list_participants(session_id).await?;
        participants
            .into_iter()
            .find(|p| p.pseudo_id == user.pseudo_id)
            .ok_or_else(|| ApiError::Status {
                status: 404,
                body: "participant not found for user".to_string(),
            })
    }

    /// Upsert a user and check if their data has been wiped (post-refactor
    /// replacement for the old `global_opt_out` flag). `data_wiped_at`
    /// being set means the user invoked self-deletion; treat as blocked.
    pub async fn check_blocklist(&self, discord_user_id: u64) -> Result<bool, ApiError> {
        let user = self.upsert_user(discord_user_id).await?;
        Ok(user.data_wiped_at.is_some())
    }

    // --- Participants ---

    /// Add a participant to a session.
    pub async fn add_participant(
        &self,
        session_id: Uuid,
        discord_user_id: u64,
        mid_session_join: bool,
        display_name: Option<String>,
    ) -> Result<ParticipantResponse, ApiError> {
        let user = self.upsert_user(discord_user_id).await?;
        if let Some(name) = display_name.as_deref() {
            let _ = self.record_display_name(&user.pseudo_id, name).await;
        }
        let resp = self
            .client
            .post(format!(
                "{}/internal/sessions/{}/participants",
                self.base_url, session_id
            ))
            .header("authorization", self.auth_header().await)
            .json(&AddParticipantRequest {
                pseudo_id: user.pseudo_id,
                mid_session_join: Some(mid_session_join),
            })
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
    }

    /// Batch-add N participants to a session in a single HTTP round trip.
    /// Upserts each user row first (sequentially — small N, not worth
    /// parallelizing), then POSTs them all in one call to the Data API
    /// batch endpoint. Returns the inserted rows in input order so
    /// callers can map them back to their (discord_user_id, ...) tuples.
    pub async fn add_participants_batch(
        &self,
        session_id: Uuid,
        participants: &[(u64, bool, Option<String>)],
    ) -> Result<Vec<ParticipantResponse>, ApiError> {
        if participants.is_empty() {
            return Ok(Vec::new());
        }
        let mut requests = Vec::with_capacity(participants.len());
        for (discord_user_id, mid_session_join, display_name) in participants {
            let user = self.upsert_user(*discord_user_id).await?;
            if let Some(name) = display_name.as_deref() {
                let _ = self.record_display_name(&user.pseudo_id, name).await;
            }
            requests.push(AddParticipantRequest {
                pseudo_id: user.pseudo_id,
                mid_session_join: Some(*mid_session_join),
            });
        }
        let resp = self
            .client
            .post(format!(
                "{}/internal/sessions/{}/participants/batch",
                self.base_url, session_id
            ))
            .header("authorization", self.auth_header().await)
            .json(&BatchAddParticipantsRequest {
                participants: requests,
            })
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
    }

    /// Record consent for a participant: resolve them via find_participant
    /// and PATCH their consent row. Fallback path used when the caller
    /// does not have a cached participant UUID.
    pub async fn record_consent(
        &self,
        session_id: Uuid,
        discord_user_id: u64,
        scope: &str,
    ) -> Result<(), ApiError> {
        let participant = self.find_participant(session_id, discord_user_id).await?;
        self.record_consent_by_id(participant.id, scope).await
    }

    /// Fast-path consent update for callers that already have the
    /// participant's Data API UUID cached on the local Session. Skips
    /// find_participant entirely — one HTTP round trip instead of three.
    pub async fn record_consent_by_id(
        &self,
        participant_id: Uuid,
        scope: &str,
    ) -> Result<(), ApiError> {
        let resp = self
            .client
            .patch(format!(
                "{}/internal/participants/{}/consent",
                self.base_url, participant_id
            ))
            .header("authorization", self.auth_header().await)
            .json(&UpdateConsentRequest {
                consent_scope: Some(scope.to_string()),
                consented_at: Some(Utc::now()),
            })
            .send()
            .await?;
        check_status(resp).await?;
        Ok(())
    }

    /// Toggle a license flag (no_llm_training or no_public_release).
    /// Fallback path. Returns the new (no_llm_training, no_public_release)
    /// tuple for the caller to display in the refreshed button row.
    #[allow(dead_code)]
    pub async fn toggle_license_flag(
        &self,
        session_id: Uuid,
        discord_user_id: u64,
        field: &str,
    ) -> Result<(bool, bool), ApiError> {
        let participant = self.find_participant(session_id, discord_user_id).await?;
        self.toggle_license_flag_by_id(
            participant.id,
            field,
            participant.no_llm_training,
            participant.no_public_release,
        )
        .await
    }

    /// Fast-path license toggle for callers that already have the
    /// participant's Data API UUID cached AND know the current flag
    /// values (e.g. from the previous PATCH response or the initial
    /// add_participants_batch response). Returns the new
    /// (no_llm_training, no_public_release) tuple.
    #[allow(dead_code)]
    pub async fn toggle_license_flag_by_id(
        &self,
        participant_id: Uuid,
        field: &str,
        current_no_llm: bool,
        current_no_public: bool,
    ) -> Result<(bool, bool), ApiError> {
        let (no_llm, no_public) = match field {
            "no_llm_training" => (Some(!current_no_llm), None),
            "no_public_release" => (None, Some(!current_no_public)),
            _ => return Ok((current_no_llm, current_no_public)),
        };
        self.set_license_flags_by_id(participant_id, no_llm, no_public)
            .await
    }

    /// Set license flags directly — absolute (not toggle) semantics. `None`
    /// leaves the field unchanged on the server. Used by:
    ///   - F4 default-flag writes on Accept (both `Some(false)`).
    ///   - Harness `POST /license` (one field explicitly set).
    ///   - Future portal license UI.
    pub async fn set_license_flags_by_id(
        &self,
        participant_id: Uuid,
        no_llm_training: Option<bool>,
        no_public_release: Option<bool>,
    ) -> Result<(bool, bool), ApiError> {
        let resp = self
            .client
            .patch(format!(
                "{}/internal/participants/{}/license",
                self.base_url, participant_id
            ))
            .header("authorization", self.auth_header().await)
            .json(&UpdateLicenseRequest {
                no_llm_training,
                no_public_release,
            })
            .send()
            .await?;
        let updated: ParticipantResponse = check_status(resp).await?.json().await?;
        Ok((updated.no_llm_training, updated.no_public_release))
    }

    /// List participants for a session.
    pub async fn list_participants(
        &self,
        session_id: Uuid,
    ) -> Result<Vec<ParticipantResponse>, ApiError> {
        let resp = self
            .client
            .get(format!(
                "{}/internal/sessions/{}/participants",
                self.base_url, session_id
            ))
            .header("authorization", self.auth_header().await)
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
    }

    // --- Audio ---

    /// Upload a PCM audio chunk with retry on transient errors (R7).
    ///
    /// Per-chunk headers (required by the locked data-api spec):
    /// - `X-Capture-Started-At` — UTC timestamp of the first sample in this
    ///   chunk, ISO-8601. For pre-gate flushed chunks this is the original
    ///   capture time (possibly minutes before gate-open); for post-gate
    ///   live chunks it is "now when the accumulator rolled over." The
    ///   data-api uses this to reconstruct the stream as if it had been
    ///   arriving live even for chunks that were cached pre-gate.
    /// - `X-Duration-Ms` — PCM-derived wall-clock duration of this chunk.
    /// - `X-Client-Chunk-Id` — idempotency key, `{session}:{pseudo}:{seq}`.
    ///
    /// Retry policy, mirroring `chronicle-worker::api_client::download_chunk_with_retry`:
    /// - **5xx** or network error: retry up to 3 times with 1s/2s/4s backoff.
    /// - **401**: re-authenticate once, retry.
    /// - **4xx**: fail immediately.
    pub async fn upload_chunk_with_retry(
        &self,
        session_id: Uuid,
        pseudo_id: &str,
        data: Vec<u8>,
        capture_started_at: DateTime<Utc>,
        duration_ms: u64,
        client_chunk_id: &str,
    ) -> Result<(), ApiError> {
        const MAX_RETRIES: u32 = 3;
        let backoff_durations = [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
        ];
        let size = data.len();
        let url = format!(
            "{}/internal/sessions/{}/audio/{}/chunk",
            self.base_url, session_id, pseudo_id
        );
        let captured_hdr = capture_started_at.to_rfc3339();

        let mut reauth_attempted = false;
        let mut attempt: u32 = 0;

        loop {
            attempt += 1;
            let send_result = self
                .client
                .post(&url)
                .header("authorization", self.auth_header().await)
                .header("content-type", "application/octet-stream")
                .header("x-capture-started-at", &captured_hdr)
                .header("x-duration-ms", duration_ms.to_string())
                .header("x-client-chunk-id", client_chunk_id)
                .body(data.clone())
                .send()
                .await;

            match send_result {
                Ok(resp) if resp.status().is_success() => {
                    info!(
                        session_id = %session_id,
                        pseudo_id = %pseudo_id,
                        size = size,
                        attempt = attempt,
                        "chunk_uploaded"
                    );
                    return Ok(());
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();

                    // 401 — re-auth once and retry immediately (no backoff).
                    if status == 401 && !reauth_attempted {
                        reauth_attempted = true;
                        warn!(
                            session_id = %session_id,
                            pseudo_id = %pseudo_id,
                            attempt = attempt,
                            "chunk_upload_got_401_re_authenticating"
                        );
                        // Drop the 401 response body before re-auth.
                        let _ = resp.text().await;
                        if let Err(e) = self.re_authenticate().await {
                            error!(
                                session_id = %session_id,
                                pseudo_id = %pseudo_id,
                                error = %e,
                                "chunk_upload_re_auth_failed"
                            );
                            return Err(e);
                        }
                        // Don't count the 401 against the 5xx retry budget.
                        attempt -= 1;
                        continue;
                    }

                    // 5xx — transient, retry with backoff.
                    if status >= 500 {
                        let body = resp.text().await.unwrap_or_default();
                        if attempt >= MAX_RETRIES {
                            error!(
                                session_id = %session_id,
                                pseudo_id = %pseudo_id,
                                status = status,
                                attempts = attempt,
                                "chunk_upload_failed_after_retries"
                            );
                            return Err(ApiError::Status { status, body });
                        }
                        let delay = backoff_durations[(attempt - 1) as usize];
                        warn!(
                            session_id = %session_id,
                            pseudo_id = %pseudo_id,
                            status = status,
                            attempt = attempt,
                            delay_ms = delay.as_millis() as u64,
                            "chunk_upload_retrying_after_5xx"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }

                    // 4xx (including a repeat 401 after re-auth) — fail fast.
                    let body = resp.text().await.unwrap_or_default();
                    error!(
                        session_id = %session_id,
                        pseudo_id = %pseudo_id,
                        status = status,
                        "chunk_upload_failed_client_error"
                    );
                    return Err(ApiError::Status { status, body });
                }
                Err(e) => {
                    // Network / connection error — treated like 5xx.
                    if attempt >= MAX_RETRIES {
                        error!(
                            session_id = %session_id,
                            pseudo_id = %pseudo_id,
                            error = %e,
                            attempts = attempt,
                            "chunk_upload_failed_after_retries"
                        );
                        return Err(ApiError::Http(e));
                    }
                    let delay = backoff_durations[(attempt - 1) as usize];
                    warn!(
                        session_id = %session_id,
                        pseudo_id = %pseudo_id,
                        error = %e,
                        attempt = attempt,
                        delay_ms = delay.as_millis() as u64,
                        "chunk_upload_retrying_after_network_error"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
        }
    }

    // --- Metadata ---

    /// Construct an instance pointing at the supplied base URL with no auth
    /// flow. Used only by integration tests that host their own local
    /// test server; never shipped.
    #[cfg(test)]
    pub fn for_test_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            session_token: tokio::sync::RwLock::new("test-token".to_string()),
            shared_secret: String::new(),
            service_name: "test".to_string(),
        }
    }

    /// Upload session metadata (meta.json + consent.json) to the Data API.
    pub async fn write_metadata(
        &self,
        session_id: Uuid,
        meta_json: Option<serde_json::Value>,
        consent_json: Option<serde_json::Value>,
    ) -> Result<(), ApiError> {
        let mut body = serde_json::Map::new();
        if let Some(meta) = meta_json {
            body.insert("meta".to_string(), meta);
        }
        if let Some(consent) = consent_json {
            body.insert("consent".to_string(), consent);
        }

        let resp = self
            .client
            .post(format!(
                "{}/internal/sessions/{}/metadata",
                self.base_url, session_id
            ))
            .header("authorization", self.auth_header().await)
            .json(&body)
            .send()
            .await?;
        check_status(resp).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use chrono::TimeZone;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as TokioMutex;

    #[derive(Default)]
    struct Captured {
        capture_started_at: Option<String>,
        duration_ms: Option<String>,
        client_chunk_id: Option<String>,
        body_len: usize,
    }

    async fn chunk_sink(
        State(state): State<Arc<TokioMutex<Captured>>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Body) {
        let mut c = state.lock().await;
        c.capture_started_at = headers
            .get("x-capture-started-at")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        c.duration_ms = headers
            .get("x-duration-ms")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        c.client_chunk_id = headers
            .get("x-client-chunk-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        c.body_len = body.len();
        (
            StatusCode::OK,
            Body::from(r#"{"key":"k","seq":0}"#.to_string()),
        )
    }

    async fn run_test_server(
        captured: Arc<TokioMutex<Captured>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let router = Router::new()
            .route(
                "/internal/sessions/:sid/audio/:pseudo/chunk",
                post(chunk_sink),
            )
            .with_state(captured);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{}", addr);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (base, handle)
    }

    #[tokio::test]
    async fn upload_chunk_carries_capture_started_at_header() {
        let captured = Arc::new(TokioMutex::new(Captured::default()));
        let (base, _srv) = run_test_server(captured.clone()).await;

        let client = DataApiClient::for_test_base_url(base);
        let sid = uuid::Uuid::new_v4();
        let capture_ts = chrono::Utc
            .timestamp_millis_opt(1_700_000_000_000)
            .unwrap();
        client
            .upload_chunk_with_retry(
                sid,
                "pseudo-1",
                vec![1u8, 2, 3, 4],
                capture_ts,
                42,
                "client-chunk-id-xyz",
            )
            .await
            .unwrap();

        let c = captured.lock().await;
        assert_eq!(c.capture_started_at.as_deref(), Some(capture_ts.to_rfc3339().as_str()));
        assert_eq!(c.duration_ms.as_deref(), Some("42"));
        assert_eq!(c.client_chunk_id.as_deref(), Some("client-chunk-id-xyz"));
        assert_eq!(c.body_len, 4);
    }
}
