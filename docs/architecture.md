# ttrpg-collector — Architecture

## Overview

A Rust Discord bot that records per-speaker TTRPG voice sessions with explicit consent, pseudonymizes participants, and uploads raw PCM audio chunks through the Data API during recording. Part of the Open Voice Project — an open dataset of tabletop RPG session recordings.

## System Context

```
Discord Voice Channel
         │
         │ DAVE E2EE audio (per-speaker PCM via VoiceTick)
         │
    ┌────┴────┐
    │ Bot     │  Rust (serenity + songbird)
    │ Process │  Single instance, event-driven
    └────┬────┘
         │
         │ HTTP (Bearer token, shared-secret auth)
         ▼
    ovp-data-api
      • Owns Postgres (sessions, participants, consent, audit)
      • Owns S3 (audio chunks, metadata files)
```

The bot never touches Postgres or S3 directly. All persistence goes through the Data API as HTTP calls.

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

## Source Layout

```
voice-capture/
  src/
    main.rs              — Entry point, serenity client, event handlers
    config.rs            — Env var config (clap)
    state.rs             — Shared AppState (config, sessions, api client)
    session.rs           — Session struct, Phase enum, consent state machine
    harness.rs           — Dev-only E2E harness HTTP server
    telemetry.rs         — Metrics counters/gauges/histograms
    api_client.rs        — Data API HTTP client
    commands/
      consent.rs         — Consent button handler + recording startup pipeline
      license.rs         — License preference toggles
      record.rs          — /record slash command
      stop.rs            — /stop + auto-stop finalization
    voice/
      events.rs          — voice_state_update: mid-session joins, auto-stop
      receiver.rs        — VoiceTick handler, per-speaker chunked PCM, SSRC tracking
    storage/
      bundle.rs          — SessionMeta, ConsentRecord serde, pseudonymize() helper
  assets/                — TTS clips (recording_started.wav, recording_stopped.wav)
```

## Session Lifecycle

### 1. `/record` Command

```
GM runs /record
  │
  ├─ Create Session (state: AwaitingConsent)
  ├─ Check blocklist — skip globally opted-out users
  ├─ Register session + participants via Data API
  └─ Post consent embed with Accept / Decline buttons
```

### 2. Consent Collection

```
Each player clicks Accept or Decline
  │
  ├─ Record consent scope + timestamp in memory
  ├─ Push consent to Data API (audit logged server-side)
  ├─ If scope == Full: send ephemeral license follow-up
  │     "No LLM Training" / "No Public Release" buttons
  └─ Check: all responded AND quorum met?
```

### 3. Recording Start (quorum met)

```
Quorum met
  │
  ├─ Bot joins voice channel via Songbird
  ├─ Create audio pipeline (mpsc channel + buffer task)
  ├─ Wait for DAVE handshake (up to 3 retry attempts)
  ├─ Transition session to Recording
  ├─ Update session state via Data API → recording
  └─ Play recording_started.wav announcement
```

### 4. During Recording

```
VoiceTick events arrive (20ms frames, per-speaker decoded PCM)
  │
  ├─ Check: is speaker consented?
  ├─ Send packet through lock-free mpsc channel
  └─ Buffer task accumulates per-speaker PCM bytes
       └─ When buffer >= 5MB: upload chunk via Data API
```

**Audio format:** raw PCM, signed 16-bit little-endian, 48kHz, stereo.

**Mid-session joiners:** detected via voice_state_update. Consent prompt posted. Audio capture starts only after they accept.

### 5. `/stop` or Auto-Stop

```
GM runs /stop (or channel empties for 30s)
  │
  ├─ Signal audio pipeline shutdown
  ├─ Flush all speaker buffers (upload final chunks)
  ├─ Upload meta.json + consent.json via Data API
  ├─ Finalize session via Data API (status → uploaded)
  ├─ Clean up in-memory state
  └─ Leave voice channel
```

## Consent Model

### Recording Consent (gates audio capture)

| Scope | Audio Captured? |
|-------|----------------|
| `full` | Yes |
| `decline` | No |

### Data Licensing (non-blocking, defaults to fully open)

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

Deterministic, one-way. Same function used by bot, API, and frontend.

## Deployment

- **CI:** `cargo check` + `cargo clippy` on push to `dev` or PR to `dev`/`main`
- **CD:** Docker build + GHCR push + SSH deploy on tag `v*`
- **Git workflow:** `feature/*` → `dev` → `main` → tag
