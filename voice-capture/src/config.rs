//! CLI / environment variable configuration via clap.

use clap::Parser;

/// Bot configuration, populated from CLI args or environment variables.
#[derive(Parser, Clone)]
#[command(name = "ttrpg-collector", about = "TTRPG session audio collector bot")]
pub struct Config {
    /// Discord bot token
    #[arg(long, env = "DISCORD_TOKEN")]
    pub token: String,

    /// Data API base URL
    #[arg(long, env = "DATA_API_URL", default_value = "http://127.0.0.1:8001")]
    pub data_api_url: String,

    /// Shared secret for Data API auth
    #[arg(long, env = "DATA_API_SHARED_SECRET")]
    pub data_api_shared_secret: String,

    /// Local buffer directory
    #[arg(long, env = "LOCAL_BUFFER_DIR", default_value = "./sessions")]
    pub local_buffer_dir: String,

    /// Minimum participants to start recording
    #[arg(long, env = "MIN_PARTICIPANTS", default_value = "2")]
    pub min_participants: usize,

    /// Require all participants to consent
    #[arg(long, env = "REQUIRE_ALL_CONSENT", default_value = "true")]
    pub require_all_consent: bool,
}
