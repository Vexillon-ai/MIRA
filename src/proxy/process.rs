// SPDX-License-Identifier: AGPL-3.0-or-later

// src/proxy/process.rs
//! nginx subprocess management.
//!
//! `NginxProxy` starts, reloads, and stops an nginx process.  It writes the
//! generated config file before any start/reload operation.

use std::path::PathBuf;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{ProxyConfig, expand_path};
use crate::MiraError;

// ─────────────────────────────────────────────────────────────────────────────

/// Manages the nginx reverse-proxy subprocess for MIRA.
pub struct NginxProxy {
    config:       ProxyConfig,
    backend_port: u16,
    log_dir:      PathBuf,
}

impl NginxProxy {
    /// Create a new `NginxProxy` from the resolved `ProxyConfig`.
    ///
    /// `backend_port` — the port MIRA's HTTP server is listening on.
    /// `log_dir`      — directory for nginx access/error logs.
    pub fn new(config: ProxyConfig, backend_port: u16, log_dir: PathBuf) -> Self {
        Self { config, backend_port, log_dir }
    }

    /// Write the generated config and start (or reload) nginx.
    ///
    /// 1. Render `nginx.conf` from the template.
    /// 2. Write to `config.config_path`.
    /// 3. If a running nginx pid exists → send `nginx -s reload`.
    /// 4. Otherwise → start nginx with `nginx -c <config_path>`.
    pub async fn start_or_reload(&self) -> Result<(), MiraError> {
        self.write_config()?;
        let config_path = expand_path(&self.config.config_path);

        if self.is_running().await {
            info!("nginx is running — sending reload signal");
            self.nginx_signal("reload").await?;
        } else {
            info!("Starting nginx with config {:?}", config_path);
            let status = Command::new(&self.config.nginx_binary)
                .arg("-c")
                .arg(&config_path)
                .status()
                .await
                .map_err(|e| MiraError::ProxyError(format!("Failed to start nginx: {}", e)))?;

            if !status.success() {
                return Err(MiraError::ProxyError(format!(
                    "nginx exited with status: {}", status
                )));
            }
            info!("nginx started successfully");
        }

        Ok(())
    }

    /// Send `nginx -s stop` (graceful shutdown).
    pub async fn stop(&self) -> Result<(), MiraError> {
        if !self.is_running().await {
            debug!("nginx is not running — nothing to stop");
            return Ok(());
        }
        info!("Stopping nginx");
        self.nginx_signal("stop").await
    }

    /// Send `nginx -s reload` to reload configuration without dropping connections.
    pub async fn reload_config(&self) -> Result<(), MiraError> {
        self.write_config()?;
        self.nginx_signal("reload").await
    }

    /// Return `true` if the nginx pid file exists and the process is running.
    pub async fn is_running(&self) -> bool {
        let pid_path = expand_path(&self.config.pid_path);
        if !pid_path.exists() {
            return false;
        }
        // Read the PID and check if the process exists.
        match std::fs::read_to_string(&pid_path) {
            Ok(contents) => {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    // On Unix, sending signal 0 checks process existence.
                    #[cfg(unix)]
                    { unsafe { libc::kill(pid as i32, 0) == 0 } }
                    // Windows has no libc::kill. The nginx TLS proxy is a
                    // Unix-oriented backend (default binary /usr/sbin/nginx) and
                    // isn't deployed on Windows, so a precise liveness probe
                    // isn't worth a new dependency here: treat a present, valid
                    // pid file as best-effort "running".
                    #[cfg(not(unix))]
                    { let _ = pid; true }
                } else {
                    warn!("nginx pid file contains non-integer: {:?}", pid_path);
                    false
                }
            }
            Err(_) => false,
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn write_config(&self) -> Result<(), MiraError> {
        let config_path = expand_path(&self.config.config_path);

        // Ensure parent directory exists.
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MiraError::ProxyError(
                format!("Failed to create nginx config dir {:?}: {}", parent, e)
            ))?;
        }
        // Ensure log directory exists.
        std::fs::create_dir_all(&self.log_dir).map_err(|e| MiraError::ProxyError(
            format!("Failed to create nginx log dir: {}", e)
        ))?;

        let rendered = crate::proxy::template::render(
            &self.config,
            self.backend_port,
            &self.log_dir,
        );

        std::fs::write(&config_path, &rendered).map_err(|e| MiraError::ProxyError(
            format!("Failed to write nginx config to {:?}: {}", config_path, e)
        ))?;

        debug!("Wrote nginx config to {:?}", config_path);
        Ok(())
    }

    async fn nginx_signal(&self, signal: &str) -> Result<(), MiraError> {
        let config_path = expand_path(&self.config.config_path);
        let status = Command::new(&self.config.nginx_binary)
            .arg("-c")
            .arg(&config_path)
            .arg("-s")
            .arg(signal)
            .status()
            .await
            .map_err(|e| MiraError::ProxyError(
                format!("Failed to send nginx signal '{}': {}", signal, e)
            ))?;

        if !status.success() {
            return Err(MiraError::ProxyError(format!(
                "nginx -s {} exited with status: {}", signal, status
            )));
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProxyConfig, TlsConfig};
    use tempfile::TempDir;

    fn test_proxy(dir: &TempDir) -> NginxProxy {
        NginxProxy::new(
            ProxyConfig {
                enabled:           true,
                nginx_binary:      "/usr/sbin/nginx".to_string(),
                config_path:       dir.path().join("nginx.conf").to_string_lossy().to_string(),
                pid_path:          dir.path().join("nginx.pid").to_string_lossy().to_string(),
                worker_processes:  "auto".to_string(),
                websocket_support: true,
                tls: TlsConfig {
                    enabled:     false,
                    cert_path:   String::new(),
                    key_path:    String::new(),
                    listen_port: 443,
                },
            },
            8080,
            dir.path().to_path_buf(),
        )
    }

    #[tokio::test]
    async fn not_running_when_no_pid_file() {
        let dir = TempDir::new().unwrap();
        let proxy = test_proxy(&dir);
        assert!(!proxy.is_running().await);
    }

    #[test]
    fn write_config_creates_file() {
        let dir = TempDir::new().unwrap();
        let proxy = test_proxy(&dir);
        proxy.write_config().unwrap();
        let conf_path = dir.path().join("nginx.conf");
        assert!(conf_path.exists());
        let content = std::fs::read_to_string(&conf_path).unwrap();
        assert!(content.contains("server 127.0.0.1:8080"));
    }
}
