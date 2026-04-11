//! CLI / environment variable configuration via clap.

use std::collections::HashSet;

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

    /// Local buffer directory
    #[arg(long, env = "LOCAL_BUFFER_DIR", default_value = "./sessions")]
    pub local_buffer_dir: String,

    /// Minimum participants to start recording. Defaults to 1 (solo dev
    /// testing). Prod deployments that want quorum should set
    /// MIN_PARTICIPANTS=2 (or higher) in their env file.
    #[arg(long, env = "MIN_PARTICIPANTS", default_value = "1")]
    pub min_participants: usize,

    /// Require all participants to consent
    #[arg(long, env = "REQUIRE_ALL_CONSENT", default_value = "true")]
    pub require_all_consent: bool,

    /// Comma-separated Discord user IDs that auto-consent at session start
    /// without clicking the consent button. **Dev/E2E harness only.** The
    /// prod deployment leaves this empty and the collector therefore never
    /// bypasses consent.
    ///
    /// This exists so the feeder-bot fleet in `chronicle-feeder` can
    /// be recorded during E2E runs — those bots can't click Discord buttons
    /// on their own. Setting this list to non-empty on a prod deployment
    /// would break the consent contract with real users.
    #[arg(long, env = "BYPASS_CONSENT_USER_IDS", default_value = "")]
    pub bypass_consent_user_ids_raw: String,

    /// If set to true, spawn the loopback E2E harness HTTP server
    /// (`POST /record`, `POST /stop`, `GET /health`) that lets an external
    /// test runner drive the collector without a Discord client. **Dev
    /// only.** Prod leaves this false and the axum task is never spawned.
    ///
    /// Paired with `HARNESS_PORT` (default 8010) and should be bound to
    /// `127.0.0.1` on the host via the compose port mapping —
    /// `127.0.0.1:8010:8010` — never exposed publicly.
    #[arg(long, env = "HARNESS_ENABLED", default_value_t = false)]
    pub harness_enabled: bool,

    /// TCP port for the E2E harness HTTP server, inside the container.
    /// Only relevant when `harness_enabled` is true.
    #[arg(long, env = "HARNESS_PORT", default_value_t = 8010)]
    pub harness_port: u16,
}

impl Config {
    /// Parse `bypass_consent_user_ids_raw` into a deduplicated set of Discord
    /// user IDs. Empty string → empty set → bypass disabled (the prod default).
    ///
    /// Entries that fail to parse as `u64` are silently dropped — a malformed
    /// env var should not crash the bot, but the ignored value is never
    /// treated as "valid bypass id 0" either.
    pub fn bypass_consent_user_ids(&self) -> HashSet<u64> {
        self.bypass_consent_user_ids_raw
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<u64>().ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(raw: &str) -> Config {
        Config {
            token: "t".into(),
            data_api_url: "u".into(),
            data_api_shared_secret: "s".into(),
            local_buffer_dir: "b".into(),
            min_participants: 1,
            require_all_consent: true,
            bypass_consent_user_ids_raw: raw.into(),
            harness_enabled: false,
            harness_port: 8010,
        }
    }

    #[test]
    fn bypass_empty_string_yields_empty_set() {
        assert!(cfg("").bypass_consent_user_ids().is_empty());
    }

    #[test]
    fn bypass_single_id_parses() {
        let s = cfg("1490405412917215463").bypass_consent_user_ids();
        assert_eq!(s.len(), 1);
        assert!(s.contains(&1490405412917215463u64));
    }

    #[test]
    fn bypass_multiple_ids_with_whitespace() {
        let s = cfg(" 1, 2 ,3 ").bypass_consent_user_ids();
        assert_eq!(s, [1u64, 2, 3].into_iter().collect::<HashSet<u64>>());
    }

    #[test]
    fn bypass_ignores_garbage_entries() {
        let s = cfg("1,notanumber,3").bypass_consent_user_ids();
        assert_eq!(s, [1u64, 3].into_iter().collect::<HashSet<u64>>());
    }

    #[test]
    fn bypass_dedups_duplicates() {
        let s = cfg("1,1,2,1").bypass_consent_user_ids();
        assert_eq!(s.len(), 2);
    }
}
