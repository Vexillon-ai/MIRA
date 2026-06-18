---
title: Command-line reference
description: The mira command — running modes, setup, service control, upgrades, and benchmarking, with their flags.
sidebar:
  order: 2
---

MIRA is a single binary, `mira`. Run it with no arguments to start the
interactive TUI; pass a subcommand to set up, install, control, or upgrade the
service. This page lists the user-facing commands and their common flags.

```
mira [GLOBAL OPTIONS] [SUBCOMMAND]
```

Run `mira --help`, or `mira <subcommand> --help`, for the authoritative,
version-matched listing on your machine.

## Running modes

With **no subcommand**, `mira` picks a run mode from flags on the top-level
command:

| Invocation | What it does |
|---|---|
| `mira` | Start the rich interactive TUI. |
| `mira --simple` | Use the simple line-based CLI instead of the TUI. |
| `mira --server` | Run in server mode — hosts the web/HTTP API and channel pollers. This is the mode the installed service runs. |
| `mira --local` | Force the TUI to talk directly to the agent (no server). |
| `mira --server-url <URL>` | Point the TUI at a running MIRA HTTP server. Uses `MIRA_TOKEN` as the bearer token if set. Mutually exclusive with `--local`. |

Useful flags in these modes:

- `--port <PORT>` / `--host <ADDR>` — override the configured bind. Use
  `--host 0.0.0.0` to expose the API on all interfaces (for example, inside a
  port-forwarding container).
- `--layout <MODE>` / `--theme <NAME>` — override the TUI layout
  (`simple`/`standard`/`right-full`/…) or colour theme
  (`mira-dark`/`mira-light`/`dracula`/`gruvbox`/`nord`).

## Global options

These apply to any invocation, including subcommands:

| Option | Purpose |
|---|---|
| `--config <PATH>` | Use a specific config file. Default: `~/.mira/config/mira_config.json`. |
| `--data-dir <PATH>` | Override the data directory (databases, history, memory, auth, wiki, artifacts). Wins over the `data_dir` config field; equivalent to setting `MIRA_DATA_DIR`. Accepted on any subcommand. |
| `--print-config-template` | Print the annotated example config and exit. |
| `--help`, `--version` | Standard help / version output. |

> Put your data on a backed-up volume by choosing it during `mira setup`, or pass
> `--data-dir` (and let `mira install` bake it into the service). See
> [Back up & restore](../guides/backup-and-restore.md).

## Setup & installation

### `mira setup`

Guided first-run wizard. Configures an admin account, an LLM provider (validated
live), and the network/security posture, then writes a validated config. Voice
and channels are finished afterwards in the web UI.

Common flags:

- `--unattended` — run non-interactively (Docker/CI/scripts), taking answers from
  flags and environment.
- `--force` — reconfigure even if a config already exists.
- `--provider <ID>` — `ollama` | `lmstudio` | `anthropic` | `openai` |
  `openrouter` | `gemini` | `deepseek` | `groq` | `xai`.
- `--api-key <KEY>` / `--base-url <URL>` / `--model <NAME>` — provider details.
- `--admin-user <NAME>` / `--admin-pass <PASS>` — the bootstrap admin account.
- `--bind <localhost|lan>` — bind locally (default) or on all interfaces.
- `--port <PORT>` — the server port.
- `--skip-provider-test` — skip the live provider connection check.

See [Installation](../getting-started/installation.md) and the
[Quickstart](../getting-started/quickstart.md).

### `mira install`

Register MIRA as a service so the OS supervises it. By default this installs a
**user-scope** `systemd --user` unit (`~/.config/systemd/user/`).

Common flags:

- `--system` — install **system-scope** instead
  (`/etc/systemd/system/mira.service`): creates a `mira` system user and runs on
  boot regardless of who's logged in. Requires `sudo`. Use for VPS / shared-host
  deployments where the service must survive logout.
- `--config <PATH>` — the config the service should load.
- `--working-dir <PATH>` — the service working directory (default `$HOME`).
- `--web-dir <PATH>` — path to the built web bundle (`web/dist/`); auto-detected
  if omitted.
- `--no-enable` — write the unit file but don't enable/start it.
- `--force` — overwrite an existing unit without prompting.

### Service control

Once installed, control the service without touching `systemctl` directly:

| Command | What it does |
|---|---|
| `mira start` | Start the service. |
| `mira stop` | Stop the service. |
| `mira restart` | Restart the service (same as clicking Restart in the web UI). |
| `mira status` | Show the service's systemd status and recent journal. |
| `mira uninstall` | Disable and remove the service unit. |

## Upgrading

### `mira upgrade`

Update MIRA to a newer version. By default it **rebuilds from source** when a
MIRA source checkout is reachable (dev installs); otherwise it downloads and
**verifies a signed prebuilt tarball** (binary installs) against MIRA's embedded
public key before swapping the binary in. Pass `--source` or `--binary` to force
a path.

Common flags:

- `--source` / `--binary` — force the rebuild or the signed-download path
  (mutually exclusive).
- `--branch <NAME>` — *(source)* switch to a branch before pulling.
- `--version <VERSION>` — *(binary)* install a specific version (e.g. `0.84.0`);
  defaults to the latest release.
- `--no-restart` — install the new binary but don't restart the service.
- `--force` — *(source)* allow upgrading with uncommitted changes; *(binary)*
  re-install the same version (repair).

## Benchmarking

### `mira bench memory`

Run MIRA's memory stack against the **LongMemEval** long-term-memory benchmark.
It replays each conversation through MIRA's real extraction pipeline, asks the
question through the normal turn path (real memory + wiki retrieval), and
LLM-judges the answer against the gold, reporting per-question-type and overall
accuracy.

The dataset isn't bundled (it isn't redistributable) — download a LongMemEval
JSON (e.g. `longmemeval_s.json`) and point `--dataset` at it.

Common flags:

- `--dataset <PATH>` — **required** path to the LongMemEval JSON.
- `--dry-run` — print only the dataset summary (counts per type); no API spend.
- `--limit <N>` — cap the number of questions for a smoke run (default 20).
- `--all` — run the full dataset (ignores `--limit`).
- `--question-type <TYPE>` — only run one category (e.g. `single-session-user`,
  `temporal-reasoning`); the dataset is grouped by type.
- `--answer-provider <ID>` / `--judge-provider <ID>` — provider ids (from config)
  used to answer and to judge. Default: the configured primary.
- `--model <MODEL>` — override the chosen provider's model for the run (applies
  to answer + judge).
- `--extract-model <MODEL>` — pin the extraction model independently of
  `--model`, to isolate retrieval quality from answer-model capability.
- `--out <PATH>` — also write a JSON report to this path.

Examples:

```bash
# Inspect the dataset only, no API spend
mira bench memory --dataset ./longmemeval_s.json --dry-run

# Smoke run (default 20 questions)
mira bench memory --dataset ./longmemeval_s.json

# Full run with a JSON report
mira bench memory --dataset ./longmemeval_s.json --all --out results.json
```

See [Memory & the wiki](../concepts/memory-and-wiki.md) for what's being
measured.

## Other commands

A handful of narrower subcommands round out the CLI:

- **`mira tts`** — voice tooling: `probe` a backend, list `voices`, download a
  Piper voice (`download-voice`), synthesise to a file (`say`), inspect/clear the
  audio `cache`, or run an MCP `mcp-serve` voice server. See
  [Voice replies](../guides/voice-replies.md).
- **`mira sandbox`** — manage the Linux code-execution sandbox rootfs: `probe`,
  `status`, `install <language>`, `uninstall <language>`.
- **`mira deps`** — manage MIRA's bundled native dependencies (currently the ONNX
  Runtime used for in-process embeddings): `install`, `verify`, `list`.
- **`mira skill`** — Skill-author tools: `init`, `validate`, `keygen`, `sign`,
  `package`, plus a `secret` store and `refresh-bundled`.
- **`mira wiki`** — wiki tooling: `mcp-serve --user-id <ID>` exposes a user's
  wiki over MCP (for Claude Desktop and other MCP clients); `rebuild-profile`
  regenerates a wiki `profile.md` from onboarding state.
- **`mira helper-install`** — install MIRA's least-privilege **privileged
  helper** as a root systemd service (run as `sudo mira helper-install`). The
  helper enables native-tier plugin **egress filtering**; without it, a native
  plugin that declares an egress allowlist runs with no network at all.
  `mira helper-status` probes the running helper.

## Internal commands

MIRA inserts a few **internal wrapper commands** when it launches confined plugin
processes and elevated operations — for example `pkg-exec`, `ctr-run`, and the
`helper-*` daemon internals. These are not meant to be run by hand (most are
hidden from `--help`); MIRA manages them for you. Treat any subcommand not listed
above as internal.
