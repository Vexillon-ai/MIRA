// SPDX-License-Identifier: AGPL-3.0-or-later

// src/proxy/mod.rs
//! nginx reverse-proxy management for MIRA.
//!
//! This module generates a complete `nginx.conf` from the current
//! [`crate::config::ProxyConfig`] and manages the nginx subprocess lifecycle.
//!
//! # When to use
//!
//! The proxy is **optional** — set `proxy.enabled = true` in `mira_config.json`
//! to activate it.  When enabled, MIRA binds its HTTP server to `127.0.0.1`
//! (not reachable from outside) and nginx acts as the TLS-terminating reverse
//! proxy on the public interface.
//!
//! # Lifecycle
//!
//! The [`NginxProxy`] is owned by the `Gateway` and follows this lifecycle:
//!
//! 1. `NginxProxy::new(config, backend_port, log_dir)` — constructed at startup
//! 2. `start_or_reload()` — called by Gateway after the Central Server is bound
//! 3. `reload_config()` — called when config changes at runtime (SIGHUP / CLI command)
//! 4. `stop()` — called during graceful shutdown

pub mod process;
pub mod template;

pub use process::NginxProxy;

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[test]
    fn module_exports_nginx_proxy() {
        // Smoke test: ensure NginxProxy is accessible from the module root.
        let _ = std::any::type_name::<super::NginxProxy>();
    }
}
