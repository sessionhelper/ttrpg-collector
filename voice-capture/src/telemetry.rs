//! Telemetry metrics for the TTRPG collector bot.
//!
//! All metric names are prefixed with `ttrpg_` to avoid collisions.
//! Metrics are recorded via the `metrics` crate facade; a Prometheus
//! exporter can be enabled with the `prometheus` Cargo feature.
//!
//! ## Counters
//!
//! - `ttrpg_sessions_total` — incremented on /record, labeled by `outcome`
//!   (started, failed, cancelled)
//! - `ttrpg_dave_attempts_total` — per DAVE retry attempt, labeled by `outcome`
//!   (success, failure)
//! - `ttrpg_audio_packets_received` — total decoded voice packets from VoiceTick
//! - `ttrpg_audio_chunks_uploaded` — successful S3 chunk uploads
//! - `ttrpg_consent_responses_total` — labeled by `scope` (full, decline, decline_audio)
//! - `ttrpg_db_writes_total` — labeled by `operation` and `outcome` (success, failure)
//! - `ttrpg_s3_uploads_total` — labeled by `type` (chunk, meta, consent) and `outcome`
//!
//! ## Gauges
//!
//! - `ttrpg_sessions_active` — currently recording sessions (inc on start, dec on stop)
//! - `ttrpg_audio_bytes_buffered` — current bytes in speaker buffers (updated periodically)
//!
//! ## Histograms
//!
//! - `ttrpg_audio_chunk_upload_seconds` — time per chunk upload to S3

use metrics::{describe_counter, describe_gauge, describe_histogram};

/// Register all metric descriptions. Call once at startup.
pub fn describe_metrics() {
    describe_counter!(
        "ttrpg_sessions_total",
        "Total recording sessions, labeled by outcome"
    );
    describe_gauge!(
        "ttrpg_sessions_active",
        "Currently active recording sessions"
    );
    describe_counter!(
        "ttrpg_dave_attempts_total",
        "DAVE voice encryption negotiation attempts, labeled by outcome"
    );
    describe_counter!(
        "ttrpg_audio_packets_received",
        "Total decoded voice packets received from VoiceTick"
    );
    describe_gauge!(
        "ttrpg_audio_bytes_buffered",
        "Current bytes in per-speaker audio buffers"
    );
    describe_counter!(
        "ttrpg_audio_chunks_uploaded",
        "Successful S3 audio chunk uploads"
    );
    describe_histogram!(
        "ttrpg_audio_chunk_upload_seconds",
        "Time per audio chunk upload to S3"
    );
    describe_counter!(
        "ttrpg_consent_responses_total",
        "Consent responses, labeled by scope"
    );
    describe_counter!(
        "ttrpg_db_writes_total",
        "Database write operations, labeled by operation and outcome"
    );
    describe_counter!(
        "ttrpg_s3_uploads_total",
        "S3 upload operations, labeled by type and outcome"
    );
}
