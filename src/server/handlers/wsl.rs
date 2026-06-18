// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/wsl.rs
//! WSL host-URL misrouting — admin endpoints behind the "detect + one-click
//! suggest" UX. On WSL2 NAT, service URLs pointed at the Windows host's LAN IP
//! are unreachable; these surface that and offer a safe one-click swap to the
//! `windows-host` alias. See [`crate::wsl_net`].
//!
//!   GET  /api/wsl/host-url-check   — scan the live config, report misrouted URLs
//!   POST /api/wsl/fix-host-urls    — rewrite them to windows-host (LiveConfig::update)

use std::sync::Arc;

use axum::{extract::Extension, http::StatusCode, response::{IntoResponse, Response}, Json};

use crate::auth::{AuthUser, Role};

fn admin_only(caller: &AuthUser) -> Option<Response> {
    if caller.0.role != Role::Admin {
        Some((StatusCode::FORBIDDEN, "admin only").into_response())
    } else { None }
}

/// GET /api/wsl/host-url-check — is this WSL, is the windows-host alias set up,
/// and which configured URLs are misrouted (dead at their IP but reachable via
/// windows-host). The probes are blocking, so they run on a blocking thread.
pub async fn host_url_check(
    caller: AuthUser,
    Extension(live_cfg): Extension<Arc<crate::web::LiveConfig>>,
) -> Response {
    if let Some(r) = admin_only(&caller) { return r; }
    let cfg = live_cfg.get().await;
    let findings = tokio::task::spawn_blocking(move || crate::wsl_net::scan_misrouted(&cfg))
        .await.unwrap_or_default();
    (StatusCode::OK, Json(serde_json::json!({
        "is_wsl":   crate::wsl_net::is_wsl(),
        "findings": findings,
    }))).into_response()
}

/// POST /api/wsl/fix-host-urls — rewrite the misrouted URLs to windows-host via
/// the safe LiveConfig::update path. Re-scans server-side (never trusts a client
/// list) so only genuinely-misrouted URLs are touched. A restart is needed for
/// provider-chain changes to take effect.
pub async fn fix_host_urls(
    caller: AuthUser,
    Extension(live_cfg): Extension<Arc<crate::web::LiveConfig>>,
) -> Response {
    if let Some(r) = admin_only(&caller) { return r; }
    let cfg = live_cfg.get().await;
    let scan_cfg = Arc::clone(&cfg);
    let findings = tokio::task::spawn_blocking(move || crate::wsl_net::scan_misrouted(&scan_cfg))
        .await.unwrap_or_default();
    if findings.is_empty() {
        return (StatusCode::OK, Json(serde_json::json!({
            "changed": [], "note": "No misrouted Windows-host URLs found.",
        }))).into_response();
    }
    let new_cfg = match crate::wsl_net::apply_fixes(&cfg, &findings) {
        Ok(c)  => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR,
                          Json(serde_json::json!({ "error": e }))).into_response(),
    };
    match live_cfg.update(new_cfg).await {
        Ok(())  => (StatusCode::OK, Json(serde_json::json!({
            "changed": findings,
            "note": "Updated. Restart MIRA for the new provider URLs to take effect.",
        }))).into_response(),
        Err(e)  => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}
