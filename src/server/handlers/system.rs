// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/system.rs
//! System / host introspection + local-service management endpoints.
//!
//!   * `GET  /api/system/hardware`            → GPU + runtime probe + the
//!     resulting local-TTS recommendation (K2 / Q2 #10).
//!   * `GET  /api/system/chatterbox/status`   → Chatterbox server health +
//!     supervisor state (K3).
//!   * `POST /api/system/chatterbox/install`  → run the Windows one-click
//!     Chatterbox installer (K3, admin-only).
//!
//! The hardware + status reads are authenticated (any logged-in user) and
//! expose no secrets. The installer is admin-only since it runs a process.

use std::sync::Arc;
use std::time::Duration;

use axum::{extract::Extension, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use tracing::info;

use crate::auth::{AdminUser, AuthUser};
use crate::hardware;
use crate::tts::chatterbox::ChatterboxSupervisor;
use crate::web::LiveConfig;

/// Host hardware profile. Detection is memoised in `hardware::info`, so this
/// is cheap after the first call; the first call shells out to `nvidia-smi`
/// / `vulkaninfo`, hence the blocking offload.
pub async fn hardware_info(AuthUser(_): AuthUser) -> impl IntoResponse {
    let info = tokio::task::spawn_blocking(|| hardware::info().clone())
        .await
        .unwrap_or_else(|_| hardware::detect());
    Json(info)
}

/// Chatterbox health + supervisor state. When MIRA supervises the process the
/// `supervised` flag is true and the state reflects spawn/restart history;
/// otherwise it's a bare liveness probe against the configured port (covers a
/// Chatterbox the operator runs themselves, including a Windows-side one).
pub async fn chatterbox_status(
    AuthUser(_):           AuthUser,
    Extension(supervisor): Extension<Option<Arc<ChatterboxSupervisor>>>,
    Extension(cfg):        Extension<Arc<LiveConfig>>,
) -> impl IntoResponse {
    if let Some(sup) = supervisor {
        let st = sup.status().await;
        return Json(json!({
            "supervised": true,
            "running":    st.running,
            "healthy":    st.healthy,
            "starts":     st.starts,
            "pid":        st.pid,
            "last_error": st.last_error,
        }));
    }

    let snap = cfg.get().await;
    let cb   = &snap.tts.chatterbox;
    let healthy = if cb.enabled { probe_health(cb.port).await } else { false };
    Json(json!({
        "supervised": false,
        "running":    false,
        "healthy":    healthy,
        "enabled":    cb.enabled,
        "port":       cb.port,
    }))
}

async fn probe_health(port: u16) -> bool {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(3)).build() {
        Ok(c)  => c,
        Err(_) => return false,
    };
    client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Run the Chatterbox one-click installer (Windows / WSL2). Long-running —
/// the first launch downloads ~1.4 GB of model weights. Admin-only.
///
/// v1 awaits inline; if the 15-minute request budget proves awkward in
/// practice this should move to a job + status-poll like deps install.
pub async fn chatterbox_install(AdminUser(caller): AdminUser) -> impl IntoResponse {
    info!(user = %caller.username, "chatterbox: install requested");
    match crate::install::chatterbox::install().await {
        Ok(log) => (StatusCode::OK, Json(json!({ "ok": true, "log": log }))).into_response(),
        Err(e)  => (StatusCode::BAD_REQUEST, Json(json!({ "ok": false, "error": e }))).into_response(),
    }
}
