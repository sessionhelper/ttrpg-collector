mod common;

use chrono::Utc;
use sqlx::Row;
use ttrpg_collector::db::*;
use ttrpg_collector::storage::pseudonymize;
use uuid::Uuid;

use common::*;

#[tokio::test]
async fn test_create_session() {
    let pool = setup_db().await;
    cleanup_db(&pool).await;

    let session_id = Uuid::new_v4();
    let session = NewSession {
        id: session_id,
        guild_id: fake_guild_id() as i64,
        started_at: Utc::now(),
        game_system: Some("D&D 5e".into()),
        campaign_name: Some("Curse of Strahd".into()),
    };

    create_session(&pool, &session).await.unwrap();

    let row = sqlx::query("SELECT id, status, game_system FROM sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let status: String = row.get("status");
    let game_system: Option<String> = row.get("game_system");
    assert_eq!(status, "awaiting_consent");
    assert_eq!(game_system.as_deref(), Some("D&D 5e"));
}

#[tokio::test]
async fn test_add_participant() {
    let pool = setup_db().await;
    cleanup_db(&pool).await;

    let session_id = Uuid::new_v4();
    let guild_id = fake_guild_id() as i64;
    let user_id = fake_user_id();

    // Create session first
    create_session(
        &pool,
        &NewSession {
            id: session_id,
            guild_id,
            started_at: Utc::now(),
            game_system: None,
            campaign_name: None,
        },
    )
    .await
    .unwrap();

    // Add participant
    add_participant(
        &pool,
        &NewParticipant {
            session_id,
            discord_user_id: user_id,
            mid_session_join: false,
        },
    )
    .await
    .unwrap();

    // Verify user was created
    let pseudo_id = pseudonymize(user_id);
    let user_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE pseudo_id = $1)")
            .bind(&pseudo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(user_exists);

    // Verify participant record
    let db_user_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE pseudo_id = $1")
        .bind(&pseudo_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let participant_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM session_participants WHERE session_id = $1 AND user_id = $2)",
    )
    .bind(session_id)
    .bind(db_user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(participant_exists);
}

#[tokio::test]
async fn test_record_consent() {
    let pool = setup_db().await;
    cleanup_db(&pool).await;

    let session_id = Uuid::new_v4();
    let user_id = fake_user_id();

    create_session(
        &pool,
        &NewSession {
            id: session_id,
            guild_id: fake_guild_id() as i64,
            started_at: Utc::now(),
            game_system: None,
            campaign_name: None,
        },
    )
    .await
    .unwrap();

    add_participant(
        &pool,
        &NewParticipant {
            session_id,
            discord_user_id: user_id,
            mid_session_join: false,
        },
    )
    .await
    .unwrap();

    record_consent(&pool, session_id, user_id, "full").await.unwrap();

    // Verify consent scope and consented_at
    let pseudo_id = pseudonymize(user_id);
    let db_user_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE pseudo_id = $1")
        .bind(&pseudo_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT consent_scope, consented_at FROM session_participants
         WHERE session_id = $1 AND user_id = $2",
    )
    .bind(session_id)
    .bind(db_user_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let scope: Option<String> = row.get("consent_scope");
    let consented_at: Option<chrono::DateTime<Utc>> = row.get("consented_at");
    assert_eq!(scope.as_deref(), Some("full"));
    assert!(consented_at.is_some());

    // Verify audit log entry
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM consent_audit_log WHERE session_id = $1 AND user_id = $2",
    )
    .bind(session_id)
    .bind(db_user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit_count, 1);

    // Verify audit log content
    let audit_row = sqlx::query(
        "SELECT action, new_scope FROM consent_audit_log WHERE session_id = $1 AND user_id = $2",
    )
    .bind(session_id)
    .bind(db_user_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let action: String = audit_row.get("action");
    let new_scope: Option<String> = audit_row.get("new_scope");
    assert_eq!(action, "consent_recorded");
    assert_eq!(new_scope.as_deref(), Some("full"));
}

#[tokio::test]
async fn test_update_license() {
    let pool = setup_db().await;
    cleanup_db(&pool).await;

    let session_id = Uuid::new_v4();
    let user_id = fake_user_id();

    create_session(
        &pool,
        &NewSession {
            id: session_id,
            guild_id: fake_guild_id() as i64,
            started_at: Utc::now(),
            game_system: None,
            campaign_name: None,
        },
    )
    .await
    .unwrap();

    add_participant(
        &pool,
        &NewParticipant {
            session_id,
            discord_user_id: user_id,
            mid_session_join: false,
        },
    )
    .await
    .unwrap();

    update_license(&pool, session_id, user_id, "rail").await.unwrap();

    let pseudo_id = pseudonymize(user_id);
    let db_user_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE pseudo_id = $1")
        .bind(&pseudo_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let license: String = sqlx::query_scalar(
        "SELECT data_license FROM session_participants WHERE session_id = $1 AND user_id = $2",
    )
    .bind(session_id)
    .bind(db_user_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(license, "rail");
}

#[tokio::test]
async fn test_update_session_state() {
    let pool = setup_db().await;
    cleanup_db(&pool).await;

    let session_id = Uuid::new_v4();

    create_session(
        &pool,
        &NewSession {
            id: session_id,
            guild_id: fake_guild_id() as i64,
            started_at: Utc::now(),
            game_system: None,
            campaign_name: None,
        },
    )
    .await
    .unwrap();

    update_session_state(&pool, session_id, "recording").await.unwrap();

    let status: String = sqlx::query_scalar("SELECT status FROM sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(status, "recording");
}

#[tokio::test]
async fn test_finalize_session() {
    let pool = setup_db().await;
    cleanup_db(&pool).await;

    let session_id = Uuid::new_v4();

    create_session(
        &pool,
        &NewSession {
            id: session_id,
            guild_id: fake_guild_id() as i64,
            started_at: Utc::now(),
            game_system: None,
            campaign_name: None,
        },
    )
    .await
    .unwrap();

    let ended = Utc::now();
    finalize_session(
        &pool,
        &FinalizedSession {
            session_id,
            ended_at: ended,
            duration_seconds: 3600.0,
            participant_count: 4,
            s3_prefix: Some("sessions/123/abc-def".into()),
        },
    )
    .await
    .unwrap();

    let row = sqlx::query("SELECT status, ended_at, participant_count, s3_prefix FROM sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let status: String = row.get("status");
    let s3_prefix: Option<String> = row.get("s3_prefix");
    let participant_count: Option<i32> = row.get("participant_count");
    let ended_at: Option<chrono::DateTime<Utc>> = row.get("ended_at");

    assert_eq!(status, "complete");
    assert_eq!(s3_prefix.as_deref(), Some("sessions/123/abc-def"));
    assert_eq!(participant_count, Some(4));
    assert!(ended_at.is_some());
}

#[tokio::test]
async fn test_check_blocklist() {
    let pool = setup_db().await;
    cleanup_db(&pool).await;

    let blocked_user_id: u64 = 111_222_333_444_555;
    let normal_user_id: u64 = 666_777_888_999_000;

    // Insert a user with global_opt_out = true
    let pseudo_blocked = pseudonymize(blocked_user_id);
    sqlx::query(
        "INSERT INTO users (discord_id_hash, pseudo_id, global_opt_out)
         VALUES ($1, $2, TRUE)",
    )
    .bind(&pseudo_blocked)
    .bind(&pseudo_blocked)
    .execute(&pool)
    .await
    .unwrap();

    // Blocked user should return true
    let is_blocked = check_blocklist(&pool, blocked_user_id).await.unwrap();
    assert!(is_blocked);

    // Unknown user should return false (not in DB)
    let is_blocked = check_blocklist(&pool, normal_user_id).await.unwrap();
    assert!(!is_blocked);
}

#[tokio::test]
async fn test_add_participant_creates_user() {
    let pool = setup_db().await;
    cleanup_db(&pool).await;

    let session_id = Uuid::new_v4();
    let user_id = fake_user_id();

    create_session(
        &pool,
        &NewSession {
            id: session_id,
            guild_id: fake_guild_id() as i64,
            started_at: Utc::now(),
            game_system: None,
            campaign_name: None,
        },
    )
    .await
    .unwrap();

    // Verify user doesn't exist yet
    let pseudo_id = pseudonymize(user_id);
    let exists_before: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE pseudo_id = $1)")
            .bind(&pseudo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!exists_before);

    // Add participant — should auto-create user
    add_participant(
        &pool,
        &NewParticipant {
            session_id,
            discord_user_id: user_id,
            mid_session_join: false,
        },
    )
    .await
    .unwrap();

    // Now user should exist
    let exists_after: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE pseudo_id = $1)")
            .bind(&pseudo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(exists_after);
}
