# ttrpg-collector

> Org-wide conventions (Rust style, git workflow, shared-secret auth, pseudonymization, cross-service architecture) live in `/home/alex/sessionhelper-hub/CLAUDE.md`. Read that first for anything cross-cutting.

Discord bot that captures TTRPG session audio with per-participant consent and uploads it through the Data API for the Open Voice Project dataset.

## Stack

- `serenity` 0.12 — Discord gateway
- `songbird` (`next` branch) — voice capture with **DAVE E2EE** support. Required because py-cord's DAVE impl captured only ~10% of packets.
- `davey` — MLS/AES-256-GCM for DAVE
- `opus2` — Opus decode
- `reqwest` — talks to the Data API (never touches S3 or Postgres directly)
- No `ffmpeg` at runtime — raw PCM goes up, `ovp-pipeline` handles audio processing downstream.

## Layout

```
voice-capture/
  src/
    main.rs              — entry point, serenity client, event handlers
    config.rs            — clap env config
    state.rs             — AppState (sessions, api client)
    session.rs           — Session struct, Phase enum, consent state machine, metadata
    telemetry.rs         — metrics counters/gauges/histograms
    api_client.rs        — Data API HTTP client (auth, chunks, sessions, participants)
    lib.rs               — library entry for tests
    commands/
      record.rs          — /record slash command + consent embed
      stop.rs            — /stop + auto-stop finalization
    voice/
      receiver.rs        — AudioReceiver, SSRC→user mapping, channel-based chunk upload
    storage/
      bundle.rs          — SessionMeta + ConsentRecord serde, pseudonymize() helper
  migrations/            — reference SQL for the Data API schema (not run here)
  assets/                — TTS clips (recording_started.wav, recording_stopped.wav)
scripts/
  sync-s3.sh             — local data download for review
legal/                   — privacy policy, ToS, consent text, dataset card
```

## Repo-specific decisions

- **Unified `Session` struct with `Phase` enum** (AwaitingConsent / Recording / Finalizing / Complete). No scattered HashMaps.
- **Channel-based audio pipeline** — `VoiceTick` handler → lock-free mpsc → buffer task → 5MB chunks to Data API. No mutexes on the hot path.
- **SSRC → user_id mapping** via `SpeakingStateUpdate` events. Only consented users' packets leave the receiver.
- **Mid-session joiners** get a consent prompt before their audio is captured.
- **Auto-stop** after 30s empty voice channel.
- **Songbird `next` branch is pinned** — don't bump to a stable release until DAVE lands there.

## Env vars

| Var | Required | Default |
|---|---|---|
| `DISCORD_TOKEN` | yes | — |
| `DATA_API_URL` | no | `http://127.0.0.1:8001` |
| `DATA_API_SHARED_SECRET` | yes | — |
| `LOCAL_BUFFER_DIR` | no | `./sessions` |
| `MIN_PARTICIPANTS` | no | `2` |
| `REQUIRE_ALL_CONSENT` | no | `true` |

## Build / run

```bash
cp .env.example .env      # fill in credentials
cd voice-capture
cargo build --release
cargo run --release
```

Build deps: `cmake` (for Opus). No `ffmpeg` needed.

## E2E test harness (dev only)

The end-to-end harness feeder bot lives in its own sibling repo,
[`ttrpg-collector-feeder`](https://github.com/sessionhelper/ttrpg-collector-feeder).
It's a minimal Discord bot that joins a voice channel and plays a
pre-recorded WAV on demand via a loopback HTTP control API. Prod builds of
`ttrpg-collector` never include any harness code.

See `infra/dev-compose.yml` in `sessionhelper-hub` for the four feeder
services (moe, larry, curly, gygax).

## CI/CD

GitHub Actions → Docker build → GHCR → SSH deploy on tag `v*`. See `infra/collector.md` in the hub repo for the full deploy pipeline.
