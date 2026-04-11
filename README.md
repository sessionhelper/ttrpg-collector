# chronicle-bot

Rust Discord bot that records per-speaker TTRPG voice sessions with explicit
consent, captures audio through Discord's DAVE end-to-end encryption, and
streams 2 MB PCM chunks to `chronicle-data-api` during recording for
eventual release as part of the open OVP dataset.

This is the capture end of the Chronicle toolchain. Once a session is
finalized, `chronicle-worker` picks up the chunks via WebSocket events,
runs them through `chronicle-pipeline`, and posts transcripts back to
the data-api.

## Role in the stack

```
Discord voice channel
       │
       │ DAVE E2EE (MLS / AES-256-GCM)
       ▼
┌─────────────────────┐
│    chronicle-bot    │   Serenity gateway + Songbird voice + davey crate
│    (this service)   │
│                     │
│  ┌───────────────┐  │
│  │ VoiceTick     │  │   per-SSRC packet stream
│  │ decoder       │  │
│  └───────┬───────┘  │
│          │          │
│  ┌───────▼───────┐  │
│  │ per-speaker   │  │   4×s16le 48kHz stereo buffers keyed by pseudo_id
│  │ buffers       │  │
│  └───────┬───────┘  │
│          │ 2 MB chunks (≈10.92s each), retry w/ backoff
│          ▼          │
└─────────┬───────────┘
          │
          ▼
┌──────────────────────┐
│ chronicle-data-api   │
└──────────────────────┘
```

See [`docs/architecture.md`](docs/architecture.md) for the full internals:
consent state machine, DAVE reconnect heal (three-tier: OP5 + dead-connection
fallback + failure tolerance counter), buffer task, upload retry (R7), and
telemetry.

## Building

```bash
cd voice-capture
cargo build --release
```

Requires `cmake` (for the Opus build). No ffmpeg needed.

## Running

```bash
cp .env.example .env
# Edit .env — add DISCORD_TOKEN and DATA_API_SHARED_SECRET
cd voice-capture
cargo run --release
```

A healthy start looks like:

```
starting_bot version=dev
data_api_authenticated
harness_http_listening addr=0.0.0.0:8010
bot_ready user=…
guild_commands_registered guild_id=…
```

## Bot commands

| Command | Description |
|---|---|
| `/record` | Start recording in your current voice channel |
| `/stop` | Stop the current recording |

## How it works

1. GM runs `/record` in a text channel while in a voice channel
2. Bot posts a consent embed — each player clicks Accept, Decline, or Decline Audio (no audio, consent to transcript only)
3. Recording starts after everyone responds (quorum met) and the bot joins voice
4. Bot captures per-user audio via DAVE E2EE, filtered by the consented_users set
5. Per-speaker 2 MB raw PCM chunks (~10.92s of s16le stereo 48 kHz) are uploaded to the Data API continuously during recording
6. GM runs `/stop` (or the channel empties for 30s and auto-stops) — session finalized, S3 upload complete, data-api marks the session ready

## Mid-session join handling

New users joining during an active recording get a consent prompt via DM
or ephemeral follow-up. Their audio is excluded from capture until they
accept. This is also where the DAVE protocol transitions happen, which
is the specific failure class tracked in [`docs/dave-audit.md`](docs/dave-audit.md)
and [`sessionhelper-hub/docs/dave-bot-ecosystem.md`](https://github.com/sessionhelper/sessionhelper-hub/blob/main/docs/dave-bot-ecosystem.md)
— the three-tier reconnect heal is the current mitigation.

## Harness HTTP endpoint

For E2E testing (dev only), the bot exposes a small HTTP control surface
on `0.0.0.0:8010` that lets a test runner drive sessions without a real
Discord client. Used by `chronicle-feeder` for multi-user scenario
scripting. Disabled in production.

## Deploy

On tagged `v*` pushes, GitHub Actions builds and pushes to
`ghcr.io/sessionhelper/chronicle-bot:{latest,dev,vX.Y.Z}`, then fetches
the canonical prod compose file from `sessionhelper-hub/infra/prod-compose.yml`
and restarts the stack on the prod VPS. See `.github/workflows/deploy.yml`.

## License

Dataset contributions are released under CC BY-SA 4.0 with documented
per-speaker consent. See [`legal/`](legal/) for the consent text and the
dataset card draft.

## Related

- [`chronicle-data-api`](https://github.com/sessionhelper/chronicle-data-api) — chunk ingest target
- [`chronicle-feeder`](https://github.com/sessionhelper/chronicle-feeder) — E2E test harness feeder bots
- [`sessionhelper-hub/ARCHITECTURE.md`](https://github.com/sessionhelper/sessionhelper-hub/blob/main/ARCHITECTURE.md) — cross-service data flow
- [`sessionhelper-hub/SPEC.md`](https://github.com/sessionhelper/sessionhelper-hub/blob/main/SPEC.md) — OVP program spec (this bot owns G1, the current Phase 0 blocker)
- [`sessionhelper-hub/docs/dave-bot-ecosystem.md`](https://github.com/sessionhelper/sessionhelper-hub/blob/main/docs/dave-bot-ecosystem.md) — DAVE library landscape and collaborator tree
