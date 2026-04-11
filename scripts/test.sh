#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "Starting test services..."
docker compose -f "$PROJECT_DIR/docker-compose.test.yml" up -d --wait

# Wait a moment for Postgres to accept connections
echo "Waiting for Postgres to be ready..."
for i in {1..30}; do
    if pg_isready -h localhost -p 5433 -U ttrpg -q 2>/dev/null; then
        break
    fi
    sleep 1
done

# Create test bucket in MinIO (ignore error if already exists)
echo "Creating test bucket in MinIO..."
aws --endpoint-url http://localhost:9000 s3 mb s3://ovp-dataset-test 2>/dev/null || true

echo "Running tests..."
cd "$PROJECT_DIR/voice-capture"
DATABASE_URL=postgres://ttrpg:test@localhost:5433/ttrpg_test \
S3_ACCESS_KEY=minioadmin \
S3_SECRET_KEY=minioadmin \
S3_BUCKET=ovp-dataset-test \
S3_ENDPOINT=http://localhost:9000 \
cargo test -- --test-threads=1

echo "Stopping test services..."
docker compose -f "$PROJECT_DIR/docker-compose.test.yml" down

echo "Done."
