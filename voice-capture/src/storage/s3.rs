use std::path::Path;

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use tracing::{error, info};

use crate::config::Config;

pub struct S3Uploader {
    client: Client,
    bucket: String,
}

impl S3Uploader {
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

    /// Upload all files in a session directory to S3.
    /// Returns the number of files uploaded.
    pub async fn upload_session(
        &self,
        session_dir: &Path,
        guild_id: u64,
        session_id: &str,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let mut uploaded = 0;

        for entry in walkdir(session_dir)? {
            let path = entry;
            if !path.is_file() {
                continue;
            }
            // Skip raw PCM files — only upload FLAC and JSON
            if path.extension().is_some_and(|e| e == "pcm") {
                continue;
            }

            let relative = path.strip_prefix(session_dir)?;
            let key = format!("sessions/{}/{}/{}", guild_id, session_id, relative.display());

            let content_type = match path.extension().and_then(|e| e.to_str()) {
                Some("json") => "application/json",
                Some("flac") => "audio/flac",
                Some("md") => "text/markdown",
                _ => "application/octet-stream",
            };

            for attempt in 1..=3 {
                let body = ByteStream::from_path(&path).await?;
                match self
                    .client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(&key)
                    .content_type(content_type)
                    .body(body)
                    .send()
                    .await
                {
                    Ok(_) => {
                        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                        info!(key = %key, size = size, attempt = attempt, "file_uploaded");
                        uploaded += 1;
                        break;
                    }
                    Err(e) => {
                        if attempt == 3 {
                            error!(key = %key, error = %e, "upload_failed");
                            return Err(Box::new(e));
                        }
                        let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        Ok(uploaded)
    }
}

/// Simple recursive directory walk (avoids pulling in the walkdir crate)
fn walkdir(dir: &Path) -> Result<Vec<std::path::PathBuf>, std::io::Error> {
    let mut files = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path)?);
            } else {
                files.push(path);
            }
        }
    }
    Ok(files)
}
