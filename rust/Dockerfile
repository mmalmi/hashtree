# syntax=docker/dockerfile:1.7

FROM rust:bookworm AS builder

WORKDIR /app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor

# Build release binaries with persistent BuildKit caches. The target directory
# is cache-mounted, so copy final artifacts to /out before the layer commits.
RUN --mount=type=cache,id=hashtree-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=hashtree-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=hashtree-target,target=/app/target \
    cargo build --locked --release --package hashtree-cli --features s3 && \
    cargo build --locked --release --package hashtree-cashu-cli && \
    mkdir -p /out && \
    cp /app/target/release/htree /out/htree && \
    cp /app/target/release/htree-cashu /out/htree-cashu

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/htree /usr/local/bin/htree
COPY --from=builder /out/htree-cashu /usr/local/bin/htree-cashu

WORKDIR /data

EXPOSE 8080

CMD ["htree", "start", "--addr", "0.0.0.0:8080", "--data-dir", "/data"]
