use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::storage::pseudonymize;

// ---------------------------------------------------------------------------
// Helper structs
// ---------------------------------------------------------------------------

pub struct NewSession {
    pub id: Uuid,
    pub guild_id: i64,
    pub started_at: DateTime<Utc>,
    pub game_system: Option<String>,
    pub campaign_name: Option<String>,
}

pub struct NewParticipant {
    pub session_id: Uuid,
    pub discord_user_id: u64,
    pub mid_session_join: bool,
}

pub struct FinalizedSession {
    pub session_id: Uuid,
    pub ended_at: DateTime<Utc>,
    pub duration_seconds: f64,
    pub participant_count: i32,
    pub s3_prefix: Option<String>,
}

// ---------------------------------------------------------------------------
// Data-access functions
// ---------------------------------------------------------------------------

/// INSERT a new session row.
pub async fn create_session(pool: &PgPool, s: &NewSession) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sessions (id, guild_id, started_at, game_system, campaign_name, status)
         VALUES ($1, $2, $3, $4, $5, 'awaiting_consent')",
    )
    .bind(s.id)
    .bind(s.guild_id)
    .bind(s.started_at)
    .bind(&s.game_system)
    .bind(&s.campaign_name)
    .execute(pool)
    .await?;
    Ok(())
}

/// Ensure the user exists in the `users` table, then add them to
/// `session_participants`.
pub async fn add_participant(pool: &PgPool, p: &NewParticipant) -> Result<(), sqlx::Error> {
    let pseudo_id = pseudonymize(p.discord_user_id);
    let discord_id_hash = pseudo_id.clone(); // same SHA-256 derivation

    // Upsert user
    sqlx::query(
        "INSERT INTO users (discord_id_hash, pseudo_id)
         VALUES ($1, $2)
         ON CONFLICT (pseudo_id) DO NOTHING",
    )
    .bind(&discord_id_hash)
    .bind(&pseudo_id)
    .execute(pool)
    .await?;

    // Fetch user id
    let user_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE pseudo_id = $1")
        .bind(&pseudo_id)
        .fetch_one(pool)
        .await?;

    // Insert participant
    sqlx::query(
        "INSERT INTO session_participants (session_id, user_id, mid_session_join)
         VALUES ($1, $2, $3)
         ON CONFLICT (session_id, user_id) DO NOTHING",
    )
    .bind(p.session_id)
    .bind(user_id)
    .bind(p.mid_session_join)
    .execute(pool)
    .await?;

    Ok(())
}

/// Record a consent decision: update the participant row and append an
/// audit-log entry.
pub async fn record_consent(
    pool: &PgPool,
    session_id: Uuid,
    discord_user_id: u64,
    scope: &str,
) -> Result<(), sqlx::Error> {
    let pseudo_id = pseudonymize(discord_user_id);

    let user_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE pseudo_id = $1")
        .bind(&pseudo_id)
        .fetch_one(pool)
        .await?;

    // Grab previous scope for audit trail
    let previous_scope: Option<String> = sqlx::query_scalar(
        "SELECT consent_scope FROM session_participants
         WHERE session_id = $1 AND user_id = $2",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    sqlx::query(
        "UPDATE session_participants
         SET consent_scope = $1, consented_at = NOW()
         WHERE session_id = $2 AND user_id = $3",
    )
    .bind(scope)
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO consent_audit_log (user_id, session_id, action, previous_scope, new_scope)
         VALUES ($1, $2, 'consent_recorded', $3, $4)",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(&previous_scope)
    .bind(scope)
    .execute(pool)
    .await?;

    Ok(())
}

/// Toggle a license flag (no_llm_training or no_public_release) for a participant.
pub async fn toggle_license_flag(
    pool: &PgPool,
    session_id: Uuid,
    discord_user_id: u64,
    field: &str,
) -> Result<(), sqlx::Error> {
    let pseudo_id = pseudonymize(discord_user_id);

    let user_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE pseudo_id = $1")
        .bind(&pseudo_id)
        .fetch_one(pool)
        .await?;

    // Toggle the boolean field — only allow known field names to prevent SQL injection
    let query = match field {
        "no_llm_training" => {
            "UPDATE session_participants SET no_llm_training = NOT no_llm_training WHERE session_id = $1 AND user_id = $2"
        }
        "no_public_release" => {
            "UPDATE session_participants SET no_public_release = NOT no_public_release WHERE session_id = $1 AND user_id = $2"
        }
        _ => return Ok(()),
    };

    sqlx::query(query)
        .bind(session_id)
        .bind(user_id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Read current license flags for a participant.
pub async fn get_license_flags(
    pool: &PgPool,
    session_id: Uuid,
    discord_user_id: u64,
) -> Result<(bool, bool), sqlx::Error> {
    let pseudo_id = pseudonymize(discord_user_id);

    let row: (bool, bool) = sqlx::query_as(
        "SELECT sp.no_llm_training, sp.no_public_release
         FROM session_participants sp
         JOIN users u ON sp.user_id = u.id
         WHERE sp.session_id = $1 AND u.pseudo_id = $2",
    )
    .bind(session_id)
    .bind(&pseudo_id)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// Update session status (e.g. awaiting_consent -> recording).
pub async fn update_session_state(
    pool: &PgPool,
    session_id: Uuid,
    state: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sessions SET status = $1 WHERE id = $2")
        .bind(state)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Write final session metadata after recording stops.
pub async fn finalize_session(pool: &PgPool, f: &FinalizedSession) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE sessions
         SET ended_at = $1,
             participant_count = $2,
             s3_prefix = $3,
             status = 'complete'
         WHERE id = $4",
    )
    .bind(f.ended_at)
    .bind(f.participant_count)
    .bind(&f.s3_prefix)
    .bind(f.session_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Check if a user has opted out globally.
/// Returns `true` if the user is blocked (opted out).
pub async fn check_blocklist(pool: &PgPool, discord_user_id: u64) -> Result<bool, sqlx::Error> {
    let pseudo_id = pseudonymize(discord_user_id);

    let opt_out: Option<bool> =
        sqlx::query_scalar("SELECT global_opt_out FROM users WHERE pseudo_id = $1")
            .bind(&pseudo_id)
            .fetch_optional(pool)
            .await?;

    Ok(opt_out.unwrap_or(false))
}
