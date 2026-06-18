// SPDX-License-Identifier: AGPL-3.0-or-later

//! Scheduled automatic backups.
//!
//! Optional, off by default. When enabled the gateway holds a
//! `BackupScheduler` that wakes on a fixed interval, writes a fresh
//! archive to `<data_dir>/backups/mira-backup-<timestamp>.tar.gz`,
//! then prunes everything beyond the configured retention count. The
//! existing `/api/admin/backup` on-demand path is untouched.
//!
//! Held on the long-lived `Gateway` so its Drop-abort fires only at
//! process shutdown — same lifetime fix the companion scheduler
//! needed (0.189.1). Bare local in `Gateway::build()` would kill the
//! task microseconds after the "started" log; don't repeat that.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::backup::{write_backup, SCHEDULED_BACKUPS_DIR};

/// Short warm-up before the first tick so MIRA finishes opening DBs
/// before the scheduler asks for a snapshot.
const STARTUP_WARMUP: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct ScheduledBackupConfig {
    /// Seconds between snapshots. Default = 86_400 (daily).
    pub interval_secs:   u64,
    /// How many most-recent snapshots to keep on disk. Older snapshots
    /// are removed atomically after each successful write.
    pub retention_count: u32,
}

impl Default for ScheduledBackupConfig {
    fn default() -> Self {
        Self { interval_secs: 86_400, retention_count: 7 }
    }
}

/// Background task handle. Drop aborts the task; held on `Gateway`.
pub struct BackupScheduler {
    shutdown: Arc<Notify>,
    join:     Option<JoinHandle<()>>,
}

impl Drop for BackupScheduler {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if let Some(j) = self.join.take() { j.abort(); }
    }
}

impl BackupScheduler {
    pub fn spawn(
        data_dir:    PathBuf,
        config_path: PathBuf,
        cfg:         ScheduledBackupConfig,
    ) -> Self {
        let shutdown = Arc::new(Notify::new());
        let shutdown_clone = Arc::clone(&shutdown);
        let join = tokio::spawn(async move {
            tokio::time::sleep(STARTUP_WARMUP).await;
            info!("scheduled backups started (every {}s, retention={})",
                cfg.interval_secs, cfg.retention_count);
            let interval_d = Duration::from_secs(cfg.interval_secs.max(60));
            let mut iv = tokio::time::interval(interval_d);
            // First tick fires immediately — skip so the warm-up
            // actually waits before the first snapshot lands.
            iv.tick().await;
            loop {
                tokio::select! {
                    _ = iv.tick() => {
                        // Snapshot off the runtime — write_backup uses
                        // sync I/O and the sqlite3_backup loop can take
                        // ~seconds on a big DB; spawn_blocking keeps the
                        // tokio worker free.
                        let dd = data_dir.clone();
                        let cp = config_path.clone();
                        let ret = cfg.retention_count as usize;
                        let res = tokio::task::spawn_blocking(move || run_once(&dd, &cp, ret)).await;
                        match res {
                            Ok(Ok((path, bytes))) =>
                                info!("scheduled backup: wrote {} ({} bytes)",
                                    path.display(), bytes),
                            Ok(Err(e))  => warn!("scheduled backup failed: {}", e),
                            Err(e)      => warn!("scheduled backup task panicked: {}", e),
                        }
                    }
                    _ = shutdown_clone.notified() => {
                        info!("scheduled backups shutting down");
                        break;
                    }
                }
            }
        });
        Self { shutdown, join: Some(join) }
    }
}

/// Run one scheduled backup synchronously. Public so the agent's
/// `backup_create` tool can trigger an immediate snapshot using the
/// same path + naming convention as the loop. Returns `(path, bytes)`.
pub fn run_once(
    data_dir:    &Path,
    config_path: &Path,
    retention:   usize,
) -> Result<(PathBuf, u64), String> {
    let dir = data_dir.join(SCHEDULED_BACKUPS_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create backups dir: {e}"))?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let filename = format!("mira-backup-{}-{}.tar.gz", env!("CARGO_PKG_VERSION"), ts);
    let target = dir.join(&filename);

    // Stream the archive straight to disk. The on-demand endpoint
    // tempfiles + reads-into-memory to send via the response body;
    // here we write directly to the final path with no intermediate.
    {
        let f = std::fs::File::create(&target)
            .map_err(|e| format!("create backup file: {e}"))?;
        let bw = std::io::BufWriter::new(f);
        write_backup(data_dir, config_path, bw)
            .map_err(|e| format!("write_backup: {e}"))?;
    }
    let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);

    // Prune oldest beyond retention. Sort by filename — our timestamp
    // naming is monotonic so lexicographic order = chronological order.
    if let Err(e) = prune(&dir, retention) {
        warn!("scheduled backup: prune failed (non-fatal): {}", e);
    }
    Ok((target, bytes))
}

fn prune(dir: &Path, retention: usize) -> Result<(), String> {
    if retention == 0 { return Ok(()); } // 0 = unlimited
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("read backups dir: {e}"))?
        .filter_map(|e| e.ok().map(|d| d.path()))
        .filter(|p| {
            let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            n.starts_with("mira-backup-") && (n.ends_with(".tar.gz") || n.ends_with(".tar.gz.enc"))
        })
        .collect();
    files.sort();
    if files.len() <= retention { return Ok(()); }
    let drop = files.len() - retention;
    for old in files.into_iter().take(drop) {
        if let Err(e) = std::fs::remove_file(&old) {
            debug!("prune: skip {}: {}", old.display(), e);
        } else {
            info!("scheduled backup: pruned {}", old.display());
        }
    }
    Ok(())
}

/// List scheduled-backup files for this `data_dir`, newest first. Each
/// entry is `(filename, bytes, modified_unix_ms)`. Used by the listing
/// endpoint + the agent's `backup_list` tool.
pub fn list_scheduled(data_dir: &Path) -> Result<Vec<(String, u64, i64)>, String> {
    let dir = data_dir.join(SCHEDULED_BACKUPS_DIR);
    if !dir.exists() { return Ok(vec![]); }
    let mut out: Vec<(String, u64, i64)> = Vec::new();
    for e in std::fs::read_dir(&dir).map_err(|e| format!("read backups: {e}"))? {
        let e = match e { Ok(e) => e, Err(_) => continue };
        let name = match e.file_name().to_str() { Some(s) => s.to_string(), None => continue };
        if !name.starts_with("mira-backup-") { continue; }
        let md = match e.metadata() { Ok(m) => m, Err(_) => continue };
        let size = md.len();
        let mtime_ms = md.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        out.push((name, size, mtime_ms));
    }
    out.sort_by(|a, b| b.2.cmp(&a.2)); // newest first
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fake_backup(dir: &Path, name: &str, len: usize) {
        std::fs::write(dir.join(name), vec![0u8; len]).unwrap();
    }

    #[test]
    fn prune_keeps_newest_n() {
        let d = tempdir().unwrap();
        let bdir = d.path().join("backups");
        std::fs::create_dir(&bdir).unwrap();
        // Names sort lexicographically = chronologically.
        fake_backup(&bdir, "mira-backup-0.1.0-20260101T000000Z.tar.gz", 10);
        fake_backup(&bdir, "mira-backup-0.1.0-20260201T000000Z.tar.gz", 10);
        fake_backup(&bdir, "mira-backup-0.1.0-20260301T000000Z.tar.gz", 10);
        fake_backup(&bdir, "mira-backup-0.1.0-20260401T000000Z.tar.gz", 10);
        prune(&bdir, 2).unwrap();
        let mut remaining: Vec<String> = std::fs::read_dir(&bdir).unwrap()
            .filter_map(|e| e.ok().map(|d| d.file_name().to_string_lossy().into_owned()))
            .collect();
        remaining.sort();
        assert_eq!(remaining, vec![
            "mira-backup-0.1.0-20260301T000000Z.tar.gz".to_string(),
            "mira-backup-0.1.0-20260401T000000Z.tar.gz".to_string(),
        ]);
    }

    #[test]
    fn prune_retention_zero_means_unlimited() {
        let d = tempdir().unwrap();
        let bdir = d.path().join("backups");
        std::fs::create_dir(&bdir).unwrap();
        fake_backup(&bdir, "mira-backup-0.1.0-20260101T000000Z.tar.gz", 1);
        fake_backup(&bdir, "mira-backup-0.1.0-20260201T000000Z.tar.gz", 1);
        prune(&bdir, 0).unwrap();
        assert_eq!(std::fs::read_dir(&bdir).unwrap().count(), 2);
    }

    #[test]
    fn list_scheduled_returns_newest_first_and_skips_non_backups() {
        let d = tempdir().unwrap();
        let bdir = d.path().join("backups");
        std::fs::create_dir(&bdir).unwrap();
        fake_backup(&bdir, "mira-backup-0.1.0-20260101T000000Z.tar.gz", 5);
        // Tweak the mtime so the second file is unambiguously newer.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fake_backup(&bdir, "mira-backup-0.1.0-20260201T000000Z.tar.gz", 7);
        fake_backup(&bdir, "not-a-backup.txt", 3); // should be filtered out
        let out = list_scheduled(d.path()).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].0.contains("20260201"));
        assert!(out[1].0.contains("20260101"));
    }
}
