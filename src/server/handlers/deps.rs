// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/deps.rs
//! Admin endpoints for managed native dependencies: ONNX Runtime for
//! the `internal` embedding provider, and `signal-runtime` (signal-cli
//! + a bundled JRE) for the Signal channel.
//!
//! Powers the Settings page's "ONNX Runtime not installed" dialog —
//! the UI calls these on save when the user picks `provider=internal`
//! and the lib isn't on disk.
//!
//!   * `GET  /api/admin/deps`              → snapshot of every managed dep
//!   * `POST /api/admin/deps/{name}/install` → fetch + verify + extract
//!
//! Both are admin-only via the [`AdminUser`] extractor; the install
//! endpoint runs synchronously because the only managed dep today is
//! ~10 MB and finishes well inside the typical request budget. If we
//! later add multi-hundred-MB deps, switch this to a job queue with a
//! status endpoint.

use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use tracing::{info, warn};

use crate::auth::AdminUser;
use crate::install::deps;

#[derive(Debug, Deserialize, Default)]
pub struct InstallQuery {
    /// When true, re-download even if the lib is already present and
    /// the sha matches. Used to recover from a corrupted install.
    #[serde(default)]
    pub force: bool,
}

pub async fn list_deps(AdminUser(_): AdminUser) -> impl IntoResponse {
    match deps::list_status() {
        Ok(rows) => (StatusCode::OK, Json(rows)).into_response(),
        Err(e)   => (StatusCode::INTERNAL_SERVER_ERROR,
                     Json(serde_json::json!({ "error": format!("{e}") }))).into_response(),
    }
}

pub async fn install_dep(
    AdminUser(caller): AdminUser,
    Path(name):        Path<String>,
    body:              Option<Json<InstallQuery>>,
) -> impl IntoResponse {
    let force = body.map(|Json(q)| q.force).unwrap_or(false);
    info!(user = %caller.username, dep = %name, force, "deps: install requested");

    // `signal-runtime` is a convenience bundle: signal-cli + the JRE (where
    // needed), installed as a unit when the user enables the Signal channel.
    // It returns a textual summary rather than a fetched-bool.
    if name == "signal-runtime" {
        let result: Result<String, String> = tokio::task::spawn_blocking(
            move || deps::ensure_signal_runtime(force).map_err(|e| e.to_string())
        )
            .await
            .map_err(|e| format!("install thread panicked: {e}"))
            .and_then(|r| r);
        return match result {
            Ok(summary) => (StatusCode::OK, Json(serde_json::json!({
                "ok": true, "name": name, "message": summary,
            }))).into_response(),
            Err(e) => {
                warn!("deps: signal-runtime install failed: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                    "ok": false, "name": name, "error": e,
                }))).into_response()
            }
        };
    }

    // Run the blocking download/extract off the async runtime.
    // Stringify the error inside the closure so the JoinHandle is
    // Send (Box<dyn Error> isn't Send).
    let name_t = name.clone();
    let result: Result<bool, String> = tokio::task::spawn_blocking(
        move || deps::install_named(&name_t, force).map_err(|e| e.to_string())
    )
        .await
        .map_err(|e| format!("install thread panicked: {e}"))
        .and_then(|r| r);

    match result {
        Ok(true)  => (StatusCode::OK, Json(serde_json::json!({
            "ok": true, "name": name, "fetched": true,
            "message": "installed",
        }))).into_response(),
        Ok(false) => (StatusCode::OK, Json(serde_json::json!({
            "ok": true, "name": name, "fetched": false,
            "message": "already installed (sha matches)",
        }))).into_response(),
        Err(e) => {
            warn!("deps: install failed for {name}: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "ok": false, "name": name, "error": e,
            }))).into_response()
        }
    }
}
