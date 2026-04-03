FROM rust:1.94-bookworm AS builder

RUN apt-get update && apt-get install -y cmake

WORKDIR /app
COPY voice-capture/Cargo.toml voice-capture/Cargo.lock ./
COPY voice-capture/src/ src/
COPY voice-capture/migrations/ migrations/
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ttrpg-collector /usr/local/bin/ttrpg-collector
COPY voice-capture/assets/ /assets/

CMD ["ttrpg-collector"]
