# syntax=docker/dockerfile:1.7

FROM rust:bookworm AS deps

WORKDIR /app

# Pre-build the dependency graph from manifests and placeholder sources. Source
# edits then invalidate only the final crate build, while third-party deps stay
# warm in the shared BuildKit target cache.
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor

RUN --mount=type=bind,source=crates,target=/source-crates,readonly \
    --mount=type=cache,id=hashtree-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=hashtree-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=hashtree-target,target=/app/target,sharing=locked \
    set -eux; \
    mkdir -p crates; \
    for manifest in /source-crates/*/Cargo.toml; do \
        crate="$(basename "$(dirname "$manifest")")"; \
        mkdir -p "crates/$crate/src/bin" "crates/$crate/benches"; \
        cp "$manifest" "crates/$crate/Cargo.toml"; \
        printf 'pub fn __docker_dependency_cache_placeholder() {}\n' > "crates/$crate/src/lib.rs"; \
        if [ -f "/source-crates/$crate/src/main.rs" ]; then \
            printf 'fn main() {}\n' > "crates/$crate/src/main.rs"; \
        fi; \
        if [ -f "/source-crates/$crate/src/git_remote.rs" ]; then \
            printf 'fn main() {}\n' > "crates/$crate/src/git_remote.rs"; \
        fi; \
        for bin in /source-crates/"$crate"/src/bin/*.rs; do \
            [ -e "$bin" ] || continue; \
            printf 'fn main() {}\n' > "crates/$crate/src/bin/$(basename "$bin")"; \
        done; \
        for bench in /source-crates/"$crate"/benches/*.rs; do \
            [ -e "$bench" ] || continue; \
            printf 'fn main() {}\n' > "crates/$crate/benches/$(basename "$bench")"; \
        done; \
        if [ -f "/source-crates/$crate/build.rs" ]; then \
            cp "/source-crates/$crate/build.rs" "crates/$crate/build.rs"; \
        fi; \
    done; \
    cargo build --locked --release --package hashtree-cli --features s3 && \
    cargo build --locked --release --package hashtree-cashu-cli

FROM deps AS builder

COPY crates ./crates

# Build release binaries with persistent BuildKit caches. The target directory
# is cache-mounted, so copy final artifacts to /out before the layer commits.
# The deps stage builds placeholder local crates into the shared target cache.
# Touch copied workspace sources so Cargo cannot reuse a placeholder htree binary
# when Docker preserves older source mtimes.
RUN --mount=type=cache,id=hashtree-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=hashtree-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=hashtree-target,target=/app/target,sharing=locked \
    find crates -type f \( -name '*.rs' -o -name 'Cargo.toml' -o -name 'build.rs' \) -exec touch {} + && \
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
