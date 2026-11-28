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
# build.rs MUST be copied: it stages the web SPA into $OUT_DIR/web-embed, and
# (crucially) its mere presence makes cargo set OUT_DIR — without it the
# `include_dir!("$OUT_DIR/web-embed")` in static_files.rs panics with
# MissingVariable{OUT_DIR}. (The real SPA is served at runtime via MIRA_WEB_DIR
# from /app/web; build.rs writes a placeholder embed here, which is fine.)
COPY build.rs ./
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
      unzip \
 && rm -rf /var/lib/apt/lists/* \
 && curl -fsSL "https://github.com/AsamK/signal-cli/releases/download/v${SIGNAL_CLI_VERSION}/signal-cli-${SIGNAL_CLI_VERSION}.tar.gz" \
      | tar -xz -C /opt \
 && ln -sf "/opt/signal-cli-${SIGNAL_CLI_VERSION}/bin/signal-cli" /usr/local/bin/signal-cli

# ── ONNX Runtime (fastembed's in-process fallback) ────────────────────────
# fastembed (`ort-load-dynamic` feature) dlopens libonnxruntime.so at runtime
# whenever the configured embedding endpoint is unreachable — which is the
# default inside a container with no external LM Studio / Ollama. Ship a
# matching ORT build so the fallback path works out of the box. `ORT_DYLIB_PATH`
# (set below) points MIRA at it — its is_onnxruntime_available() check only looks
# at ORT_DYLIB_PATH / ~/.mira/deps, never system loader paths, so without the env
# the bundled lib would be invisible and embeddings would fall back to noop.
#
# Resilience (issue #15) + multi-arch: the artifact is pinned and checksum-verified
# (the build fails loud on a mismatch); the URL + checksum are build-arg overridable
# so an operator can point at any mirror; and a controlled fallback is tried when the
# primary URL is unavailable. For an image that only ever uses an external embedding
# endpoint, `--build-arg ONNXRUNTIME_ENABLED=false` skips the fetch entirely.
#
# Arch-aware: buildx sets TARGETARCH (amd64|arm64). The whole microsoft/onnxruntime
# repo was removed upstream, so both arches are served from OUR mirror
# (deps-onnxruntime-v<ver>): amd64 = Microsoft's linux-x64 .tgz; arm64 = Microsoft's
# build as the PyPI aarch64 .whl (a zip — hence `unzip` above). Values below MUST
# stay in step with deps/manifest.toml (single source of truth): same URLs, sha256s,
# and the lib path inside each archive. After extraction the runtime setup is
# arch-identical (same /usr/local/lib/libonnxruntime.so + ORT_DYLIB_PATH).
ARG ONNXRUNTIME_VERSION=1.20.1
ARG ONNXRUNTIME_ENABLED=true
ARG TARGETARCH
# Overrides (empty → the per-arch default below). ONNXRUNTIME_SHA256 must match
# whatever ONNXRUNTIME_URL points at.
ARG ONNXRUNTIME_URL=
ARG ONNXRUNTIME_FALLBACK_URL=
ARG ONNXRUNTIME_SHA256=
RUN set -eu; \
    if [ "${ONNXRUNTIME_ENABLED}" != "true" ]; then \
      echo "ONNXRUNTIME_ENABLED=${ONNXRUNTIME_ENABLED} — skipping bundled ONNX Runtime (external embedding endpoint only)"; \
    else \
      arch="${TARGETARCH:-}"; [ -n "${arch}" ] || arch="$(dpkg --print-architecture)"; \
      mirror="https://github.com/Vexillon-ai/MIRA/releases/download/deps-onnxruntime-v${ONNXRUNTIME_VERSION}"; \
      case "${arch}" in \
        amd64) file="onnxruntime-linux-x64-${ONNXRUNTIME_VERSION}.tgz"; \
               def_sha="67db4dc1561f1e3fd42e619575c82c601ef89849afc7ea85a003abbac1a1a105"; \
               kind="tar"; \
               libsrc="onnxruntime-linux-x64-${ONNXRUNTIME_VERSION}/lib/libonnxruntime.so.${ONNXRUNTIME_VERSION}"; \
               def_fb="https://github.com/cga-management/third-party-mirror/releases/download/onnxruntime-${ONNXRUNTIME_VERSION}/${file}" ;; \
        arm64) file="onnxruntime-${ONNXRUNTIME_VERSION}-cp312-cp312-manylinux_2_27_aarch64.manylinux_2_28_aarch64.whl"; \
               def_sha="f1f56e898815963d6dc4ee1c35fc6c36506466eff6d16f3cb9848cea4e8c8172"; \
               kind="zip"; \
               libsrc="onnxruntime/capi/libonnxruntime.so.${ONNXRUNTIME_VERSION}"; \
               def_fb="https://files.pythonhosted.org/packages/c5/9d/a42a84e10f1744dd27c6f2f9280cc3fb98f869dd19b7cd042e391ee2ab61/${file}" ;; \
        *)     echo "ERROR: unsupported arch '${arch}' for the bundled ONNX Runtime" >&2; exit 1 ;; \
      esac; \
      url="${ONNXRUNTIME_URL:-${mirror}/${file}}"; \
      fb="${ONNXRUNTIME_FALLBACK_URL:-${def_fb}}"; \
      sha="${ONNXRUNTIME_SHA256:-${def_sha}}"; \
      arc=/tmp/onnxruntime.archive; \
      if ! curl -fSL --retry 3 --retry-delay 2 -o "${arc}" "${url}"; then \
        echo "WARN: primary ONNX Runtime URL unavailable: ${url}" >&2; \
        if [ -n "${fb}" ]; then \
          echo "INFO: trying fallback: ${fb}" >&2; \
          curl -fSL --retry 3 --retry-delay 2 -o "${arc}" "${fb}"; \
        else \
          echo "ERROR: no fallback URL — cannot recover" >&2; exit 1; \
        fi; \
      fi; \
      echo "${sha}  ${arc}" | sha256sum -c -; \
      mkdir -p /opt/ort; \
      if [ "${kind}" = "zip" ]; then unzip -q "${arc}" -d /opt/ort; else tar -xzf "${arc}" -C /opt/ort; fi; \
      cp "/opt/ort/${libsrc}" "/usr/local/lib/libonnxruntime.so.${ONNXRUNTIME_VERSION}"; \
      ln -sf "/usr/local/lib/libonnxruntime.so.${ONNXRUNTIME_VERSION}" /usr/local/lib/libonnxruntime.so; \
      ldconfig; \
      rm -rf /opt/ort "${arc}"; \
    fi

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
    MIRA_WEB_DIR=/app/web \
    ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so

# Bind-mount any host directory here to persist config/state across recreates.
VOLUME ["/data"]
WORKDIR /data
EXPOSE 8080

# Liveness, not readiness. `/api/status` is auth-gated (always 401 → forever
# "unhealthy"). Probe `/livez` — the dependency-free liveness route (0.339.0
# health-endpoint split) that returns 200 whenever the HTTP server is serving,
# without a provider round-trip. This is exactly the container's health question
# ("is MIRA up?"), so a plain 200 check is correct: readiness (provider reachable)
# lives on the separate `/readyz`, which a just-installed or localhost-only
# container often can't satisfy yet and which is not what a HEALTHCHECK wants.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS -o /dev/null http://127.0.0.1:8080/livez

# tini stays PID 1 (clean signal handling); the entrypoint script adds first-run
# onboarding, then execs the CMD (or a `docker compose run` subcommand).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
# `--host 0.0.0.0` overrides the config default of 127.0.0.1 so the
# port-forward from the host actually reaches the server.
CMD ["mira", "--server", "--host", "0.0.0.0", "--port", "8080"]
