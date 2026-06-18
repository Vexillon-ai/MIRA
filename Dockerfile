# syntax=docker/dockerfile:1.7
#
# Multi-stage build for MIRA.
#   1. web-builder  — compiles the React SPA in a node image.
#   2. rust-builder — builds the release binary in a rust image.
#   3. runtime      — slim Debian image with JRE + signal-cli + the binary.
#
# Build:   docker build -t mira:local .
# Run:     docker run --rm -p 8080:8080 -v "$PWD/data:/data" mira:local
# Compose: docker compose up -d   (preferred — see docker-compose.yml)

# ── 1. Build the web SPA ──────────────────────────────────────────────────
FROM node:20-bookworm-slim AS web-builder
WORKDIR /web
# Copy lockfiles first so the npm layer caches across source-only changes.
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# ── 2. Build the Rust binary ──────────────────────────────────────────────
# Cargo.toml declares MSRV 1.85, but transitive deps (`time` 0.3.47 et al.)
# now require ≥1.88. Pin to a recent stable so the image rebuilds remain
# reproducible without dragging in untested compiler changes.
FROM rust:1.90-bookworm AS rust-builder

# Native build deps for vendored C/C++ sources:
#   cmake        — audiopus_sys (libopus), whisper-rs-sys (whisper.cpp)
#   clang/libclang-dev — bindgen for whisper-rs-sys + others
#   pkg-config   — surfaced by several -sys crates that probe before falling
#                  back to vendored sources
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      cmake \
      clang \
      libclang-dev \
      pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build
# Copy the full project — the deps trick (Cargo.toml-only stub build) breaks
# with workspaces+lib+bin, and the apt/JRE layers in stage 3 are the slow
# ones anyway. Cargo's own incremental cache handles iterative rebuilds.
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
COPY tests/ ./tests/
COPY config/ ./config/
COPY prompts/ ./prompts/
# Embedded at compile time via include_dir!/include_str! — the build fails
# without these. Copy ONLY the release PUBLIC key from verification/ (never the
# private signing key).
COPY mira-docs/ ./mira-docs/
COPY bundled-skills/ ./bundled-skills/
COPY deps/ ./deps/
COPY verification/release-pubkey.minisign ./verification/release-pubkey.minisign
RUN cargo build --release --locked --bin mira

# ── 3. Runtime image ──────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Pin signal-cli so a broken upstream release can't silently change the
# behaviour of an image rebuild. Override at build time with `--build-arg`.
ARG SIGNAL_CLI_VERSION=0.14.2
# fastembed (`ort-load-dynamic` feature) dlopens libonnxruntime.so at runtime
# whenever the configured embedding endpoint is unreachable — which is the
# default inside a container with no external LM Studio / Ollama. Ship a
# matching ORT build so the fallback path works out of the box.
ARG ONNXRUNTIME_VERSION=1.20.0

ENV DEBIAN_FRONTEND=noninteractive

# JRE for signal-cli, curl for the HEALTHCHECK, ca-certs for outbound HTTPS,
# tini as PID 1 so signals reach the binary cleanly (signal-cli children
# need SIGTERM forwarding for graceful shutdown).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      default-jre-headless \
      tini \
 && curl -fsSL "https://github.com/AsamK/signal-cli/releases/download/v${SIGNAL_CLI_VERSION}/signal-cli-${SIGNAL_CLI_VERSION}.tar.gz" \
      | tar -xz -C /opt \
 && ln -sf "/opt/signal-cli-${SIGNAL_CLI_VERSION}/bin/signal-cli" /usr/local/bin/signal-cli \
 && curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v${ONNXRUNTIME_VERSION}/onnxruntime-linux-x64-${ONNXRUNTIME_VERSION}.tgz" \
      | tar -xz -C /opt \
 && cp "/opt/onnxruntime-linux-x64-${ONNXRUNTIME_VERSION}/lib/libonnxruntime.so.${ONNXRUNTIME_VERSION}" /usr/local/lib/ \
 && ln -sf "/usr/local/lib/libonnxruntime.so.${ONNXRUNTIME_VERSION}" /usr/local/lib/libonnxruntime.so \
 && ldconfig \
 && rm -rf "/opt/onnxruntime-linux-x64-${ONNXRUNTIME_VERSION}" /var/lib/apt/lists/*

COPY --from=rust-builder /build/target/release/mira /usr/local/bin/mira
COPY --from=web-builder  /web/dist                  /app/web
# First-run onboarding wrapper (interactive `docker compose run mira setup`, or
# unattended from MIRA_SETUP_* env on first boot). See the script header.
COPY scripts/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# `~` resolves under HOME, so default config + data dirs land in the bind-
# mounted /data volume. MIRA_WEB_DIR is read by the static-files resolver
# in the server.
ENV HOME=/data \
    MIRA_WEB_DIR=/app/web

# Bind-mount any host directory here to persist config/state across recreates.
VOLUME ["/data"]
WORKDIR /data
EXPOSE 8080

# Liveness, not readiness. `/api/status` is auth-gated (always 401 → forever
# "unhealthy"), and `/health` is provider-gated (503 until the LLM endpoint is
# reachable — which a just-installed or localhost-only container often isn't yet).
# Treat the container as healthy whenever the HTTP server answers `/health` at
# all: 200 (provider ready) OR 503 (up, provider not yet reachable). Only a
# refused/absent connection — i.e. the server isn't serving — is unhealthy.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/health) \
   && { [ "$code" = 200 ] || [ "$code" = 503 ]; }

# tini stays PID 1 (clean signal handling); the entrypoint script adds first-run
# onboarding, then execs the CMD (or a `docker compose run` subcommand).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
# `--host 0.0.0.0` overrides the config default of 127.0.0.1 so the
# port-forward from the host actually reaches the server.
CMD ["mira", "--server", "--host", "0.0.0.0", "--port", "8080"]
