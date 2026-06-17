# syntax=docker/dockerfile:1

# ---- Build stage ----
# rust:1-bookworm ships a full C toolchain, which rusqlite's `bundled`
# feature needs to compile SQLite from source. reqwest/tokio-tungstenite
# use rustls, so no OpenSSL dev headers are required.
FROM rust:1-bookworm AS builder
WORKDIR /app

# Copy manifests and source, then build the release binary. build.rs captures
# the git SHA / build timestamp; git is absent in this stage, so it falls back
# to "unknown" for the SHA, which is fine.
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo build --release --locked --bin copilot-api

# ---- Runtime stage ----
# debian:bookworm-slim matches the glibc/OpenSSL-CA runtime the binary was
# linked against. We avoid a static musl build because rusqlite (bundled) and
# ring make musl cross-compilation fiddly; a slim glibc image is leaner to keep
# correct and still small.
FROM debian:bookworm-slim AS runtime

# ca-certificates for outbound TLS to GitHub/Copilot; curl for the healthcheck.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/copilot-api /usr/local/bin/copilot-api

# The server persists GitHub tokens / config under a home dir. By default this
# is $HOME/.local/share/copilot-api, but COPILOT_API_HOME can relocate it.
# We point it at /data and expose that as a volume so state survives restarts.
ENV COPILOT_API_HOME=/data
RUN mkdir -p /data
VOLUME ["/data"]

# The default listen port; COPILOT_API_PORT overrides it for both the server
# (via the clap env binding) and the healthcheck below.
ENV COPILOT_API_PORT=4141
EXPOSE 4141

# Probe /readyz (ready only once a Copilot token and the model list are loaded)
# rather than / (always 200), and derive the port from COPILOT_API_PORT so a
# remapped port stays in sync with the server.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS "http://localhost:${COPILOT_API_PORT}/readyz" || exit 1

ENTRYPOINT ["copilot-api"]
CMD ["start"]
