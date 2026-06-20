// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/update_check.rs
//
//! GET /api/admin/update-check — compare the running binary's
//! Cargo-stamped version against the latest release published to the
//! configured Releases API. Returns a tiny JSON so the admin UI can
//! render a banner without bundling semver logic.
//!
//! Off by default — `server.update_check.enabled` gates this entirely.
//! When disabled, the endpoint returns the disabled marker rather than
//! ever hitting the network.

use std::sync::Arc;
use std::time::Duration;

use axum::{Extension, Json};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::auth::AuthUser;
use crate::auth::Role;
use crate::config::MiraConfig;

/// One element of the GitLab Releases API response. We only need
/// `tag_name`; the field is also present in GitHub's release shape
/// under the same name so the same struct works for both forks.
#[derive(Debug, Deserialize)]
struct ReleaseEntry {
    tag_name: String,
    #[serde(default)]
    name:     Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    released_at: Option<String>,
    #[serde(rename = "_links")]
    #[serde(default)]
    links: Option<ReleaseLinks>,
}

#[derive(Debug, Deserialize)]
struct ReleaseLinks {
    #[serde(rename = "self")]
    #[serde(default)]
    self_url: Option<String>,
}

pub async fn update_check(
    AuthUser(me): AuthUser,
    Extension(cfg_arc): Extension<Arc<crate::web::LiveConfig>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Admin-only — surfacing a release URL to every user clutters the
    // chat UI and the upgrade action is admin-scoped anyway.
    if me.role != Role::Admin {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "admin only" })));
    }

    let cfg: Arc<MiraConfig> = cfg_arc.get().await;
    let current = env!("CARGO_PKG_VERSION").to_string();

    if !cfg.server.update_check.enabled || cfg.server.update_check.source_url.is_empty() {
        return (StatusCode::OK, Json(json!({
            "enabled":         false,
            "current":         current,
            "newer_available": false,
        })));
    }

    // Cheap HTTP — short timeout so a misconfigured / unreachable
    // Releases API doesn't hang the UI poll. We don't cache: the UI
    // is expected to poll on a long interval (hourly+) and the
    // request itself is single-digit KB.
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
            "error": format!("http client: {e}"),
        }))),
    };

    // GitHub's API rejects requests without a User-Agent (and ignores the
    // Accept header for non-GitHub sources), so set both unconditionally.
    let resp = match client.get(&cfg.server.update_check.source_url)
        .header("User-Agent", concat!("mira/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({
            "error": format!("upstream fetch: {e}"),
        }))),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!("update_check: upstream returned {status}: {body}");
        return (StatusCode::BAD_GATEWAY, Json(json!({
            "error": format!("upstream HTTP {status}"),
        })));
    }
    let releases: Vec<ReleaseEntry> = match resp.json().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({
            "error": format!("upstream parse: {e}"),
        }))),
    };

    let latest = releases.into_iter().next();
    let Some(latest) = latest else {
        // No releases on the source — treat as up-to-date so we don't
        // spam the banner. Operators see this when they haven't tagged
        // a v0.X.Y release yet on their fork.
        return (StatusCode::OK, Json(json!({
            "enabled":         true,
            "current":         current,
            "newer_available": false,
            "note":            "no releases published yet",
        })));
    };

    // Strip a leading `v` so `v0.146.0` parses the same as `0.146.0`.
    let latest_tag = latest.tag_name.trim_start_matches('v').to_string();
    let newer = compare_versions(&current, &latest_tag).is_lt();

    (StatusCode::OK, Json(json!({
        "enabled":         true,
        "current":         current,
        "latest":          latest_tag,
        "latest_name":     latest.name,
        "released_at":     latest.released_at,
        "release_url":     latest.links.and_then(|l| l.self_url),
        "description":     latest.description,
        "newer_available": newer,
    })))
}

/// POST /api/admin/upgrade — trigger an in-place binary upgrade (download →
/// verify signature → atomic swap → supervisor restart). Admin-only.
///
/// Returns 202 immediately; the work runs on a **detached OS thread** because
/// `run_binary_upgrade` uses `reqwest::blocking` (illegal inside the async
/// runtime) and ends by restarting the service (which replaces this process).
/// The UI shows an "upgrading…" state and reconnects once the new build is up.
///
/// Safety: the upgrade verifies the minisign signature against the embedded
/// public key *before* swapping, and only swaps after the new binary is fully on
/// disk — any failure (no release, bad signature, download error) leaves the
/// running binary untouched.
pub async fn upgrade(
    AuthUser(me): AuthUser,
    Extension(cfg_arc): Extension<Arc<crate::web::LiveConfig>>,
) -> (StatusCode, Json<serde_json::Value>) {
    if me.role != Role::Admin {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "admin only" })));
    }
    let cfg: Arc<MiraConfig> = cfg_arc.get().await;
    if !cfg.server.update_check.enabled {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "update checking is disabled (server.update_check.enabled = false)"
        })));
    }

    std::thread::spawn(|| {
        let opts = crate::install::binary_upgrade::BinaryUpgradeOptions {
            version: None,        // latest
            no_restart: false,
            force: false,
            provider: None,       // env/default selects the forge
            release_base_url: None,
            token: None,          // reads $MIRA_RELEASE_TOKEN if set
        };
        match crate::install::binary_upgrade::run_binary_upgrade(opts) {
            Ok(()) => tracing::info!("self-upgrade: completed (service restarting)"),
            Err(e) => tracing::error!("self-upgrade failed (running binary untouched): {e}"),
        }
    });

    (StatusCode::ACCEPTED, Json(json!({
        "status": "started",
        "message": "Upgrade started — MIRA will download, verify, swap, and restart. \
                    This page will reconnect shortly."
    })))
}

/// SemVer-aware comparison. `0.145.1` < `0.146.0` < `0.146.1`. Falls
/// back to string compare when either side isn't parseable — better
/// than panicking on a hand-edited tag.
fn compare_versions(current: &str, latest: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(current), semver::Version::parse(latest)) {
        (Ok(c), Ok(l)) => c.cmp(&l),
        _ => current.cmp(latest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compare_works_on_normal_releases() {
        assert!(compare_versions("0.145.1", "0.146.0").is_lt());
        assert!(compare_versions("0.146.0", "0.146.0").is_eq());
        assert!(compare_versions("0.147.0", "0.146.0").is_gt());
    }

    #[test]
    fn semver_compare_handles_garbage_input_with_string_fallback() {
        // Should not panic.
        let _ = compare_versions("not-a-version", "0.146.0");
        let _ = compare_versions("0.146.0", "wibble");
    }
}
