// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/signal_cli/daemon.rs

//! Lifecycle management for the signal-cli REST API daemon.
//!
//! MIRA starts signal-cli in `daemon` mode on startup when enabled.
//! The daemon exposes an HTTP REST API that `SignalCliClient` uses.
//!
//! signal-cli invocation:
//!   signal-cli -u <phone> daemon --http 127.0.0.1:<port>

use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::time::{sleep, Duration};
use tracing::{info, warn};
use crate::MiraError;

/// Status of the signal-cli daemon
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonStatus {
    NotStarted,
    Starting,
    Running { pid: u32 },
    Failed(String),
    Stopped,
}

/// Manages the lifecycle of the signal-cli REST daemon process
pub struct SignalCliDaemon {
    pub binary: String,
    pub phone_number: String,
    pub port: u16,
    pub data_dir: String,
    /// When set, exported as `JAVA_HOME` to the daemon child so the
    /// signal-cli launcher uses MIRA's managed JRE instead of (or in the
    /// absence of) a system Java. `None` for the self-contained native
    /// build, or when relying on a system Java already on `PATH`.
    java_home: Option<String>,
    child: Option<Child>,
    pub status: DaemonStatus,
}

impl SignalCliDaemon {
    pub fn new(binary: String, phone_number: String, port: u16, data_dir: String) -> Self {
        Self {
            binary,
            phone_number,
            port,
            data_dir,
            java_home: None,
            child: None,
            status: DaemonStatus::NotStarted,
        }
    }

    /// Point the daemon at a specific Java runtime (exported as
    /// `JAVA_HOME` on the child). Builder-style so existing `new` callers
    /// stay unchanged.
    pub fn with_java_home(mut self, java_home: Option<String>) -> Self {
        self.java_home = java_home;
        self
    }

    /// Start the daemon. Waits up to `timeout_secs` for it to become healthy.
    pub async fn start(&mut self, timeout_secs: u64) -> Result<(), MiraError> {
        if matches!(self.status, DaemonStatus::Running { .. }) {
            info!("signal-cli daemon already running");
            return Ok(());
        }

        info!(
            "Starting signal-cli daemon: {} -u {} daemon --http 127.0.0.1:{}",
            self.binary, self.phone_number, self.port
        );

        self.status = DaemonStatus::Starting;

        let http_addr = format!("127.0.0.1:{}", self.port);
        let args = [
            "--config", &self.data_dir,
            "-u", &self.phone_number,
            "daemon",
            "--http",
            &http_addr,
            "--receive-mode", "on-connection",
        ];

        let child = spawn_signal_cli(&self.binary, &args, self.java_home.as_deref())
            .map_err(|e| MiraError::ProviderError(format!(
                "Failed to start signal-cli: {}. Is signal-cli installed and in PATH? \
                 On Windows, point `cli_binary` at the full path to signal-cli.bat \
                 (and ensure a Java 17+ runtime is installed).", e
            )))?;

        let pid = child.id().unwrap_or(0);
        self.child = Some(child);
        self.status = DaemonStatus::Running { pid };
        info!("signal-cli daemon started with PID {}", pid);

        // Wait for it to be ready
        self.wait_for_ready(timeout_secs).await?;
        Ok(())
    }

    /// Poll the health endpoint until it responds (or timeout)
    async fn wait_for_ready(&self, timeout_secs: u64) -> Result<(), MiraError> {
        // signal-cli 0.13+ does not expose a /v1/health endpoint — any HTTP
        // response (including 404) means the daemon is up and accepting requests.
        let url = format!("http://127.0.0.1:{}/v1/health", self.port);
        let client = reqwest::Client::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

        while std::time::Instant::now() < deadline {
            if let Ok(resp) = client.get(&url).send().await {
                if resp.status().as_u16() < 500 {
                    info!("signal-cli daemon is ready at port {}", self.port);
                    return Ok(());
                }
            }
            sleep(Duration::from_millis(500)).await;
        }

        Err(MiraError::ProviderError(format!(
            "signal-cli daemon did not become ready within {}s", timeout_secs
        )))
    }

    /// Stop the daemon: kill the signal-cli process (and, on Windows, its whole
    /// tree — see `stop` notes below).
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            info!("Stopping signal-cli daemon");
            // On Windows the daemon is launched via `cmd /C signal-cli.bat`,
            // which spawns the JVM as a grandchild. Killing the `cmd.exe`
            // handle alone orphans the JVM, so terminate the whole tree first.
            #[cfg(windows)]
            if let Some(pid) = child.id() {
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
            if let Err(e) = child.kill().await {
                warn!("Failed to kill signal-cli daemon: {}", e);
            }
        }
        self.status = DaemonStatus::Stopped;
    }

    /// Check if the process is still alive
    pub fn is_running(&mut self) -> bool {
        if let Some(ref mut child) = self.child {
            // try_wait returns Ok(None) if still running
            matches!(child.try_wait(), Ok(None))
        } else {
            false
        }
    }
}

/// Spawn the signal-cli daemon process, accounting for how each platform
/// launches it.
///
/// On Unix the binary (a shell wrapper or native build) is executed directly.
/// On Windows signal-cli ships as `signal-cli.bat`, which `CreateProcess`
/// cannot launch directly — batch files must run through `cmd.exe`. We detect a
/// non-`.exe` binary and route it through `cmd /C` so the stock Windows
/// distribution works; a real `.exe` (e.g. a GraalVM native build) is launched
/// directly.
#[cfg(windows)]
fn spawn_signal_cli(binary: &str, args: &[&str], java_home: Option<&str>) -> std::io::Result<Child> {
    let mut cmd = if binary.to_ascii_lowercase().ends_with(".exe") {
        let mut c = Command::new(binary);
        c.args(args);
        c
    } else {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(binary).args(args);
        c
    };
    if let Some(jh) = java_home {
        cmd.env("JAVA_HOME", jh);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

#[cfg(not(windows))]
fn spawn_signal_cli(binary: &str, args: &[&str], java_home: Option<&str>) -> std::io::Result<Child> {
    let mut cmd = Command::new(binary);
    cmd.args(args);
    if let Some(jh) = java_home {
        cmd.env("JAVA_HOME", jh);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

/// Generate the config snippet MIRA writes to `config.toml` when the user
/// asks it to "configure Signal". Returns the TOML string for the [signal] block.
pub fn generate_config_snippet(phone: &str, port: u16, data_dir: &str, binary: &str) -> String {
    format!(
        r#"[signal]
enabled = true
phone_number = "{phone}"
rest_port = {port}
cli_binary = "{binary}"
data_dir = "{data_dir}"
socket_path = "/run/signald/signald.sock"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_initial_status() {
        let d = SignalCliDaemon::new(
            "signal-cli".to_string(),
            "+15551234567".to_string(),
            8080,
            "/tmp/signal-data".to_string(),
        );
        assert_eq!(d.status, DaemonStatus::NotStarted);
        assert!(d.child.is_none());
    }

    #[test]
    fn test_generate_config_snippet() {
        let snippet = generate_config_snippet("+15551234567", 8080, "/home/user/.signal", "signal-cli");
        assert!(snippet.contains("enabled = true"));
        assert!(snippet.contains("+15551234567"));
        assert!(snippet.contains("8080"));
    }

    #[tokio::test]
    async fn test_start_fails_gracefully_if_binary_missing() {
        let mut daemon = SignalCliDaemon::new(
            "signal-cli-does-not-exist-xyz".to_string(),
            "+15551234567".to_string(),
            18080,
            "/tmp".to_string(),
        );
        let result = daemon.start(2).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("signal-cli") || err.contains("No such file"));
    }
}
