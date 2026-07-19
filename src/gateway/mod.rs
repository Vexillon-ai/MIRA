// SPDX-License-Identifier: AGPL-3.0-or-later

// src/gateway/mod.rs
//! Gateway — the authoritative service factory and lifecycle owner for MIRA.
//!
//! The Gateway:
//! 1. Runs the full startup sequence (via [`GatewayBuilder`]).
//! 2. Owns `Arc<AgentCore>` — the single shared reasoning engine.
//! 3. Owns `MiraServer` — the axum HTTP server.
//! 4. Optionally owns `NginxProxy` — the TLS reverse proxy.
//! 5. Exposes `run_until_shutdown()` which binds the TCP socket and serves
//!  until SIGTERM / SIGINT.
//!
//! # Usage
//!
//! ```rust,ignore
//! let gateway = GatewayBuilder::new()
//!.config_path_opt(args.config)
//!.build()
//!.await?;
//!
//! // TUI or headless server:
//! if args.tui {
//!   mira::tui::run(gateway.agent_core.clone(), args.into()).await;
//! } else {
//!   gateway.run_until_shutdown().await?;
//! }
//! ```

pub mod builder;
pub mod channel_manager;

pub use builder::GatewayBuilder;
pub use channel_manager::ChannelManager;

// Re-exports for backward compatibility.
pub use crate::config::{Config, MiraConfig as GatewayConfig};

use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

use crate::agent::AgentCore;
use crate::auth::LocalAuthService;
use crate::channel_accounts::ChannelAccountStore;
use crate::config::MiraConfig;
use crate::history::HistoryStore;
use crate::notifications::NotificationBus;
use crate::proxy::NginxProxy;
use crate::server::MiraServer;
use crate::web::LiveConfig;
use crate::MiraError;

use channel_manager::ChannelManager as ChannelManagerRuntime;

// ─────────────────────────────────────────────────────────────────────────────

// Fully-wired MIRA application instance.
// // Constructed by [`GatewayBuilder::build()`].  `Arc<AgentCore>` is the
// primary handle that other surfaces (TUI, future browser client) consume.
pub struct Gateway {
    pub config:       Arc<MiraConfig>,
    pub agent_core:   Arc<AgentCore>,
    // Multi-agent registry — every spawned worker registers here.
    // Built at startup; empty until the first worker spawns.
    pub agent_registry: Arc<crate::agent::AgentRegistry>,
    // Multi-agent supervisor — spawn / interrupt / pause / resume.
    // Wraps `agent_registry` and pulls executors from a resolver
    // that consults the loaded SkillRegistry.
    pub supervisor:   Arc<crate::agent::Supervisor>,
    // Auth service (available for callers like the TUI).
    pub auth_service: Option<Arc<LocalAuthService>>,
    // Conversation history store.
    pub history:      Option<Arc<HistoryStore>>,
    // Hot-reloadable config wrapper.
    pub live_config:  Option<Arc<LiveConfig>>,
    // Cross-channel notification bus.
    pub notification_bus: Arc<NotificationBus>,
    // Per-user channel account store (Signal / Telegram).
    pub channel_accounts: Option<Arc<ChannelAccountStore>>,
    pub(crate) server:           MiraServer,
    pub(crate) proxy:            Option<NginxProxy>,
    // Owns per-account signal-cli daemons + the telegram lookup table.
    pub(crate) channel_manager:  Arc<tokio::sync::RwLock<ChannelManagerRuntime>>,
    // Kept alive so the cleanup loop runs for the lifetime of the Gateway.
    pub(crate) _session_cleanup: tokio::task::JoinHandle<()>,
    // Background calendar-sync engine. `Some(_)` only when an external
    // sync_provider is configured; aborts on drop.
    pub(crate) _calendar_sync: Option<crate::calendar::SyncEngine>,
    // automations worker (schedules + heartbeats). `None` if the
    // store failed to open (non-fatal). Aborts on drop.
    pub(crate) _automations_worker: Option<tokio::task::JoinHandle<()>>,
    /// event subscriber loop. Listens on the
    // in-process [`crate::events::EventBus`] and dispatches matching
    // `event_subscriptions`. Aborts on drop.
    pub(crate) _event_subscriber: Option<tokio::task::JoinHandle<()>>,
    // Companion proactive-delivery scheduler (check-ins + daily briefings).
    // MUST be held on the long-lived `Gateway` — `CompanionScheduler`'s Drop
    // aborts the spawned tokio task, so if this lived only in `build()`'s
    // scope the task would die microseconds after the "started" log fires
    // (which is exactly the bug that silently broke morning briefings/
    // check-ins for everyone since 0.169.0). `None` when the companion
    // system or history store wasn't wired.
    pub(crate) _companion_scheduler: Option<crate::companion::scheduler::CompanionScheduler>,
    // Scheduled-backup loop (off by default; `backup.scheduled_enabled`).
    // Same lifetime-on-Gateway pattern as `_companion_scheduler` — its
    // `Drop` aborts the spawned tokio task, so it MUST live for the
    // whole `Gateway` lifetime, not a local in `build()`.
    pub(crate) _backup_scheduler: Option<crate::install::backup_scheduler::BackupScheduler>,
}

impl Gateway {
    // Bind the TCP socket and serve until SIGTERM / SIGINT is received.
    //     // If nginx proxy is running, it is stopped cleanly before returning.
    pub async fn run_until_shutdown(self) -> Result<(), MiraError> {
        // Mint a long-lived bearer token for same-host TUI use.
        // Rotated on every server start; failure is non-fatal — server mode
        // still works, the TUI just has to log in by password instead.
        if let Some(ref auth) = self.auth_service {
            let token_path = crate::config::resolve_state_path(&self.config.tui.auto_token_path);
            match auth.issue_local_admin_token(LOCAL_TOKEN_TTL_SECS) {
                Ok(Some(tok)) => {
                    match write_local_token(&token_path, &tok) {
                        Ok(()) => info!("Local TUI token written to {}", token_path.display()),
                        Err(e) => warn!("Could not write local TUI token (non-fatal): {}", e),
                    }
                }
                Ok(None) => warn!("No admin account — skipping local TUI token mint"),
                Err(e)   => warn!("Local TUI token mint failed (non-fatal): {}", e),
            }
        }

        let proxy               = self.proxy;
        let channel_manager     = Arc::clone(&self.channel_manager);
        let restart_notify      = Arc::clone(&self.server.restart_notify);
        // Where the clean-shutdown marker lives, so the next boot knows this
        // stop was intentional (not a crash) — see health::boot.
        let data_dir            = self.config.data_dir_path();

        // when running under the Windows SCM, the install
        // module populates a shared notify that the SCM Stop /
        // Shutdown control handler trips. We await it alongside the
        // other shutdown sources so SCM-driven stops reach the same
        // graceful path. `None` on non-Windows targets and on Windows
        // console launches.
        #[cfg(target_os = "windows")]
        let scm_shutdown = crate::install::windows::external_shutdown_notify();
        #[cfg(not(target_os = "windows"))]
        let scm_shutdown: Option<Arc<tokio::sync::Notify>> = None;

        let shutdown = async move {
            // `pending()` is a future that never resolves — used as
            // the "no SCM notify" branch so the select macro always
            // has a valid arm without conditional compilation.
            let scm = async {
                match scm_shutdown {
                    Some(n) => n.notified().await,
                    None    => std::future::pending::<()>().await,
                }
            };
            // SIGTERM (what `systemctl stop/restart` and most supervisors send)
            // must reach this graceful path too — otherwise an operator restart
            // kills the process uncleanly, skipping teardown AND looking like a
            // crash to the restart-count detector. `pending()` on non-unix.
            let sigterm = async {
                #[cfg(unix)]
                {
                    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                        Ok(mut s) => { s.recv().await; }
                        Err(_)    => std::future::pending::<()>().await,
                    }
                }
                #[cfg(not(unix))]
                { std::future::pending::<()>().await }
            };
            // Whether this shutdown is a deliberate RESTART (API restart button /
            // self-upgrade) vs a genuine stop. Drives the force-exit exit code so
            // a restart is relaunched deterministically even when the graceful path
            // times out (Fix 3): on Windows SCM a clean exit(0) gets no recovery and
            // leaves the service Stopped, so a restart exits non-zero to trigger it
            // (systemd Restart=always / launchd KeepAlive relaunch regardless).
            let mut is_restart = false;
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("ctrl_c received — stopping MIRA");
                }
                _ = sigterm => {
                    info!("SIGTERM received — stopping MIRA (supervisor should relaunch)");
                }
                _ = restart_notify.notified() => {
                    is_restart = true;
                    info!("Restart requested (API/self-upgrade) — stopping MIRA to relaunch");
                }
                _ = scm => {
                    info!("SCM stop received — stopping MIRA");
                }
            }

            // This is a graceful stop from a known source → mark it clean so the
            // next boot isn't counted as a crash by the restart-loop detector.
            if let Err(e) = crate::health::boot::mark_clean_shutdown(&data_dir) {
                warn!("could not write clean-shutdown marker (non-fatal): {e}");
            }

            // Stop all per-account signal-cli daemons + listeners.
            // Take the write lock briefly — handlers that might be
            // holding it for individual start/stop calls finish in
            // milliseconds, so contention here is bounded.
            channel_manager.write().await.shutdown().await;
            tracing::info!("channel manager stopped");

            // Stop nginx proxy (non-fatal).
            if let Some(p) = proxy {
                if let Err(e) = p.stop().await {
                    tracing::warn!("nginx stop failed (non-fatal): {}", e);
                }
            }

            // Hard-deadline fallback: the web UI holds long-lived SSE
            // connections open (/api/notifications/stream, /api/logs/stream)
            // which never complete on their own. axum's graceful shutdown
            // would wait on them indefinitely. After a short drain window,
            // force-exit so the supervisor can relaunch us.
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(FORCE_EXIT_GRACE_SECS)).await;
                // Exit non-zero on a restart so a supervisor that only relaunches
                // on failure (Windows SCM recovery) still brings MIRA back; a
                // genuine stop exits 0 to stay stopped. The clean-shutdown marker
                // was already written above, so this non-zero exit is NOT counted
                // as a crash by the restart-loop detector.
                let code = if is_restart { 1 } else { 0 };
                warn!(
                    "Graceful-shutdown grace period ({}s) elapsed — forcing process exit ({})",
                    FORCE_EXIT_GRACE_SECS, code,
                );
                std::process::exit(code);
            });
        };

        self.server.run_until_shutdown(shutdown).await
    }
}

// How long to let axum drain in-flight requests before we force-exit.
// SSE streams (web UI notifications/logs) keep the graceful shutdown
// from completing naturally, so we always cap the wait.
const FORCE_EXIT_GRACE_SECS: u64 = 3;

// ── Local TUI token helpers ───────────────────────────────────────────────────

// Lifetime of the local TUI bearer token. Rotated on every server restart,
// so a long lifetime is safe — the file goes away with the server.
const LOCAL_TOKEN_TTL_SECS: i64 = 90 * 24 * 60 * 60;

// Write a token atomically to `path` with mode 0600 (owner read/write only).
// Creates the parent directory if missing.
fn write_local_token(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write to a sibling tmp file first and rename, so a reader never sees
    // a half-written token.
    let tmp = path.with_extension("token.tmp");
    std::fs::write(&tmp, token)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_token_written_atomically_with_0600_on_unix() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.token");

        write_local_token(&path, "abc.def.ghi").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc.def.ghi");
        assert!(!path.with_extension("token.tmp").exists(), "tmp file should be renamed away");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn local_token_overwrites_existing_file() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("local.token");
        write_local_token(&path, "first").unwrap();
        write_local_token(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }
}
