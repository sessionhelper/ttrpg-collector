#!/usr/bin/env bash
#
# Generate the four feeder-bot voice files for the E2E harness using Piper TTS.
#
# Voices actually used:
#   moe    — en_US-lessac-medium
#   larry  — en_US-ryan-medium
#   curly  — en_US-kusal-medium
#   gygax  — en_GB-alan-medium
#
# Models are pulled from https://huggingface.co/rhasspy/piper-voices. They live
# in ./piper-models/ (gitignored) — each is ~60 MB. The output WAVs are small
# enough to check into git.
#
# Prereq: `piper` CLI on PATH. Install with:
#   uv tool install piper-tts
#   # or: pipx install piper-tts
#
# Re-running is safe: models are only re-downloaded if missing.

set -euo pipefail

cd "$(dirname "$0")"

MODELS_DIR="piper-models"
mkdir -p "$MODELS_DIR"

HF_BASE="https://huggingface.co/rhasspy/piper-voices/resolve/main"

# model_key|hf_path|line|output
VOICES=(
  "en_US-lessac-medium|en/en_US/lessac/medium|Why I oughta! Pick a card, any card.|moe.wav"
  "en_US-ryan-medium|en/en_US/ryan/medium|Moe, Moe, Moe — what do I do now?|larry.wav"
  "en_US-kusal-medium|en/en_US/kusal/medium|Nyuk nyuk nyuk! I'm a victim of soicumstance!|curly.wav"
  "en_GB-alan-medium|en/en_GB/alan/medium|You enter a dimly-lit tavern. The bartender eyes you suspiciously. Roll for perception.|gygax.wav"
)

download_if_missing() {
  local url="$1"
  local dest="$2"
  if [[ ! -s "$dest" ]]; then
    echo "  downloading $(basename "$dest")"
    curl -sSL -o "$dest" "$url"
  fi
}

count=0
for entry in "${VOICES[@]}"; do
  IFS='|' read -r model hf_path line output <<< "$entry"
  echo "→ $output ($model)"

  onnx="$MODELS_DIR/${model}.onnx"
  json="$MODELS_DIR/${model}.onnx.json"

  download_if_missing "$HF_BASE/$hf_path/${model}.onnx" "$onnx"
  download_if_missing "$HF_BASE/$hf_path/${model}.onnx.json" "$json"

  echo "$line" | piper --model "$onnx" --config "$json" --output-file "$output"
  count=$((count + 1))
done

echo "generated $count wavs"
