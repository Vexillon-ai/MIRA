// SPDX-License-Identifier: AGPL-3.0-or-later

// src/main.rs

//! MIRA - Multi-tasking Intelligent Responsive Assistant
//!
//! "Your life's loyal partner. Always ready to assist."
//!
//! Main entry point. `main()` is intentionally thin — all startup logic lives
//! in `GatewayBuilder`; the three runtime modes (server / TUI / simple CLI)
//! are dispatched from here.

mod tui;

use std::error::Error;
use std::sync::Arc;
use clap::{Parser, Subcommand};
use tracing::info;

use mira::config::MiraConfig;
use mira::gateway::GatewayBuilder;

// ── CLI arguments ─────────────────────────────────────────────────────────────

// MIRA - Multi-tasking Intelligent Responsive Assistant
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    // Optional subcommand. When set, takes precedence over the flag-based
    // run modes below (server / TUI / simple CLI).
    #[command(subcommand)]
    pub command: Option<Command>,

    // Run in server mode (for Telegram/Signal webhooks)
    #[arg(short, long)]
    pub server: bool,

    // Server port (overrides config file value)
    #[arg(long)]
    pub port: Option<u16>,

    // Server bind address (overrides config file value). Use `0.0.0.0` to
    // expose the API on all interfaces — required when running inside a
    // container that's port-forwarding to the host.
    #[arg(long)]
    pub host: Option<String>,

    // Use simple reedline CLI instead of the rich TUI
    #[arg(long)]
    pub simple: bool,

    // Force the TUI to run in local mode (direct AgentCore, no server).
    // Mutually exclusive with --server-url.
    #[arg(long)]
    pub local: bool,

    // Point the TUI at a MIRA HTTP server. Mutually exclusive with --local.
    // When set, MIRA_TOKEN env is used as the bearer token if present.
    #[arg(long, value_name = "URL")]
    pub server_url: Option<String>,

    // TUI layout mode (overrides config): simple|standard|right-full|left-full|right-only|left-only
    #[arg(long)]
    pub layout: Option<String>,

    // TUI colour theme (overrides config): mira-dark|mira-light|dracula|gruvbox|nord
    #[arg(long)]
    pub theme: Option<String>,

    // Path to a custom config file (default: ~/.mira/config/mira_config.json)
    #[arg(long, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,

    // Override the data directory (databases, history, memory, auth, exports).
    // Wins over the `data_dir` config field; equivalent to setting
    // MIRA_DATA_DIR. Place MIRA's state on a backed-up volume / external disk.
    // `mira setup` persists your choice; `mira install` bakes it into the
    // service. Global — accepted on any subcommand.
    #[arg(long, value_name = "PATH", global = true)]
    pub data_dir: Option<std::path::PathBuf>,

    // Print the annotated example config template and exit
    #[arg(long)]
    pub print_config_template: bool,
}

// Top-level subcommands. Runtime modes (`--server`, `--simple`, TUI) stay as
// flags on `Args` so the most common invocations (`mira`, `mira --server`)
// remain unchanged.
#[derive(Subcommand, Debug)]
pub enum Command {
    // Manage Tier 4 sandbox rootfs entries (5)
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },
    // Text-to-speech: probe backends, list voices, synthesise audio
    Tts {
        #[command(subcommand)]
        action: TtsAction,
    },
    // Internal: launch a packaged plugin process under lightweight confinement
    // (no-new-privs + safe resource caps), then exec it. Not for interactive
    // use — MIRA inserts this wrapper when installing a package.
    #[command(hide = true)]
    PkgExec {
        // Max file size the process may create, in MiB.
        #[arg(long)]
        fsize_mb: Option<u64>,
        // Run with no network access (fresh empty network namespace).
        #[arg(long)]
        no_network: bool,
        // Enter a mount namespace and remount the host root read-only,
        // carving writable holes (`--rw-path`) and masking secrets
        // (`--mask-path`).
        #[arg(long)]
        fs_scope: bool,
        // A path to keep writable under `--fs-scope` (repeatable).
        #[arg(long = "rw-path")]
        rw_paths: Vec<String>,
        // A path to hide behind an empty overlay under `--fs-scope` (repeatable).
        #[arg(long = "mask-path")]
        mask_paths: Vec<String>,
        // An allowlisted egress host for the native-tier egress filter
        // (repeatable). When set, the launcher enters a network namespace and
        // asks the privileged helper to filter it to these hosts.
        #[arg(long = "egress-host")]
        egress_hosts: Vec<String>,
        // The real command + args to run (after `--`).
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    // Internal: run a packaged container component behind a per-host egress
    // allowlist — starts the NET_ADMIN egress sidecar, runs the plugin in its
    // filtered network namespace (stdio inherited), and tears it down on exit.
    // Not for interactive use; MIRA inserts this wrapper at install.
    #[command(hide = true)]
    CtrRun {
        // Egress-allowlist mode: `egress` (Tier A — NET_ADMIN nft sidecar) or
        // `proxy` (Tier B — internal network + HTTP/S allowlist proxy).
        #[arg(long, default_value = "egress")]
        mode: String,
        // Container engine binary (docker/podman).
        #[arg(long)]
        engine: String,
        // Upstream DNS the sidecar forwards allowlisted lookups to.
        #[arg(long)]
        upstream: String,
        // Plugin image ref.
        #[arg(long)]
        image: String,
        // Allowlisted hostname (repeatable).
        #[arg(long = "allow")]
        allow: Vec<String>,
        // `host:container` bind mount (repeatable).
        #[arg(long = "volume")]
        volumes: Vec<String>,
        // Env var name to forward into the container (repeatable).
        #[arg(long = "env")]
        envs: Vec<String>,
        // Memory ceiling.
        #[arg(long, default_value = "512m")]
        memory: String,
        // Max PIDs.
        #[arg(long, default_value_t = 256)]
        pids: u32,
    },
    // Internal: run the privileged helper daemon (root `mira-helper.service`).
    // Serves a fixed enum of elevated ops over a locked unix socket.
    #[command(hide = true)]
    HelperDaemon {
        // Socket path to bind.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        // Only accept connections from this uid (the MIRA user).
        #[arg(long)]
        owner_uid: Option<u32>,
    },
    // Show the privileged helper's status (probes the daemon over its socket).
    HelperStatus {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    // Install the privileged helper as a root systemd service. Run as root:
    // `sudo mira helper-install`.
    HelperInstall {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        // MIRA-user uid the socket is owned by (defaults to $SUDO_UID).
        #[arg(long)]
        owner_uid: Option<u32>,
    },
    // Install the WSL host-alias boot hook (root). Maps a stable hostname
    // (`windows-host`) to the WSL2 NAT gateway in /etc/hosts, refreshed every
    // boot, so Windows-host services keep working across reboots. Run as root:
    // `sudo mira wsl-host-alias-install`. (Also done automatically by
    // `sudo mira helper-install` on WSL.)
    WslHostAliasInstall {
        // Hostname to map to the Windows host (default `windows-host`).
        #[arg(long)]
        alias: Option<String>,
    },
    // Internal/debug: ask the helper to provision native-tier egress filtering
    // for a confined subprocess pid (drives the `NetAllow` op directly).
    #[command(hide = true)]
    HelperNetallow {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        // The confined subprocess pid (must own a network namespace).
        #[arg(long)]
        pid: u32,
        // Allowlisted host (repeatable): `--allow a.com --allow b.org`.
        #[arg(long = "allow")]
        allow: Vec<String>,
        // Upstream DNS resolver IPv4 (defaults to 1.1.1.1).
        #[arg(long)]
        upstream: Option<String>,
    },
    // Internal/debug: tear down native-tier egress filtering for a pid
    // (drives the `NetTeardown` op directly).
    #[command(hide = true)]
    HelperNetteardown {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        #[arg(long)]
        pid: u32,
    },
    // Install MIRA as a systemd user service so the OS supervises it.
    // On first install, run `mira install` and then complete onboarding
    // in the web UI. See design-docs/install-and-supervisor.md.
    Install {
        // Path to the config file the service should load.
        // Defaults to `~/.mira/config/mira_config.json`.
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
        // Working directory for the service. Defaults to `$HOME`.
        #[arg(long, value_name = "PATH")]
        working_dir: Option<std::path::PathBuf>,
        // Path to the built React bundle (`web/dist/`). When omitted,
        // install auto-detects and warns if no bundle is found.
        #[arg(long, value_name = "PATH")]
        web_dir: Option<std::path::PathBuf>,
        // Write the unit file but don't `systemctl --user enable --now`.
        #[arg(long)]
        no_enable: bool,
        // Overwrite an existing unit file without prompting.
        #[arg(long)]
        force: bool,
        // Install system-scope: writes /etc/systemd/system/mira.service,
        // creates the `mira` system user, runs as that user on boot
        // regardless of who's logged in. Requires sudo. Use for VPS /
        // shared-host deployments where the service must survive logout.
        // Default (false) installs user-scope (~/.config/systemd/user/).
        #[arg(long)]
        system: bool,
    },
    // First-run guided setup wizard: configure an admin account, an LLM
    // provider (validated live), and the network/security posture, then write a
    // validated config. Voice + channels are finished in the web UI afterwards.
    Setup {
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
        // Run non-interactively (Docker/CI/scripts) — take answers from flags/env.
        #[arg(long)]
        unattended: bool,
        // Reconfigure even if a config already exists.
        #[arg(long)]
        force: bool,
        // Provider id: ollama | lmstudio | anthropic | openai | openrouter | gemini | deepseek | groq | xai.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        admin_user: Option<String>,
        #[arg(long)]
        admin_pass: Option<String>,
        // Network bind: "localhost" (default) or "lan" (0.0.0.0).
        #[arg(long)]
        bind: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        // Skip the live provider connection test.
        #[arg(long)]
        skip_provider_test: bool,
    },
    // Disable and remove the MIRA systemd user service unit.
    Uninstall,
    // Start the MIRA service (requires `mira install`).
    Start,
    // Stop the MIRA service.
    Stop,
    // Restart the MIRA service. Equivalent to clicking Restart in the web UI.
    Restart,
    // Show the service's systemd status (active state, recent journal).
    Status,
    // Skill author tools (slice A8 of design-docs/skills-and-agents.md):
    // scaffold, validate, generate signing keys, sign, and package
    // Skills for upload via the web UI.
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    // Upgrade to a newer version. Defaults to source-rebuild when a
    // MIRA source repo is reachable (dev installs); otherwise
    // downloads + verifies the matching prebuilt tarball from the
    // release pipeline (binary installs). Pass `--binary` / `--source`
    // to force a specific path.
    Upgrade {
        // Force the source-rebuild path. Requires a git checkout +
        // cargo on this machine. Mutually exclusive with --binary.
        #[arg(long, conflicts_with = "binary")]
        source: bool,
        // Force the prebuilt-binary download path. Verifies the
        // downloaded tarball against the embedded minisign public
        // key before swapping. Mutually exclusive with --source.
        #[arg(long, conflicts_with = "source")]
        binary: bool,
        // (--source only) Switch to a specific branch before pulling.
        // Defaults to the currently-checked-out branch.
        #[arg(long, value_name = "NAME")]
        branch: Option<String>,
        // (--binary only) Specific version to install (e.g. `0.84.0`
        // or `v0.84.0`). Defaults to whatever the release provider's
        // API reports as the latest release.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
        // (--binary only) Access token for fetching from a private
        // release host (e.g. a private GitLab/GitHub fork). Reads from
        // `$MIRA_RELEASE_TOKEN` when not passed.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        // Build / install the new binary but don't restart the
        // service afterwards.
        #[arg(long)]
        no_restart: bool,
        // --source: allow upgrading with uncommitted changes in the
        // source tree.
        // --binary: re-install the same version (useful for repair
        // or for exercising the download + verify + swap pipeline).
        #[arg(long)]
        force: bool,
    },
    // Roll back to the binary + config saved before the last upgrade.
    // Snapshots are created automatically on every upgrade. Works even
    // if the current build crash-loops (run it from an admin terminal).
    Rollback {
        // Specific version to roll back to (e.g. 0.292.3). Defaults to
        // the most recent snapshot.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
        // List available rollback snapshots and exit.
        #[arg(long)]
        list: bool,
        // Restore the binary + config but don't restart the service.
        #[arg(long)]
        no_restart: bool,
    },
    // Manage MIRA's bundled native dependencies (currently ONNX
    // Runtime, used by fastembed for in-process embeddings). Pinned
    // versions live in deps/manifest.toml; downloads land in
    // ~/.mira/deps/<name>/ and are verified by SHA-256 before
    // extraction.
    Deps {
        #[command(subcommand)]
        action: DepsAction,
    },
    // Wiki tooling (Slice G). Currently: `mira wiki mcp-serve` to
    // expose a user's wiki over Model Context Protocol on stdio,
    // for connection by Claude Desktop and other MCP-aware clients.
    Wiki {
        #[command(subcommand)]
        action: WikiAction,
    },
    // Benchmarking (roadmap #9). `mira bench memory` runs MIRA's memory
    // stack against the LongMemEval dataset and reports accuracy.
    Bench {
        #[command(subcommand)]
        action: BenchAction,
    },
    // Out-of-process MIRA-Guardian liveness sentinel. A separate supervised
    // process that probes the main MIRA server's /health and raises a direct
    // web-push alarm if MIRA goes down — the one failure the co-resident watch
    // can't catch (it shares MIRA's fate). Driven by `guardian.process.*` config
    // (off by default). Use the global `--data-dir` to point at MIRA's data dir.
    // See design-docs/guardian-separate-process.md.
    GuardianWatch,
    // Install the Guardian liveness sentinel as its own supervised unit
    // (`mira-guardian-watch.service`), separate from the main MIRA service so it
    // outlives a server crash. Run this after enabling `guardian.process.enabled`
    // in Settings → Guardian. Linux/systemd today; macOS/Windows land in a
    // follow-up (a documented manual unit meanwhile).
    GuardianInstall {
        // Config file the sentinel should load. Defaults to the standard path.
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
        // Working directory for the sentinel service. Defaults to `$HOME`.
        #[arg(long, value_name = "PATH")]
        working_dir: Option<std::path::PathBuf>,
        // Write the unit but don't `enable --now`.
        #[arg(long)]
        no_enable: bool,
        // Install system-scope (requires sudo), mirroring `mira install --system`.
        #[arg(long)]
        system: bool,
    },
    // Remove the Guardian liveness sentinel's supervised unit.
    GuardianUninstall,
}

#[derive(Subcommand, Debug)]
pub enum BenchAction {
    // Run the LongMemEval memory benchmark in replay mode. Supply a path to
    // a downloaded LongMemEval JSON (e.g. `longmemeval_s.json`); the dataset
    // is not redistributable. Replays each haystack through the real memory +
    // wiki extractors, answers the question, and LLM-judges vs the gold.
    Memory {
        // Path to the LongMemEval dataset JSON.
        #[arg(long, value_name = "PATH")]
        dataset: std::path::PathBuf,
        // Cap the number of questions (smoke runs). Ignored with --all.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        // Run the full dataset (ignore --limit).
        #[arg(long, default_value_t = false)]
        all: bool,
        // Only run questions of this type (e.g. `single-session-user`,
        // `temporal-reasoning`). The dataset is grouped by type, so this is
        // how a smoke run samples a specific category.
        #[arg(long, value_name = "TYPE")]
        question_type: Option<String>,
        // Provider id (from config) used to answer. Default: configured primary.
        #[arg(long, value_name = "ID")]
        answer_provider: Option<String>,
        // Provider id used to judge. Default: same as the answer provider.
        #[arg(long, value_name = "ID")]
        judge_provider: Option<String>,
        // Override the chosen provider's model for this run (e.g.
        // `google/gemini-2.5-flash-lite` on OpenRouter). Applies to answer +
        // judge. Default: the provider's configured model.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        // Pin the extraction model independently of `--model`. When set,
        // haystack replay extracts memories with this model while the answer +
        // judge use `--model`. Isolates the answer model's effect from
        // extraction quality. Default: extraction shares the answer model.
        #[arg(long, value_name = "MODEL")]
        extract_model: Option<String>,
        // Write a JSON report to this path (in addition to the stdout summary).
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
        // Print only the dataset summary (counts per type) — no API spend.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    // Measurement-only context baseline (no API spend): reports how many
    // tokens MIRA's CURRENT fixed-turn window sends on synthetic
    // conversations, its context-window utilisation, and turns dropped.
    // The "before" numbers for the context-compaction work.
    Context {
        // Conversation lengths (turns) to measure, comma-separated.
        #[arg(long, value_name = "N,N,…", default_value = "10,40,100,300", value_delimiter = ',')]
        turns: Vec<usize>,
        // Average characters per message (≈ 4 chars/token).
        #[arg(long, default_value_t = 320)]
        avg_msg_chars: usize,
        // Model context window (tokens) used for the utilisation %.
        #[arg(long, default_value_t = 128_000)]
        context_length: usize,
        // Fixed system-prompt + memory overhead estimate (tokens).
        #[arg(long, default_value_t = 1500)]
        system_tokens: usize,
        // Output reservation for the budget column. 0 = agent.max_response_tokens.
        #[arg(long, default_value_t = 0)]
        max_response_tokens: usize,
        // Safety margin held back in the budget column (tokens).
        #[arg(long, default_value_t = 1024)]
        safety_margin: usize,
        // Write a CSV report to this path.
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum WikiAction {
    // Run an MCP server on stdio that serves the named user's wiki
    // as MCP resources. Intended to be launched by an MCP client
    // (Claude Desktop config, etc.) rather than typed at a terminal.
    McpServe {
        // User id whose wiki should be exposed. Required — no default
        // because cross-user exposure is the worst-case footgun here.
        #[arg(long, value_name = "USER_ID")]
        user_id: String,
        // Override the data directory. Defaults to the configured
        // data_dir (typically ~/.mira/data).
        #[arg(long, value_name = "PATH")]
        data_dir: Option<std::path::PathBuf>,
    },
    // Rebuild the named user's wiki `profile.md` from their current
    // onboarding state. One-shot replay of the onboarding -> wiki
    // bridge — useful for users who completed onboarding before the
    // bridge was wired in, or after a wiki reset. Idempotent.
    // Omit --user-id to rebuild for every user with a profile row.
    RebuildProfile {
        // User id whose profile.md should be rebuilt. If omitted,
        // every user with an onboarded_at timestamp is rebuilt.
        #[arg(long, value_name = "USER_ID")]
        user_id: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum DepsAction {
    // Download + verify + extract any deps that aren't already
    // installed for this platform. Idempotent — already-present
    // deps with matching SHA-256 are skipped.
    Install {
        // Re-fetch + reinstall even if the dep is already present.
        // Useful after manifest version bumps or to repair a
        // corrupted install.
        #[arg(long)]
        force: bool,
    },
    // Check that every manifest-declared dep for this platform is
    // installed at the expected path. Exits non-zero with a clear
    // message when something's missing.
    Verify,
    // List all manifest-declared deps and their installed status.
    List,
}

#[derive(Subcommand, Debug)]
pub enum SkillAction {
    // Scaffold a new Skill directory containing a starter `skill.toml`.
    Init {
        // Reverse-DNS Skill id (e.g. `com.example.myskill`).
        id: String,
        // Where to create the directory. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
    },
    // Validate the manifest at `<dir>/skill.toml`. Checks parsing,
    // declared paths, directory-id agreement.
    Validate {
        // Path to a Skill directory containing `skill.toml`.
        path: std::path::PathBuf,
    },
    // Generate a fresh ed25519 publisher keypair. Writes the secret key
    // to a file (mode 0600) and prints the public key + fingerprint.
    Keygen {
        // Where to write the secret key. Defaults to
        // `~/.mira/keys/<short-fingerprint>.ed25519`.
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
    },
    // Sign a Skill manifest in place, writing the resulting
    // `[verification]` block back into `skill.toml`.
    Sign {
        // Path to a Skill directory containing `skill.toml`.
        path: std::path::PathBuf,
        // Path to a secret key file produced by `mira skill keygen`.
        #[arg(long, value_name = "PATH")]
        key:  std::path::PathBuf,
    },
    // Build a `.miraskill` (gzipped tar) archive from a Skill directory,
    // suitable for upload via the web UI's Install Skill flow.
    Package {
        // Path to the Skill directory.
        path: std::path::PathBuf,
        // Output file path. Defaults to `<id>-<version>.miraskill` in the
        // current directory.
        #[arg(long, value_name = "PATH")]
        out:  Option<std::path::PathBuf>,
    },
    // Manage encrypted env-var secrets used by subprocess adapters
    // (Claude Code, OpenCode, HERMES). Values are stored in
    // `~/.mira/data/skill_secrets.db`, encrypted with the master
    // key at `~/.mira/data/master.key`. Back that file up — losing
    // it discards every stored secret.
    Secret {
        #[command(subcommand)]
        action: SkillSecretAction,
    },
    // Re-extract bundled skills (the ones shipped with MIRA itself)
    // onto disk. By default only refreshes skills whose bundled
    // manifest version is newer than what's installed — same logic
    // the boot path runs. Pass `--force` to overwrite every bundled
    // skill regardless of version, useful when a manifest changed
    // without a version bump (dev workflow) or when a user has
    // edited an extracted manifest and wants to discard the changes.
    // User uninstall markers (.bundled-uninstalled/<id>) always
    // suppress refresh — clear them via the web UI's reinstall flow.
    RefreshBundled {
        // Overwrite every bundled skill regardless of on-disk version.
        #[arg(long)]
        force: bool,
        // Refresh only this skill id (e.g. `com.mira.claudecode`). Without
        // this flag, every bundled skill is considered.
        #[arg(long, value_name = "ID")]
        id:    Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SkillSecretAction {
    // Set or update one secret. The value is read from the prompt
    // (or from `--from-stdin` for piped use); never accepts via
    // argv so it doesn't land in shell history.
    Set {
        // Reverse-DNS skill id (e.g. `com.mira.claudecode`).
        skill: String,
        // Env-var key (e.g. `ANTHROPIC_API_KEY`).
        key:   String,
        // `system` (host-wide) or `user:<username>`. Default `system`.
        #[arg(long, value_name = "SCOPE")]
        scope: Option<String>,
        // Read the value from stdin instead of prompting. Useful for
        // `cat key.txt | mira skill secret set...`.
        #[arg(long)]
        from_stdin: bool,
    },
    // List the keys (NOT values) registered for a skill.
    List {
        skill: String,
        // Scope filter. `system`, `user:<username>`, or unset for
        // system-wide keys.
        #[arg(long, value_name = "SCOPE")]
        scope: Option<String>,
    },
    // Delete one secret.
    Delete {
        skill: String,
        key:   String,
        #[arg(long, value_name = "SCOPE")]
        scope: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SandboxAction {
    // List installed rootfs entries with disk usage
    Status,
    // Diagnose whether the host can run the sandbox at all
    Probe,
    // Download, verify, and extract a language rootfs
    Install {
        // Language to install (currently only `python`)
        language: String,
        // Re-download even if a cached archive exists
        #[arg(long)]
        force: bool,
    },
    // Remove an installed rootfs from disk
    Uninstall {
        // Language to remove (currently only `python`)
        language: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum TtsAction {
    // Probe backend health (latency, configured engine, version note)
    Probe {
        // Backend id (e.g. `piper`, `espeak`). Defaults to the configured backend.
        #[arg(long)]
        backend: Option<String>,
    },
    // List voices for a backend
    Voices {
        // Backend id. Defaults to the configured backend.
        #[arg(long)]
        backend: Option<String>,
    },
    // Download a Piper voice from the curated list (e.g. `en_US-amy-medium`)
    DownloadVoice {
        voice_id: String,
    },
    // Synthesise text and write the audio to a file
    Say {
        // Text to synthesise
        text: String,
        // Voice id (defaults to the configured default)
        #[arg(long)]
        voice: Option<String>,
        // Speech rate multiplier (0.5–2.0). Defaults to 1.0.
        #[arg(long)]
        speed: Option<f32>,
        // Backend id override (e.g. `piper`, `espeak`)
        #[arg(long)]
        backend: Option<String>,
        // Output file path. Defaults to `./mira-tts.<ext>` (extension picked from codec).
        #[arg(short, long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
    },
    // Inspect or clear the on-disk audio cache
    Cache {
        #[command(subcommand)]
        action: TtsCacheAction,
    },
    // Run an MCP stdio server exposing a `synthesize` tool that speaks
    // text with MIRA's configured voice (returns an audio clip). Connect
    // it from MIRA's /mcp page or any MCP host (Claude Desktop, etc.).
    McpServe,
}

#[derive(Subcommand, Debug)]
pub enum TtsCacheAction {
    // Print cache entry count and total size
    Stats,
    // Delete all cached audio
    Clear,
}

// ── Entry point ───────────────────────────────────────────────────────────────

// Sync entry point. Intercepts `pkg-exec` BEFORE the async runtime starts —
// `unshare(CLONE_NEWUSER)` (for plugin network isolation) requires a
// single-threaded process, which tokio's multi-thread runtime is not.
fn main() -> Result<(), Box<dyn Error>> {
    // Windows post-restart relauncher: a deliberate restart exits the service
    // cleanly (exit 0, no SCM crash event) and spawns a detached copy of us
    // with MIRA_WIN_RELAUNCH set, whose only job is to start the service again
    // once SCM marks it Stopped. Handle that here, before any arg parsing —
    // the relauncher takes no CLI args. See install::windows.
    #[cfg(target_os = "windows")]
    {
        if mira::install::windows::maybe_run_relauncher() {
            return Ok(());
        }
    }

    let args = Args::parse();

    // Wire the global `--data-dir` flag into MIRA_DATA_DIR so a single resolver
    // (config::data_dir_env_override) covers the flag, the env, and the service
    // launch args. Done first thing — before setup / install / the restore swap
    // config load / the server — so every path agrees on the data location.
    if let Some(d) = args.data_dir.as_ref() {
        // Safe: single-threaded here — this runs at the very top of sync main(),
        // before the tokio runtime or any other thread is created.
        unsafe { std::env::set_var("MIRA_DATA_DIR", d); }
    }
    // Likewise wire `--config` into MIRA_CONFIG. The console `--server` path uses
    // `args.config` directly, but the Windows SCM service re-enters via
    // `service_main`, which has no access to the parsed args and must read the
    // config path from the environment — otherwise it would fall back to the
    // default path (for LocalSystem that's %SystemRoot%\System32\config\
    // systemprofile\.mira) and ignore the config `mira install` baked into the
    // service launch command. (Also keeps the pending-restore swap below in sync.)
    if let Some(c) = args.config.as_ref() {
        unsafe { std::env::set_var("MIRA_CONFIG", c); }
    }
    if let Some(Command::PkgExec { fsize_mb, no_network, fs_scope, rw_paths, mask_paths, egress_hosts, argv }) =
        args.command.as_ref()
    {
        let spec = mira::packages::launcher::ConfineSpec {
            fsize_mb: *fsize_mb,
            no_network: *no_network,
            fs_scope: *fs_scope,
            rw_paths: rw_paths.clone(),
            mask_paths: mask_paths.clone(),
            egress: egress_hosts.clone(),
        };
        let e = mira::packages::launcher::exec_confined(&spec, argv);
        eprintln!("mira pkg-exec: failed to launch: {e}");
        std::process::exit(127);
    }
    if let Some(Command::CtrRun { mode, engine, upstream, image, allow, volumes, envs, memory, pids }) =
        args.command.as_ref()
    {
        use mira::packages::container::{self, ContainerSpec, EgressRunOpts, NetworkMode};
        let mut plugin = ContainerSpec::new(image.clone(), NetworkMode::None);
        plugin.memory = memory.clone();
        plugin.pids_limit = *pids;
        plugin.env_keys = envs.clone();
        plugin.volumes = volumes
            .iter()
            .filter_map(|v| v.split_once(':').map(|(h, c)| (h.to_string(), c.to_string())))
            .collect();
        let opts = EgressRunOpts {
            engine: engine.clone(),
            upstream: upstream.clone(),
            allow: allow.clone(),
            plugin,
        };
        let code = match mode.as_str() {
            "proxy" => container::run_proxy_confined(&opts),
            _ => container::run_egress_confined(&opts),
        };
        std::process::exit(code);
    }
    // Privileged-helper subcommands are Linux-only (Unix-socket daemon with
    // SO_PEERCRED auth + /proc network-namespace egress). cfg-gate them so
    // Windows/macOS still compile; the not(linux) arm below reports clearly.
    #[cfg(target_os = "linux")]
    {
    if let Some(Command::HelperDaemon { socket, owner_uid }) = args.command.as_ref() {
        let sock = socket.clone().unwrap_or_else(mira::privhelper::default_socket);
        let opts = mira::privhelper::daemon::DaemonOpts { socket_path: sock, owner_uid: *owner_uid };
        if let Err(e) = mira::privhelper::daemon::run(&opts) {
            eprintln!("mira helper-daemon: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    if let Some(Command::HelperStatus { socket }) = args.command.as_ref() {
        let sock = socket.clone().unwrap_or_else(mira::privhelper::default_socket);
        let probe = mira::privhelper::client::probe(&sock);
        // On WSL, always surface the host-alias state (independent of the daemon).
        if mira::install::is_wsl() {
            let st = mira::privhelper::wsl::host_alias_status(mira::privhelper::wsl::DEFAULT_ALIAS);
            let installed = st.get("unit_present").and_then(|v| v.as_bool()).unwrap_or(false);
            println!("WSL host-alias: {}", if installed { "installed" } else { "NOT installed (run: sudo mira wsl-host-alias-install)" });
            println!("{}", serde_json::to_string_pretty(&st).unwrap_or_default());
        }
        match probe {
            Some(data) => {
                println!("privileged helper: available");
                println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default());
                std::process::exit(0);
            }
            None => {
                println!("privileged helper: NOT available at {} (degraded — best-effort only)", sock.display());
                std::process::exit(1);
            }
        }
    }
    if let Some(Command::HelperInstall { socket, owner_uid }) = args.command.as_ref() {
        let sock = socket.clone().unwrap_or_else(mira::privhelper::default_socket);
        let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("mira"));
        let uid = match mira::privhelper::resolve_owner_uid(*owner_uid) {
            Ok(u) => u,
            Err(e) => { eprintln!("mira helper-install: {e}"); std::process::exit(1); }
        };
        match mira::privhelper::install(&exe, &sock, uid) {
            Ok(()) => { println!("✓ mira-helper installed + started (socket {}, owner uid {uid})", sock.display()); std::process::exit(0); }
            Err(e) => { eprintln!("mira helper-install: {e}"); std::process::exit(1); }
        }
    }
    if let Some(Command::WslHostAliasInstall { alias }) = args.command.as_ref() {
        let alias = alias.clone().unwrap_or_else(|| mira::privhelper::wsl::DEFAULT_ALIAS.to_string());
        match mira::privhelper::wsl::install_host_alias(&alias) {
            Ok(d) => {
                println!("✓ WSL host-alias installed");
                println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
                println!("\nPoint Windows-host service URLs at http://{alias}:<PORT> (e.g. LM Studio: http://{alias}:1234/v1).");
                std::process::exit(0);
            }
            Err(e) => { eprintln!("mira wsl-host-alias-install: {e}"); std::process::exit(1); }
        }
    }
    if let Some(Command::HelperNetallow { socket, pid, allow, upstream }) = args.command.as_ref() {
        let sock = socket.clone().unwrap_or_else(mira::privhelper::default_socket);
        match mira::privhelper::client::net_allow(&sock, *pid, allow, upstream.as_deref()) {
            Ok(data) => { println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default()); std::process::exit(0); }
            Err(e) => { eprintln!("mira helper-netallow: {e}"); std::process::exit(1); }
        }
    }
    if let Some(Command::HelperNetteardown { socket, pid }) = args.command.as_ref() {
        let sock = socket.clone().unwrap_or_else(mira::privhelper::default_socket);
        match mira::privhelper::client::net_teardown(&sock, *pid) {
            Ok(()) => { println!("✓ egress torn down for pid {pid}"); std::process::exit(0); }
            Err(e) => { eprintln!("mira helper-netteardown: {e}"); std::process::exit(1); }
        }
    }
    } // end #[cfg(target_os = "linux")] privileged-helper handlers

    // Non-Linux: the helper doesn't exist here. Intercept its subcommands with a
    // clear message + non-zero exit, before they fall through to the unreachable!
    // arm in the post-config dispatch.
    #[cfg(not(target_os = "linux"))]
    if matches!(
        args.command.as_ref(),
        Some(
            Command::HelperDaemon { .. }
                | Command::HelperStatus { .. }
                | Command::HelperInstall { .. }
                | Command::WslHostAliasInstall { .. }
                | Command::HelperNetallow { .. }
                | Command::HelperNetteardown { .. }
        )
    ) {
        eprintln!(
            "the privileged helper is Linux-only (egress allowlist via network \
             namespaces with SO_PEERCRED auth); not supported on this platform."
        );
        std::process::exit(1);
    }

    // First-run setup wizard — runs in the SYNC main, before any tokio runtime
    // exists: it uses `reqwest::blocking` (for the live provider test) + blocking
    // `dialoguer` prompts, which must not run inside an async runtime context.
    if let Some(Command::Setup {
        config, unattended, force, provider, api_key, base_url, model,
        admin_user, admin_pass, bind, port, skip_provider_test,
    }) = args.command.as_ref()
    {
        let opts = mira::setup::SetupOptions {
            config_path: config.clone(),
            data_dir: args.data_dir.clone(),
            unattended: *unattended,
            force: *force,
            provider: provider.clone(),
            api_key: api_key.clone(),
            base_url: base_url.clone(),
            model: model.clone(),
            admin_user: admin_user.clone(),
            admin_pass: admin_pass.clone(),
            bind: bind.clone(),
            port: *port,
            skip_provider_test: *skip_provider_test,
        };
        match mira::setup::run(opts) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("mira setup: {e}");
                std::process::exit(1);
            }
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn Error>> {
    // Windows SCM probe. When the binary is launched by
    // Service Control Manager (not from a console), we MUST connect
    // to SCM via the dispatcher within ~30s of process start. The
    // dispatcher blocks until the service stops; control never
    // returns from this branch in the service case. Console launches
    // get a fast Err and fall through to the normal CLI flow.
    //
    // Done before clap so the SCM never sees us as an unresponsive
    // service. Done after maybe_apply_runtime_env so the service's
    // tokio runtime resolves bundled deps the same way the console
    // path does.
    mira::install::deps::maybe_apply_runtime_env();

    #[cfg(target_os = "windows")]
    {
        if mira::install::windows::try_run_as_service().is_ok() {
            return Ok(());
        }
        // Err — console launch. The error is the standard SCM
        // "couldn't connect to dispatcher" path; ignore and continue.
    }

    // Q1.5 — pending-restore swap. If the previous boot's restore
    // endpoint staged an uploaded backup, apply it BEFORE any
    // SQLite connection opens (otherwise the running process would
    // be holding locks on files we're about to replace). The
    // resolver here mirrors what default_config_path uses + the
    // MIRA_CONFIG env override the rest of the binary respects.
    {
        let cfg_path = std::env::var_os("MIRA_CONFIG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(mira::config::default_config_path);
        let data_dir = mira::config::default_data_dir_path();
        match mira::install::backup::apply_pending_restore(&data_dir, &cfg_path) {
            Ok(true)  => eprintln!("✓ applied pending restore from {}", data_dir.display()),
            Ok(false) => { /* no marker; normal boot */ }
            Err(e)    => eprintln!("⚠ pending restore failed: {e} — continuing with current data"),
        }
    }

    // Best-effort: reap orphan container-egress sidecars / internal networks left
    // by any hard-killed `ctr-run` (normal teardown handles the common case).
    tokio::task::spawn_blocking(|| {
        if let Some(engine) = mira::packages::container::detect_engine() {
            mira::packages::container::reap_orphan_sidecars(&engine);
        }
    });

    let args = Args::parse();

    if args.print_config_template {
        println!("{}", mira::config::schema::EXAMPLE_JSONC);
        return Ok(());
    }

    // Install / uninstall must run *before* config load — `mira install` is
    // expected to work on a fresh machine where no config file exists yet.
    if let Some(Command::Install { config, working_dir, web_dir, no_enable, force, system }) = args.command.as_ref() {
        // Default working_dir for --system points at /var/lib/mira (the
        // system user's home), not the caller's $HOME. Otherwise the
        // service tries to write data into root's home.
        let default_wd = || if *system {
            std::path::PathBuf::from("/var/lib/mira")
        } else {
            default_home_dir()
        };
        // Same for the config path — under --system it lives at /etc/mira/
        // so the operator can manage it with the rest of /etc.
        let default_cfg = || if *system {
            std::path::PathBuf::from("/etc/mira/mira_config.json")
        } else {
            mira::config::default_config_path()
        };
        let opts = mira::install::InstallOptions {
            config_path: config.clone().unwrap_or_else(default_cfg),
            working_dir: working_dir.clone().unwrap_or_else(default_wd),
            web_dir:     web_dir.clone(),
            no_enable:   *no_enable,
            force:       *force,
            system:      *system,
        };
        return mira::install::run_install(opts).map_err(|e| -> Box<dyn Error> {
            // Print to stderr so a piped install script can detect failures.
            eprintln!("mira install: {e}");
            "install failed".into()
        });
    }
    if matches!(args.command, Some(Command::Uninstall)) {
        return mira::install::run_uninstall().map_err(|e| -> Box<dyn Error> {
            eprintln!("mira uninstall: {e}");
            "uninstall failed".into()
        });
    }
    // Guardian sentinel unit install/uninstall — like install/uninstall, these
    // manage a supervised unit and must work before config load.
    if let Some(Command::GuardianInstall { config, working_dir, no_enable, system }) = args.command.as_ref() {
        let default_wd = || if *system {
            std::path::PathBuf::from("/var/lib/mira")
        } else {
            default_home_dir()
        };
        let default_cfg = || if *system {
            std::path::PathBuf::from("/etc/mira/mira_config.json")
        } else {
            mira::config::default_config_path()
        };
        let opts = mira::install::InstallOptions {
            config_path: config.clone().unwrap_or_else(default_cfg),
            working_dir: working_dir.clone().unwrap_or_else(default_wd),
            web_dir:     None,
            no_enable:   *no_enable,
            force:       false,
            system:      *system,
        };
        return mira::install::run_guardian_install(opts).map_err(|e| -> Box<dyn Error> {
            eprintln!("mira guardian-install: {e}");
            "guardian-install failed".into()
        });
    }
    if matches!(args.command, Some(Command::GuardianUninstall)) {
        return mira::install::run_guardian_uninstall().map_err(|e| -> Box<dyn Error> {
            eprintln!("mira guardian-uninstall: {e}");
            "guardian-uninstall failed".into()
        });
    }

    // Service-control subcommands also bypass config load — they shell out
    // to systemctl and never touch MIRA's runtime state.
    let svc_result: Option<(&str, Result<(), Box<dyn Error>>)> = match &args.command {
        Some(Command::Start)   => Some(("mira start",   mira::install::run_start())),
        Some(Command::Stop)    => Some(("mira stop",    mira::install::run_stop())),
        Some(Command::Restart) => Some(("mira restart", mira::install::run_restart())),
        Some(Command::Status)  => Some(("mira status",  mira::install::run_status())),
        _ => None,
    };
    if let Some((label, result)) = svc_result {
        return result.map_err(|e| -> Box<dyn Error> {
            eprintln!("{label}: {e}");
            "service control failed".into()
        });
    }

    if let Some(Command::Upgrade { source, binary, branch, version, token, no_restart, force }) =
        args.command.as_ref()
    {
        // Route between source vs binary upgrade. Explicit flags win;
        // otherwise auto-detect: if a MIRA source repo is reachable
        // (dev install), default to source — that path is what the
        // dev workflow has always done. Otherwise default to binary.
        let want_binary = if *binary { true }
                          else if *source { false }
                          else { !mira::install::upgrade::source_dir_reachable() };

        if want_binary {
            let opts = mira::install::binary_upgrade::BinaryUpgradeOptions {
                version:          version.clone(),
                no_restart:       *no_restart,
                force:            *force,
                provider:         None,
                release_base_url: None,
                token:            token.clone(),
            };
            return mira::install::run_binary_upgrade(opts).map_err(|e| -> Box<dyn Error> {
                eprintln!("mira upgrade --binary: {e}");
                "binary upgrade failed".into()
            });
        }

        let opts = mira::install::upgrade::UpgradeOptions {
            branch:     branch.clone(),
            no_restart: *no_restart,
            force:      *force,
        };
        return mira::install::run_upgrade(opts).map_err(|e| -> Box<dyn Error> {
            eprintln!("mira upgrade: {e}");
            "upgrade failed".into()
        });
    }

    if let Some(Command::Rollback { version, list, no_restart }) = args.command.as_ref() {
        if *list {
            let snaps = mira::install::rollback::list_snapshots();
            if snaps.is_empty() {
                println!("No rollback snapshots yet — one is saved automatically on each upgrade.");
            } else {
                println!("Available rollback snapshots (newest first):");
                for s in &snaps {
                    println!("  v{}  ({}{})", s.version, s.dir.display(),
                        if s.config.is_some() { ", +config" } else { "" });
                }
            }
            return Ok(());
        }
        let opts = mira::install::rollback::RollbackOptions {
            version:    version.clone(),
            no_restart: *no_restart,
        };
        return mira::install::rollback::run_rollback(opts).map_err(|e| -> Box<dyn Error> {
            eprintln!("mira rollback: {e}");
            "rollback failed".into()
        });
    }

    // Managed-deps subcommands — fetch + verify pinned native libs
    // (ONNX Runtime today). Pure filesystem + network, no MIRA runtime.
    if let Some(Command::Deps { action }) = args.command.as_ref() {
        let cmd = match action {
            DepsAction::Install { force } =>
                mira::install::deps::DepsCommand::Install { force: *force },
            DepsAction::Verify  => mira::install::deps::DepsCommand::Verify,
            DepsAction::List    => mira::install::deps::DepsCommand::List,
        };
        return mira::install::deps::run(cmd).map_err(|e| -> Box<dyn Error> {
            eprintln!("mira deps: {e}");
            "deps command failed".into()
        });
    }

    // Skill author tools — pure filesystem ops, no config / runtime needed.
    if let Some(Command::Skill { action }) = args.command.as_ref() {
        // Secret management is a separate path because it needs the
        // running data dir (to find the SecretsStore master key + DB).
        if let SkillAction::Secret { action: secret_action } = action {
            return run_skill_secret(secret_action, args.config.clone())
                .map_err(|e| -> Box<dyn Error> {
                    eprintln!("mira skill secret: {e}");
                    "skill secret command failed".into()
                });
        }
        // Bundled-skill refresh — same lookup as the boot path.
        if let SkillAction::RefreshBundled { force, id } = action {
            return run_skill_refresh_bundled(*force, id.as_deref(), args.config.clone())
                .map_err(|e| -> Box<dyn Error> {
                    eprintln!("mira skill refresh-bundled: {e}");
                    "skill refresh-bundled failed".into()
                });
        }
        let cli_cmd = match action {
            SkillAction::Init     { id, out }       => mira::skills::cli::SkillCliCommand::Init     { id: id.clone(), out: out.clone() },
            SkillAction::Validate { path }          => mira::skills::cli::SkillCliCommand::Validate { path: path.clone() },
            SkillAction::Keygen   { out }           => mira::skills::cli::SkillCliCommand::Keygen   { out: out.clone() },
            SkillAction::Sign     { path, key }     => mira::skills::cli::SkillCliCommand::Sign     { path: path.clone(), key: key.clone() },
            SkillAction::Package  { path, out }     => mira::skills::cli::SkillCliCommand::Package  { path: path.clone(), out: out.clone() },
            SkillAction::Secret { .. } | SkillAction::RefreshBundled { .. } =>
                unreachable!("handled above"),
        };
        return mira::skills::cli::run(cli_cmd).map_err(|e| -> Box<dyn Error> {
            eprintln!("mira skill: {e}");
            "skill command failed".into()
        });
    }

    dotenvy::dotenv().ok();

    // Load config up-front so logging can be initialised before Gateway build.
    let config = Arc::new(MiraConfig::load(args.config.clone()).unwrap_or_else(|e| {
        eprintln!("ERROR: {}", e);
        eprintln!();
        eprintln!("Fix the issue above and restart MIRA.");
        eprintln!("Run `mira --print-config-template` to see an annotated example config.");
        std::process::exit(1);
    }));

    init_logging(&config);
    info!("Starting MIRA v{}", env!("CARGO_PKG_VERSION"));

    // ── Subcommand dispatch ───────────────────────────────────────────────────
    if let Some(command) = args.command {
        return match command {
            Command::Sandbox { action } => run_sandbox_command(&config, action).await,
            Command::Tts     { action } => run_tts_command(&config, action).await,
            Command::Wiki    { action } => run_wiki_command(&config, action),
            Command::Bench   { action } => run_bench_command(&config, action).await,
            Command::GuardianWatch => mira::guardian_sentinel::run(std::sync::Arc::clone(&config))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>),
            // Install/Uninstall, service-control, upgrade, and skill commands
            // are all dispatched before config load above.
            Command::Install { .. }
            | Command::Setup { .. }
            | Command::Uninstall
            | Command::Start | Command::Stop | Command::Restart | Command::Status
            | Command::Upgrade { .. }
            | Command::Rollback { .. }
            | Command::Skill   { .. }
            | Command::Deps    { .. }
            | Command::PkgExec { .. }
            | Command::CtrRun  { .. }
            | Command::HelperDaemon  { .. }
            | Command::HelperStatus  { .. }
            | Command::HelperInstall { .. }
            | Command::WslHostAliasInstall { .. }
            | Command::HelperNetallow { .. }
            | Command::HelperNetteardown { .. }
            | Command::GuardianInstall { .. }
            | Command::GuardianUninstall => unreachable!(),
        };
    }

    // ── Server mode ───────────────────────────────────────────────────────────
    if args.server {
        // R1: after an upgrade, clear a sidelined Windows binary (`mira.exe.old`),
        // and refuse to boot if THIS binary is too old to safely read data a
        // newer one migrated — clear guidance beats a cryptic crash / corruption.
        mira::install::binary_upgrade::cleanup_sidelined_binary();
        let data_dir = config.data_dir_path();
        if let Err(msg) = mira::install::data_version::guard(&data_dir) {
            eprintln!("ERROR: {msg}");
            std::process::exit(1);
        }

        let mut cfg = (*config).clone();
        if let Some(port) = args.port { cfg.server.port = port; }
        if let Some(host) = args.host.clone() { cfg.server.host = host; }
        let gateway = GatewayBuilder::new()
            .with_config(Arc::new(cfg))
            .build()
            .await?;
        // Migrations have run by now — record that this version owns the data.
        mira::install::data_version::stamp(&data_dir);
        gateway.run_until_shutdown().await?;
        return Ok(());
    }

    // ── Simple CLI branch (needs AgentCore) ───────────────────────────────────
    if args.simple {
        let gateway = GatewayBuilder::new()
            .with_config(Arc::clone(&config))
            .build()
            .await?;
        let user_id = gateway.auth_service.as_ref()
            .and_then(|a| a.current_admin_user_id().ok().flatten())
            .unwrap_or_else(|| "local-user".to_owned());
        run_simple_cli(gateway.agent_core, gateway.config, user_id).await?;
        return Ok(());
    }

    // ── TUI branch: resolve mode first, only build Gateway for Local ──────────
    let inputs = tui::mode::inputs_from(&config, args.local, args.server_url.as_deref());
    let mode   = tui::mode::resolve_tui_mode(&inputs, &config.tui.auto_token_path)
        .map_err(|e| -> Box<dyn Error> { e.into() })?;

    let backend_label: &'static str = match &mode {
        tui::mode::TuiMode::Local        => "local",
        tui::mode::TuiMode::Server { .. } => "server",
    };

    let ui_config = tui::TuiUiConfig {
        layout:        tui::layout::LayoutMode::from_str(
                           &args.layout.unwrap_or_else(|| config.tui.layout.clone())),
        theme_name:    args.theme.unwrap_or_else(|| config.tui.theme.clone()),
        backend_label: backend_label.to_string(),
    };

    let backend: Arc<dyn tui::backend::TuiBackend> = match mode {
        tui::mode::TuiMode::Local => {
            info!("TUI mode: local (direct AgentCore)");
            let gateway = GatewayBuilder::new()
                .with_config(Arc::clone(&config))
                .build()
                .await?;
            let session_id = format!("tui-{}", uuid::Uuid::new_v4());
            // Shell access = admin. Stamp conversations with the admin user
            // so the Web UI sidebar shows TUI history to the same account.
            let user_id = gateway.auth_service.as_ref()
                .and_then(|a| a.current_admin_user_id().ok().flatten())
                .unwrap_or_else(|| "local-user".to_owned());
            Arc::new(tui::backend::local::LocalBackend::new(
                gateway.agent_core,
                gateway.history,
                session_id,
                user_id,
                Arc::clone(&config),
            ))
        }
        tui::mode::TuiMode::Server { url, token_source } => {
            info!("TUI mode: server → {}", url);
            let token = load_token(&token_source)?;
            Arc::new(tui::backend::server::ServerBackend::new(url, token)
                .map_err(|e| -> Box<dyn Error> { e.into() })?)
        }
    };

    tui::run(backend, Arc::clone(&config), ui_config).await?;

    Ok(())
}

// `$HOME`, used as the default `WorkingDirectory=` in the systemd unit.
// Falls back to `/` if HOME is somehow unset — the service will still
// start; MIRA's tilde-expansion handles the rest.
fn default_home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

// Resolve a `TokenSource` into the bearer string the ServerBackend uses.
// Reading the token file here (rather than inside the backend) lets us
// fail fast with a clear error before the alternate-screen terminal
// takes over.
fn load_token(src: &tui::mode::TokenSource) -> Result<String, Box<dyn Error>> {
    match src {
        tui::mode::TokenSource::Env(tok) => Ok(tok.clone()),
        tui::mode::TokenSource::TokenFile(path) => {
            let expanded = tui::mode::expand_token_path(path);
            std::fs::read_to_string(&expanded)
                .map(|s| s.trim().to_owned())
                .map_err(|e| -> Box<dyn Error> {
                    format!(
                        "Cannot read local TUI token at {}: {}. \
                         Start `mira --server` first, or set MIRA_TOKEN, or pass --local.",
                        expanded.display(), e,
                    ).into()
                })
        }
    }
}

// ── Sandbox subcommand ────────────────────────────────────────────────────────

// `mira skill secret <set|list|delete>` — manage the encrypted vault.
// Synchronous (no tokio runtime needed) so it stays runnable in the
// pre-config CLI dispatch block of `main`.
fn run_skill_secret(
    action: &SkillSecretAction,
    config_override: Option<std::path::PathBuf>,
) -> Result<(), Box<dyn Error>> {
    use mira::skills::{SecretScope, SecretsStore};
    use mira::skills::secrets::default_paths;
    use rpassword::prompt_password;
    use std::io::Read;

    // We don't need the full MiraConfig, just data_dir. Load it the
    // same way the gateway does so the path resolves identically.
    let cfg = MiraConfig::load(config_override).map_err(|e| -> Box<dyn Error> {
        format!("config load: {e}").into()
    })?;
    let data_dir = cfg.data_dir_path();
    let (db, key) = default_paths(&data_dir);
    let store = SecretsStore::open(&db, &key)
        .map_err(|e| -> Box<dyn Error> { format!("open vault: {e}").into() })?;

    fn parse_scope(raw: Option<&String>) -> Result<(SecretScope, String), String> {
        match raw.map(String::as_str) {
            None | Some("system") => Ok((SecretScope::System, String::new())),
            Some(s) if s.starts_with("user:") => {
                let id = s["user:".len()..].trim();
                if id.is_empty() {
                    Err("scope `user:` needs a user id (e.g. `user:alice` or `user:<uuid>`)".into())
                } else {
                    Ok((SecretScope::User, id.to_string()))
                }
            }
            Some(other) => Err(format!(
                "unknown scope {other:?}. Use `system` or `user:<id>`."
            )),
        }
    }

    match action {
        SkillSecretAction::Set { skill, key: skey, scope, from_stdin } => {
            let (scope_kind, scope_id) = parse_scope(scope.as_ref())?;
            let value = if *from_stdin {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)
                    .map_err(|e| -> Box<dyn Error> { format!("stdin read: {e}").into() })?;
                // Allow trailing newline from `echo "x" | …`.
                buf.trim_end_matches(['\n', '\r']).to_string()
            } else {
                prompt_password(format!("Value for {skill}.{skey}: "))
                    .map_err(|e| -> Box<dyn Error> { format!("prompt: {e}").into() })?
            };
            if value.is_empty() {
                return Err("refusing to store an empty value (use `delete` to remove)".into());
            }
            store.set(scope_kind, &scope_id, skill, skey, &value)
                .map_err(|e| -> Box<dyn Error> { format!("set: {e}").into() })?;
            println!(
                "✓ stored {skill}.{skey} (scope={}{}) — value redacted",
                scope_kind.as_str(),
                if scope_id.is_empty() { String::new() } else { format!(":{scope_id}") },
            );
            Ok(())
        }
        SkillSecretAction::List { skill, scope } => {
            let (scope_kind, scope_id) = parse_scope(scope.as_ref())?;
            let entries = store.list(scope_kind, &scope_id, skill)
                .map_err(|e| -> Box<dyn Error> { format!("list: {e}").into() })?;
            if entries.is_empty() {
                println!("(no secrets registered for {skill} in scope={}{})",
                    scope_kind.as_str(),
                    if scope_id.is_empty() { String::new() } else { format!(":{scope_id}") },
                );
            } else {
                for e in entries {
                    let when = chrono::DateTime::<chrono::Utc>::from_timestamp(e.updated_at, 0)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M:%SZ").to_string())
                        .unwrap_or_default();
                    println!("  {:30}  updated_at={when}", e.key);
                }
            }
            Ok(())
        }
        SkillSecretAction::Delete { skill, key: skey, scope } => {
            let (scope_kind, scope_id) = parse_scope(scope.as_ref())?;
            let removed = store.delete(scope_kind, &scope_id, skill, skey)
                .map_err(|e| -> Box<dyn Error> { format!("delete: {e}").into() })?;
            if removed {
                println!("✓ deleted {skill}.{skey}");
            } else {
                println!("(nothing to delete: {skill}.{skey} was not registered)");
            }
            Ok(())
        }
    }
}

// `mira skill refresh-bundled [--force] [--id <skill_id>]`
fn run_skill_refresh_bundled(
    force:           bool,
    only_id:         Option<&str>,
    config_override: Option<std::path::PathBuf>,
) -> Result<(), Box<dyn Error>> {
    use mira::skills::bundled::RefreshOutcome;

    let cfg = MiraConfig::load(config_override).map_err(|e| -> Box<dyn Error> {
        format!("config load: {e}").into()
    })?;
    let skills_dir = mira::skills::default_skills_dir(&cfg.data_dir_path());
    let report = mira::skills::bundled::extract_or_refresh(&skills_dir, force)
        .map_err(|e| -> Box<dyn Error> { format!("refresh: {e}").into() })?;

    let mut printed = 0usize;
    for (id, outcome) in &report {
        if let Some(filter) = only_id {
            if id != filter { continue; }
        }
        match outcome {
            RefreshOutcome::Extracted        => println!("✓ {id}: extracted"),
            RefreshOutcome::Refreshed { from, to } =>
                println!("✓ {id}: refreshed {from} → {to}"),
            RefreshOutcome::Forced { from, to } =>
                println!("✓ {id}: force-refreshed {from} → {to}"),
            RefreshOutcome::UpToDate         => println!("· {id}: up-to-date"),
            RefreshOutcome::Skipped { reason } =>
                println!("- {id}: skipped ({reason})"),
        }
        printed += 1;
    }
    if printed == 0 {
        if let Some(filter) = only_id {
            return Err(format!("no bundled skill matches id={filter:?}").into());
        }
        println!("(nothing to refresh — no bundled skills found)");
    }
    println!();
    println!("Restart MIRA to load the refreshed manifests:");
    println!("  systemctl --user restart mira    # systemd installs");
    println!("  mira restart                      # if you ran `mira install`");
    Ok(())
}

async fn run_bench_command(config: &Arc<MiraConfig>, action: BenchAction) -> Result<(), Box<dyn Error>> {
    match action {
        BenchAction::Memory { dataset, limit, all, question_type, answer_provider, judge_provider, model, extract_model, out, dry_run } => {
            let opts = mira::bench::MemoryBenchOptions {
                dataset,
                limit: if all { None } else { Some(limit) },
                question_type,
                answer_provider,
                judge_provider,
                model,
                extract_model,
                out,
                dry_run,
            };
            mira::bench::run::run_memory_bench(opts, Arc::clone(config)).await?;
            Ok(())
        }
        BenchAction::Context { turns, avg_msg_chars, context_length, system_tokens, max_response_tokens, safety_margin, out } => {
            let opts = mira::bench::ContextBenchOptions {
                turns, avg_msg_chars, context_length, system_tokens, max_response_tokens, safety_margin, out,
            };
            mira::bench::context::run_context_bench(opts, Arc::clone(config)).await?;
            Ok(())
        }
    }
}

fn run_wiki_command(config: &MiraConfig, action: WikiAction) -> Result<(), Box<dyn Error>> {
    match action {
        WikiAction::McpServe { user_id, data_dir } => {
            if !config.wiki.enabled {
                eprintln!("mira wiki mcp-serve: wiki feature is disabled in config (wiki.enabled = false)");
                return Err("wiki disabled".into());
            }
            if !config.wiki.mcp.enabled {
                eprintln!("mira wiki mcp-serve: MCP server is disabled (wiki.mcp.enabled = false)");
                return Err("mcp disabled".into());
            }
            let dd = data_dir.unwrap_or_else(|| config.data_dir_path());
            // The MCP loop is synchronous and reads / writes stdio. Logs
            // go to stderr only — every byte on stdout must be a JSON-RPC
            // frame the client can parse.
            mira::wiki::mcp::run_stdio(&dd, &user_id)
                .map_err(|e| -> Box<dyn Error> { format!("mcp serve failed: {e}").into() })
        }
        WikiAction::RebuildProfile { user_id } => run_rebuild_profile(config, user_id),
    }
}

fn run_rebuild_profile(
    config:  &MiraConfig,
    user_id: Option<String>,
) -> Result<(), Box<dyn Error>> {
    use mira::auth::LocalAuthService;
    use mira::memory::MemorySystem;
    use mira::wiki::{GitPolicy, WikiRegistry};
    use mira::tools::onboarding::rebuild_wiki_profile;

    if !config.wiki.enabled {
        eprintln!("mira wiki rebuild-profile: wiki is disabled in config (wiki.enabled = false)");
        return Err("wiki disabled".into());
    }
    let data_dir = config.data_dir_path();

    // jwt_secret is only consulted on token sign/verify paths, which
    // this command never hits — a placeholder is fine. We deliberately
    // don't call the gateway's `ensure_jwt_secret` to avoid mutating
    // the config file as a side effect of a read-mostly CLI.
    let auth = LocalAuthService::new(
        &data_dir.join("auth.db"),
        "cli-rebuild-profile".into(),
        config.security.session_days,
    ).map_err(|e| -> Box<dyn Error> { format!("auth open failed: {e}").into() })?;

    let memory = MemorySystem::new_keyword_only(data_dir.join("memory.db"))
        .map_err(|e| -> Box<dyn Error> { format!("memory open failed: {e}").into() })?;

    let mut wiki_reg = WikiRegistry::new(data_dir.clone());
    if config.wiki.git.enabled {
        wiki_reg = wiki_reg.with_git(GitPolicy {
            auto_commit: config.wiki.git.auto_commit,
        });
    }

    let users: Vec<String> = match user_id {
        Some(id) => vec![id],
        None => auth.list_users()
            .map_err(|e| -> Box<dyn Error> { format!("list users: {e}").into() })?
            .into_iter()
            .filter_map(|u| {
                // Only users with a profile row are candidates — others
                // have nothing to mirror.
                match auth.get_profile(&u.id) {
                    Ok(Some(_)) => Some(u.id),
                    _           => None,
                }
            })
            .collect(),
    };

    if users.is_empty() {
        println!("No users with a profile row found — nothing to rebuild.");
        return Ok(());
    }

    let mut failures = 0;
    for uid in &users {
        println!("Rebuilding profile.md for user '{uid}'…");
        match rebuild_wiki_profile(&auth, &memory, &wiki_reg, &data_dir, uid) {
            Ok(s) => {
                println!(
                    "  personal_details={}  sections={}  about_me_seeds={}",
                    s.personal_details,
                    if s.sections.is_empty() { "(none)".into() } else { s.sections.join(", ") },
                    s.about_me_seed_count,
                );
            }
            Err(e) => {
                eprintln!("  failed: {e}");
                failures += 1;
            }
        }
    }
    if failures > 0 {
        return Err(format!("{failures} user(s) failed; see errors above").into());
    }
    println!("Done. Rebuilt profile.md for {} user(s).", users.len());
    Ok(())
}

async fn run_sandbox_command(
    config: &MiraConfig,
    action: SandboxAction,
) -> Result<(), Box<dyn Error>> {
    use mira::sandbox::rootfs::{RootfsManager, PYTHON};

    let manager = RootfsManager::new(&config.data_dir_path());

    match action {
        SandboxAction::Status => {
            let entries = manager.list();
            if entries.is_empty() {
                println!("No sandbox rootfs installed.");
                println!("Install python with:  mira sandbox install python");
                println!("(target: python {} from python-build-standalone)", PYTHON.version);
            } else {
                println!("Installed sandbox rootfs:");
                for e in entries {
                    println!(
                        "  {:8}  {}    {:>7.1} MB    {}",
                        e.language,
                        e.version,
                        e.size_bytes as f64 / (1024.0 * 1024.0),
                        e.path.display(),
                    );
                }
            }
        }
        SandboxAction::Probe => {
            let backend = mira::sandbox::default_backend();
            println!("Sandbox backend: {}", backend.name());
            println!("Reports supported: {}", backend.supported());

            let userns_ok = std::path::Path::new("/proc/self/ns/user").exists();
            println!("/proc/self/ns/user present:    {}", userns_ok);

            match std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
                Ok(s) => println!("unprivileged_userns_clone:    {}",  s.trim()),
                Err(_) => println!("unprivileged_userns_clone:    (sysctl absent — typical on newer kernels, usually fine)"),
            }
            match std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns") {
                Ok(s) => println!("apparmor_restrict_userns:     {} (1 means AppArmor is blocking unprivileged userns — Ubuntu 24.04 default)", s.trim()),
                Err(_) => {}
            }
            if !backend.supported() {
                eprintln!();
                eprintln!("Backend reports unsupported on this host. `code_run` will fail.");
                std::process::exit(1);
            }
        }
        SandboxAction::Install { language, force } => {
            match language.as_str() {
                "python" => {
                    let path = manager.install_python(force).await?;
                    println!("Installed python {} at {}", PYTHON.version, path.display());
                }
                other => {
                    eprintln!("Unknown sandbox language: {other}");
                    eprintln!("Currently supported: python");
                    std::process::exit(1);
                }
            }
        }
        SandboxAction::Uninstall { language } => {
            match language.as_str() {
                "python" => {
                    if manager.uninstall_python()? {
                        println!("Removed python rootfs from {}", manager.python_root().display());
                    } else {
                        println!("python rootfs not installed; nothing to do.");
                    }
                }
                other => {
                    eprintln!("Unknown sandbox language: {other}");
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

// ── TTS subcommand ────────────────────────────────────────────────────────────

async fn run_tts_command(
    config: &MiraConfig,
    action: TtsAction,
) -> Result<(), Box<dyn Error>> {
    use mira::tts::{PiperBackend, PiperConfig, TtsService};

    let svc = TtsService::from_config(config);

    match action {
        TtsAction::Probe { backend } => {
            let id = backend.clone().unwrap_or_else(|| svc.resolve_backend(None, None));
            println!("Backend: {id}");
            match svc.probe(backend.as_deref()).await {
                Ok(p) => {
                    println!("Healthy: {}", p.healthy);
                    if let Some(ms) = p.latency_ms { println!("Latency: {ms} ms"); }
                    if let Some(n)  = p.note       { println!("Note: {n}"); }
                    if !p.healthy { std::process::exit(1); }
                }
                Err(e) => {
                    eprintln!("✗ Probe failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        TtsAction::Voices { backend } => {
            let voices = svc.list_voices(backend.as_deref()).await?;
            if voices.is_empty() {
                println!("No voices available for this backend.");
            } else {
                println!("{:<28} {:<10} {:<8} {:<10} {}", "ID", "LANG", "GENDER", "DOWNLOAD", "NAME");
                for v in voices {
                    println!(
                        "{:<28} {:<10} {:<8} {:<10} {}",
                        v.id,
                        v.language,
                        v.gender.unwrap_or_else(|| "-".into()),
                        if v.is_downloaded { "yes" } else { "no" },
                        v.name,
                    );
                }
            }
        }
        TtsAction::DownloadVoice { voice_id } => {
            // Voice downloads are a Piper-specific concern — wire a backend
            // directly so we can call `ensure_voice` without going through the
            // trait-erased service.
            let mut piper_cfg = PiperConfig::under_data_dir(&config.data_dir_path());
            if !config.tts.internal.voices_dir.is_empty() {
                piper_cfg.voices_dir = mira::config::expand_path(&config.tts.internal.voices_dir);
            }
            if !config.tts.internal.binary_path.is_empty() {
                piper_cfg.binary_path = Some(mira::config::expand_path(&config.tts.internal.binary_path));
            }
            piper_cfg.auto_download = true;
            let piper = PiperBackend::new(piper_cfg);
            println!("Downloading voice {voice_id}…");
            match piper.ensure_voice_path(&voice_id).await {
                Ok(p) => println!("✓ Voice ready at {}", p.display()),
                Err(e) => {
                    eprintln!("✗ Failed to download voice: {e}");
                    eprintln!("  (Only voices in the curated manifest can be auto-downloaded.)");
                    std::process::exit(1);
                }
            }
        }
        TtsAction::Say { text, voice, speed, backend, output } => {
            let buf = match svc.speak(
                &text,
                voice.as_deref(),
                speed,
                None,
                backend.as_deref(),
                Some("cli"),
            ).await {
                Ok(b)  => b,
                Err(e) => {
                    eprintln!("✗ Synthesise failed: {e}");
                    std::process::exit(1);
                }
            };

            let path = output.unwrap_or_else(||
                std::path::PathBuf::from(format!("mira-tts.{}", buf.codec.extension())));
            std::fs::write(&path, &buf.bytes)?;
            println!("✓ Wrote {} bytes to {}", buf.bytes.len(), path.display());
            println!("  Codec: {}", buf.codec.content_type());
        }
        TtsAction::Cache { action } => match action {
            TtsCacheAction::Stats => {
                let s = svc.cache_stats().await;
                let mb = s.total_bytes as f64 / (1024.0 * 1024.0);
                println!("Entries:    {}", s.entries);
                println!("Total size: {} bytes ({:.2} MB)", s.total_bytes, mb);
            }
            TtsCacheAction::Clear => {
                svc.cache_clear().await?;
                println!("✓ TTS cache cleared.");
            }
        },
        TtsAction::McpServe => {
            // run_stdio is a blocking loop that owns its own tokio runtime
            // (synthesis is async). Run it on a dedicated OS thread so it
            // isn't nested inside this command's async runtime.
            let cfg = config.clone();
            let res = std::thread::spawn(move || mira::tts::mcp::run_stdio(&cfg))
                .join()
                .map_err(|_| -> Box<dyn Error> { "tts mcp-serve thread panicked".into() })?;
            res?;
        }
    }
    Ok(())
}

// ── Logging setup ─────────────────────────────────────────────────────────────

fn init_logging(config: &MiraConfig) {
    // Shared with the Windows service entry (install::windows::service_main) so
    // both write logs the same way and the Logs page always has a file to tail.
    mira::log_filter::init_to_file(&config.logging.level, &config.log_file_path());
}

// ── Simple reedline CLI (--simple flag) ───────────────────────────────────────
//
// Legacy interactive mode. Uses the old Agent/ActiveProvider stack directly so
// the rich command set (/memory-*, /tool-run, /provider-use, etc.) continues to
// work without rewriting those features against AgentCore.

use std::io::Write as _;
use tracing::{debug, warn};
use reedline::{DefaultCompleter, DefaultHinter, DefaultValidator, FileBackedHistory, Reedline, Signal};
use mira::{SimpleAgent, ChatMessage};
use mira::{SessionStore, ContextSummarizer};
use mira::providers::{ModelProvider, lmstudio::LmStudioProvider, openrouter::OpenRouterProvider, local::OllamaProvider};
use mira::tools::ToolArgs;
use mira::agent::AgentCore;
use serde_json::json;

const C_BRIGHT_YELLOW: &str = "\x1B[93m";
const C_YELLOW:        &str = "\x1B[33m";
const C_LIGHT_BLUE:    &str = "\x1B[94m";
const C_RESET:         &str = "\x1B[0m";

macro_rules! sys_println {
    ()          => { println!() };
    ($($a:tt)*) => { println!("{}{}{}", C_BRIGHT_YELLOW, format!($($a)*), C_RESET) };
}
macro_rules! cmd_println {
    ()          => { println!() };
    ($($a:tt)*) => { println!("{}{}{}", C_YELLOW, format!($($a)*), C_RESET) };
}

// Runtime provider handle — allows switching without `Box<dyn>` in the hot path.
enum ActiveProvider {
    LmStudio(LmStudioProvider),
    OpenRouter(OpenRouterProvider),
    Ollama(OllamaProvider),
}

impl ActiveProvider {
    async fn generate_stream_to_stdout(
        &self,
        messages: &[ChatMessage],
        options: &mira::GenerationOptions,
        spinner_stop: Arc<std::sync::atomic::AtomicBool>,
        response_color: &'static str,
    ) -> Result<String, String> {
        let clear_spinner = {
            let flag = spinner_stop.clone();
            let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
            move || {
                if !once.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    print!("\r\x1B[K{}", response_color);
                    std::io::stdout().flush().ok();
                }
            }
        };
        match self {
            ActiveProvider::LmStudio(p) => {
                let cs = clear_spinner.clone();
                let mut on_tok = move |tok: String| { cs(); print!("{}", tok); std::io::stdout().flush().ok(); };
                p.generate_stream(messages, options, &mut on_tok).await.map(|r| r.content).map_err(|e| e.to_string())
            }
            ActiveProvider::OpenRouter(p) => {
                let cs = clear_spinner.clone();
                let mut on_tok = move |tok: String| { cs(); print!("{}", tok); std::io::stdout().flush().ok(); };
                p.generate_stream(messages, options, &mut on_tok).await.map(|r| r.content).map_err(|e| e.to_string())
            }
            ActiveProvider::Ollama(p) => {
                let result = p.generate(messages, options).await;
                clear_spinner();
                result.map(|r| { print!("{}", r.content); std::io::stdout().flush().ok(); r.content }).map_err(|e| e.to_string())
            }
        }
    }

    fn display_name(&self) -> String {
        match self {
            ActiveProvider::LmStudio(p)  => format!("LM Studio ({})", p.model_name()),
            ActiveProvider::OpenRouter(p) => format!("OpenRouter ({})", p.model_name()),
            ActiveProvider::Ollama(p)    => format!("Ollama ({})", p.model_name()),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for ActiveProvider {
    fn name(&self) -> &str {
        match self {
            ActiveProvider::LmStudio(_)   => "lmstudio",
            ActiveProvider::OpenRouter(_) => "openrouter",
            ActiveProvider::Ollama(_)     => "ollama",
        }
    }

    async fn generate(
        &self,
        messages: &[ChatMessage],
        options: &mira::GenerationOptions,
    ) -> Result<mira::GenerationResponse, mira::MiraError> {
        match self {
            ActiveProvider::LmStudio(p)   => p.generate(messages, options).await,
            ActiveProvider::OpenRouter(p) => p.generate(messages, options).await,
            ActiveProvider::Ollama(p)     => p.generate(messages, options).await,
        }
    }

    async fn health_check(&self) -> bool {
        match self {
            ActiveProvider::LmStudio(p)   => p.health_check().await,
            ActiveProvider::OpenRouter(p) => p.health_check().await,
            ActiveProvider::Ollama(p)     => p.health_check().await,
        }
    }
}

async fn run_simple_cli(
    core:    Arc<AgentCore>,
    config:  Arc<MiraConfig>,
    user_id: String,
) -> Result<(), Box<dyn Error>> {
    let lmstudio_url   = config.providers.lmstudio.url.clone();
    let lmstudio_model = config.providers.lmstudio.default_model.clone();
    let ollama_url     = config.providers.ollama.url.clone();
    let ollama_model   = config.providers.ollama.default_model.clone();
    let openrouter_api_key  = config.providers.openrouter.api_key.clone();
    let openrouter_model    = config.providers.openrouter.default_model.clone();
    let tool_round_cap = config.agent.max_tool_round_tokens;
    let response_cap   = config.agent.max_response_tokens;

    // Start signal-cli daemon if configured.
    let _signal_daemon = if config.channels.signal.enabled {
        if let Some(ref phone) = config.channels.signal.phone_number {
            use mira::providers::signal_cli::daemon::SignalCliDaemon;
            let mut daemon = SignalCliDaemon::new(
                config.channels.signal.cli_binary.clone(),
                phone.clone(),
                config.channels.signal.rest_port,
                config.channels.signal.data_dir.clone(),
            );
            match daemon.start(15).await {
                Ok(()) => {
                    info!("✓ signal-cli daemon started on port {}", config.channels.signal.rest_port);
                    sys_println!("✓ Signal integration active (port {})", config.channels.signal.rest_port);
                }
                Err(e) => {
                    eprintln!("⚠ Failed to start signal-cli daemon: {}", e);
                    eprintln!("  Signal integration will be unavailable.");
                }
            }
            Some(daemon)
        } else {
            warn!("Signal enabled but no phone_number in config — skipping daemon start");
            None
        }
    } else {
        None
    };

    sys_println!();
    for line in mira::banner::render("simple").lines() {
        sys_println!("{}", line);
    }
    sys_println!();
    sys_println!("Your life's loyal partner. Always ready to assist.");
    sys_println!("Running in simple CLI mode (--simple). Use without --simple for the rich TUI.");
    sys_println!("/help for commands · /quit to exit");
    sys_println!();

    // Re-use memory, tools, sessions from the shared AgentCore.
    let memory_system  = Arc::clone(&core.memory);
    let tool_registry  = Arc::clone(&core.tools);
    let session_store  = Arc::clone(&core.sessions);

    let cli_session_id = format!("cli-{}", uuid::Uuid::new_v4());
    let mut cli_session = session_store
        .get_or_create(cli_session_id.clone(), user_id.clone(), "cli".to_string())
        .await;
    let _cleanup_task = SessionStore::start_cleanup_task(
        Arc::clone(&session_store),
        config.session.cleanup_interval_secs,
    );

    let summarizer = ContextSummarizer::new();
    let mut agent  = SimpleAgent::new(
        "You are MIRA (Multi-tasking Intelligent Responsive Assistant), a helpful AI assistant. \n\nYour ethos: 'Your life's loyal partner. Always ready to assist.'\n\nBe concise but thorough in your responses. Show reasoning when appropriate. Remember context from earlier in the conversation."
    );

    let mut active_provider = ActiveProvider::LmStudio(
        LmStudioProvider::new(lmstudio_url.clone(), lmstudio_model.clone())
            .with_token_caps(tool_round_cap, response_cap)
    );

    if let ActiveProvider::LmStudio(ref p) = active_provider {
        if p.health_check().await {
            info!("✓ LM Studio is available (primary)");
        } else {
            eprintln!("⚠ Warning: LM Studio appears to be unavailable at {}", lmstudio_url);
        }
    }
    match &openrouter_api_key {
        Some(_) => info!("✓ OpenRouter configured as fallback"),
        None    => warn!("OpenRouter not configured (no OPENROUTER_API_KEY)"),
    }

    // Reedline setup
    let history_path = config.data_dir_path().join("history.txt");
    std::fs::create_dir_all(history_path.parent().unwrap()).ok();

    let commands: Vec<String> = vec![
        "/help", "/quit", "/exit", "/bye", "/clear", "/new",
        "/ctx", "/tokens", "/export ", "/version",
        "/model-list", "/model-use ",
        "/memory-store ", "/memory-search ", "/memory-list", "/memory-delete ",
        "/tool-list", "/tool-run ",
        "/session-info", "/session-summary", "/session-clear",
        "/provider-list", "/provider-use ",
        "/signal-setup",
    ].into_iter().map(|s| s.to_string()).collect();

    let mut dc = DefaultCompleter::with_inclusions(&['/', '-']).set_min_word_len(1);
    dc.insert(commands);

    use reedline::{ColumnarMenu, Emacs, KeyCode, KeyModifiers, MenuBuilder, ReedlineEvent, ReedlineMenu};
    let completion_menu = Box::new(ColumnarMenu::default().with_name("completion_menu"));
    let mut keybindings = reedline::default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE, KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );

    let mut line_editor = Reedline::create()
        .with_history(Box::new(FileBackedHistory::with_file(1000, history_path)?))
        .with_completer(Box::new(dc))
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_hinter(Box::new(DefaultHinter::default()))
        .with_validator(Box::new(DefaultValidator));

    let prompt = reedline::DefaultPrompt::new(
        reedline::DefaultPromptSegment::Basic("MIRA".to_string()),
        reedline::DefaultPromptSegment::Empty,
    );

    loop {
        let sig = match line_editor.read_line(&prompt) {
            Ok(s)  => s,
            Err(e) => { eprintln!("Input error: {}", e); break; }
        };

        let input = match sig {
            Signal::Success(ref s) => s.trim(),
            Signal::CtrlC          => continue,
            Signal::CtrlD          => break,
        };

        if input.is_empty() { continue; }

        match input.to_lowercase().as_str() {
            "/quit" | "/exit" | "/bye" | "quit" | "exit" | ":q" => {
                info!("User exited. Final context: {} messages, ~{} tokens",
                     agent.message_count(), agent.token_estimate());
                break;
            }
            "/help" | "help" | ":h" => {
                cmd_println!();
                cmd_println!("Available commands:");
                cmd_println!("  /help              - Show this help message");
                cmd_println!("  /quit              - Exit MIRA  (also: /exit, /bye)");
                cmd_println!("  /clear             - Clear screen");
                cmd_println!("  /new               - Start new conversation (clear context)");
                cmd_println!("  /ctx               - Show context info (messages, tokens, model)");
                cmd_println!("  /tokens            - Show current token estimate");
                cmd_println!("  /version           - Show MIRA version");
                cmd_println!("  /export [file]     - Export conversation to markdown (default: mira-export.md)");
                cmd_println!();
                cmd_println!("Provider / model commands:");
                cmd_println!("  /provider-list         - List available providers");
                cmd_println!("  /provider-use <name>   - Switch provider (lmstudio/openrouter/ollama)");
                cmd_println!("  /model-list            - List available models");
                cmd_println!("  /model-use <idx>       - Switch model by index");
                cmd_println!("  Current: {}", active_provider.display_name());
                cmd_println!();
                cmd_println!("Memory commands:");
                cmd_println!("  /memory-store <text>   - Store a memory");
                cmd_println!("  /memory-search <query> - Keyword search memories");
                cmd_println!("  /memory-list           - List all memories");
                cmd_println!("  /memory-delete <id>    - Delete memory by id");
                cmd_println!();
                cmd_println!("Session commands:");
                cmd_println!("  /session-info          - Show current session info");
                cmd_println!("  /session-summary       - Generate conversation summary");
                cmd_println!("  /session-clear         - Clear session history");
                cmd_println!();
                cmd_println!("Tool commands:");
                cmd_println!("  /tool-run <name> [args] - Run a tool");
                cmd_println!("  /tool-list              - List available tools");
                cmd_println!();
                cmd_println!("Tips:");
                cmd_println!("  - Press Tab for autocomplete suggestions");
                cmd_println!("  - Use Up/Down arrows for command history");
                cmd_println!();
            }
            "/clear" | "clear" | ":c" => {
                print!("\x1B[2J\x1B[1;1H");
            }
            "/new" => {
                agent.reset();
                sys_println!("✓ New conversation started. Context cleared.");
            }
            "/ctx" => {
                cmd_println!();
                cmd_println!("Context Information:");
                cmd_println!("  Messages: {}", agent.message_count());
                cmd_println!("  Estimated tokens: ~{}", agent.token_estimate());
                cmd_println!("  System prompt: {} chars", agent.context.system_prompt.len());
                cmd_println!("  Current model: {}", active_provider.display_name());
                if let Ok(count) = memory_system.count() {
                    cmd_println!("  Stored memories: {}", count);
                }
                cmd_println!();
            }
            _ if input.starts_with("/provider-list") => {
                cmd_println!();
                cmd_println!("Available providers:");
                cmd_println!("  [0] lmstudio   — LM Studio  ({})", lmstudio_url);
                cmd_println!("  [1] openrouter — OpenRouter (api.openrouter.ai)");
                cmd_println!("  [2] ollama     — Ollama     ({})", ollama_url);
                cmd_println!();
                cmd_println!("Active: {}", active_provider.display_name());
                cmd_println!("Use '/provider-use <name>' or '/model-use <index>' to switch.");
            }
            _ if input.starts_with("/provider-use ") => {
                let name = input[14..].trim().to_lowercase();
                match name.as_str() {
                    "lmstudio" | "lm-studio" | "lm_studio" | "0" => {
                        active_provider = ActiveProvider::LmStudio(
                            LmStudioProvider::new(lmstudio_url.clone(), lmstudio_model.clone())
                                .with_token_caps(tool_round_cap, response_cap)
                        );
                        sys_println!("✓ Switched to: {}", active_provider.display_name());
                    }
                    "openrouter" | "open-router" | "1" => {
                        let key = openrouter_api_key.clone().unwrap_or_default();
                        if key.is_empty() {
                            eprintln!("✗ No OpenRouter API key.");
                        } else {
                            active_provider = ActiveProvider::OpenRouter(OpenRouterProvider::new(key, openrouter_model.clone()));
                            sys_println!("✓ Switched to: {}", active_provider.display_name());
                        }
                    }
                    "ollama" | "2" => {
                        active_provider = ActiveProvider::Ollama(OllamaProvider::new(ollama_url.clone(), ollama_model.clone()));
                        sys_println!("✓ Switched to: {}", active_provider.display_name());
                    }
                    _ => eprintln!("✗ Unknown provider '{}'. Use /provider-list to see options.", name),
                }
            }
            _ if input.starts_with("/model-list") => {
                cmd_println!();
                cmd_println!("Available models:");
                let models = [
                    format!("LM Studio  — {} @ {}", lmstudio_model, lmstudio_url),
                    format!("OpenRouter — {} @ openrouter.ai", openrouter_model),
                    format!("Ollama     — {} @ {}", ollama_model, ollama_url),
                ];
                let tags = ["LM Studio", "OpenRouter", "Ollama"];
                for (i, m) in models.iter().enumerate() {
                    let marker = if active_provider.display_name().contains(tags[i]) { "✓" } else { " " };
                    cmd_println!("  [{}] {} {}", i, marker, m);
                }
                cmd_println!();
                cmd_println!("Use '/model-use <index>' to switch.");
            }
            _ if input.starts_with("/model-use ") => {
                match input[11..].trim().parse::<usize>() {
                    Ok(0) => {
                        active_provider = ActiveProvider::LmStudio(
                            LmStudioProvider::new(lmstudio_url.clone(), lmstudio_model.clone())
                                .with_token_caps(tool_round_cap, response_cap)
                        );
                        sys_println!("✓ Switched to: {}", active_provider.display_name());
                    }
                    Ok(1) => {
                        let key = openrouter_api_key.clone().unwrap_or_default();
                        if key.is_empty() {
                            eprintln!("✗ No OpenRouter API key.");
                        } else {
                            active_provider = ActiveProvider::OpenRouter(OpenRouterProvider::new(key, openrouter_model.clone()));
                            sys_println!("✓ Switched to: {}", active_provider.display_name());
                        }
                    }
                    Ok(2) => {
                        active_provider = ActiveProvider::Ollama(OllamaProvider::new(ollama_url.clone(), ollama_model.clone()));
                        sys_println!("✓ Switched to: {}", active_provider.display_name());
                    }
                    Ok(_) => eprintln!("✗ Invalid model index. Use '/model-list' to see options."),
                    Err(_) => eprintln!("✗ Invalid syntax. Usage: /model-use <index>"),
                }
            }
            _ if input.starts_with("/memory-store ") => {
                match memory_system.store_auto(input[14..].to_string()).await {
                    Ok(id) => sys_println!("✓ Memory stored (id={})", id),
                    Err(e) => eprintln!("✗ Failed to store memory: {}", e),
                }
            }
            _ if input.starts_with("/memory-search ") => {
                let results = memory_system.search(&input[15..]);
                if results.is_empty() {
                    cmd_println!("No memories found matching '{}'", &input[15..]);
                } else {
                    cmd_println!("Found {} memory(ies):", results.len());
                    for (i, mem) in results.iter().enumerate().take(10) {
                        cmd_println!("  {}. [{}] {}", i + 1, mem.category, mem.content);
                    }
                }
            }
            _ if input.starts_with("/memory-semantic ") => {
                match memory_system.semantic_search(&input[17..], 5).await {
                    Ok(results) if results.is_empty() => cmd_println!("No semantically similar memories found."),
                    Ok(results) => {
                        cmd_println!("Found {} semantically similar memory(ies):", results.len());
                        for (i, (_id, content, score)) in results.iter().enumerate() {
                            cmd_println!("  {}. [similarity={:.2}] {}", i + 1, score, content);
                        }
                    }
                    Err(e) => eprintln!("✗ Semantic search failed: {}", e),
                }
            }
            _ if input == "/memory-list" => {
                let count = memory_system.count().unwrap_or(0);
                if count == 0 {
                    cmd_println!("No memories stored yet.");
                } else {
                    cmd_println!("Total: {} memories", count);
                    for cat in [mira::Category::Fact, mira::Category::Preference,
                               mira::Category::Skill, mira::Category::Relationship,
                               mira::Category::Project] {
                        let items = memory_system.get_by_category(&cat);
                        if !items.is_empty() {
                            cmd_println!();
                            cmd_println!("  {}: {} item(s)", cat, items.len());
                            for mem in items.iter().take(5) {
                                cmd_println!("    - {}", mem.content);
                            }
                        }
                    }
                }
            }
            _ if input.starts_with("/memory-delete ") => {
                match input[15..].trim().parse::<u64>() {
                    Ok(id) => match memory_system.delete(id).await {
                        Ok(true)  => sys_println!("✓ Memory {} deleted.", id),
                        Ok(false) => sys_println!("✗ No memory with id {} found.", id),
                        Err(e)    => eprintln!("✗ Delete failed: {}", e),
                    },
                    Err(_) => eprintln!("✗ Invalid id. Usage: /memory-delete <id>"),
                }
            }
            _ if input == "/session-clear" => {
                agent.reset();
                cli_session = session_store
                    .get_or_create(format!("cli-{}", uuid::Uuid::new_v4()), user_id.clone(), "cli".to_string())
                    .await;
                sys_println!("✓ Session cleared. Conversation history reset.");
            }
            _ if input == "/session-info" => {
                cmd_println!();
                cmd_println!("Session Information:");
                cmd_println!("  Active sessions: {}", session_store.len().await);
                cmd_println!("  Max turns per session: {}", config.session.max_turns);
                cmd_println!("  Session timeout: {}s", config.session.timeout_secs);
                cmd_println!();
            }
            _ if input == "/session-summary" => {
                let messages = agent.context.messages_vec();
                if messages.len() < 10 {
                    cmd_println!("Not enough conversation turns to summarize (need at least 10, have {})", messages.len());
                } else {
                    sys_println!("Generating summary of {} turns...", messages.len());
                    match summarizer.summarize(&active_provider, &messages).await {
                        Ok(summary) => {
                            cmd_println!();
                            cmd_println!("=== Conversation Summary ===");
                            cmd_println!("{}", summary.summary);
                            cmd_println!("============================");
                            cmd_println!("Turns summarized: {}", summary.turn_count);
                            cmd_println!("Compression ratio: {:.2}x", summary.compression_ratio());
                        }
                        Err(e) => eprintln!("Failed to generate summary: {}", e),
                    }
                }
            }
            _ if input.starts_with("/tool-list") => {
                let tools = tool_registry.list_tools();
                if tools.is_empty() {
                    cmd_println!("No tools registered.");
                } else {
                    cmd_println!("Available tools:");
                    for name in &tools {
                        if let Some(tool) = tool_registry.get(name) {
                            cmd_println!("  - {}: {}", name, tool.description());
                        }
                    }
                }
            }
            _ if input.starts_with("/tool-run ") => {
                let rest  = &input[10..];
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.is_empty() {
                    cmd_println!("Usage: /tool-run <name> [json_args]");
                    continue;
                }
                let tool_name = parts[0];
                let args_str  = if parts.len() > 1 { parts[1..].join(" ") } else { "{}".to_string() };
                let args: ToolArgs = serde_json::from_str(&args_str).unwrap_or_else(|_| json!({}));
                match tool_registry.execute(tool_name, args).await {
                    Ok(result) => {
                        if result.success {
                            cmd_println!("Output:\n{}", result.output);
                        } else {
                            eprintln!("Error: {:?}", result.error);
                        }
                    }
                    Err(e) => eprintln!("Failed to execute tool: {}", e),
                }
            }
            "/version" => {
                sys_println!("MIRA v{}", env!("CARGO_PKG_VERSION"));
            }
            "/tokens" => {
                cmd_println!();
                cmd_println!("Token Usage:");
                cmd_println!("  Messages in context : {}", agent.message_count());
                cmd_println!("  Estimated tokens    : ~{}", agent.token_estimate());
                cmd_println!("  System prompt chars : {}", agent.context.system_prompt.len());
                cmd_println!();
            }
            _ if input.starts_with("/export") => {
                let path = input[7..].trim();
                let path = if path.is_empty() { "mira-export.md" } else { path };
                match std::fs::File::create(path) {
                    Err(e) => eprintln!("✗ Could not create '{}': {}", path, e),
                    Ok(mut f) => {
                        writeln!(f, "# MIRA Conversation Export\n").ok();
                        for msg in &agent.context.messages_vec() {
                            let role = match msg.role {
                                mira::types::MessageRole::User      => "User",
                                mira::types::MessageRole::Assistant => "Assistant",
                                mira::types::MessageRole::System    => "System",
                                mira::types::MessageRole::Tool      => "Tool",
                            };
                            writeln!(f, "**{}**: {}\n", role, msg.content).ok();
                        }
                        sys_println!("✓ Exported {} messages to '{}'", agent.message_count(), path);
                    }
                }
            }
            _ if input == "/signal-setup" => {
                use mira::providers::signal_cli::daemon::generate_config_snippet;
                use std::io::{stdin, BufRead};
                sys_println!();
                sys_println!("=== Signal-CLI Setup ===");
                print!("{}  Your Signal phone number (E.164, e.g. +15551234567): {}", C_BRIGHT_YELLOW, C_RESET);
                std::io::stdout().flush()?;
                let mut phone = String::new();
                stdin().lock().read_line(&mut phone)?;
                let phone = phone.trim().to_string();
                let snippet = generate_config_snippet(
                    &phone,
                    config.channels.signal.rest_port,
                    &config.channels.signal.data_dir,
                    &config.channels.signal.cli_binary,
                );
                cmd_println!();
                cmd_println!("Add the following to your config file at {:?}:", config.config_path);
                cmd_println!();
                cmd_println!("{}", snippet);
                print!("{}Write this to config automatically? (y/N): {}", C_BRIGHT_YELLOW, C_RESET);
                std::io::stdout().flush()?;
                let mut confirm = String::new();
                stdin().lock().read_line(&mut confirm)?;
                if confirm.trim().to_lowercase() == "y" {
                    let mut new_config = (*config).clone();
                    new_config.channels.signal.enabled = true;
                    new_config.channels.signal.phone_number = Some(phone.clone());
                    if let Err(e) = new_config.save() {
                        eprintln!("✗ Failed to save config: {}", e);
                    } else {
                        sys_println!("✓ Config saved. Restart MIRA to activate Signal.");
                    }
                }
                sys_println!();
            }
            _ => {
                // Retrieve relevant memories (semantic, fallback to keyword).
                let relevant_memories: Vec<mira::MemoryItem> = match memory_system.semantic_search(input, 5).await {
                    Ok(results) => results.into_iter().map(|(id, content, score)| mira::MemoryItem {
                        id, content, category: mira::Category::Fact,
                        tags: vec![], source: None,
                        created_at: chrono::Utc::now(), relevance_score: score,
                        scope: mira::memory::storage::Scope::User,
                        scope_id: None, created_by: None,
                        supersedes: None, superseded_by: None,
                        strength: 1.0, effective_strength: score,
                        access_count: 0, last_reinforced: 0,
                        stability: "stable".into(),
                        source_channel: None,
                        source_conversation_id: None,
                        source_message_id: None,
                    }).collect(),
                    Err(_) => memory_system.search(input),
                };
                if !relevant_memories.is_empty() {
                    debug!("Retrieved {} relevant memories for query", relevant_memories.len());
                }

                agent.context.add_user_message(input);

                print!("\n");
                std::io::stdout().flush()?;

                let mut system_prompt = String::from(
                    "You are MIRA (Multi-tasking Intelligent Responsive Assistant), a helpful AI assistant. \n\nYour ethos: 'Your life's loyal partner. Always ready to assist.'\n\nBe concise but thorough in your responses."
                );
                if !relevant_memories.is_empty() {
                    system_prompt.push_str("\n\n--- Relevant Memories ---\n");
                    for mem in relevant_memories.iter().take(5) {
                        system_prompt.push_str(&format!("- [{}] {}\n", mem.category, mem.content));
                    }
                    system_prompt.push_str("Use these memories to inform your response when relevant.\n---\n");
                }

                let mut messages = vec![ChatMessage::system(system_prompt)];
                for msg in agent.context.messages_vec().iter().skip(1) {
                    messages.push(msg.clone());
                }

                let options = mira::GenerationOptions::default();
                let spinner_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let spinner_flag = spinner_stop.clone();
                let spinner_thread = std::thread::spawn(move || {
                    let dots = [".", "..", "..."];
                    let mut i = 0usize;
                    loop {
                        if spinner_flag.load(std::sync::atomic::Ordering::Relaxed) { break; }
                        print!("\r\x1B[K{}\x1B[1m**Thinking{}\x1B[22m{}", C_BRIGHT_YELLOW, dots[i % 3], C_RESET);
                        std::io::stdout().flush().ok();
                        i += 1;
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                });

                let result = active_provider
                    .generate_stream_to_stdout(&messages, &options, spinner_stop, C_LIGHT_BLUE)
                    .await;

                print!("{}", C_RESET);
                std::io::stdout().flush()?;
                spinner_thread.join().ok();

                match result {
                    Ok(response_content) => {
                        println!();
                        agent.context.add_assistant_message(&response_content);
                        info!("Context: {} messages, ~{} tokens", agent.message_count(), agent.token_estimate());

                        // Heuristic auto-extraction
                        {
                            let extractor = mira::HeuristicExtractor::new();
                            for candidate in mira::HeuristicExtractor::new().extract(input).iter().filter(|c| c.confidence >= 0.75) {
                                match memory_system.store_auto(candidate.content.clone()).await {
                                    Ok(id) => debug!("Auto-extracted memory {}: {}", id, candidate.content),
                                    Err(e) => debug!("Auto-extract store failed: {}", e),
                                }
                                let _ = extractor; // suppress unused warning
                            }
                        }

                        // LLM-based auto-extraction (background)
                        {
                            use mira::memory::auto_extract::LlmExtractor;
                            let recent_turns: Vec<(String, String)> = cli_session
                                .conversation_history.iter().rev().take(6).rev()
                                .map(|t| (t.role.clone(), t.content.clone())).collect();
                            if recent_turns.len() >= 2 {
                                let prompt_text = LlmExtractor::build_prompt(&recent_turns);
                                let lms_url   = lmstudio_url.clone();
                                let lms_model = lmstudio_model.clone();
                                let mem_sys   = Arc::clone(&memory_system);
                                let (tx, mut rx) = tokio::sync::oneshot::channel::<Vec<String>>();
                                tokio::spawn(async move {
                                    let provider = LmStudioProvider::new(lms_url, lms_model);
                                    let msgs = vec![
                                        ChatMessage::system("You are a concise memory extraction assistant."),
                                        ChatMessage::user(prompt_text),
                                    ];
                                    let opts = mira::GenerationOptions { temperature: 0.1, max_tokens: Some(256), ..Default::default() };
                                    if let Ok(resp) = provider.generate(&msgs, &opts).await {
                                        tx.send(LlmExtractor::parse_response(&resp.content)).ok();
                                    }
                                });
                                if let Ok(memories) = rx.try_recv() {
                                    for content in memories {
                                        mem_sys.store_auto(content).await.ok();
                                    }
                                }
                            }
                        }

                        cli_session.add_turn("user", input.to_string());
                        cli_session.add_turn("assistant", response_content.clone());
                        cli_session.truncate_history(50);
                        session_store.update(cli_session.clone()).await;

                        // Auto-summarize on context growth
                        if agent.token_estimate() > 4000 && agent.message_count() >= 10 {
                            info!("Context approaching limit (~{} tokens), triggering auto-summarization",
                                 agent.token_estimate());
                            let msgs = agent.context.messages_vec();
                            match summarizer.summarize(&active_provider, &msgs).await {
                                Ok(summary) => {
                                    info!("Auto-summary generated ({:.1}x compression ratio)", summary.compression_ratio());
                                    agent.reset();
                                    agent.context.add_assistant_message(
                                        format!("[Previous conversation summary: {}]", summary.summary));
                                }
                                Err(e) => warn!("Auto-summarization failed (will continue without): {}", e),
                            }
                        }
                    }
                    Err(e) => {
                        agent.context.messages.pop();
                        eprintln!("\nError: {}", e);
                        eprintln!("Troubleshooting:");
                        eprintln!("  1. Make sure LM Studio is running with Local Server enabled");
                        eprintln!("  2. Check that the model is loaded in LM Studio");
                        eprintln!("  3. Verify the endpoint {} is accessible", lmstudio_url);
                    }
                }
                println!();
            }
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tui::mode::TokenSource;

    #[test]
    fn load_token_from_env_returns_value_unchanged() {
        let src = TokenSource::Env("abc.xyz".to_string());
        assert_eq!(load_token(&src).unwrap(), "abc.xyz");
    }

    #[test]
    fn load_token_from_file_reads_and_trims() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.token");
        // Trailing newline simulates `echo > file` behaviour.
        std::fs::write(&path, "signed.jwt.value\n").unwrap();

        let src = TokenSource::TokenFile(path.to_string_lossy().into_owned());
        assert_eq!(load_token(&src).unwrap(), "signed.jwt.value");
    }

    #[test]
    fn load_token_from_missing_file_errors_with_hint() {
        let src = TokenSource::TokenFile("/definitely/does/not/exist.token".to_string());
        let err = load_token(&src).unwrap_err().to_string();
        assert!(err.contains("MIRA_TOKEN") || err.contains("--local"),
                "error should hint at remediation, got: {}", err);
    }
}
