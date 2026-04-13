# Multi-arch (amd64 + arm64) build via cross-compilation.
#
# We always run the Rust compiler on the native build host ($BUILDPLATFORM)
# and cross-compile to $TARGETPLATFORM. This is ~5-10x faster than running
# rustc under QEMU emulation and avoids the emulator's fragile memory
# behaviour when linking large crates like ring / aws-lc-sys.

FROM --platform=$BUILDPLATFORM rust:1.94-bookworm AS builder

ARG BUILDPLATFORM
ARG TARGETPLATFORM
ARG TARGETARCH

# Build-time deps:
# - cmake  : required by aws-lc-sys (pulled in by rustls via default features)
# - gcc/g++ cross toolchains for the "other" architecture, so we can always
#   link the target regardless of which host arch this image is built on.
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross \
        gcc-x86-64-linux-gnu g++-x86-64-linux-gnu libc6-dev-amd64-cross \
    && rm -rf /var/lib/apt/lists/*

# Map Docker's TARGETARCH ("amd64" / "arm64") to a Rust target triple and
# register the corresponding rustup target + cross linker.
RUN set -eux; \
    case "$TARGETARCH" in \
        amd64) RUST_TARGET=x86_64-unknown-linux-gnu ;; \
        arm64) RUST_TARGET=aarch64-unknown-linux-gnu ;; \
        *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    rustup target add "$RUST_TARGET"; \
    echo "$RUST_TARGET" > /tmp/rust_target

# Tell cargo / cc / cmake which compiler to invoke for each possible target.
# The vars for the "wrong" target are harmless when unused.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CXX_x86_64_unknown_linux_gnu=x86_64-linux-gnu-g++ \
    AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar

WORKDIR /app
COPY voice-capture/Cargo.toml voice-capture/Cargo.lock ./
COPY voice-capture/src/ src/

RUN cargo build --release --target "$(cat /tmp/rust_target)" \
 && mkdir -p /out \
 && cp "target/$(cat /tmp/rust_target)/release/chronicle-bot" /out/chronicle-bot

# Runtime image is target-native (no --platform override) so it ends up as
# $TARGETPLATFORM in the final manifest.
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/chronicle-bot /usr/local/bin/chronicle-bot
COPY voice-capture/assets/ /assets/

CMD ["chronicle-bot"]
