# chronicle-bot

Discord bot that records TTRPG voice sessions for an open audio transcription dataset.

Records per-speaker audio tracks with explicit consent, uploads raw PCM chunks through the Data API during recording, for eventual release on HuggingFace under CC BY-SA 4.0.

## Building

```bash
cd voice-capture
cargo build --release
```

Requires `cmake` (for Opus). No `ffmpeg` needed.

## Running

```bash
cp .env.example .env
# Edit .env — add DISCORD_TOKEN and DATA_API_SHARED_SECRET
cd voice-capture
cargo run --release
```

## Bot Commands

| Command | Description |
|---------|-------------|
| `/record` | Start recording the voice channel you're in |
| `/stop` | Stop the current recording |

## How It Works

1. GM runs `/record` in a text channel while in a voice channel
2. Bot posts a consent embed — each player clicks Accept or Decline
3. Recording starts after everyone responds (quorum met)
4. Bot joins voice, captures per-user audio via DAVE E2EE
5. 5MB raw PCM chunks are uploaded to the Data API during recording
6. GM runs `/stop` (or channel empties for 30s) — session finalized

## License

Dataset contributions are released under CC BY-SA 4.0.
