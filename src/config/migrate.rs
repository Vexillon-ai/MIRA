// SPDX-License-Identifier: AGPL-3.0-or-later

// src/config/migrate.rs
//! Migration from the legacy `~/.mira/config.toml` format to the new
//! `~/.mira/config/mira_config.json` format.
//!
//! Called once during first-run when the old TOML file is detected.
//! On success the TOML file is removed.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{info, warn};

use crate::MiraError;
use super::MiraConfig;

// ── Legacy TOML structs ──────────────────────────────────────────────────────
// Mirrors the old `gateway/config.rs` structs well enough to deserialise the
// TOML.  Fields we no longer support are simply ignored via `#[allow(dead_code)]`.

#[derive(Debug, Deserialize, Default)]
struct LegacyConfig {
    #[serde(default)] data_dir: Option<String>,
    #[serde(default)] primary_provider: Option<String>,
    #[serde(default)] providers: LegacyProviders,
    #[serde(default)] cli: LegacyCli,
    #[serde(default)] logging: LegacyLogging,
    #[serde(default)] signal: LegacySignal,
    #[serde(default)] memory: LegacyMemory,
    #[serde(default)] session: LegacySession,
    #[serde(default)] tui: LegacyTui,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyProviders {
    #[serde(default)] local: LegacyLocal,
    #[serde(default)] lmstudio: LegacyLmStudio,
    #[serde(default)] openrouter: LegacyOpenRouter,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyLocal {
    #[serde(default)] ollama_url: Option<String>,
    #[serde(default)] default_model: Option<String>,
    #[serde(default)] timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyLmStudio {
    #[serde(default)] url: Option<String>,
    #[serde(default)] default_model: Option<String>,
    #[serde(default)] timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyOpenRouter {
    #[serde(default)] api_key: Option<String>,
    #[serde(default)] base_url: Option<String>,
    #[serde(default)] default_model: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyCli {
    #[serde(default)] colored_output: Option<bool>,
    #[serde(default)] streaming: Option<bool>,
    #[serde(default)] prompt: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyLogging {
    #[serde(default)] level: Option<String>,
    #[serde(default)] format: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacySignal {
    #[serde(default)] enabled: Option<bool>,
    #[serde(default)] phone_number: Option<String>,
    #[serde(default)] rest_port: Option<u16>,
    #[serde(default)] socket_path: Option<String>,
    #[serde(default)] cli_binary: Option<String>,
    #[serde(default)] data_dir: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyMemory {
    #[serde(default)] vector_backend: Option<String>,
    #[serde(default)] embedding_dim: Option<usize>,
    #[serde(default)] per_user_isolation: Option<bool>,
    #[serde(default)] share_across_channels: Option<bool>,
    #[serde(default)] similarity_threshold: Option<f64>,
    #[serde(default)] embedding_cache_size: Option<usize>,
    #[serde(default)] qdrant_url: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacySession {
    #[serde(default)] cleanup_interval_secs: Option<u64>,
    #[serde(default)] timeout_secs: Option<u64>,
    #[serde(default)] max_turns: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyTui {
    #[serde(default)] theme: Option<String>,
    #[serde(default)] layout: Option<String>,
    #[serde(default)] show_timestamps: Option<bool>,
    #[serde(default)] show_token_count: Option<bool>,
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Returns the path to the legacy TOML config if it exists.
pub fn find_legacy_toml() -> Option<PathBuf> {
    let path = dirs::home_dir()?.join(".mira").join("config.toml");
    if path.exists() { Some(path) } else { None }
}

/// Interactively prompt the user to migrate the TOML config.
///
/// Returns `Some(MiraConfig)` if the user accepted and migration succeeded,
/// `None` if the user declined or if migration failed (after printing a
/// warning).
pub fn prompt_and_migrate(toml_path: &Path, new_json_path: &Path) -> Option<MiraConfig> {
    eprintln!();
    eprintln!("─────────────────────────────────────────────────────────");
    eprintln!("  MIRA: Legacy configuration detected");
    eprintln!("─────────────────────────────────────────────────────────");
    eprintln!("  Found: {}", toml_path.display());
    eprintln!("  MIRA now uses a JSON config file:");
    eprintln!("    {}", new_json_path.display());
    eprintln!();
    eprintln!("  Migrate the old TOML file to the new JSON format?");
    eprintln!("  The old file will be removed after a successful migration.");
    eprint!("  [Y/n] > ");

    // Flush so the prompt appears before we block on stdin
    use std::io::Write;
    std::io::stderr().flush().ok();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        warn!("Could not read stdin during migration prompt — skipping migration");
        return None;
    }

    let answer = input.trim().to_lowercase();
    if answer == "n" || answer == "no" {
        eprintln!("  Skipping migration. A fresh default config will be created.");
        eprintln!("─────────────────────────────────────────────────────────");
        return None;
    }

    // User said yes (or just hit Enter)
    match migrate_toml_to_config(toml_path) {
        Ok(cfg) => {
            eprintln!("  ✓ Migration successful.");
            // Remove the old file
            if let Err(e) = std::fs::remove_file(toml_path) {
                warn!("Could not remove legacy config.toml: {}", e);
                eprintln!("  ⚠ Could not remove '{}': {}", toml_path.display(), e);
                eprintln!("    You may delete it manually.");
            } else {
                info!("Removed legacy config.toml at {:?}", toml_path);
                eprintln!("  ✓ Removed {}", toml_path.display());
            }
            eprintln!("─────────────────────────────────────────────────────────");
            Some(cfg)
        }
        Err(e) => {
            eprintln!("  ✗ Migration failed: {}", e);
            eprintln!("    A fresh default config will be created instead.");
            eprintln!("    Your old file has NOT been removed.");
            eprintln!("─────────────────────────────────────────────────────────");
            None
        }
    }
}

// ── Internal ─────────────────────────────────────────────────────────────────

fn migrate_toml_to_config(path: &Path) -> Result<MiraConfig, MiraError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| MiraError::ConfigError(format!("Cannot read legacy config: {}", e)))?;

    let legacy: LegacyConfig = toml::from_str(&content)
        .map_err(|e| MiraError::ConfigError(format!("Cannot parse legacy config.toml: {}", e)))?;

    // Map old "full" layout name to new "right-full"
    let layout = legacy.tui.layout
        .map(|l| if l == "full" { "right-full".to_string() } else { l })
        .unwrap_or_else(|| "standard".to_string());

    let mut cfg = MiraConfig::default();

    // data_dir
    if let Some(d) = legacy.data_dir {
        cfg.data_dir = d;
    }

    // primary_provider — old "local/ollama" → "ollama"
    if let Some(p) = legacy.primary_provider {
        cfg.primary_provider = match p.as_str() {
            "local/ollama" | "local" | "ollama" => "ollama".to_string(),
            "lmstudio" => "lmstudio".to_string(),
            "openrouter" => "openrouter".to_string(),
            other => {
                warn!("Unknown primary_provider '{}' in legacy config — defaulting to 'lmstudio'", other);
                "lmstudio".to_string()
            }
        };
    }

    // providers.ollama
    if let Some(u) = legacy.providers.local.ollama_url     { cfg.providers.ollama.url           = u; }
    if let Some(m) = legacy.providers.local.default_model  { cfg.providers.ollama.default_model  = m; }
    if let Some(t) = legacy.providers.local.timeout_secs   { cfg.providers.ollama.timeout_secs   = t; }

    // providers.lmstudio
    if let Some(u) = legacy.providers.lmstudio.url           { cfg.providers.lmstudio.url           = u; }
    if let Some(m) = legacy.providers.lmstudio.default_model { cfg.providers.lmstudio.default_model = m; }
    if let Some(t) = legacy.providers.lmstudio.timeout_secs  { cfg.providers.lmstudio.timeout_secs  = t; }

    // providers.openrouter
    cfg.providers.openrouter.api_key      = legacy.providers.openrouter.api_key;
    if let Some(u) = legacy.providers.openrouter.base_url     { cfg.providers.openrouter.base_url     = u; }
    if let Some(m) = legacy.providers.openrouter.default_model{ cfg.providers.openrouter.default_model= m; }

    // cli
    if let Some(v) = legacy.cli.colored_output { cfg.cli.colored_output = v; }
    if let Some(v) = legacy.cli.streaming      { cfg.cli.streaming      = v; }
    if let Some(v) = legacy.cli.prompt         { cfg.cli.prompt         = v; }

    // logging
    if let Some(v) = legacy.logging.level  { cfg.logging.level  = v; }
    if let Some(v) = legacy.logging.format { cfg.logging.format = v; }

    // channels.signal
    if let Some(v) = legacy.signal.enabled      { cfg.channels.signal.enabled      = v; }
    cfg.channels.signal.phone_number             = legacy.signal.phone_number;
    if let Some(v) = legacy.signal.rest_port     { cfg.channels.signal.rest_port    = v; }
    if let Some(v) = legacy.signal.socket_path   { cfg.channels.signal.socket_path  = v; }
    if let Some(v) = legacy.signal.cli_binary    { cfg.channels.signal.cli_binary   = v; }
    if let Some(v) = legacy.signal.data_dir      { cfg.channels.signal.data_dir     = v; }

    // memory
    if let Some(v) = legacy.memory.vector_backend       { cfg.memory.vector_backend       = v; }
    if let Some(v) = legacy.memory.embedding_dim        { cfg.memory.embedding_dim        = v; }
    if let Some(v) = legacy.memory.per_user_isolation   { cfg.memory.per_user_isolation   = v; }
    if let Some(v) = legacy.memory.share_across_channels{ cfg.memory.share_across_channels= v; }
    if let Some(v) = legacy.memory.similarity_threshold { cfg.memory.similarity_threshold = v as f32; }
    if let Some(v) = legacy.memory.embedding_cache_size { cfg.memory.embedding_cache_size = v; }
    if let Some(v) = legacy.memory.qdrant_url           { cfg.memory.qdrant_url           = v; }

    // session
    if let Some(v) = legacy.session.cleanup_interval_secs { cfg.session.cleanup_interval_secs = v; }
    if let Some(v) = legacy.session.timeout_secs          { cfg.session.timeout_secs          = v; }
    if let Some(v) = legacy.session.max_turns             { cfg.session.max_turns             = v; }

    // tui
    if let Some(v) = legacy.tui.theme           { cfg.tui.theme           = v; }
    cfg.tui.layout = layout;
    if let Some(v) = legacy.tui.show_timestamps  { cfg.tui.show_timestamps  = v; }
    if let Some(v) = legacy.tui.show_token_count { cfg.tui.show_token_count = v; }

    Ok(cfg)
}
