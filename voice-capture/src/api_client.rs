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

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        let auth: AuthResponse = resp.json().await?;

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

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

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

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        Ok(resp.json().await?)
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

        if !resp.status().is_success() {
            let status_code = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status_code, body });
        }

        Ok(resp.json().await?)
    }

    /// Finalize a session: set ended_at, participant_count, and status=complete
    /// (replaces db::finalize_session).
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
                status: Some("complete".to_string()),
            })
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        Ok(resp.json().await?)
    }

    // --- Users ---

    /// Upsert a user and check if they're on the blocklist (replaces db::check_blocklist).
    /// Returns true if the user has opted out globally.
    pub async fn check_blocklist(&self, discord_user_id: u64) -> Result<bool, ApiError> {
        let pseudo_id = pseudonymize(discord_user_id);

        // First ensure user exists
        let _ = self
            .client
            .post(format!("{}/internal/users", self.base_url))
            .header("authorization", self.auth_header())
            .json(&CreateUserRequest {
                discord_id_hash: pseudo_id.clone(),
                pseudo_id: pseudo_id.clone(),
            })
            .send()
            .await?;

        // Then check opt-out status
        let resp = self
            .client
            .get(format!("{}/internal/users/{}", self.base_url, pseudo_id))
            .header("authorization", self.auth_header())
            .send()
            .await?;

        if resp.status().as_u16() == 404 {
            // User not found = not blocked
            return Ok(false);
        }

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        let user: UserResponse = resp.json().await?;
        Ok(user.global_opt_out)
    }

    // --- Participants ---

    /// Add a participant to a session (replaces db::add_participant).
    /// Also upserts the user first.
    pub async fn add_participant(
        &self,
        session_id: Uuid,
        discord_user_id: u64,
        mid_session_join: bool,
    ) -> Result<ParticipantResponse, ApiError> {
        let pseudo_id = pseudonymize(discord_user_id);

        // Upsert user first to get user UUID
        let user_resp = self
            .client
            .post(format!("{}/internal/users", self.base_url))
            .header("authorization", self.auth_header())
            .json(&CreateUserRequest {
                discord_id_hash: pseudo_id.clone(),
                pseudo_id: pseudo_id.clone(),
            })
            .send()
            .await?;

        if !user_resp.status().is_success() {
            let status = user_resp.status().as_u16();
            let body = user_resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        let user: UserResponse = user_resp.json().await?;

        // Add participant with user_id
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

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        Ok(resp.json().await?)
    }

    /// Record consent for a participant (replaces db::record_consent).
    /// Looks up the participant by session + user, then patches consent.
    pub async fn record_consent(
        &self,
        session_id: Uuid,
        discord_user_id: u64,
        scope: &str,
    ) -> Result<(), ApiError> {
        let pseudo_id = pseudonymize(discord_user_id);

        // Get user UUID
        let user_resp = self
            .client
            .get(format!("{}/internal/users/{}", self.base_url, pseudo_id))
            .header("authorization", self.auth_header())
            .send()
            .await?;

        if !user_resp.status().is_success() {
            let status = user_resp.status().as_u16();
            let body = user_resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        let user: UserResponse = user_resp.json().await?;

        // List participants to find this user's participant row
        let participants = self.list_participants(session_id).await?;
        let participant = participants
            .iter()
            .find(|p| p.user_id == Some(user.id))
            .ok_or_else(|| ApiError::Status {
                status: 404,
                body: "participant not found for user".to_string(),
            })?;

        // Patch consent
        let resp = self
            .client
            .patch(format!(
                "{}/internal/participants/{}/consent",
                self.base_url, participant.id
            ))
            .header("authorization", self.auth_header())
            .json(&UpdateConsentRequest {
                consent_scope: Some(scope.to_string()),
                consented_at: Some(Utc::now()),
            })
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        Ok(())
    }

    /// Toggle a license flag (replaces db::toggle_license_flag).
    pub async fn toggle_license_flag(
        &self,
        session_id: Uuid,
        discord_user_id: u64,
        field: &str,
    ) -> Result<(), ApiError> {
        let pseudo_id = pseudonymize(discord_user_id);

        let user_resp = self
            .client
            .get(format!("{}/internal/users/{}", self.base_url, pseudo_id))
            .header("authorization", self.auth_header())
            .send()
            .await?;

        if !user_resp.status().is_success() {
            let status = user_resp.status().as_u16();
            let body = user_resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        let user: UserResponse = user_resp.json().await?;

        let participants = self.list_participants(session_id).await?;
        let participant = participants
            .iter()
            .find(|p| p.user_id == Some(user.id))
            .ok_or_else(|| ApiError::Status {
                status: 404,
                body: "participant not found for user".to_string(),
            })?;

        // Toggle: read current value, flip it
        let (no_llm, no_public) = match field {
            "no_llm_training" => (Some(!participant.no_llm_training), None),
            "no_public_release" => (None, Some(!participant.no_public_release)),
            _ => return Ok(()),
        };

        let resp = self
            .client
            .patch(format!(
                "{}/internal/participants/{}/license",
                self.base_url, participant.id
            ))
            .header("authorization", self.auth_header())
            .json(&UpdateLicenseRequest {
                no_llm_training: no_llm,
                no_public_release: no_public,
            })
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        Ok(())
    }

    /// Get license flags for a participant (replaces db::get_license_flags).
    pub async fn get_license_flags(
        &self,
        session_id: Uuid,
        discord_user_id: u64,
    ) -> Result<(bool, bool), ApiError> {
        let pseudo_id = pseudonymize(discord_user_id);

        let user_resp = self
            .client
            .get(format!("{}/internal/users/{}", self.base_url, pseudo_id))
            .header("authorization", self.auth_header())
            .send()
            .await?;

        if !user_resp.status().is_success() {
            return Ok((false, false));
        }
        let user: UserResponse = user_resp.json().await?;

        let participants = self.list_participants(session_id).await?;
        let participant = participants
            .iter()
            .find(|p| p.user_id == Some(user.id));

        match participant {
            Some(p) => Ok((p.no_llm_training, p.no_public_release)),
            None => Ok((false, false)),
        }
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

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        Ok(resp.json().await?)
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

    /// Upload session metadata (meta.json + consent.json) to the Data API
    /// (replaces s3.upload_bytes for metadata files).
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

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }

        Ok(())
    }
}
