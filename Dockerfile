# Multi-stage Dockerfile.
#
# Two final targets:
#   ttrpg-collector (default) — prod collector binary, NO e2e-harness feature
#   feeder-bot                — dev-only feeder bot for E2E tests, requires
#                               the `e2e-harness` feature. Never ship this to
#                               prod; it only exists so the test harness can
#                               drive simulated participants into voice
#                               channels.
#
# Select the target at build time:
#   docker build --target ttrpg-collector -t ttrpg-collector:dev .
#   docker build --target feeder-bot      -t ttrpg-collector-feeder:dev .

FROM rust:1.94-bookworm AS builder-base
RUN apt-get update && apt-get install -y cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY voice-capture/Cargo.toml voice-capture/Cargo.lock ./
COPY voice-capture/src/ src/

# Collector build stage — no features, safe for prod.
FROM builder-base AS builder-collector
RUN cargo build --release --bin ttrpg-collector

# Feeder build stage — e2e-harness feature, dev only.
FROM builder-base AS builder-feeder
RUN cargo build --release --bin feeder-bot --features e2e-harness

# --- Runtime images ---

FROM debian:bookworm-slim AS ttrpg-collector
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder-collector /app/target/release/ttrpg-collector /usr/local/bin/ttrpg-collector
COPY voice-capture/assets/ /assets/
CMD ["ttrpg-collector"]

FROM debian:bookworm-slim AS feeder-bot
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder-feeder /app/target/release/feeder-bot /usr/local/bin/feeder-bot
COPY voice-capture/assets/ /assets/
CMD ["feeder-bot"]
