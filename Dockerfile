FROM rust:1.94-bookworm AS builder

RUN apt-get update && apt-get install -y cmake && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY voice-capture/Cargo.toml voice-capture/Cargo.lock ./
COPY voice-capture/src/ src/
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/chronicle-bot /usr/local/bin/chronicle-bot
COPY voice-capture/assets/ /assets/

CMD ["chronicle-bot"]
