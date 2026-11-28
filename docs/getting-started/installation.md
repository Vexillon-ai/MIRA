---
title: Install MIRA
description: Get MIRA running on your own machine — one-line installer, Docker, or a manual binary.
sidebar:
  order: 1
---

This guide gets MIRA running on your own machine. Pick the method that suits
you — the **one-line installer** is the quickest for most people; **Docker** is
great if you already run containers.

## Before you start

You'll need:

- **A machine to run it on** — Linux, macOS, Windows, or anywhere Docker runs. A
  small always-on box (a home server, a mini PC, a VPS) is ideal since MIRA is a
  long-running service.
- **An LLM provider** — either an API key for a hosted provider (Anthropic,
  OpenAI, OpenCode Go, OpenRouter, DeepSeek, Gemini) **or** a local
  OpenAI-compatible server
  (LM Studio, Ollama, llama.cpp). You'll enter this during setup; you can change
  or add providers later.

That's it — MIRA bundles everything else (web UI, database, voice models are
fetched on demand).

## Option A — One-line installer (recommended)

On **Linux or macOS**, run:

```bash
curl -fsSL https://get.vexillon.ai/install.sh | sh
```

On **Windows** (PowerShell):

```powershell
irm https://get.vexillon.ai/install.ps1 | iex
```

The installer:

1. Detects your platform and downloads the matching signed release.
2. Verifies the download's checksum and puts the `mira` binary on your `PATH`
   (under `~/.local/bin`, no `sudo` needed).
3. Runs the guided **`mira setup`** wizard — you'll create an admin account,
   choose your LLM provider, and set a couple of security options.
4. Runs **`mira install`** to register MIRA as a background service so the OS
   keeps it running and restarts it on reboot.
5. Opens your browser to the web UI to finish up.

When it's done, MIRA is running at **http://localhost:8080**. Continue with the
**[Quickstart](quickstart.md)**.

> **Pin a version** by passing `--version`:
> `curl -fsSL https://get.vexillon.ai/install.sh | sh -s -- --version X.Y.Z`

## Option B — Docker

MIRA ships a `Dockerfile` and a `docker-compose.yml`. From a checkout of the
repository:

```bash
docker compose up -d
```

Then open **http://localhost:8080** and complete onboarding in the browser. To
use a different host port, set `MIRA_PORT`:

```bash
MIRA_PORT=9090 docker compose up -d
```

The compose file mounts `./data` into the container — that directory holds your
config, databases, and logs. **Back up `./data` to back up your whole install.**
Docker is the supervisor here: `restart: unless-stopped` means MIRA comes back
after a reboot and honours the **Restart** button in the web UI.

### Prebuilt signed image (GHCR)

Rather than build locally, you can pull the official image, published to the
GitHub Container Registry on each release and **signed with cosign** under the
`vexillon.ai` identity. It's **multi-arch** — `linux/amd64` and `linux/arm64`, so
Docker pulls the right one automatically on an Intel/AMD box, an Apple Silicon Mac,
or ARM cloud/Raspberry Pi:

```bash
docker pull ghcr.io/vexillon-ai/mira:latest        # or a specific :v0.X.Y
docker run --rm -p 8080:8080 -v "$PWD/data:/data" ghcr.io/vexillon-ai/mira:latest
```

Verify the signature before you run it (recommended). With
[cosign](https://docs.sigstore.dev/cosign/installation/) installed and the
project's public key (`verification/cosign.pub`, from a release checkout):

```bash
cosign verify --key verification/cosign.pub ghcr.io/vexillon-ai/mira:latest
```

A valid signature confirms the image was built and published by the MIRA release
pipeline from the public source and hasn't been swapped in the registry.

### Building the image yourself

The image build bundles a copy of ONNX Runtime (for in-process embeddings when
no external embedding endpoint is configured). The download is pinned and
checksum-verified; if you build behind a mirror or the upstream is unavailable,
override the source without editing the `Dockerfile`:

```bash
# Point at your own mirror (set the matching checksum):
docker build \
  --build-arg ONNXRUNTIME_URL=https://your-mirror/onnxruntime-linux-x64-1.20.1.tgz \
  --build-arg ONNXRUNTIME_SHA256=<sha256> \
  -t mira:local .

# Or skip it entirely if you only ever use an external embedding endpoint
# (LM Studio / Ollama):
docker build --build-arg ONNXRUNTIME_ENABLED=false -t mira:local .
```

## Option C — Manual binary

If you'd rather not pipe a script to your shell, do the same steps by hand:

1. Download the release for your platform and put the `mira` binary somewhere on
   your `PATH` (e.g. `~/.local/bin/mira`), then make it executable
   (`chmod +x`).
2. Run the setup wizard:
   ```bash
   mira setup
   ```
   It walks you through the admin account, your LLM provider, and security.
3. Register the background service so the OS supervises it:
   ```bash
   mira install
   ```
4. Open **http://localhost:8080**.

> Prefer a system-wide service instead of a per-user one? Use
> `mira install --system` (Linux).

## Option D — Build from source

Most people should use a prebuilt release (Option A or C) — it's signed and
needs no toolchain. Build from source only if you're hacking on MIRA or need a
platform we don't publish a binary for.

### System prerequisites

MIRA links a few vendored C/C++ libraries (libopus for voice, whisper.cpp for
STT, and others), so a source build needs a C/C++ toolchain and a handful of
development libraries in addition to a [Rust toolchain](https://rustup.rs) and
[Node.js](https://nodejs.org) (for the web UI). Install them with your package
manager first — otherwise the build fails partway through with an opaque error
from a `-sys` crate:

| Prerequisite | Debian / Ubuntu | Fedora / RHEL | macOS (Homebrew) |
| --- | --- | --- | --- |
| C/C++ compiler | `build-essential` | `gcc gcc-c++ make` | Xcode CLT (`xcode-select --install`) |
| CMake | `cmake` | `cmake` | `cmake` |
| Clang + libclang (bindgen) | `clang libclang-dev` | `clang clang-devel` | `llvm` |
| pkg-config | `pkg-config` | `pkg-config` | `pkg-config` |
| Opus codec (voice) | `libopus-dev` | `libopus-devel` | `opus` |
| eSpeak NG (TTS) | `espeak-ng` | `espeak-ng` | `espeak-ng` |

For example, on Debian/Ubuntu:

```bash
sudo apt-get install -y build-essential cmake clang libclang-dev \
  pkg-config libopus-dev espeak-ng
```

### Build

```bash
# Web UI (embedded into the binary at build time)
cd web && npm ci && npm run build && cd ..

# Release binary
cargo build --release --bin mira
```

The binary lands at `target/release/mira`; from there follow Option C to run
`mira setup` and `mira install`.

> **Voice (`--features kokoro`)** pulls in the `ort` crate, which downloads a
> native ONNX Runtime library **at build time**. That build therefore needs
> network access to the ONNX Runtime release (or a mirror). If you build behind
> a proxy or offline, point the `ort` crate at a pre-downloaded copy with its
> `ORT_LIB_LOCATION` environment variable, or omit the `kokoro` feature.

## Choosing where your data lives

By default MIRA stores everything under `~/.mira/data`. To put it elsewhere — a
backed-up volume or an external disk — choose the location during `mira setup`,
or pass `--data-dir /path/to/data`. `mira install` pins your choice into the
service so it survives restarts.

## Updating

```bash
mira upgrade
```

This fetches the latest signed release and swaps the binary in place; the
service restarts itself. (You can also update from the web UI, Settings →
Server.)

To remove MIRA completely — the service, the binary, and your data — see
**[Uninstall MIRA](uninstall.md)**.

## Next steps

- **[Quickstart](quickstart.md)** — finish first-run setup and have your first
  conversation.
- **[Connect a channel](../guides/connect-a-channel.md)** — reach MIRA on
  Telegram, Signal, or email.
