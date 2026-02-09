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

# Minimal runtime: bookworm-slim (~27 MB) with curl for Coolify health checks.
# Matches the builder's glibc version. Has /bin/sh required by Coolify's
# health check mechanism (it runs curl inside the container).
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --no-create-home appuser

COPY --from=builder /app/target/release/pr-agent-rs /usr/local/bin/pr-agent-rs

USER appuser

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["curl", "-sf", "http://localhost:3000/"]

ENTRYPOINT ["/usr/local/bin/pr-agent-rs"]
CMD ["serve"]
