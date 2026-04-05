//! HTTP client for the Data API.
//!
//! Replaces direct Postgres + S3 access. Every persistence operation
//! goes through the Data API as an HTTP call.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, info};
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
    pub user_id: Option<Uuid>,
    pub consent_scope: Option<String>,
    pub consented_at: Option<DateTime<Utc>>,
    pub withdrawn_at: Option<DateTime<Utc>>,
    pub mid_session_join: bool,
    pub no_llm_training: bool,
    pub no_public_release: bool,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct UserResponse {
    pub id: Uuid,
    pub discord_id_hash: String,
    pub pseudo_id: String,
    pub global_opt_out: bool,
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
    discord_id_hash: String,
    pseudo_id: String,
}

#[derive(Serialize)]
struct AddParticipantRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<Uuid>,
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
    session_token: String,
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
    /// Authenticate with the Data API using a shared secret.
    /// Returns a client ready to make authenticated requests.
    pub async fn authenticate(
        base_url: &str,
        shared_secret: &str,
        service_name: &str,
    ) -> Result<Self, ApiError> {
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base_url}/internal/auth"))
            .json(&AuthRequest {
                shared_secret: shared_secret.to_string(),
                service_name: service_name.to_string(),
            })
            .send()
            .await?;

        let auth: AuthResponse = check_status(resp).await?.json().await?;

        Ok(Self {
            client,
            base_url: base_url.to_string(),
            session_token: auth.session_token,
        })
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.session_token)
    }

    // --- Heartbeat ---

    /// Send a heartbeat to keep the service session alive.
    pub async fn heartbeat(&self) -> Result<(), ApiError> {
        let resp = self
            .client
            .post(format!("{}/internal/heartbeat", self.base_url))
            .header("authorization", self.auth_header())
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
            .header("authorization", self.auth_header())
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
    pub async fn update_session_state(
        &self,
        session_id: Uuid,
        status: &str,
    ) -> Result<SessionResponse, ApiError> {
        let resp = self
            .client
            .patch(format!("{}/internal/sessions/{}", self.base_url, session_id))
            .header("authorization", self.auth_header())
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
            .header("authorization", self.auth_header())
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
    /// transcription. The `ovp-worker` polls `?status=uploaded` to find
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
            .header("authorization", self.auth_header())
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
            .header("authorization", self.auth_header())
            .json(&CreateUserRequest {
                discord_id_hash: pseudo_id.clone(),
                pseudo_id,
            })
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
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
            .find(|p| p.user_id == Some(user.id))
            .ok_or_else(|| ApiError::Status {
                status: 404,
                body: "participant not found for user".to_string(),
            })
    }

    /// Upsert a user and check if they're on the blocklist (replaces db::check_blocklist).
    /// Returns true if the user has opted out globally.
    pub async fn check_blocklist(&self, discord_user_id: u64) -> Result<bool, ApiError> {
        let user = self.upsert_user(discord_user_id).await?;
        Ok(user.global_opt_out)
    }

    // --- Participants ---

    /// Add a participant to a session.
    pub async fn add_participant(
        &self,
        session_id: Uuid,
        discord_user_id: u64,
        mid_session_join: bool,
    ) -> Result<ParticipantResponse, ApiError> {
        let user = self.upsert_user(discord_user_id).await?;
        let resp = self
            .client
            .post(format!(
                "{}/internal/sessions/{}/participants",
                self.base_url, session_id
            ))
            .header("authorization", self.auth_header())
            .json(&AddParticipantRequest {
                user_id: Some(user.id),
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
        participants: &[(u64, bool)],
    ) -> Result<Vec<ParticipantResponse>, ApiError> {
        if participants.is_empty() {
            return Ok(Vec::new());
        }
        let mut requests = Vec::with_capacity(participants.len());
        for &(discord_user_id, mid_session_join) in participants {
            let user = self.upsert_user(discord_user_id).await?;
            requests.push(AddParticipantRequest {
                user_id: Some(user.id),
                mid_session_join: Some(mid_session_join),
            });
        }
        let resp = self
            .client
            .post(format!(
                "{}/internal/sessions/{}/participants/batch",
                self.base_url, session_id
            ))
            .header("authorization", self.auth_header())
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
            .header("authorization", self.auth_header())
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
        let resp = self
            .client
            .patch(format!(
                "{}/internal/participants/{}/license",
                self.base_url, participant_id
            ))
            .header("authorization", self.auth_header())
            .json(&UpdateLicenseRequest {
                no_llm_training: no_llm,
                no_public_release: no_public,
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
            .header("authorization", self.auth_header())
            .send()
            .await?;
        Ok(check_status(resp).await?.json().await?)
    }

    // --- Audio ---

    /// Upload a PCM audio chunk (replaces s3.upload_bytes for audio).
    /// Sends raw bytes as the request body.
    pub async fn upload_chunk(
        &self,
        session_id: Uuid,
        pseudo_id: &str,
        data: Vec<u8>,
    ) -> Result<(), ApiError> {
        let size = data.len();
        for attempt in 1..=3 {
            let resp = self
                .client
                .post(format!(
                    "{}/internal/sessions/{}/audio/{}/chunk",
                    self.base_url, session_id, pseudo_id
                ))
                .header("authorization", self.auth_header())
                .header("content-type", "application/octet-stream")
                .body(data.clone())
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    info!(session_id = %session_id, pseudo_id = %pseudo_id, size = size, attempt = attempt, "chunk_uploaded");
                    return Ok(());
                }
                Ok(r) => {
                    let status = r.status().as_u16();
                    let body = r.text().await.unwrap_or_default();
                    if attempt == 3 {
                        error!(session_id = %session_id, pseudo_id = %pseudo_id, status = status, "chunk_upload_failed");
                        return Err(ApiError::Status { status, body });
                    }
                }
                Err(e) => {
                    if attempt == 3 {
                        error!(session_id = %session_id, pseudo_id = %pseudo_id, error = %e, "chunk_upload_failed");
                        return Err(ApiError::Http(e));
                    }
                }
            }
            let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
            tokio::time::sleep(backoff).await;
        }
        Ok(())
    }

    // --- Metadata ---

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
            .header("authorization", self.auth_header())
            .json(&body)
            .send()
            .await?;
        check_status(resp).await?;
        Ok(())
    }
}
