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
  OpenAI, OpenRouter, DeepSeek, Gemini) **or** a local OpenAI-compatible server
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

## Next steps

- **[Quickstart](quickstart.md)** — finish first-run setup and have your first
  conversation.
- **[Connect a channel](../guides/connect-a-channel.md)** — reach MIRA on
  Telegram, Signal, or email.
