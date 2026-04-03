use clap::Parser;

#[derive(Parser, Clone)]
#[command(name = "ttrpg-collector", about = "TTRPG session audio collector bot")]
pub struct Config {
    /// Discord bot token
    #[arg(long, env = "DISCORD_TOKEN")]
    pub token: String,

    /// S3-compatible endpoint URL
    #[arg(long, env = "S3_ENDPOINT")]
    pub s3_endpoint: String,

    /// S3 access key
    #[arg(long, env = "S3_ACCESS_KEY")]
    pub s3_access_key: String,

    /// S3 secret key
    #[arg(long, env = "S3_SECRET_KEY")]
    pub s3_secret_key: String,

    /// S3 bucket name
    #[arg(long, env = "S3_BUCKET", default_value = "ttrpg-dataset-raw")]
    pub s3_bucket: String,

    /// Local buffer directory
    #[arg(long, env = "LOCAL_BUFFER_DIR", default_value = "./sessions")]
    pub local_buffer_dir: String,

    /// Minimum participants to start recording
    #[arg(long, env = "MIN_PARTICIPANTS", default_value = "2")]
    pub min_participants: usize,

    /// Require all participants to consent
    #[arg(long, env = "REQUIRE_ALL_CONSENT", default_value = "true")]
    pub require_all_consent: bool,

    /// S3 chunk size in bytes before flushing (default 50MB)
    #[arg(long, env = "S3_CHUNK_SIZE", default_value = "52428800")]
    pub s3_chunk_size: usize,

    /// Postgres database URL
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,
}
