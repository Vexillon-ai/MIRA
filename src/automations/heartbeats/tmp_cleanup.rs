// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/tmp_cleanup.rs
//! Tmp cleanup heartbeat.
//!
//! Sweeps stale per-call sandbox scratch dirs (created by `code_run` via
//! `tempfile`). Anything older than the threshold is removed regardless
//! of whether the originating process is still alive — `tempfile` uses
//! its own RAII drop to clean up on graceful exit, so leftovers here are
//! by definition orphaned.

use std::path::Path;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

pub struct TmpCleanup;

#[async_trait]
impl HeartbeatTask for TmpCleanup {
    fn name(&self) -> &'static str { "tmp_cleanup" }

    async fn run(
        &self,
        ctx:  &HeartbeatContext,
        args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        let max_age_hours = args.get("max_age_hours")
            .and_then(|v| v.as_u64())
            .unwrap_or(24);

        // We sweep the data_dir's `tmp/` subdirectory if present. Code-run
        // may use system `/tmp` directly; that's the OS's responsibility
        // and we deliberately don't touch it.
        let tmp_dir = ctx.data_dir.join("tmp");
        let removed = sweep(&tmp_dir, max_age_hours)?;
        Ok(HeartbeatOutcome {
            summary: format!(
                "tmp_cleanup: removed {removed} entr(y/ies) older than {max_age_hours}h \
                 from {}",
                tmp_dir.display()
            ),
        })
    }
}

fn sweep(dir: &Path, max_age_hours: u64) -> Result<usize, MiraError> {
    if !dir.exists() {
        debug!("tmp_cleanup: {} does not exist, nothing to do", dir.display());
        return Ok(0);
    }

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(max_age_hours * 3_600))
        .ok_or_else(|| MiraError::ConfigError("max_age_hours too large".into()))?;

    let mut removed = 0usize;
    let entries = std::fs::read_dir(dir).map_err(MiraError::IoError)?;
    for ent in entries {
        let ent = match ent { Ok(e) => e, Err(_) => continue };
        let path = ent.path();

        let modified = match ent.metadata().and_then(|m| m.modified()) {
            Ok(t)  => t,
            Err(e) => { warn!("tmp_cleanup stat {}: {e}", path.display()); continue; }
        };
        if modified >= cutoff { continue; }

        let res = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match res {
            Ok(())  => removed += 1,
            Err(e)  => warn!("tmp_cleanup remove {}: {e}", path.display()),
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
        let n = sweep(Path::new("/no/such/path/x"), 24).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn fresh_file_is_kept() {
        let d = tempdir().unwrap();
        let f = d.path().join("scratch");
        File::create(&f).unwrap();
        let n = sweep(d.path(), 24).unwrap();
        assert_eq!(n, 0);
        assert!(f.exists());
    }
}
