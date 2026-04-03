//! S3-compatible object storage uploads with retry.
//!
//! Wraps the AWS SDK client configured for Hetzner Object Storage.
//! Provides `upload_bytes` for streaming PCM chunks and metadata files.

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use tracing::{error, info};

use crate::config::Config;

/// S3-compatible upload client. One instance shared across the bot.
pub struct S3Uploader {
    client: Client,
    bucket: String,
}

impl S3Uploader {
    /// Build a new uploader from the app config.
    pub fn new(config: &Config) -> Self {
        let creds = Credentials::new(
            &config.s3_access_key,
            &config.s3_secret_key,
            None,
            None,
            "env",
        );

        let s3_config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("auto"))
            .endpoint_url(&config.s3_endpoint)
            .credentials_provider(creds)
            .force_path_style(true)
            .build();

        let client = Client::from_conf(s3_config);

        Self {
            client,
            bucket: config.s3_bucket.clone(),
        }
    }

    /// Upload raw bytes to S3 at the given key. Used for PCM audio chunks.
    pub async fn upload_bytes(
        &self,
        key: &str,
        data: Vec<u8>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let size = data.len();
        for attempt in 1..=3 {
            let body = ByteStream::from(data.clone());
            match self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .content_type("application/octet-stream")
                .body(body)
                .send()
                .await
            {
                Ok(_) => {
                    info!(key = %key, size = size, attempt = attempt, "bytes_uploaded");
                    return Ok(());
                }
                Err(e) => {
                    if attempt == 3 {
                        error!(key = %key, error = %e, "bytes_upload_failed");
                        return Err(Box::new(e));
                    }
                    let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
                    tokio::time::sleep(backoff).await;
                }
            }
        }
        Ok(())
    }

}
