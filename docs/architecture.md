# ttrpg-collector — Architecture

## Overview

A Rust Discord bot that records per-speaker TTRPG voice sessions with explicit consent, pseudonymizes participants, and uploads audio to S3 in chunks during recording. Part of the Open Voice Project — an open dataset of tabletop RPG session recordings.

## System Context

```
Discord Voice Channel
         │
         │ DAVE E2EE audio (per-speaker PCM via VoiceTick)
         │
    ┌────┴────┐
    │ Bot     │  Rust (serenity + songbird)
    │ Process │  Single instance, event-driven
    └──┬──┬───┘
       │  │
       │  ├──→ S3 (Hetzner Object Storage)
       │  │      metadata: meta.json, consent.json (written at start, updated at stop)
       │  │      audio: 50MB raw PCM chunks per speaker (uploaded during recording)
       │  │
       │  └──→ Postgres
       │         sessions, participants, consent state, audit log
       │         (source of truth for the frontend portal)
       │
       └──→ ovp-pipeline (Rust library crate)
              triggered after /stop — transcribes audio, writes segments to Postgres
```

## Stack

| Component | Technology |
|-----------|-----------|
| Discord library | serenity 0.12 |
| Voice receive | songbird `next` branch (DAVE E2EE support) |
| Async runtime | tokio 1 |
| S3 client | aws-sdk-s3 |
| Database | sqlx 0.8 (Postgres) |
| Pseudonymization | sha2 (SHA-256) |
| CLI config | clap 4 (env var loading) |
| Logging | tracing + tracing-subscriber |
| Audio pipeline | ovp-pipeline crate (post-recording transcription) |

## Source Layout

```
voice-capture/
  src/
    main.rs              — Entry point, serenity client, event handlers
    config.rs            — Env var config (clap)
    state.rs             — Shared AppState (config, consent, bundles, S3, DB pool)
    db.rs                — Postgres data access layer
    commands/
      mod.rs
      record.rs          — /record slash command
      stop.rs            — /stop + auto-stop (finalize + upload)
    consent/
      mod.rs
      manager.rs         — Consent state machine, quorum logic
      embeds.rs          — Discord embed + button builders
    voice/
      mod.rs
      receiver.rs        — VoiceTick handler, per-speaker chunked PCM writer, SSRC tracking
    storage/
      mod.rs
      bundle.rs          — Session directory structure, meta.json, consent.json, pseudonymization
      s3.rs              — S3 upload with retry
  migrations/
    001_initial.sql      — users, sessions, session_participants, consent_audit_log
    002_transcripts.sql  — transcript_segments, segment_flags, segment_edits
  Cargo.toml
```

## Session Lifecycle

### 1. `/record` Command

```
GM runs /record [system] [campaign]
  │
  ├─ Create ConsentSession (state: AwaitingConsent)
  ├─ Create SessionBundle (session_id = UUID v4, local dirs)
  ├─ Check blocklist — skip globally opted-out users
  ├─ Write session + participants to Postgres
  └─ Post consent embed with Accept / Decline buttons
```

### 2. Consent Collection

```
Each player clicks Accept or Decline
  │
  ├─ Record consent scope + timestamp in memory
  ├─ Write consent to Postgres + audit log
  ├─ If scope == Full: send ephemeral license follow-up
  │     "No LLM Training" / "No Public Release" buttons (non-blocking)
  │     Default: open (CC BY-SA 4.0) if ignored
  └─ Check: all responded AND quorum met?
```

### 3. Recording Start (quorum met)

```
Quorum met
  │
  ├─ Bot joins voice channel via Songbird
  ├─ Register AudioReceiver (VoiceTick handler)
  ├─ Register SpeakingTracker (SSRC → user_id mapper)
  ├─ Update session state → Recording (Postgres)
  ├─ Write partial meta.json to S3 (no ended_at yet)
  └─ Write consent.json to S3
```

### 4. During Recording

```
VoiceTick events arrive (20ms frames, per-speaker decoded PCM)
  │
  ├─ Check: is speaker in consented_users set?
  ├─ Map SSRC → user_id → pseudo_id
  ├─ Write PCM bytes to ChunkedWriter buffer
  └─ When buffer >= 50MB:
       ├─ Upload chunk to S3: sessions/{guild}/{session}/audio/{pseudo_id}/chunk_{seq:04}.pcm
       ├─ Increment sequence number
       └─ Clear buffer
```

**Audio format in S3:** raw PCM, signed 16-bit little-endian, 48kHz, stereo. Zero encoding overhead.

**Mid-session joiners:** detected via voice_state_update. Added to consent session, DB participant record created. Audio capture starts only after they respond to consent prompt.

### 5. `/stop` or Auto-Stop

```
GM runs /stop (or channel empties for 30s)
  │
  ├─ Flush all ChunkedWriters (upload final partial buffers)
  ├─ Overwrite meta.json in S3 (final: adds ended_at, duration, counts)
  ├─ Overwrite consent.json in S3 (if any mid-session changes)
  ├─ Finalize session in Postgres (ended_at, duration, s3_prefix, status=uploaded)
  ├─ Trigger ovp-pipeline transcription (async, non-blocking)
  ├─ Clean up in-memory state (consent session, bundle, SSRC maps)
  └─ Leave voice channel
```

## Data Flow: S3 Layout

```
sessions/{guild_id}/{session_id}/
  meta.json                              # Written at recording start, overwritten at stop
  consent.json                           # Written at recording start, overwritten at stop
  audio/
    {pseudo_id_a}/
      chunk_0000.pcm                     # 50MB raw PCM chunks
      chunk_0001.pcm                     # ~4.4 minutes each at 48kHz stereo s16le
      chunk_0002.pcm
    {pseudo_id_b}/
      chunk_0000.pcm
      chunk_0001.pcm
      ...
```

## Data Flow: Postgres

Bot writes to these tables:

| Table | When | What |
|-------|------|------|
| `sessions` | /record | Create with status=awaiting_consent |
| `sessions` | quorum met | Update status=recording |
| `sessions` | /stop | Update ended_at, duration, s3_prefix, status=uploaded |
| `users` | consent button | Upsert by pseudo_id |
| `session_participants` | /record | Create per participant |
| `session_participants` | consent button | Update scope, consented_at |
| `session_participants` | license button | Toggle no_llm_training or no_public_release |
| `consent_audit_log` | consent button | Insert grant/decline entry |

DB writes are **non-blocking** — errors are logged, recording continues. S3 is the fallback. Sessions can be backfilled from S3 meta.json/consent.json.

## Consent Model

### Recording Consent (gates audio capture)

| Scope | Audio Captured? | In Dataset? |
|-------|----------------|-------------|
| `full` | Yes | Depends on data_license |
| `decline_audio` | No (present but not recorded) | No |
| `decline` | No | No |

### Data Licensing (non-blocking, defaults to fully open)

Two independent flags per participant per session:

| `no_llm_training` | `no_public_release` | Published? | LLM Training? |
|---|---|---|---|
| false | false | Yes (ovp-open, CC BY-SA 4.0) | Yes |
| true | false | Yes (ovp-rail, CC BY-SA 4.0 + RAIL) | No |
| false | true | No | Yes (internal only) |
| true | true | No | No |

Two-step flow: Accept/Decline gates recording. Ephemeral follow-up shows two independent toggle buttons. Both default to off (fully open) if ignored.

## Pseudonymization

```rust
pub fn pseudonymize(user_id: u64) -> String {
    SHA256(user_id.to_string())[0..8].to_hex()  // 16 hex chars
}
```

Deterministic, one-way. Same function used by bot, API, and frontend (server-side only).

## Deployment

### Infrastructure

- **VPS** — single server, Docker Compose
- **Container** — `ghcr.io/sessionhelper/ttrpg-collector:latest`
- **S3** — Hetzner Object Storage (S3-compatible, path-style addressing)
- **Postgres** — shared with the Axum API service
- **No ffmpeg** — raw PCM uploaded directly, pipeline crate handles all audio processing

### CI/CD

| Workflow | Trigger | What |
|----------|---------|------|
| `ci.yml` | Push to `dev`, PRs to `dev`/`main` | `cargo check --release` + `cargo clippy` |
| `deploy.yml` | Push tag `v*` | Build Docker → push to GHCR → SSH deploy to VPS |

### Git Workflow

- **main** — production, deploys on tag. Only receives merges from `dev`.
- **dev** — integration branch. Feature branches merge here first.
- **feature/*** — branch from `dev`, merge back to `dev` via `--no-ff`

```
feature/foo ──→ dev (merge --no-ff) ──→ main (merge --no-ff) ──→ tag v0.x.x ──→ CI/CD deploy
```

**Rules:**
- Never commit directly to `main` or `dev`
- Never merge a feature branch directly to `main`
- Always merge `dev` → `main` when ready to release, then tag

### Environment Variables

```
DISCORD_TOKEN=
DATABASE_URL=postgres://user:pass@localhost/ttrpg_collector
S3_ACCESS_KEY=
S3_SECRET_KEY=
S3_BUCKET=ttrpg-dataset-raw
S3_ENDPOINT=https://nbg1.your-objectstorage.com
LOCAL_BUFFER_DIR=./sessions
MIN_PARTICIPANTS=2
REQUIRE_ALL_CONSENT=true
```

## Error Handling

| Failure | Impact | Recovery |
|---------|--------|----------|
| Postgres down | No DB writes | S3 still receives audio + metadata. Backfill from S3 later. |
| S3 upload fails | Chunk retry 3x with exponential backoff | If all retries fail, chunk stays in local buffer. Manual recovery. |
| Bot crash mid-recording | Audio up to last uploaded chunk is safe in S3 | Partial metadata in S3 + Postgres. Session marked incomplete. |
| Discord disconnect | Songbird auto-reconnect handles transient drops | Extended outage: audio gap, but chunks before/after are intact. |
