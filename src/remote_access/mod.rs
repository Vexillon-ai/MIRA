// SPDX-License-Identifier: AGPL-3.0-or-later

// src/remote_access/mod.rs
//! Remote-access support — reach a self-hosted MIRA from the mobile app while
//! away from the LAN, without opening router ports.
//!
//! This module does three things: (1) hold the operator's configured remote
//! URL, (2) best-effort auto-detect a Tailscale tunnel URL when none is
//! configured (see [`tailscale`]), and (3) surface the effective remote URL +
//! setup status for the pairing QR and the admin UI. It opens **no** inbound
//! ports and embeds **no** VPN client — the operator still installs Tailscale
//! (or configures a tunnel) once; we remove the URL-typing and guesswork.
//!
//! Effective remote-URL precedence: explicit `server.remote_url` config
//! (validated) > Tailscale auto-detect > none.

pub mod tailscale;

use crate::config::MiraConfig;

/// The effective remote URL baked into pairing + shown in the admin UI:
/// configured (validated) first, else Tailscale auto-detect, else `None`.
pub async fn effective_remote_url(cfg: &MiraConfig) -> Option<String> {
    if let Some(u) = configured_remote_url(cfg) {
        return Some(u);
    }
    tailscale::detect_cached(cfg.server.port).await.derived_url
}

/// Full remote-access status for the admin surface (`GET /api/admin/remote-access`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RemoteStatus {
    /// The URL currently baked into pairing (config or auto-detected), if any.
    pub effective_url:      Option<String>,
    /// Where `effective_url` came from: `config` | `tailscale` | `none`.
    pub source:             &'static str,
    /// The operator-configured `remote_url` (validated + normalised), if any.
    pub configured_url:     Option<String>,
    /// Config held a non-empty `remote_url` that failed validation (UI can warn).
    pub configured_invalid: bool,
    /// Best-effort Tailscale detection.
    pub tailscale:          tailscale::TailscaleStatus,
    /// The local port MIRA serves on (for the copy-paste setup commands).
    pub mira_port:          u16,
}

/// Assemble the current remote-access status. `redetect` forces a fresh
/// Tailscale probe (bypassing the short cache).
pub async fn status(cfg: &MiraConfig, redetect: bool) -> RemoteStatus {
    let ts = if redetect {
        tailscale::detect(cfg.server.port).await
    } else {
        tailscale::detect_cached(cfg.server.port).await
    };
    let configured_url = configured_remote_url(cfg);
    let configured_invalid = cfg.server.remote_url.as_deref()
        .map(|s| !s.trim().is_empty() && !is_valid_remote_url(s))
        .unwrap_or(false);
    let (effective_url, source) = if let Some(u) = configured_url.clone() {
        (Some(u), "config")
    } else if let Some(u) = ts.derived_url.clone() {
        (Some(u), "tailscale")
    } else {
        (None, "none")
    };
    RemoteStatus {
        effective_url,
        source,
        configured_url,
        configured_invalid,
        tailscale: ts,
        mira_port: cfg.server.port,
    }
}

/// Validate an operator-supplied remote URL: it must be an absolute `http`/
/// `https` URL with a non-empty authority (host). Everything else — empty,
/// relative, `ftp://`, a bare host, `javascript:` — is rejected.
pub fn is_valid_remote_url(s: &str) -> bool {
    let s = s.trim();
    let rest = match s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")) {
        Some(r) => r,
        None    => return false,
    };
    // Authority ends at the first path/query/fragment delimiter. It must be
    // non-empty and must not start with a delimiter (e.g. `http:///path`).
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    !authority.trim().is_empty()
}

/// Trim + drop a trailing slash from a valid remote URL, or `None` if invalid.
pub fn normalize_remote_url(s: &str) -> Option<String> {
    let s = s.trim();
    is_valid_remote_url(s).then(|| s.trim_end_matches('/').to_string())
}

/// The operator-configured remote URL (validated + normalised), if set + valid.
/// An invalid value is treated as unset (the caller falls back to auto-detect).
pub fn configured_remote_url(cfg: &MiraConfig) -> Option<String> {
    cfg.server.remote_url.as_deref().and_then(normalize_remote_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_absolute_http_https_only() {
        assert!(is_valid_remote_url("https://mira.my-tailnet.ts.net"));
        assert!(is_valid_remote_url("http://mira.example.com:8080"));
        assert!(is_valid_remote_url("https://mira.example.com/"));
        // Rejected:
        assert!(!is_valid_remote_url(""));
        assert!(!is_valid_remote_url("   "));
        assert!(!is_valid_remote_url("mira.example.com"));       // no scheme
        assert!(!is_valid_remote_url("ftp://mira.example.com")); // wrong scheme
        assert!(!is_valid_remote_url("https://"));               // empty authority
        assert!(!is_valid_remote_url("http:///path"));           // empty authority
        assert!(!is_valid_remote_url("/relative/path"));
    }

    #[test]
    fn normalises_trailing_slash() {
        assert_eq!(
            normalize_remote_url("  https://mira.example.com/  ").as_deref(),
            Some("https://mira.example.com"),
        );
        assert_eq!(normalize_remote_url("not a url"), None);
    }
}
