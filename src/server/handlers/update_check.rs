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

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{Extension, Json};
use axum::extract::Query;
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

/// Cached upstream result, refreshed at most once per `update_check.frequency`
/// (the UI's "Check now" forces a refresh with `?force=true`). Process-global —
/// shared by the HTTP handler and the background auto-check task.
#[derive(Clone)]
struct Cached {
    body:       serde_json::Value,
    checked_at: chrono::DateTime<chrono::Utc>,
}
static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
fn cache() -> &'static Mutex<Option<Cached>> { CACHE.get_or_init(|| Mutex::new(None)) }

#[derive(Debug, Deserialize)]
pub struct UpdateCheckQuery {
    /// `?force=true` bypasses the cache and refreshes immediately — the
    /// Settings "Check now" button.
    #[serde(default)]
    pub force: bool,
}

pub async fn update_check(
    AuthUser(me): AuthUser,
    Extension(cfg_arc): Extension<Arc<crate::web::LiveConfig>>,
    Query(q): Query<UpdateCheckQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Admin-only — surfacing a release URL to every user clutters the
    // chat UI and the upgrade action is admin-scoped anyway.
    if me.role != Role::Admin {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "admin only" })));
    }

    let cfg: Arc<MiraConfig> = cfg_arc.get().await;
    let current = env!("CARGO_PKG_VERSION").to_string();

    if !cfg.server.update_check.enabled || cfg.server.update_check.source_url.is_empty() {
        let mut body = json!({ "enabled": false, "current": current, "newer_available": false });
        merge(&mut body, upgrade_capabilities());
        return (StatusCode::OK, Json(body));
    }

    // Serve from cache while it's fresh and no force-refresh was requested. This
    // frequency-gates the upstream call: with many admins polling the banner we
    // still hit the Releases API at most once per configured interval.
    if !q.force {
        if let Some(c) = cache().lock().ok().and_then(|g| g.clone()) {
            let fresh = chrono::Utc::now()
                .signed_duration_since(c.checked_at)
                .to_std()
                .map(|age| age < cfg.server.update_check.refresh_interval())
                .unwrap_or(false);
            if fresh {
                return (StatusCode::OK, Json(with_meta(c.body, c.checked_at, true)));
            }
        }
    }

    // Stale or forced → fetch upstream. Only successful results are cached.
    match fetch_upstream(&cfg, &current).await {
        Ok(body) => {
            let now = chrono::Utc::now();
            if let Ok(mut g) = cache().lock() {
                *g = Some(Cached { body: body.clone(), checked_at: now });
            }
            (StatusCode::OK, Json(with_meta(body, now, false)))
        }
        Err((status, err)) => (status, Json(err)),
    }
}

/// Attach `last_checked` + `from_cache` + host upgrade capabilities so the UI
/// can render "checked N ago" and decide which action (upgrade / guidance) to
/// show. Capabilities are computed live (never cached) since they're cheap and
/// reflect the current host.
fn with_meta(mut body: serde_json::Value, checked_at: chrono::DateTime<chrono::Utc>, from_cache: bool) -> serde_json::Value {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("last_checked".into(), json!(checked_at.to_rfc3339()));
        obj.insert("from_cache".into(), json!(from_cache));
    }
    merge(&mut body, upgrade_capabilities());
    body
}

/// Shallow-merge `extra`'s object fields into `body` (both must be objects).
fn merge(body: &mut serde_json::Value, extra: serde_json::Value) {
    if let (Some(dst), Some(src)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in src { dst.insert(k.clone(), v.clone()); }
    }
}

/// How MIRA can update on THIS host, so the Settings card shows the right
/// action: an in-place "Upgrade now" where we can swap + restart, or platform
/// guidance where we can't (Docker → pull a new image; unsupervised → restart
/// manually).
fn upgrade_capabilities() -> serde_json::Value {
    use crate::install::{detect_host, supervisor_unit_path, HostKind};
    let host = detect_host();
    let service_managed = supervisor_unit_path().map(|p| p.exists()).unwrap_or(false);
    let host_kind = match host {
        HostKind::LinuxSystemdUser | HostKind::LinuxNoSystemdUser => "linux",
        HostKind::Docker  => "docker",
        HostKind::Macos   => "macos",
        HostKind::Windows => "windows",
        HostKind::Other   => "other",
    };
    let (can_self_upgrade, guidance): (bool, Option<&str>) = match host {
        HostKind::Docker => (false, Some(
            "This is a Docker install — pull the new image tag and recreate the container \
             (e.g. `docker compose pull && docker compose up -d`). MIRA can't rebuild its own image."
        )),
        HostKind::Other => (false, Some("Unrecognised host — upgrade manually with `mira upgrade`.")),
        _ if service_managed => (true, None),
        _ => (false, Some(
            "No managed service detected — MIRA can download + swap the binary but can't restart \
             itself. Run `mira upgrade` from a terminal, or restart MIRA afterwards."
        )),
    };
    json!({
        "host_kind":        host_kind,
        "service_managed":  service_managed,
        "can_self_upgrade": can_self_upgrade,
        "upgrade_guidance": guidance,
    })
}

/// GET /api/admin/rollback — list saved rollback snapshots (admin-only).
pub async fn rollback_list(AuthUser(me): AuthUser) -> (StatusCode, Json<serde_json::Value>) {
    if me.role != Role::Admin {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "admin only" })));
    }
    let snapshots: Vec<serde_json::Value> = crate::install::rollback::list_snapshots()
        .iter()
        .map(|s| json!({ "version": s.version, "has_config": s.config.is_some() }))
        .collect();
    (StatusCode::OK, Json(json!({
        "current":   env!("CARGO_PKG_VERSION"),
        "snapshots": snapshots,
    })))
}

#[derive(Debug, Deserialize)]
pub struct RollbackRequest {
    /// Version to roll back to. Omit for the most recent snapshot.
    #[serde(default)]
    pub version: Option<String>,
}

/// POST /api/admin/rollback — restore a snapshot (binary + config) and restart.
/// Detached thread + 202, same as `upgrade` (the restart replaces this process).
pub async fn rollback(
    AuthUser(me): AuthUser,
    Json(req): Json<RollbackRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if me.role != Role::Admin {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "admin only" })));
    }
    // Pre-validate so the client gets a real error instead of a silent 202.
    let snaps = crate::install::rollback::list_snapshots();
    if snaps.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "no rollback snapshots available — one is saved automatically on each upgrade"
        })));
    }
    if let Some(v) = req.version.as_deref() {
        let v = v.trim_start_matches('v');
        if !snaps.iter().any(|s| s.version == v) {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": format!("no snapshot for version {v}")
            })));
        }
    }

    let version = req.version.clone();
    std::thread::spawn(move || {
        let opts = crate::install::rollback::RollbackOptions { version, no_restart: false };
        match crate::install::rollback::run_rollback(opts) {
            Ok(())  => tracing::info!("rollback: completed (service restarting)"),
            Err(e)  => tracing::error!("rollback failed (binary untouched on error): {e}"),
        }
    });
    (StatusCode::ACCEPTED, Json(json!({
        "status":  "started",
        "message": "Rollback started — MIRA will restore the previous binary + config and restart. \
                    This page will reconnect shortly."
    })))
}

/// Hit the Releases API and build the comparison body. Returns `(status, err)`
/// on failure so callers can surface it without polluting the cache.
async fn fetch_upstream(
    cfg: &MiraConfig,
    current: &str,
) -> Result<serde_json::Value, (StatusCode, serde_json::Value)> {
    // Cheap HTTP — short timeout so a misconfigured / unreachable Releases API
    // doesn't hang the caller. GitHub rejects requests without a User-Agent.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": format!("http client: {e}") })))?;
    let resp = client.get(&cfg.server.update_check.source_url)
        .header("User-Agent", concat!("mira/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .send().await
        .map_err(|e| (StatusCode::BAD_GATEWAY, json!({ "error": format!("upstream fetch: {e}") })))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!("update_check: upstream returned {status}: {body}");
        return Err((StatusCode::BAD_GATEWAY, json!({ "error": format!("upstream HTTP {status}") })));
    }
    let releases: Vec<ReleaseEntry> = resp.json().await
        .map_err(|e| (StatusCode::BAD_GATEWAY, json!({ "error": format!("upstream parse: {e}") })))?;

    let Some(latest) = releases.into_iter().next() else {
        // No releases yet — treat as up-to-date rather than spamming the banner.
        return Ok(json!({
            "enabled":         true,
            "current":         current,
            "newer_available": false,
            "note":            "no releases published yet",
        }));
    };
    let latest_tag = latest.tag_name.trim_start_matches('v').to_string();
    let newer = compare_versions(current, &latest_tag).is_lt();
    Ok(json!({
        "enabled":         true,
        "current":         current,
        "latest":          latest_tag,
        "latest_name":     latest.name,
        "released_at":     latest.released_at,
        "release_url":     latest.links.and_then(|l| l.self_url),
        "description":     latest.description,
        "newer_available": newer,
    }))
}

/// Best-effort background refresh used by the frequency-gated auto-check task,
/// so the cache is warm (and the banner accurate) even before anyone polls.
pub async fn refresh_cache(cfg: &MiraConfig) {
    if !cfg.server.update_check.enabled || cfg.server.update_check.source_url.is_empty() {
        return;
    }
    let current = env!("CARGO_PKG_VERSION").to_string();
    if let Ok(body) = fetch_upstream(cfg, &current).await {
        if let Ok(mut g) = cache().lock() {
            *g = Some(Cached { body, checked_at: chrono::Utc::now() });
        }
    }
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
