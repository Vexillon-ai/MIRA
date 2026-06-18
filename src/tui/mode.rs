// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/mode.rs
//! Decide whether the TUI talks to `AgentCore` directly (Local) or to the
//! MIRA HTTP server (Server). Centralising this here keeps `main.rs` thin
//! and makes the decision matrix unit-testable.
//!
//! See `design-docs/phase6-tui-server-mode.md` for the full matrix.

use mira::config::{expand_path, MiraConfig};

// Sources of configuration that influence the mode choice.
// // Separating this from the global `MiraConfig` / CLI args lets us unit-test
// the decision matrix without touching disk.
pub struct ModeInputs<'a> {
    // `--local` flag set on CLI.
    pub cli_local:          bool,
    // `--server-url URL` value from CLI, if any.
    pub cli_server_url:     Option<&'a str>,
    // `tui.mode` value from config: expected `"auto" | "local" | "server"`.
    pub config_mode:        &'a str,
    // `tui.server_url` value from config.
    pub config_server_url:  &'a str,
    // `server.enabled` from config.
    pub server_enabled:     bool,
    // Contents of `MIRA_TOKEN` env, if present.
    pub env_token:          Option<String>,
}

// Decision returned by the resolver.
// // `Local` keeps the  direct-`AgentCore` path; `Server` holds the
// server URL for the HTTP client. Reachability probing happens at the
// caller (async context); the resolver itself is pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiMode {
    Local,
    Server { url: String, token_source: TokenSource },
}

// Where the TUI should look for its bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    // Read from `tui.auto_token_path`. Used for same-host server mode.
    TokenFile(String),
    // Use the value of `MIRA_TOKEN`. Used when the TUI connects to a remote
    // server (typically with `--server-url`).
    Env(String),
}

// Pure decision function. Returns `Err` with a user-facing message when the
// combination is invalid (e.g. both `--local` and `--server-url`).
pub fn resolve_tui_mode(
    inputs:             &ModeInputs,
    config_token_path:  &str,
) -> Result<TuiMode, String> {
    // 1. Flag conflicts — fail loud, don't guess.
    if inputs.cli_local && inputs.cli_server_url.is_some() {
        return Err(
            "--local and --server-url cannot be used together. Pick one.".into()
        );
    }

    // 2. Explicit CLI flags win over config.
    if inputs.cli_local {
        return Ok(TuiMode::Local);
    }
    if let Some(url) = inputs.cli_server_url {
        let token_source = token_source_for(inputs, config_token_path, /*remote=*/true);
        return Ok(TuiMode::Server {
            url:          url.to_string(),
            token_source,
        });
    }

    // 3. Config-driven choice.
    match inputs.config_mode {
        "local" => Ok(TuiMode::Local),

        "server" => Ok(TuiMode::Server {
            url:          inputs.config_server_url.to_string(),
            token_source: token_source_for(inputs, config_token_path, /*remote=*/false),
        }),

        "auto" => {
            if inputs.server_enabled {
                Ok(TuiMode::Server {
                    url:          inputs.config_server_url.to_string(),
                    token_source: token_source_for(inputs, config_token_path, /*remote=*/false),
                })
            } else {
                Ok(TuiMode::Local)
            }
        }

        other => Err(format!(
            "Invalid tui.mode '{}' in config. Use one of: auto, local, server.",
            other
        )),
    }
}

// Prefer `MIRA_TOKEN` env when connecting to a remote URL; otherwise use the
// token file the server mints at startup. Env always wins if present, so a
// user can point a local TUI at a different account by exporting the token.
fn token_source_for(
    inputs:             &ModeInputs,
    config_token_path:  &str,
    remote:             bool,
) -> TokenSource {
    if let Some(ref tok) = inputs.env_token {
        return TokenSource::Env(tok.clone());
    }
    if remote {
        // Remote with no env token — still fall back to token file, the
        // caller will surface a clear error on read failure.
        return TokenSource::TokenFile(config_token_path.to_string());
    }
    TokenSource::TokenFile(config_token_path.to_string())
}

// Convenience: build `ModeInputs` from CLI args and a loaded config.
pub fn inputs_from<'a>(
    config:          &'a MiraConfig,
    cli_local:       bool,
    cli_server_url:  Option<&'a str>,
) -> ModeInputs<'a> {
    ModeInputs {
        cli_local,
        cli_server_url,
        config_mode:        &config.tui.mode,
        config_server_url:  &config.tui.server_url,
        server_enabled:     config.server.enabled,
        env_token:          std::env::var("MIRA_TOKEN").ok(),
    }
}

// Expand `~` in a token path so callers can hand the result straight to
// `std::fs::read_to_string`.
pub fn expand_token_path(path: &str) -> std::path::PathBuf {
    expand_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base<'a>() -> ModeInputs<'a> {
        ModeInputs {
            cli_local:          false,
            cli_server_url:     None,
            config_mode:        "auto",
            config_server_url:  "http://127.0.0.1:8082",
            server_enabled:     false,
            env_token:          None,
        }
    }

    const TOK: &str = "~/.mira/data/local.token";

    #[test]
    fn cli_local_forces_local() {
        let mut i = base(); i.cli_local = true;
        assert_eq!(resolve_tui_mode(&i, TOK).unwrap(), TuiMode::Local);
    }

    #[test]
    fn cli_server_url_forces_server() {
        let mut i = base(); i.cli_server_url = Some("http://remote:9000");
        match resolve_tui_mode(&i, TOK).unwrap() {
            TuiMode::Server { url,.. } => assert_eq!(url, "http://remote:9000"),
            _ => panic!("expected server mode"),
        }
    }

    #[test]
    fn conflicting_flags_error() {
        let mut i = base();
        i.cli_local      = true;
        i.cli_server_url = Some("http://remote:9000");
        assert!(resolve_tui_mode(&i, TOK).is_err());
    }

    #[test]
    fn config_local_mode() {
        let mut i = base(); i.config_mode = "local";
        assert_eq!(resolve_tui_mode(&i, TOK).unwrap(), TuiMode::Local);
    }

    #[test]
    fn config_server_mode_uses_config_url() {
        let mut i = base(); i.config_mode = "server";
        match resolve_tui_mode(&i, TOK).unwrap() {
            TuiMode::Server { url,.. } => assert_eq!(url, "http://127.0.0.1:8082"),
            _ => panic!("expected server mode"),
        }
    }

    #[test]
    fn auto_with_server_enabled_picks_server() {
        let mut i = base(); i.config_mode = "auto"; i.server_enabled = true;
        matches!(resolve_tui_mode(&i, TOK).unwrap(), TuiMode::Server { .. });
    }

    #[test]
    fn auto_without_server_enabled_picks_local() {
        let mut i = base(); i.config_mode = "auto"; i.server_enabled = false;
        assert_eq!(resolve_tui_mode(&i, TOK).unwrap(), TuiMode::Local);
    }

    #[test]
    fn invalid_mode_errors() {
        let mut i = base(); i.config_mode = "bogus";
        assert!(resolve_tui_mode(&i, TOK).is_err());
    }

    #[test]
    fn env_token_overrides_file() {
        let mut i = base();
        i.config_mode = "server";
        i.env_token   = Some("xyz".to_string());
        match resolve_tui_mode(&i, TOK).unwrap() {
            TuiMode::Server { token_source: TokenSource::Env(t), .. } => assert_eq!(t, "xyz"),
            _ => panic!("expected env token source"),
        }
    }

    #[test]
    fn no_env_token_falls_back_to_file() {
        let mut i = base(); i.config_mode = "server"; i.env_token = None;
        match resolve_tui_mode(&i, TOK).unwrap() {
            TuiMode::Server { token_source: TokenSource::TokenFile(p), .. } => assert_eq!(p, TOK),
            _ => panic!("expected token-file source"),
        }
    }
}
