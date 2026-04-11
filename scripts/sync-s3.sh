#!/usr/bin/env bash
# Sync S3 bucket contents to a local directory.
# Only downloads files not already present locally.
# Converts any .pcm files to .flac after download.
#
# Usage: ./sync-s3.sh /path/to/local/dir
#
# Reads S3 credentials from .env or environment variables:
#   S3_ENDPOINT, S3_ACCESS_KEY, S3_SECRET_KEY, S3_BUCKET

set -euo pipefail

LOCAL_DIR="${1:-.}"

# Load .env if it exists
SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
if [ -f "$SCRIPT_DIR/.env" ]; then
    export $(grep -v '^#' "$SCRIPT_DIR/.env" | grep -E '^S3_' | xargs)
fi

: "${S3_ENDPOINT:?Set S3_ENDPOINT}"
: "${S3_ACCESS_KEY:?Set S3_ACCESS_KEY}"
: "${S3_SECRET_KEY:?Set S3_SECRET_KEY}"
: "${S3_BUCKET:=ovp-dataset-raw}"

export AWS_ACCESS_KEY_ID="$S3_ACCESS_KEY"
export AWS_SECRET_ACCESS_KEY="$S3_SECRET_KEY"

mkdir -p "$LOCAL_DIR"

echo "Syncing s3://$S3_BUCKET → $LOCAL_DIR"
aws s3 sync \
    "s3://$S3_BUCKET/" \
    "$LOCAL_DIR/" \
    --endpoint-url "$S3_ENDPOINT" \
    --size-only

# Convert any PCM files to FLAC
CONVERTED=0
while IFS= read -r -d '' pcm; do
    flac="${pcm%.pcm}.flac"
    if [ ! -f "$flac" ]; then
        echo "Converting $(basename "$pcm") → $(basename "$flac")"
        ffmpeg -y -f s16le -ar 48000 -ac 2 -i "$pcm" -ac 1 "$flac" 2>/dev/null
        CONVERTED=$((CONVERTED + 1))
    fi
done < <(find "$LOCAL_DIR" -name "*.pcm" -print0)

echo "Done. $CONVERTED PCM files converted to FLAC."
