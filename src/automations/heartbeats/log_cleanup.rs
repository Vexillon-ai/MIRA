// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/log_cleanup.rs
//! Log cleanup heartbeat.
//!
//! `tracing-appender` already rotates daily; this heartbeat just deletes
//! files older than the retention window so the logs dir doesn't grow
//! without bound. Default: 30 days. Override via `args.retain_days`.

use std::path::Path;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

pub struct LogCleanup;

#[async_trait]
impl HeartbeatTask for LogCleanup {
    fn name(&self) -> &'static str { "log_cleanup" }

    async fn run(
        &self,
        ctx:  &HeartbeatContext,
        args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        let retain_days = args.get("retain_days")
            .and_then(|v| v.as_u64())
            .unwrap_or(30);
        let logs_dir = ctx.data_dir.join("logs");
        let removed = sweep(&logs_dir, retain_days)?;
        Ok(HeartbeatOutcome {
            summary: format!(
                "log_cleanup: removed {removed} file(s) older than {retain_days}d \
                 from {}",
                logs_dir.display()
            ),
        })
    }
}

fn sweep(dir: &Path, retain_days: u64) -> Result<usize, MiraError> {
    if !dir.exists() {
        debug!("log_cleanup: {} does not exist, nothing to do", dir.display());
        return Ok(0);
    }

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(retain_days * 86_400))
        .ok_or_else(|| MiraError::ConfigError("retain_days too large".into()))?;

    let mut removed = 0usize;
    let entries = std::fs::read_dir(dir)
        .map_err(|e| MiraError::IoError(e))?;
    for ent in entries {
        let ent = match ent { Ok(e) => e, Err(_) => continue };
        let path = ent.path();
        if !path.is_file() { continue; }

        let modified = match ent.metadata().and_then(|m| m.modified()) {
            Ok(t)  => t,
            Err(e) => { warn!("log_cleanup stat {}: {e}", path.display()); continue; }
        };
        if modified < cutoff {
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) => warn!("log_cleanup remove {}: {e}", path.display()),
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn missing_dir_returns_zero() {
        let n = sweep(Path::new("/no/such/dir/here"), 30).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn fresh_file_is_kept() {
        // Files created right now are well within any retention window.
        let d = tempdir().unwrap();
        let f = d.path().join("recent.log");
        File::create(&f).unwrap();
        let n = sweep(d.path(), 30).unwrap();
        assert_eq!(n, 0);
        assert!(f.exists());
    }
}
