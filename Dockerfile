FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config cmake perl gcc g++ && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Cache dependencies: copy manifests and build a dummy project first.
# This layer is only invalidated when Cargo.toml/Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Now copy real source and rebuild (only recompiles our crate, deps are cached)
COPY src/ src/
COPY settings/ settings/
RUN cargo build --release

# Distroless: ~20 MB base, no shell, no package manager, minimal attack surface.
# cc-debian12 includes glibc (matching bookworm builder) but nothing else.
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /app/target/release/pr-agent-rs /usr/local/bin/pr-agent-rs

EXPOSE 3000

# Use the binary's built-in health subcommand (no curl/wget needed).
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/pr-agent-rs", "health"]

ENTRYPOINT ["/usr/local/bin/pr-agent-rs"]
CMD ["serve"]
