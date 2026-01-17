FROM rust:bookworm AS builder

WORKDIR /app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build release binary with S3 support
RUN cargo build --release --package hashtree-cli --features s3

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/htree /usr/local/bin/htree

WORKDIR /data

EXPOSE 8080

CMD ["htree", "start", "--addr", "0.0.0.0:8080", "--data-dir", "/data"]
