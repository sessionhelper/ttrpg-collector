# chronicle-bot — Architecture

## Overview

A Rust Discord bot that records per-speaker TTRPG voice sessions with
explicit consent, pseudonymizes participants, and uploads raw PCM audio
chunks through the Data API during recording. Part of the Open Voice
Project — an open dataset of tabletop RPG session recordings.

## System Context

```
Discord Voice Channel
         │
         │ DAVE E2EE audio (per-speaker PCM via VoiceTick)
         │
    ┌────┴────┐
    │ Bot     │  Rust (serenity + songbird-next with DAVE support)
    │ Process │  Single instance, event-driven
    └────┬────┘
         │
         │ HTTP (Bearer token, shared-secret auth)
         ▼
    chronicle-data-api
      • Owns Postgres (sessions, participants, consent, audit)
      • Owns S3 (audio chunks, metadata files)
      • Broadcasts ChunkUploaded events to the worker over WS
```

The bot never touches Postgres or S3 directly. All persistence goes
through the Data API as HTTP calls.

## Stack

| Component | Technology |
|-----------|-----------|
| Discord library | serenity 0.12 |
| Voice receive | songbird `next` branch (DAVE E2EE support) |
| Async runtime | tokio 1 |
| HTTP client | reqwest (talks to Data API) |
| Pseudonymization | sha2 (SHA-256, first 8 bytes hex) |
| CLI config | clap 4 (env var loading) |
| Logging | tracing + tracing-subscriber |
| Metrics | metrics crate (optional Prometheus exporter) |
| Harness HTTP | axum (dev-only, wired behind `HARNESS_ENABLED`) |

## Source Layout

```
voice-capture/
  src/
    main.rs              — Entry point, serenity client, event handlers
    config.rs            — Env var config (clap)
    state.rs             — Shared AppState (config, sessions, api client, ctx)
    session.rs           — Session struct, Phase enum, consent state machine,
                           DAVE heal state (ssrcs_seen, op5 channels,
                           recording_stable AtomicBool)
    harness.rs           — Dev-only E2E harness HTTP server:
                             /health, /status, /record, /stop
                           `/status` exposes recording + stable so the
                           test runner can wait for DAVE to settle.
    telemetry.rs         — Metrics counters/gauges/histograms
    api_client.rs        — Data API HTTP client
    commands/
      mod.rs             — Dispatch
      record.rs          — /record slash command
      stop.rs            — /stop + auto-stop finalization
      consent.rs         — Consent button handler, start_recording_pipeline,
                           spawn_dave_heal_task (three-tier health monitor)
      license.rs         — License preference toggles
    voice/
      mod.rs
      events.rs          — voice_state_update: mid-session joins, auto-stop
      receiver.rs        — VoiceTick handler, per-speaker chunked PCM,
                           SSRC tracking, ssrcs_seen insert, OP5 forwarding
    storage/
      bundle.rs          — SessionMeta, ConsentRecord serde, pseudonymize()
  assets/                — TTS clips (recording_started.wav, recording_stopped.wav)
```

## Session Lifecycle

### 1. `/record` command

```
GM runs /record
  │
  ├─ Create Session (state: AwaitingConsent)
  ├─ Check blocklist — skip globally opted-out users
  ├─ Register session via Data API
  ├─ Batch-insert participants via POST /internal/sessions/{id}/participants/batch
  └─ Post consent embed with Accept / Decline buttons
```

### 2. Consent collection

```
Each player clicks Accept or Decline
  │
  ├─ Record consent scope + timestamp in memory
  ├─ Push consent to Data API via PATCH /internal/participants/{id}/consent
  │  (audit logged server-side in the same txn)
  ├─ If scope == Full: send ephemeral license follow-up
  │     "No LLM Training" / "No Public Release" buttons
  └─ Check: all responded AND quorum met?
```

### 3. Recording start (quorum met)

```
Quorum met
  │
  ├─ Bot joins voice channel via Songbird
  ├─ Create audio pipeline (mpsc channel + buffer task)
  ├─ Wait for DAVE handshake (up to 3 retry attempts)
  ├─ Transition session to Recording
  ├─ Update session state via Data API → recording
  ├─ Play recording_started.wav announcement
  └─ Spawn DAVE heal task (three tiers running concurrently):
       1. Initial OP5 window (10s per OP5 user)
       2. Periodic fallback (every 10s after stable)
       3. Dead-connection fallback (30s of empty ssrcs_seen after stable)
     On any detection: leave → 2s → rejoin → reattach receiver → clear
     ssrcs_seen → replay start announcement → set recording_stable.
```

### 4. During recording

```
VoiceTick events arrive (20ms frames, per-speaker decoded PCM)
  │
  ├─ Insert SSRC into ssrcs_seen (heal-task read path)
  ├─ Check: is speaker consented?
  ├─ Send packet through lock-free mpsc channel
  └─ Buffer task accumulates per-speaker PCM bytes
       └─ When buffer >= 2MB: upload chunk via Data API
```

**Audio format:** raw PCM, signed 16-bit little-endian, 48 kHz, stereo.
One 2 MB chunk = ~10.92 seconds of audio per speaker.

**Mid-session joiners:** detected via voice_state_update. Consent prompt
posted. Audio capture starts only after they accept (their user_id is added
to `consented_users` on accept).

### 5. `/stop` or auto-stop

```
GM runs /stop (or channel empties for 30s)
  │
  ├─ Signal audio pipeline shutdown
  ├─ Flush all speaker buffers (upload final chunks)
  ├─ Upload meta.json + consent.json via Data API
  ├─ Finalize session via Data API (PATCH status → uploaded)
  │    → data-api broadcasts SessionStatusChanged{uploaded}
  │      which the worker consumes to finalize its active streaming session
  ├─ Clean up in-memory state
  └─ Leave voice channel
```

## DAVE Health Monitor — three tiers

The single detection signal (OP5 without matching SSRC) is applied at three
different scopes inside `spawn_dave_heal_task`:

1. **Initial check** — Runs a 10-second window immediately after
   `recording_started`. For each OP5 event received during that window, waits
   up to 10 s for the SSRC to show up in `ssrcs_seen`. If SSRCs are arriving
   but `mapped_count < consented_count`, heals immediately (covers the missed
   OP5 edge-trigger case where songbird was already receiving RTP before
   `SpeakingStateUpdate` fired).

2. **Periodic fallback** — Every 10 seconds after stabilization, re-drains
   `op5_rx`, re-reads the live `consented_count`, and re-checks against
   `ssrcs_seen`. This covers mid-session DAVE degradation caused by e.g. a
   user disconnecting and re-joining.

3. **Dead-connection fallback** — If the session has stabilized but
   `ssrcs_seen` stays empty for 30 seconds, the underlying voice
   connection is treated as dead and the heal fires.

Any detection triggers the same heal sequence (leave → 2 s → rejoin → reattach
the audio receiver with a fresh op5 channel → clear `ssrcs_seen` → replay the
start announcement). The heal runs at most once.

`recording_stable` (`Arc<AtomicBool>` on the session) is flipped once either
the initial check passes cleanly or the heal completes, and is surfaced via
the harness `/status` endpoint so the E2E test runner knows when feeders can
safely start transmitting without racing the DAVE handshake.

## Consent Model

### Recording consent (gates audio capture)

| Scope | Audio Captured? |
|-------|----------------|
| `full` | Yes |
| `decline` | No |

### Data licensing (non-blocking, defaults to fully open)

Two independent flags per participant per session:

| `no_llm_training` | `no_public_release` | Published? | LLM Training? |
|---|---|---|---|
| false | false | Yes (CC BY-SA 4.0) | Yes |
| true | false | Yes (CC BY-SA 4.0 + RAIL) | No |
| false | true | No | Yes (internal only) |
| true | true | No | No |

## Pseudonymization

```rust
pub fn pseudonymize(user_id: u64) -> String {
    SHA256(user_id.to_string())[0..8].to_hex()  // 16 hex chars
}
```

Deterministic, one-way. Same function used by bot, data-api, frontend, and
feeder.

## Deployment

- **CI:** `cargo check` + `cargo clippy` on push to `dev` or PR to `dev`/`main`
- **CD:** Docker build + GHCR push + SSH deploy on tag `v*`
- **Git workflow:** `feature/*` → `dev` → `main` → tag
