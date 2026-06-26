// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/status.rs
//! GET /api/status — system health dashboard

use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::{Extension, response::IntoResponse};
use serde::Serialize;

use crate::agent::AgentCore;
use crate::auth::AuthUser;
use crate::history::HistoryStore;

static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub fn init_start_time() {
    START_TIME.get_or_init(std::time::Instant::now);
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub version:          &'static str,
    pub uptime_secs:      u64,
    pub now_utc:          i64,
    /// System-wide aggregate counts + provider name are admin-only — `None`
    /// for non-admin callers (operational fields below stay populated).
    pub active_sessions:  Option<usize>,
    pub memory_count:     Option<usize>,
    pub conversation_count: Option<usize>,
    pub message_count:    Option<usize>,
    pub provider_name:    Option<String>,
    /// True when MIRA is running under a supervisor that will relaunch it
    /// after a clean exit. The web UI uses this to label the Restart button
    /// honestly: when false, exiting the process leaves nothing to bring it
    /// back, so the button degrades to "Stop server".
    pub supervised:       bool,
    /// Which supervisor was detected, when known. One of "systemd",
    /// "docker", "launchd", or null.
    pub supervisor:       Option<&'static str>,
    /// Host machine metrics (CPU / memory / disk). Admin-only — `None` for
    /// non-admin callers, since host load + capacity is fleet-wide posture.
    pub machine:          Option<crate::health::process::MachineMetrics>,
}

pub async fn status_handler(
    // Require login (was fully open). Operational fields (version, uptime,
    // supervisor) are returned to every authenticated user; system-wide
    // aggregate counts + the provider name are admin-only (trimmed to None
    // for non-admins) since they leak fleet-wide posture.
    AuthUser(user):     AuthUser,
    Extension(agent):   Extension<Arc<AgentCore>>,
    Extension(history): Extension<Arc<HistoryStore>>,
    Extension(data_dir): Extension<crate::server::handlers::onboarding::DataDir>,
) -> impl IntoResponse {
    let is_admin = user.role == crate::auth::models::Role::Admin;

    let uptime = START_TIME.get()
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);

    let now_utc = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let supervisor = detect_supervisor();

    // Compute the system-wide counts only for admins; non-admins get None.
    let (active_sessions, memory_count, conv_count, msg_count, provider_name) = if is_admin {
        let active_sessions  = Some(agent.sessions.len().await);
        let memory_count: Option<usize> = agent.memory.count().ok().map(|n| n as usize);
        let (conv_count, msg_count) = history.stats().unwrap_or((None, None));
        (active_sessions, memory_count, conv_count, msg_count, Some(agent.provider.name().to_owned()))
    } else {
        (None, None, None, None, None)
    };

    // Host machine metrics — admin-only (host load/capacity is fleet posture).
    let machine = if is_admin {
        Some(crate::health::process::machine_metrics(&data_dir.0))
    } else {
        None
    };

    axum::Json(StatusResponse {
        version:            env!("CARGO_PKG_VERSION"),
        uptime_secs:        uptime,
        now_utc,
        active_sessions,
        memory_count,
        conversation_count: conv_count,
        message_count:      msg_count,
        provider_name,
        supervised:         supervisor.is_some(),
        supervisor,
        machine,
    })
}

/// Probe a small set of well-known signals to figure out whether something
/// will relaunch us after `exit(0)`. Order matters: we check the most
/// authoritative signal (env vars set by the supervisor itself) before the
/// container heuristics, which can match even when the user is supervising
/// MIRA some other way inside the container.
fn detect_supervisor() -> Option<&'static str> {
    // systemd sets INVOCATION_ID for every service start. Works for both
    // `systemctl --user` (Linux + WSL) and system-scoped units.
    if std::env::var_os("INVOCATION_ID").is_some() {
        return Some("systemd");
    }
    // launchd sets XPC_SERVICE_NAME for every LaunchAgent/LaunchDaemon it
    // spawns. Match our own bundle id so we don't confuse the system shell
    // (which gets `XPC_SERVICE_NAME=com.apple.…`) with a real service.
    if let Ok(svc) = std::env::var("XPC_SERVICE_NAME") {
        if svc.starts_with("com.mira") {
            return Some("launchd");
        }
    }
    // OCI runtimes (Docker, Podman, containerd). The marker file is the
    // most reliable; fall back to cgroup inspection on runtimes that don't
    // create one.
    if std::path::Path::new("/.dockerenv").exists() {
        return Some("docker");
    }
    if let Ok(cg) = std::fs::read_to_string("/proc/1/cgroup") {
        if cg.contains("docker") || cg.contains("containerd") || cg.contains("podman") {
            return Some("docker");
        }
    }
    // Windows: supervised iff the Service Control Manager launched us (the
    // dispatcher ran `service_main`, which sets the shutdown notify). We key
    // off that — not merely `target_os = "windows"` — because a bare console
    // `mira serve` is NOT supervised (exiting wouldn't relaunch). Under SCM,
    // the recovery actions set at install relaunch us on the non-zero exit an
    // app-initiated restart produces, the same exit→relaunch contract as
    // systemd/launchd, so the web-UI Restart button is valid.
    #[cfg(target_os = "windows")]
    {
        if crate::install::windows::is_running_under_scm() {
            return Some("scm");
        }
    }
    None
}
