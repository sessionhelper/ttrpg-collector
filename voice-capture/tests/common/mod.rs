use std::path::PathBuf;

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::Client;
use sqlx::PgPool;
use uuid::Uuid;

/// Connect to the test Postgres instance and run migrations.
pub async fn setup_db() -> PgPool {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");

    let pool = PgPool::connect(&database_url)
        .await
        .expect("Failed to connect to test database");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("Failed to run migrations");

    pool
}

/// Build an S3 client pointed at the local MinIO instance and ensure the test
/// bucket exists.
pub async fn setup_s3() -> (Client, String) {
    let endpoint = std::env::var("S3_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".into());
    let access_key = std::env::var("S3_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into());
    let secret_key = std::env::var("S3_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into());
    let bucket = std::env::var("S3_BUCKET").unwrap_or_else(|_| "ttrpg-dataset-test".into());

    let creds = Credentials::new(&access_key, &secret_key, None, None, "test");

    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(&endpoint)
        .credentials_provider(creds)
        .force_path_style(true)
        .build();

    let client = Client::from_conf(s3_config);

    // Create bucket if it doesn't exist
    let _ = client.create_bucket().bucket(&bucket).send().await;

    (client, bucket)
}

/// Delete all rows from test tables (in dependency order).
pub async fn cleanup_db(pool: &PgPool) {
    sqlx::query("DELETE FROM consent_audit_log")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM session_participants")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM sessions")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM users")
        .execute(pool)
        .await
        .ok();
}

/// Remove all objects from the test bucket.
pub async fn cleanup_s3(client: &Client, bucket: &str) {
    let list = client.list_objects_v2().bucket(bucket).send().await;
    if let Ok(output) = list {
        if let Some(contents) = output.contents() {
            for obj in contents {
                if let Some(key) = obj.key() {
                    let _ = client
                        .delete_object()
                        .bucket(bucket)
                        .key(key)
                        .send()
                        .await;
                }
            }
        }
    }
}

/// Generate a random session UUID string.
pub fn fake_session_id() -> String {
    Uuid::new_v4().to_string()
}

/// Generate a fake guild ID for testing.
pub fn fake_guild_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    900_000_000_000_000_000 + nanos
}

/// Generate a fake user ID for testing.
pub fn fake_user_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    100_000_000_000_000_000 + nanos
}

/// Create a temporary session directory with some test files.
pub fn create_test_session_dir(session_id: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ttrpg-test-{}", session_id));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
