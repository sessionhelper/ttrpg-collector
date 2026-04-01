# ttrpg-collector

Discord bot for collecting TTRPG session audio for an open dataset.

## Stack

- Python 3.12+, py-cord (async Discord bot with voice receive)
- pydantic-settings for config
- boto3/aioboto3 for S3-compatible uploads (Hetzner Object Storage)
- structlog for logging
- pytest for testing, ruff for linting
- Docker for deployment, GitHub Actions for CI/CD

## Layout

```
src/collector/
  bot.py              — Entry point, cog loading
  config.py           — Env var config (pydantic-settings)
  cogs/recording.py   — /record, /stop, /status slash commands
  cogs/notes.py       — /notes modal command
  voice/recorder.py   — Join channel, manage AudioSink
  voice/sink.py       — Custom AudioSink writing per-user PCM to disk
  voice/stream_manager.py — Per-user stream tracking, joins/leaves
  consent/manager.py  — Consent state machine
  consent/views.py    — Discord button views for consent
  consent/types.py    — Enums
  storage/s3_upload.py — S3-compatible upload with retry
  storage/local_buffer.py — Temp dir management, orphan recovery
  storage/session_bundle.py — meta.json, consent.json, pii.json assembly
  metadata/           — Session metadata, quality filters
```

## Running locally

```bash
uv sync
cp .env.example .env   # fill in DISCORD_TOKEN + S3 creds
uv run python -m collector.bot
```

## Running with Docker

```bash
docker compose up -d
```

## Testing

```bash
uv run pytest
uv run ruff check src/ tests/
```

## Git Workflow

- **main** — production branch, deploys to VPS on push. Never commit directly.
- **dev** — integration branch. Feature branches merge here first.
- **feature/*** — short-lived branches for individual features. Branch from dev, merge back into dev via PR or `--no-ff` merge.
- When dev is stable, merge dev into main to trigger deploy.

## Deploying

Push to `main` → GitHub Actions runs tests → builds Docker image → pushes to GHCR → SSHs to VPS and restarts.

## Key Decisions

- py-cord voice receive uses C bindings (libopus, libsodium) under the hood
- Raw PCM buffered to disk during recording, converted to WAV on stop
- Consent required before any audio is captured
- User snowflakes pseudonymized in public metadata
- S3 storage is provider-agnostic (Hetzner, Backblaze, MinIO — any S3-compatible endpoint)
