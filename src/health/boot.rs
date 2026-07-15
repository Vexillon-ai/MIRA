// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/boot.rs
//! Track process restart history so the `process.restart_count_24h`
//! detector can flag **crash loops** without depending on systemd journal
//! parsing — and, crucially, without alarming on *operator-initiated*
//! restarts (config changes, upgrades, `systemctl restart`).
//!
//! Each startup [`record_boot`] appends an entry to
//! `<data_dir>/boot_history.json`. Whether that boot followed a **clean**
//! shutdown is derived from a marker file: MIRA's graceful-shutdown path calls
//! [`mark_clean_shutdown`], and `record_boot` consumes that marker — present =
//! the previous run exited cleanly (planned restart), absent = the previous run
//! died uncleanly (crash / OOM / kill -9 / power loss). Only *unclean* restarts
//! count toward the crash-loop threshold.
//!
//! The file is a JSON array capped to the last 64 entries. For backward
//! compatibility it still reads the legacy `[i64, i64, …]` timestamp array
//! (those pre-upgrade entries are treated as clean, so an upgrade never trips
//! the crash-loop alarm on its own history).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAX_ENTRIES: usize = 64;

fn path_for(data_dir: &Path) -> PathBuf {
    data_dir.join("boot_history.json")
}

fn clean_marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join("clean_shutdown.marker")
}

/// One recorded boot: when it happened and whether it followed a clean shutdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootEntry {
    pub ts: i64,
    /// True when the previous run shut down gracefully (operator restart);
    /// false when it died uncleanly (a crash — what a loop is made of).
    #[serde(default = "default_clean")]
    pub clean: bool,
}

fn default_clean() -> bool { true }

/// Accepts either the new `{ts, clean}` object or a legacy bare `i64` timestamp.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawEntry {
    Full(BootEntry),
    Legacy(i64),
}

impl From<RawEntry> for BootEntry {
    fn from(r: RawEntry) -> Self {
        match r {
            RawEntry::Full(e)   => e,
            // Legacy entries predate cleanliness tracking; treat as clean so an
            // upgrade doesn't retroactively flag old restarts as crashes.
            RawEntry::Legacy(ts) => BootEntry { ts, clean: true },
        }
    }
}

/// Write the clean-shutdown marker. Called from the graceful-shutdown path so
/// the *next* boot knows this run ended intentionally. Best-effort.
pub fn mark_clean_shutdown(data_dir: &Path) -> std::io::Result<()> {
    let path = clean_marker_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, chrono::Utc::now().timestamp().to_string())
}

/// Append this boot to the history. `clean` is derived by consuming the
/// clean-shutdown marker: present → the prior run exited gracefully; absent →
/// it died uncleanly. Trims to the last MAX_ENTRIES. Best-effort — a write
/// failure is logged at the call site; a boot-history I/O issue never aborts
/// startup.
pub fn record_boot(data_dir: &Path) -> std::io::Result<()> {
    let marker = clean_marker_path(data_dir);
    let clean  = marker.exists();
    // Consume the marker so a later crash isn't mistaken for a clean restart.
    let _ = std::fs::remove_file(&marker);

    let path = path_for(data_dir);
    let mut history = read_boots(data_dir).unwrap_or_default();
    history.push(BootEntry { ts: chrono::Utc::now().timestamp(), clean });
    if history.len() > MAX_ENTRIES {
        let drop = history.len() - MAX_ENTRIES;
        history.drain(..drop);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(&history).map_err(std::io::Error::other)?;
    std::fs::write(&path, bytes)
}

/// Read the boot history from disk. Missing file → empty Vec (a fresh install
/// hasn't recorded any boot yet). Tolerates the legacy timestamp-array format.
pub fn read_boots(data_dir: &Path) -> std::io::Result<Vec<BootEntry>> {
    let path = path_for(data_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)?;
    let raw: Vec<RawEntry> = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    Ok(raw.into_iter().map(BootEntry::from).collect())
}

/// Total boots on or after `since` (unix seconds) — clean + unclean.
pub fn count_boots_since(data_dir: &Path, since: i64) -> std::io::Result<usize> {
    Ok(read_boots(data_dir)?.iter().filter(|e| e.ts >= since).count())
}

/// **Unclean** restarts on or after `since` — the crash-loop signal, excluding
/// operator-initiated (clean) restarts.
pub fn count_crashes_since(data_dir: &Path, since: i64) -> std::io::Result<usize> {
    Ok(read_boots(data_dir)?.iter().filter(|e| e.ts >= since && !e.clean).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmarked_boots_count_as_crashes() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..3 { record_boot(dir.path()).unwrap(); }
        assert_eq!(count_boots_since(dir.path(), 0).unwrap(), 3);
        assert_eq!(count_crashes_since(dir.path(), 0).unwrap(), 3); // no marker → crashes
    }

    #[test]
    fn clean_marked_boots_are_not_crashes() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate: graceful shutdown → clean marker → next boot is clean.
        mark_clean_shutdown(dir.path()).unwrap();
        record_boot(dir.path()).unwrap();
        mark_clean_shutdown(dir.path()).unwrap();
        record_boot(dir.path()).unwrap();
        // A third boot with NO marker = a crash.
        record_boot(dir.path()).unwrap();
        assert_eq!(count_boots_since(dir.path(), 0).unwrap(), 3);
        assert_eq!(count_crashes_since(dir.path(), 0).unwrap(), 1); // only the unmarked one
    }

    #[test]
    fn legacy_timestamp_array_reads_as_clean() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path_for(dir.path()), b"[100, 200, 300]").unwrap();
        assert_eq!(count_boots_since(dir.path(), 0).unwrap(), 3);
        assert_eq!(count_crashes_since(dir.path(), 0).unwrap(), 0); // legacy = clean
    }

    #[test]
    fn missing_file_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_boots_since(dir.path(), 0).unwrap(), 0);
        assert_eq!(count_crashes_since(dir.path(), 0).unwrap(), 0);
    }

    #[test]
    fn cap_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..(MAX_ENTRIES + 5) { record_boot(dir.path()).unwrap(); }
        assert_eq!(read_boots(dir.path()).unwrap().len(), MAX_ENTRIES);
    }
}
