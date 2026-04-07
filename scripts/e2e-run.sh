#!/usr/bin/env bash
# e2e-run.sh — Drive a full multi-speaker E2E recording session.
#
# Orchestrates the collector and feeder fleet to capture a multi-speaker
# session end-to-end. The key sequencing constraint is that feeders must
# join voice ONE AT A TIME with a delay between each join, so the DAVE/MLS
# key exchange completes for each participant before the next one joins.
#
# Why: Discord's DAVE protocol uses MLS (Messaging Layer Security) for
# end-to-end encryption key exchange. Each voice channel join triggers an
# MLS proposal -> commit -> acknowledge cycle. If two participants join
# before the first cycle completes, the MLS commit pipeline collides:
# the collector's pending commit for participant A gets cleared when
# participant B's proposal arrives, causing A's add to be lost from the
# MLS group. The collector then cannot decrypt A's audio (NoDecryptorForUser).
#
# The fix: stagger joins with a delay between each, waiting for the
# collector to confirm DAVE handshake stability before proceeding.
#
# Usage:
#   GUILD_ID=... CHANNEL_ID=... ./scripts/e2e-run.sh
#
# Optional env:
#   COLLECTOR_URL      — default http://127.0.0.1:8010
#   FEEDER_URLS        — comma-separated, default http://127.0.0.1:8003,...:8006
#   JOIN_DELAY_SECS    — seconds between feeder joins (default 5)
#   PLAY_DURATION_SECS — how long to let feeders play before stopping (default 30)

set -euo pipefail

# --- Config ---

GUILD_ID="${GUILD_ID:?GUILD_ID is required}"
CHANNEL_ID="${CHANNEL_ID:?CHANNEL_ID is required}"
COLLECTOR_URL="${COLLECTOR_URL:-http://127.0.0.1:8010}"
PLAY_DURATION_SECS="${PLAY_DURATION_SECS:-30}"

# MLS commit round-trip takes ~1-3 seconds on Discord's voice servers.
# 5 seconds gives comfortable margin for the full proposal -> commit ->
# acknowledge -> update_ratchets cycle to complete before the next join.
JOIN_DELAY_SECS="${JOIN_DELAY_SECS:-5}"

# Default feeder fleet: moe (8003), larry (8004), curly (8005), gygax (8006)
if [[ -z "${FEEDER_URLS:-}" ]]; then
    FEEDER_URLS="http://127.0.0.1:8003,http://127.0.0.1:8004,http://127.0.0.1:8005,http://127.0.0.1:8006"
fi

IFS=',' read -ra FEEDERS <<< "$FEEDER_URLS"
FEEDER_COUNT=${#FEEDERS[@]}

echo "=== E2E multi-speaker recording test ==="
echo "Guild:     $GUILD_ID"
echo "Channel:   $CHANNEL_ID"
echo "Collector: $COLLECTOR_URL"
echo "Feeders:   $FEEDER_COUNT (join delay: ${JOIN_DELAY_SECS}s)"
echo ""

# --- Helper ---

fail() { echo "FAIL: $1" >&2; exit 1; }

http_ok() {
    local code
    code=$(curl -s -o /dev/null -w "%{http_code}" "$@")
    [[ "$code" == "200" ]]
}

# --- Preflight: check all services are healthy ---

echo "--- Preflight checks ---"

echo -n "Collector health... "
http_ok "$COLLECTOR_URL/health" || fail "collector not healthy at $COLLECTOR_URL/health"
echo "OK"

for i in "${!FEEDERS[@]}"; do
    url="${FEEDERS[$i]}"
    echo -n "Feeder $((i+1)) health ($url)... "
    http_ok "$url/health" || fail "feeder not healthy at $url/health"
    echo "OK"
done

echo ""

# --- Step 1: Join feeders one at a time with stagger ---
#
# This is the critical section. Each join triggers an MLS Add proposal on
# the voice server. The collector must process the proposal, create a
# commit, send it back, and have it acknowledged before the next feeder
# joins. The delay gives the full MLS commit cycle time to complete.

echo "--- Joining feeders (staggered, ${JOIN_DELAY_SECS}s between each) ---"

JOINED_FEEDERS=()

cleanup() {
    echo ""
    echo "--- Cleanup: leaving feeders from voice ---"
    for url in "${JOINED_FEEDERS[@]}"; do
        echo -n "  Leaving $url... "
        curl -s -X POST "$url/leave" > /dev/null 2>&1 || true
        echo "done"
    done
}
trap cleanup EXIT

for i in "${!FEEDERS[@]}"; do
    url="${FEEDERS[$i]}"
    echo -n "  Joining feeder $((i+1)) ($url)... "
    response=$(curl -s -w "\n%{http_code}" -X POST \
        -H "Content-Type: application/json" \
        -d "{\"guild_id\": $GUILD_ID, \"channel_id\": $CHANNEL_ID}" \
        "$url/join")
    code=$(echo "$response" | tail -1)
    [[ "$code" == "200" ]] || fail "feeder join failed (HTTP $code): $(echo "$response" | head -1)"
    JOINED_FEEDERS+=("$url")
    echo "OK"

    if [[ $i -lt $((FEEDER_COUNT - 1)) ]]; then
        echo "  Waiting ${JOIN_DELAY_SECS}s for MLS commit cycle to complete..."
        sleep "$JOIN_DELAY_SECS"
    fi
done

# Extra settle time after last join for final MLS epoch to stabilize.
# 10 seconds gives Discord's voice server time to process all feeders'
# MLS proposals sequentially. Without this, the collector's join races
# the last feeder's MLS commit and some speakers' decryptors won't be
# established.
MLS_SETTLE_SECS="${MLS_SETTLE_SECS:-10}"
echo "  Waiting ${MLS_SETTLE_SECS}s for MLS group to stabilize..."
sleep "$MLS_SETTLE_SECS"

echo ""

# --- Step 2: Start recording on the collector ---

echo "--- Starting recording ---"
record_response=$(curl -s -w "\n%{http_code}" -X POST \
    -H "Content-Type: application/json" \
    -d "{\"guild_id\": $GUILD_ID, \"channel_id\": $CHANNEL_ID}" \
    "$COLLECTOR_URL/record")
record_code=$(echo "$record_response" | tail -1)
record_body=$(echo "$record_response" | head -1)

if [[ "$record_code" != "200" ]]; then
    fail "collector /record failed (HTTP $record_code): $record_body"
fi

SESSION_ID=$(echo "$record_body" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_id'])" 2>/dev/null || echo "unknown")
echo "Recording started. Session ID: $SESSION_ID"
echo ""

# --- Step 3: Wait for recording to stabilize ---
#
# The collector's DAVE heal task needs time to check all speakers and
# optionally reconnect. Feeders must NOT transmit during this window —
# if they're already speaking when the collector joins, OP5 is
# edge-triggered away and the health check has a blind spot.
#
# Poll GET /status?guild_id= until stable=true, then tell feeders
# to play. This is the backchannel that replaces the old "sleep and
# hope" approach.

STABLE_TIMEOUT="${STABLE_TIMEOUT:-60}"
echo "--- Waiting for recording to stabilize (up to ${STABLE_TIMEOUT}s) ---"
stable_start=$(date +%s)
while true; do
    status_json=$(curl -sf "$COLLECTOR_URL/status?guild_id=$GUILD_ID" 2>/dev/null || echo "{}")
    is_stable=$(echo "$status_json" | python3 -c "import sys,json; print(json.load(sys.stdin).get('stable', False))" 2>/dev/null || echo "False")
    if [[ "$is_stable" == "True" ]]; then
        elapsed=$(( $(date +%s) - stable_start ))
        echo "  Stable after ${elapsed}s"
        break
    fi
    elapsed=$(( $(date +%s) - stable_start ))
    if [[ $elapsed -ge $STABLE_TIMEOUT ]]; then
        echo "  WARNING: timed out after ${STABLE_TIMEOUT}s, proceeding anyway"
        break
    fi
    sleep 1
done

echo ""

# --- Step 4: Play audio on all feeders ---

echo "--- Playing audio on all feeders ---"
for i in "${!FEEDERS[@]}"; do
    url="${FEEDERS[$i]}"
    echo -n "  Playing on feeder $((i+1)) ($url)... "
    http_ok -X POST "$url/play" || fail "feeder /play failed at $url"
    echo "OK"
done

echo ""
echo "Letting feeders play for ${PLAY_DURATION_SECS}s..."
sleep "$PLAY_DURATION_SECS"

# --- Step 5: Stop recording ---

echo ""
echo "--- Stopping recording ---"
stop_response=$(curl -s -w "\n%{http_code}" -X POST \
    -H "Content-Type: application/json" \
    -d "{\"guild_id\": $GUILD_ID}" \
    "$COLLECTOR_URL/stop")
stop_code=$(echo "$stop_response" | tail -1)

if [[ "$stop_code" != "200" ]]; then
    echo "WARNING: collector /stop returned HTTP $stop_code"
else
    echo "Recording stopped and finalized."
fi

echo ""

# --- Step 6: Wait for session upload to complete ---

echo "--- Waiting for session upload ---"
DATA_API_URL="${DATA_API_URL:-http://127.0.0.1:8001}"
SHARED_SECRET="${SHARED_SECRET:?SHARED_SECRET is required for audio diff}"

# Authenticate
API_TOKEN=$(curl -sf "$DATA_API_URL/internal/auth" \
    -H "Content-Type: application/json" \
    -d "{\"shared_secret\": \"$SHARED_SECRET\", \"service_name\": \"e2e-test\"}" \
    | python3 -c "import sys,json; print(json.load(sys.stdin).get('session_token',''))" 2>/dev/null)

if [[ -z "$API_TOKEN" ]]; then
    echo "WARNING: could not authenticate with data-api, skipping audio diff"
else
    # Wait for status=uploaded (or transcribing/transcribed if worker is fast)
    for i in $(seq 1 30); do
        status=$(curl -sf "$DATA_API_URL/internal/sessions/$SESSION_ID" \
            -H "Authorization: Bearer $API_TOKEN" 2>/dev/null \
            | python3 -c "import sys,json; print(json.load(sys.stdin).get('status',''))" 2>/dev/null)
        if [[ "$status" == "uploaded" || "$status" == "transcribing" || "$status" == "transcribed" ]]; then
            echo "  Session status: $status"
            break
        fi
        sleep 1
    done

    # --- Step 7: Audio waveform evaluation ---

    AUDIO_DIFF="${AUDIO_DIFF:-/home/alex/test-data/audio-diff.py}"
    SOURCE_DIR="${SOURCE_DIR:-/home/alex/test-data/sessions/04-chaotic-tavern/audio}"

    if [[ -f "$AUDIO_DIFF" ]]; then
        echo ""
        echo "--- Audio waveform evaluation ---"
        uv run --with "scipy<1.17" --with numpy --with requests \
            python3 "$AUDIO_DIFF" \
            --session-id "$SESSION_ID" \
            --data-api-url "$DATA_API_URL" \
            --shared-secret "$SHARED_SECRET" \
            --source-dir "$SOURCE_DIR" \
            2>&1 || echo "WARNING: audio-diff failed (non-fatal)"
    else
        echo "  Skipping audio diff ($AUDIO_DIFF not found)"
    fi
fi

echo ""
echo "=== E2E run complete (session: $SESSION_ID) ==="
