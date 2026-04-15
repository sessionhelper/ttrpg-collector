//! Telemetry metrics for the chronicle-bot.
//!
//! All metric names are prefixed with `chronicle_`. Metrics are recorded via
//! the `metrics` crate facade; a Prometheus exporter can be enabled with the
//! `prometheus` Cargo feature.

use metrics::{describe_counter, describe_gauge, describe_histogram};

/// Register all metric descriptions. Call once at startup.
pub fn describe_metrics() {
    // --- Sessions ---
    describe_counter!(
        "chronicle_sessions_total",
        "Total recording sessions, labeled by outcome"
    );
    describe_gauge!(
        "chronicle_sessions_active",
        "Currently active recording sessions"
    );

    // --- DAVE / voice ---
    describe_counter!(
        "chronicle_dave_attempts_total",
        "DAVE voice-encryption negotiation attempts, labeled by outcome"
    );
    describe_counter!(
        "chronicle_audio_packets_received",
        "Total decoded voice packets received from VoiceTick"
    );

    // --- Pre-consent disk cache ---
    describe_gauge!(
        "chronicle_prerolled_chunks_cached_bytes",
        "Current on-disk bytes of pre-consent cached PCM chunks \
         (sum of per-session cache-dir sizes)"
    );
    describe_counter!(
        "chronicle_prerolled_chunks_dropped_total",
        "Pre-consent chunk files dropped by the LOCAL_BUFFER_MAX_SECS cap"
    );

    // --- Uploads ---
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
        "chronicle_uploads_total",
        "Data API upload operations, labeled by type and outcome"
    );

    // --- Consent ---
    describe_counter!(
        "chronicle_consent_responses_total",
        "Consent responses, labeled by scope"
    );

    // --- Interaction wrapper ---
    describe_histogram!(
        "chronicle_interaction_ack_us",
        "Microseconds between interaction handler entry and the ack/defer \
         response being sent. Discord's 3-second window is 3_000_000 us."
    );
    describe_histogram!(
        "chronicle_interaction_handle_us",
        "Microseconds between interaction handler entry and the handler's \
         full completion (ack + business logic + final Discord call)."
    );

    // --- Stabilization gate ---
    describe_histogram!(
        "chronicle_stabilization_gate_secs",
        "Wall-clock seconds spent in AwaitingStabilization before the gate \
         opened (or the session was cancelled)."
    );
}
