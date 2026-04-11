//! Telemetry metrics for the TTRPG collector bot.
//!
//! All metric names are prefixed with `chronicle_` to avoid collisions.
//! Metrics are recorded via the `metrics` crate facade; a Prometheus
//! exporter can be enabled with the `prometheus` Cargo feature.
//!
//! ## Counters
//!
//! - `chronicle_sessions_total` — incremented on /record, labeled by `outcome`
//!   (started, failed, cancelled)
//! - `chronicle_dave_attempts_total` — per DAVE retry attempt, labeled by `outcome`
//!   (success, failure)
//! - `chronicle_audio_packets_received` — total decoded voice packets from VoiceTick
//! - `chronicle_audio_chunks_uploaded` — successful chunk uploads via Data API
//! - `chronicle_consent_responses_total` — labeled by `scope` (full, decline, decline_audio)
//! - `chronicle_uploads_total` — labeled by `type` (chunk, metadata) and `outcome`
//!
//! ## Gauges
//!
//! - `chronicle_sessions_active` — currently recording sessions (inc on start, dec on stop)
//! - `chronicle_audio_bytes_buffered` — current bytes in speaker buffers (updated periodically)
//!
//! ## Histograms
//!
//! - `chronicle_audio_chunk_upload_seconds` — time per chunk upload to Data API

use metrics::{describe_counter, describe_gauge, describe_histogram};

/// Register all metric descriptions. Call once at startup.
pub fn describe_metrics() {
    describe_counter!(
        "chronicle_sessions_total",
        "Total recording sessions, labeled by outcome"
    );
    describe_gauge!(
        "chronicle_sessions_active",
        "Currently active recording sessions"
    );
    describe_counter!(
        "chronicle_dave_attempts_total",
        "DAVE voice encryption negotiation attempts, labeled by outcome"
    );
    describe_counter!(
        "chronicle_audio_packets_received",
        "Total decoded voice packets received from VoiceTick"
    );
    describe_gauge!(
        "chronicle_audio_bytes_buffered",
        "Current bytes in per-speaker audio buffers"
    );
    describe_counter!(
        "chronicle_audio_chunks_uploaded",
        "Successful audio chunk uploads via Data API"
    );
    describe_histogram!(
        "chronicle_audio_chunk_upload_seconds",
        "Time per audio chunk upload to Data API"
    );
    describe_counter!(
        "chronicle_consent_responses_total",
        "Consent responses, labeled by scope"
    );
    describe_counter!(
        "chronicle_uploads_total",
        "Data API upload operations, labeled by type and outcome"
    );
}
