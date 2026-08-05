# wl-trx64 — containerized TRX64 emulator daemon (Spec 799).
#
# One container = one trx64-daemon process = one live C64 (CLAUDE.md "One Machine Per
# Process"). Streams BIN_VIC frames + audio + the JSON-RPC monitor over ONE WebSocket.
# Consumer writes .crt/.d64 into the shared /play volume and drives the daemon over WS.
#
# Build (Apple Silicon host → amd64 for QNAP Container Station):
#   container build --arch amd64 -f docker/Dockerfile -t wl-trx64:dev .
# (Docker equivalent: docker buildx build --platform linux/amd64 -f docker/Dockerfile .)
#
# NOTE the build context is the REPO ROOT (`.`), not docker/, so the workspace sources
# AND docker/roms/ are both reachable. docker/.dockerignore keeps the context small.

# ── builder ─────────────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder
# g++ compiles the vendored reSID C++ (crates/trx64-core/vendor/resid via build.rs).
# The daemon links reSID statically and does NOT use cpal/ALSA (that is trx64-cli only),
# so no libasound here or in the runtime.
RUN apt-get update \
    && apt-get install -y --no-install-recommends g++ \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p trx64-daemon \
    && strip target/release/trx64-daemon

# ── runtime ─────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
# Product version — keep in lockstep with [workspace.package] version in Cargo.toml,
# and tag the image with the same number (wl-trx64:<version>, plus :latest).
# The WS protocol epoch is SEPARATE (see RUNTIME_VERSION in the daemon).
ARG TRX64_VERSION=0.1.0
LABEL org.opencontainers.image.title="wl-trx64" \
      org.opencontainers.image.description="TRX64 C64 runtime daemon (WS on 4340)" \
      org.opencontainers.image.version="${TRX64_VERSION}" \
      org.opencontainers.image.licenses="GPL-3.0-or-later"
# libstdc++6 + libgcc: the reSID C++ runtime the statically-linked daemon still needs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends libstdc++6 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 -s /usr/sbin/nologin trx64 \
    && mkdir -p /play /state \
    && chown trx64:trx64 /play /state

COPY --from=builder /src/target/release/trx64-daemon /usr/local/bin/trx64-daemon
# ROMs (Commodore IP — baked, gitignored, LAN-only per Spec 799 §9.1). The daemon's
# rom_dir() resolves C64RE_ROOT/resources/roms first (see main.rs rom_dir cross-platform).
COPY docker/roms /opt/trx64/resources/roms

ENV C64RE_ROOT=/opt/trx64 \
    TRX64_PORT=4340 \
    TRX64_BIND=0.0.0.0
EXPOSE 4340
# `/play` = shared media (ro from the consumer's side); `/state` = optional undumps/checkpoints.
VOLUME ["/play", "/state"]
USER trx64
WORKDIR /state

# Emulator VIEW = the DEFAULT streaming daemon (A/V push + auto-run on connect). NOT
# --headless (that disables the frames the browser canvas needs — Spec 799 §3/§4.2 said
# --headless from the pre-Spec-767 era when --stream was opt-in; it is now the default).
# Sessions are created over WS (session/create), not via a start flag.
# Bind 0.0.0.0 (TRX64_BIND) so the sibling web-app container can reach the daemon over the
# bridge — the local-127.0.0.1 default would only be reachable from inside this container.
ENTRYPOINT ["/bin/sh", "-c", "exec /usr/local/bin/trx64-daemon --port ${TRX64_PORT:-4340} --bind ${TRX64_BIND:-127.0.0.1}"]
