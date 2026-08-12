# syntax=docker/dockerfile:1
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
# BuildKit cache mounts. Without them every build recompiled all 433 crates from zero —
# and one of them, libduckdb-sys, is a C++ amalgamation that alone accounts for ~31 of the
# 36 minutes. It does not change when our Rust does, so recompiling it per push was pure
# waste. Cached: the downloaded crates, the git checkouts, and `target/`.
#
# WHY THE COPY AT THE END: a cache mount is not part of the image layer, so anything left
# in `/src/target` is invisible to `COPY --from=builder`. The binary has to be lifted out
# to a real path inside the same RUN, while the mount is still there.
#
# `sharing=locked` because two concurrent builds writing one cargo dir corrupt it.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release -p trx64-daemon \
    && strip target/release/trx64-daemon \
    && mkdir -p /out \
    && cp target/release/trx64-daemon /out/trx64-daemon

# ── runtime ─────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
# Product version — keep in lockstep with [workspace.package] version in Cargo.toml,
# and tag the image with the same number (wl-trx64:<version>, plus :latest).
# The WS protocol epoch is SEPARATE (see RUNTIME_VERSION in the daemon).
ARG TRX64_VERSION=0.3.2
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

COPY --from=builder /out/trx64-daemon /usr/local/bin/trx64-daemon
# ROMs are NOT baked in (Spec 801 §A). They are Commodore's property, so an image that
# carries them cannot be published — not to a registry, not as a release artifact. The
# operator supplies them once, read-only, at the path rom_dir() already probes first:
#
#   -v <host>/roms:/opt/trx64/resources/roms:ro
#
# Six files, ~68 KB. Normally a copy of a set the operator already owns rather than a
# download — no third party is involved and nothing has to be reachable at boot.
RUN mkdir -p /opt/trx64/resources/roms && chown -R trx64:trx64 /opt/trx64

ENV C64RE_ROOT=/opt/trx64 \
    TRX64_PORT=4340 \
    TRX64_BIND=0.0.0.0
EXPOSE 4340
# `/play` = shared media (ro from the consumer's side); `/state` = optional undumps/
# checkpoints; the ROM mount is required for the machine to boot at all.
VOLUME ["/play", "/state", "/opt/trx64/resources/roms"]
USER trx64
WORKDIR /state

# Emulator VIEW = the DEFAULT streaming daemon (A/V push + auto-run on connect). NOT
# --headless (that disables the frames the browser canvas needs — Spec 799 §3/§4.2 said
# --headless from the pre-Spec-767 era when --stream was opt-in; it is now the default).
# Sessions are created over WS (session/create), not via a start flag.
# Bind 0.0.0.0 (TRX64_BIND) so the sibling web-app container can reach the daemon over the
# bridge — the local-127.0.0.1 default would only be reachable from inside this container.
ENTRYPOINT ["/bin/sh", "-c", "exec /usr/local/bin/trx64-daemon --port ${TRX64_PORT:-4340} --bind ${TRX64_BIND:-127.0.0.1}"]
