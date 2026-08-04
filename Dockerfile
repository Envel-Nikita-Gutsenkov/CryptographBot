# ── Builder ────────────────────────────────────────────────────────────────────
FROM rust:slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
# Cache deps layer
RUN mkdir src && echo 'fn main(){}' > src/main.rs \
    && cargo build --release 2>/dev/null || true \
    && rm -rf src

COPY src ./src
RUN rm -f src/models.rs && touch src/main.rs && cargo build --release --locked

# ── Runtime ────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates procps \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -s /bin/false cryptograph

WORKDIR /app
COPY --from=builder /build/target/release/cryptograph /app/cryptograph
COPY models.json /app/models.json

RUN mkdir -p /app/data \
    && chown -R cryptograph:cryptograph /app
USER cryptograph

ENV RUST_LOG=info
ENTRYPOINT ["/app/cryptograph"]
