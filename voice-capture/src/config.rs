//! CLI / environment variable configuration via clap.
//!
//! `BYPASS_CONSENT_USER_IDS` was removed in the features-pass refactor:
//! programmatic testing now flows through the harness HTTP surface
//! (`POST /enrol`, `/consent`, `/license`, `/record`, `/stop`), which
//! exercises the same `SessionCmd` actor commands as real Discord
//! interactions. No privileged user IDs.

use std::path::PathBuf;

use clap::Parser;

/// Bot configuration, populated from CLI args or environment variables.
#[derive(Parser, Clone)]
#[command(name = "chronicle-bot", about = "TTRPG session audio collector bot")]
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

    /// Local directory for the per-session pre-consent chunk cache.
    /// Empty string → default `<tmpdir>/chronicle-bot` (usually `/tmp/chronicle-bot`).
    /// Each participant writes to `<dir>/<session_id>/<pseudo_id>/chunk_<seq:06>.pcm`.
    #[arg(long, env = "LOCAL_BUFFER_DIR", default_value = "")]
    pub local_buffer_dir_raw: String,

    /// Maximum seconds of cached audio retained per participant before oldest-first
    /// drop kicks in. Defaults to 7200 (2h). On overflow the oldest chunk files are
    /// unlinked and `chronicle_prerolled_chunks_dropped_total` increments.
    #[arg(long, env = "LOCAL_BUFFER_MAX_SECS", default_value_t = 7200)]
    pub local_buffer_max_secs: u64,

    /// Seconds of continuous "all expected SSRCs healthy + user-mapped" before
    /// the stabilization gate opens and the "Recording started" announcement plays.
    /// Before the gate: leave/rejoin/retry freely. After: heal silently.
    #[arg(long, env = "STABILIZATION_GATE_SECS", default_value_t = 3)]
    pub stabilization_gate_secs: u64,

    /// Minimum participants to start recording. Defaults to 1 (solo dev testing).
    #[arg(long, env = "MIN_PARTICIPANTS", default_value = "1")]
    pub min_participants: usize,

    /// Require all participants to consent.
    #[arg(long, env = "REQUIRE_ALL_CONSENT", default_value = "true")]
    pub require_all_consent: bool,

    /// If set to true, spawn the loopback E2E harness HTTP server. Dev only.
    /// Endpoints: `POST /record`, `/enrol`, `/consent`, `/license`, `/stop`,
    /// `GET /health`, `GET /status`.
    #[arg(long, env = "HARNESS_ENABLED", default_value_t = false)]
    pub harness_enabled: bool,

    /// TCP port for the E2E harness HTTP server, inside the container.
    #[arg(long, env = "HARNESS_PORT", default_value_t = 8010)]
    pub harness_port: u16,

    /// Bind address for the harness HTTP server. Defaults to loopback for
    /// host-run safety; in Docker set HARNESS_BIND=0.0.0.0 so the compose
    /// port mapping (`127.0.0.1:<port>:<port>`) can reach it. Host-side
    /// safety is still enforced by the loopback-only port mapping.
    #[arg(long, env = "HARNESS_BIND", default_value = "127.0.0.1")]
    pub harness_bind: std::net::IpAddr,
}

impl Config {
    /// Resolve the effective local buffer directory.
    pub fn local_buffer_dir(&self) -> PathBuf {
        let raw = self.local_buffer_dir_raw.trim();
        if raw.is_empty() {
            std::env::temp_dir().join("chronicle-bot")
        } else {
            PathBuf::from(raw)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(local_buffer_dir_raw: &str) -> Config {
        Config {
            token: "t".into(),
            data_api_url: "u".into(),
            data_api_shared_secret: "s".into(),
            local_buffer_dir_raw: local_buffer_dir_raw.into(),
            local_buffer_max_secs: 7200,
            stabilization_gate_secs: 3,
            min_participants: 1,
            require_all_consent: true,
            harness_enabled: false,
            harness_port: 8010,
            harness_bind: "127.0.0.1".parse().unwrap(),
        }
    }

    #[test]
    fn local_buffer_dir_defaults_to_tmpdir() {
        let c = cfg("");
        assert_eq!(c.local_buffer_dir(), std::env::temp_dir().join("chronicle-bot"));
    }

    #[test]
    fn local_buffer_dir_uses_explicit_value() {
        let c = cfg("/var/run/chronicle");
        assert_eq!(c.local_buffer_dir(), PathBuf::from("/var/run/chronicle"));
    }
}
