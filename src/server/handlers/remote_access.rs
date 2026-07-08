// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/remote_access.rs
//! `GET /api/admin/remote-access` — admin-only status + guided setup for
//! reaching this server remotely (Tailscale-first, but the `remote_url`
//! mechanism is generic). Returns the detected Tailscale state, the effective
//! remote URL currently baked into pairing, and copy-paste setup commands.
//!
//! Exposes **no** secrets (no pairing secrets, no tokens) and opens **no**
//! ports — it is detection + config + guidance only. Setting/overriding
//! `remote_url` goes through the normal `PUT /api/config` (`server.remote_url`).

use std::sync::Arc;

use axum::{extract::Query, Extension, Json};
use serde::{Deserialize, Serialize};

use crate::auth::AdminUser;
use crate::web::LiveConfig;

#[derive(Deserialize)]
pub struct RemoteAccessQuery {
    /// `?redetect=true` forces a fresh Tailscale probe, bypassing the cache.
    #[serde(default)]
    pub redetect: bool,
}

/// Copy-paste setup commands for the operator, parameterised by the local port.
#[derive(Serialize)]
struct SetupGuide {
    /// One-liner install (Linux + macOS share Tailscale's install script).
    install:       Vec<String>,
    /// Bring Tailscale up + front MIRA over HTTPS.
    enable:        Vec<String>,
    /// Extra manual steps that live in the Tailscale admin console.
    console_notes: Vec<String>,
    /// Windows install pointer (no install.sh there).
    windows_note:  String,
    docs:          String,
}

fn setup_guide(port: u16) -> SetupGuide {
    SetupGuide {
        install: vec![
            "curl -fsSL https://tailscale.com/install.sh | sh".to_string(),
        ],
        enable: vec![
            "sudo tailscale up".to_string(),
            format!("sudo tailscale serve --bg http://localhost:{port}"),
        ],
        console_notes: vec![
            "In the Tailscale admin console, enable MagicDNS and HTTPS certificates \
             (Settings → DNS) so your node gets a valid `*.ts.net` certificate."
                .to_string(),
            "Install Tailscale on your phone and sign into the same tailnet — then \
             the pairing QR's remote URL just works, away from home."
                .to_string(),
        ],
        windows_note:
            "On Windows, install Tailscale from https://tailscale.com/download, run \
             `tailscale up`, then `tailscale serve --bg http://localhost:"
                .to_string()
                + &port.to_string()
                + "`.",
        docs: "https://tailscale.com/kb/1223/funnel-serve-use-cases".to_string(),
    }
}

// GET /api/admin/remote-access  (admin-only)
pub async fn remote_access_status(
    AdminUser(_caller):  AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Query(q):            Query<RemoteAccessQuery>,
) -> Json<serde_json::Value> {
    let cfg    = live_cfg.get().await;
    let status = crate::remote_access::status(&cfg, q.redetect).await;
    let setup  = setup_guide(status.mira_port);
    Json(serde_json::json!({
        "status": status,
        "setup":  setup,
    }))
}
