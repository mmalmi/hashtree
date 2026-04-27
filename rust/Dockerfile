# syntax=docker/dockerfile:1.7

FROM rust:bookworm AS builder
ARG BUILD_CASHU=0

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
COPY crates ./crates

# Build release binaries with persistent BuildKit caches. Even when Docker
# invalidates this layer for source edits, Cargo's target cache keeps unchanged
# dependencies and local crates warm.
RUN --mount=type=cache,id=hashtree-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=hashtree-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=hashtree-target,target=/app/target,sharing=locked \
    cargo build --locked --release --package hashtree-cli --features s3 && \
    if [ "$BUILD_CASHU" = "1" ]; then \
        cargo build --locked --release --package hashtree-cashu-cli; \
    fi && \
    mkdir -p /out && \
    cp /app/target/release/htree /out/htree && \
    if [ "$BUILD_CASHU" = "1" ]; then \
        cp /app/target/release/htree-cashu /out/htree-cashu; \
    fi

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/htree /usr/local/bin/htree

WORKDIR /data

EXPOSE 8080

CMD ["htree", "start", "--addr", "0.0.0.0:8080", "--data-dir", "/data"]
