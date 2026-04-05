# ttrpg-collector

Discord bot for collecting TTRPG session audio for an open dataset.

## Code Style

Follow [Rust Design Patterns](https://rust-unofficial.github.io/patterns/) and [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).

**Readability:**
- Add comments that explain **why**, not what. Every function should have a brief doc comment explaining its purpose and rationale.
- Complex logic blocks should have inline comments — especially Discord API quirks, timing constraints, and error handling decisions.
- Keep comments concise but sufficient for someone reading the code for the first time.

**Patterns:**
- Use `Result` + `?` for error propagation — keep the happy path flat, no nested if/match chains.
- Use enums with data for state machines — match on state, don't if-check fields.
- Use iterators over manual loops. Use `filter`, `map`, `for_each` over index-based iteration.
- Use the typestate pattern when invalid transitions should be compile errors.
- Avoid premature abstraction — three similar lines beats a premature generic.

**Anti-patterns (don't do these):**
- No deeply nested if-statements — flatten with early returns and `?`.
- No stringly-typed state — use enums.
- No scattered state across multiple HashMaps — use a struct that owns its data.
- No manual mutex juggling for hot paths — use channels.

## Stack

- Rust (serenity + songbird)
- songbird `next` branch for DAVE E2EE voice receive
- aws-sdk-s3 for S3-compatible uploads (Hetzner Object Storage)
- sqlx 0.8 for Postgres
- No ffmpeg — raw PCM uploaded to S3, pipeline crate handles audio processing

## Layout

```
voice-capture/
  src/
    main.rs              — entry point, serenity client, event handlers
    config.rs            — env var config (clap)
    state.rs             — simplified AppState (sessions, s3, db)
    session.rs           — unified Session struct, Phase enum, consent, metadata serialization
    telemetry.rs         — metrics descriptions (counters, gauges, histograms)
    db.rs                — Postgres data access layer
    commands/
      record.rs          — /record slash command
      stop.rs            — /stop + auto-stop (finalize + S3 upload)
    voice/
      receiver.rs        — channel-based audio pipeline, chunked S3 upload, DAVE detection
    storage/
      bundle.rs          — serialization structs, pseudonymization
      s3.rs              — S3 upload_bytes with retry
  assets/
    recording_started.wav — TTS clip played on recording start
    recording_stopped.wav — TTS clip played on /stop
  migrations/
    001_initial.sql      — users, sessions, participants, audit log
    002_transcripts.sql  — segments, flags, edits
  build.rs               — git version string (tag/branch-hash-dirty)
  Cargo.toml
scripts/
  sync-s3.sh             — download S3 data locally
legal/                   — privacy policy, ToS, consent text, dataset card
```

## Building

```bash
cd voice-capture
cargo build --release
```

Requires: cmake (for opus build), ffmpeg (runtime, for PCM→FLAC)

## Running

```bash
cp .env.example .env  # fill in credentials
cd voice-capture
cargo run --release
```

## E2E test harness (dev only)

The end-to-end harness feeder bot lives in its own sibling repo,
[`ttrpg-collector-feeder`](https://github.com/sessionhelper/ttrpg-collector-feeder).
It's a minimal Discord bot that joins a voice channel and plays a
pre-recorded WAV on demand via a loopback HTTP control API. Prod builds of
`ttrpg-collector` never include any harness code.

See `infra/dev-compose.yml` in `sessionhelper-hub` for the four feeder
services (moe, larry, curly, gygax).

## Git Workflow

- **main** — production. Only receives merges from `dev`. Tag to deploy.
- **dev** — integration branch. Feature branches merge here first.
- **feature/*** — branch from `dev`, merge back to `dev` via `--no-ff`

**Never commit directly to main or dev. Never merge feature branches directly to main.**

```
feature/foo → dev (--no-ff) → main (--no-ff) → tag v0.x.x → CI/CD deploy
```

## Key Decisions

- Full Rust — py-cord's DAVE voice receive was fundamentally broken (~10% packet capture)
- Songbird handles DAVE/E2EE natively with near-zero packet loss
- PCM written to disk, converted to FLAC on /stop, uploaded to S3
- Per-user tracks via SSRC → user_id mapping from SpeakingStateUpdate events
- Simple SHA256 hash for pseudonymization (no salt — voice is public anyway)
