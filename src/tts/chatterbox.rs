// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/chatterbox.rs
//! Chatterbox AMD Vulkan TTS server supervisor (K3 / Q2 #10).
//!
//! [Chatterbox](https://github.com/tarekedOz/Chatterbox_AMDVulkan) is an
//! OpenAI-compatible TTS server that runs Kokoro/Chatterbox-Turbo very fast
//! on AMD Radeon GPUs via Vulkan. MIRA talks to it through the normal
//! `openai`-style TTS backend (registered as `chatterbox`); this module
//! optionally **manages the process**: spawn it, poll `GET /health`, and
//! restart it on crash with capped backoff.
//!
//! Same-host only. On WSL2 with a Windows-native Chatterbox, MIRA can't
//! manage a cross-OS process — leave `supervise = false` and just point the
//! backend at the URL. The recommendation (K2) flags this case.
//!
//! Runtime note: the spawn/health/restart loop can't be exercised on a box
//! without the Chatterbox binary, so this is verified by compile + the
//! pure-logic unit tests (backoff, URL building); the live loop is validated
//! on a machine that has Chatterbox installed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Live state of the supervised process, surfaced by `GET
/// /api/system/chatterbox/status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SupervisorState {
    /// A child process is currently spawned.
    pub running:  bool,
    /// Last `/health` probe succeeded.
    pub healthy:  bool,
    /// How many times we've (re)spawned the process this session.
    pub starts:   u32,
    /// OS pid of the current child, if any.
    pub pid:      Option<u32>,
    /// Most recent error (spawn failure, non-zero exit, …).
    pub last_error: Option<String>,
}

pub struct ChatterboxSupervisor {
    binary_path: PathBuf,
    port:        u16,
    extra_args:  Vec<String>,
    http:        reqwest::Client,
    state:       Arc<Mutex<SupervisorState>>,
}

impl ChatterboxSupervisor {
    pub fn new(binary_path: PathBuf, port: u16, extra_args: Vec<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { binary_path, port, extra_args, http, state: Arc::new(Mutex::new(SupervisorState::default())) }
    }

    pub fn health_url(&self) -> String {
        format!("http://127.0.0.1:{}/health", self.port)
    }

    /// Snapshot of supervisor state plus a fresh liveness probe (so the UI
    /// reflects a server started outside MIRA too).
    pub async fn status(&self) -> SupervisorState {
        let healthy = self.health_ok().await;
        let mut s = self.state.lock().await.clone();
        s.healthy = healthy;
        s
    }

    async fn health_ok(&self) -> bool {
        match self.http.get(self.health_url()).send().await {
            Ok(r)  => r.status().is_success(),
            Err(_) => false,
        }
    }

    /// Background supervise loop. Spawn the process, watch it, restart on
    /// exit with exponential backoff (capped). Idles politely when the
    /// binary isn't present yet (e.g. before the installer has run), so an
    /// install that lands later is picked up without a MIRA restart.
    pub async fn run(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);

        loop {
            if !self.binary_path.exists() {
                {
                    let mut s = self.state.lock().await;
                    s.running = false;
                    s.healthy = false;
                    s.last_error = Some(format!(
                        "Chatterbox binary not found at {}", self.binary_path.display()
                    ));
                }
                // Recheck periodically — the installer may put it there later.
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }

            info!("chatterbox: starting server {} (port {})", self.binary_path.display(), self.port);
            let spawn = Command::new(&self.binary_path)
                .args(&self.extra_args)
                .kill_on_drop(true)
                .spawn();

            let mut child = match spawn {
                Ok(c) => c,
                Err(e) => {
                    warn!("chatterbox: spawn failed: {e}");
                    let mut s = self.state.lock().await;
                    s.running = false;
                    s.last_error = Some(format!("spawn failed: {e}"));
                    drop(s);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            };

            {
                let mut s = self.state.lock().await;
                s.running = true;
                s.pid     = child.id();
                s.starts += 1;
                s.last_error = None;
            }

            // Watch for exit while periodically refreshing health.
            let exit = loop {
                tokio::select! {
                    status = child.wait() => break status,
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {
                        let ok = self.health_ok().await;
                        self.state.lock().await.healthy = ok;
                        // A clean first health success resets the backoff so a
                        // long-lived server doesn't carry a stale penalty.
                        if ok { backoff = Duration::from_secs(1); }
                    }
                }
            };

            {
                let mut s = self.state.lock().await;
                s.running = false;
                s.healthy = false;
                s.pid     = None;
                match &exit {
                    Ok(st) if st.success() => { s.last_error = None; }
                    Ok(st) => { s.last_error = Some(format!("exited with {st}")); }
                    Err(e) => { s.last_error = Some(format!("wait failed: {e}")); }
                }
            }
            warn!("chatterbox: server exited ({exit:?}); restarting in {:?}", backoff);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_url_uses_port() {
        let s = ChatterboxSupervisor::new(PathBuf::from("/nope"), 8087, vec![]);
        assert_eq!(s.health_url(), "http://127.0.0.1:8087/health");
        let s = ChatterboxSupervisor::new(PathBuf::from("/nope"), 9000, vec![]);
        assert_eq!(s.health_url(), "http://127.0.0.1:9000/health");
    }

    #[tokio::test]
    async fn status_reports_unhealthy_when_nothing_running() {
        // No server on this port → health probe fails, default state.
        let s = ChatterboxSupervisor::new(PathBuf::from("/nonexistent-binary"), 8099, vec![]);
        let st = s.status().await;
        assert!(!st.healthy);
        assert!(!st.running);
        assert_eq!(st.starts, 0);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        // Mirror the loop's arithmetic to lock the policy.
        let max = Duration::from_secs(30);
        let mut b = Duration::from_secs(1);
        let seq: Vec<u64> = (0..7).map(|_| { let v = b.as_secs(); b = (b * 2).min(max); v }).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 30, 30]);
    }
}
