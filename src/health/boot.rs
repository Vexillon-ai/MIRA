// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/boot.rs
//! Track process restart history so the `process.restart_count_24h`
//! detector can flag crash loops without depending on systemd journal
//! parsing.
//!
//! On startup the gateway calls [`record_boot`], which appends the
//! current unix timestamp to `<data_dir>/boot_history.json` (capped to
//! the last 64 entries — bounded against runaway crash-loop bloat).
//! The detector then reads the file and counts entries in the last 24h.
//!
//! No fancy structure — pure JSON array of i64 timestamps. Cheap to
//! parse, cheap to write, no DB locks contended at the worst possible
//! moment (startup).

use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 64;

fn path_for(data_dir: &Path) -> PathBuf {
    data_dir.join("boot_history.json")
}

/// Append `now` to the boot history file, trimming to the last
/// MAX_ENTRIES. Best-effort — a write failure is logged at the call
/// site; we never want a boot-history I/O issue to abort startup.
pub fn record_boot(data_dir: &Path) -> std::io::Result<()> {
    let path = path_for(data_dir);
    let mut history = read_boots(data_dir).unwrap_or_default();
    history.push(chrono::Utc::now().timestamp());
    if history.len() > MAX_ENTRIES {
        let drop = history.len() - MAX_ENTRIES;
        history.drain(..drop);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(&history)
        .map_err(std::io::Error::other)?;
    std::fs::write(&path, bytes)
}

/// Read the boot timestamps from disk. Missing file → empty Vec, not
/// an error: a fresh install hasn't recorded any boot yet.
pub fn read_boots(data_dir: &Path) -> std::io::Result<Vec<i64>> {
    let path = path_for(data_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

/// How many boots happened on or after `since` (unix seconds).
pub fn count_boots_since(data_dir: &Path, since: i64) -> std::io::Result<usize> {
    let history = read_boots(data_dir)?;
    Ok(history.iter().filter(|t| **t >= since).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_count_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..3 { record_boot(dir.path()).unwrap(); }
        assert_eq!(count_boots_since(dir.path(), 0).unwrap(), 3);
        assert_eq!(count_boots_since(dir.path(), i64::MAX).unwrap(), 0);
    }

    #[test]
    fn missing_file_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_boots_since(dir.path(), 0).unwrap(), 0);
    }

    #[test]
    fn cap_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..(MAX_ENTRIES + 5) { record_boot(dir.path()).unwrap(); }
        assert_eq!(read_boots(dir.path()).unwrap().len(), MAX_ENTRIES);
    }
}
