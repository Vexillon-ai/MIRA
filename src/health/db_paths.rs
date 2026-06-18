// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/db_paths.rs
//! Central registry of every SQLite DB MIRA opens, plus its WAL sidecar.
//!
//! The integrity-check + WAL-size detectors iterate this list. Adding a
//! new DB to MIRA means appending one entry here — the detectors and
//! the future "DB sizes" UI pick it up automatically.
//!
//! Naming is the SQLite filename without the `.db` suffix; that's what
//! shows up in detector messages, watchdog incidents, and (eventually)
//! the dashboard. Keep it short.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DbEntry {
    /// Short label used in detector messages and dashboard rows.
    pub name: &'static str,
    /// Absolute path to the SQLite file.
    pub path: PathBuf,
}

impl DbEntry {
    /// `<file>-wal` sidecar path. Returns whether or not the sidecar
    /// currently exists; checking presence is the caller's job.
    pub fn wal_path(&self) -> PathBuf {
        let mut p = self.path.clone();
        let mut os = p.file_name().unwrap_or_default().to_os_string();
        os.push("-wal");
        p.set_file_name(os);
        p
    }
}

/// Every DB MIRA opens at gateway startup. Order is roughly by domain
/// (automations / auth / history / memory / agents / misc) — purely
/// cosmetic, drives nothing.
pub fn all_dbs(data_dir: &Path) -> Vec<DbEntry> {
    let names = [
        "automations",
        "auth",
        "history",
        "memory",
        "agent_audit",
        "tools",
        "calendar",
        "skill_secrets",
        "skill_prefs",
        "admin_policy_rules",
        "health",
    ];
    names.iter().map(|n| DbEntry {
        name: *n,
        path: data_dir.join(format!("{n}.db")),
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wal_path_appends_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let dbs = all_dbs(dir.path());
        let auth = dbs.iter().find(|e| e.name == "auth").unwrap();
        assert_eq!(auth.path, dir.path().join("auth.db"));
        assert_eq!(auth.wal_path(), dir.path().join("auth.db-wal"));
    }
}
