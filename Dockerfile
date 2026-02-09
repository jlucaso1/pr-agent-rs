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

FROM gcr.io/distroless/cc-debian12

COPY --from=builder /app/target/release/pr-agent-rs /usr/local/bin/pr-agent-rs

USER nonroot

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["pr-agent-rs", "health"]

ENTRYPOINT ["/usr/local/bin/pr-agent-rs"]
CMD ["serve"]
