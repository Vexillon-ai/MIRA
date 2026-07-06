// SPDX-License-Identifier: AGPL-3.0-or-later

// src/install/data_version.rs
//! Data-dir version stamp + boot compatibility guard (R1).
//!
//! An upgrade may migrate the on-disk data (DBs, wiki, …) forward. Migrations
//! here are ADDITIVE by discipline, so an older binary can usually still read a
//! newer data dir. When that stops being true — a breaking/destructive
//! migration — the release that introduces it bumps [`DATA_MIN_READER`]. On
//! boot we compare: if THIS binary is older than the data's recorded min-reader,
//! we refuse to start with clear guidance instead of crashing cryptically or
//! corrupting state. Pairs with `mira rollback` (binary+config) and, for the
//! breaking case, a pre-upgrade backup restore.

use std::path::{Path, PathBuf};

/// Oldest MIRA version whose reader can safely open data written by THIS build.
/// Bump this ONLY in a release that lands a migration older binaries can't read
/// (destructive / renaming / format change). Additive column/table adds keep it
/// as-is. Kept well below any real release so the guard stays inert until a
/// genuine breaking change declares itself.
pub const DATA_MIN_READER: &str = "0.0.0";

fn stamp_path(data_dir: &Path) -> PathBuf {
    data_dir.join(".mira-data-version.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Stamp {
    /// Version that last wrote / migrated this data dir.
    written_by: String,
    /// Oldest binary that may read this data dir (the writer's DATA_MIN_READER).
    min_reader: String,
}

/// Record that THIS binary now owns the data dir. Call after migrations run at
/// startup. Best-effort — a write failure only means the next boot's guard has
/// stale info, never a crash.
pub fn stamp(data_dir: &Path) {
    let s = Stamp {
        written_by: env!("CARGO_PKG_VERSION").to_string(),
        min_reader: DATA_MIN_READER.to_string(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&s) {
        let _ = std::fs::write(stamp_path(data_dir), json);
    }
}

/// Boot guard: refuse to start if this (older) binary can't safely read data a
/// newer binary migrated. `Err(message)` when incompatible; `Ok(())` when safe
/// or when there's no stamp yet (fresh install / pre-guard data).
pub fn guard(data_dir: &Path) -> Result<(), String> {
    let Ok(raw) = std::fs::read_to_string(stamp_path(data_dir)) else { return Ok(()) };
    let Ok(stamp) = serde_json::from_str::<Stamp>(&raw) else { return Ok(()) };
    let current = env!("CARGO_PKG_VERSION");
    if version_lt(current, &stamp.min_reader) {
        return Err(format!(
            "This MIRA binary is v{current}, but the data directory was migrated by v{} and \
             requires at least v{} to open safely.\n\
             You've rolled back further than the on-disk data supports. Options:\n\
             • roll forward — install v{} or newer, or\n\
             • restore the pre-upgrade backup taken before that upgrade \
             (`mira backup list` / `mira backup restore`).\n\
             MIRA is refusing to start to avoid corrupting your data.",
            stamp.written_by, stamp.min_reader, stamp.written_by,
        ));
    }
    Ok(())
}

fn version_lt(a: &str, b: &str) -> bool {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(x), Ok(y)) => x < y,
        _ => a < b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_stamp_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(guard(tmp.path()).is_ok());
    }

    #[test]
    fn stamp_then_guard_same_version_ok() {
        let tmp = tempfile::tempdir().unwrap();
        stamp(tmp.path());
        assert!(guard(tmp.path()).is_ok());
    }

    #[test]
    fn older_binary_than_min_reader_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        // Simulate a newer binary having stamped a high min_reader.
        let s = Stamp { written_by: "9.9.9".into(), min_reader: "9.9.9".into() };
        std::fs::write(stamp_path(tmp.path()), serde_json::to_string(&s).unwrap()).unwrap();
        let err = guard(tmp.path()).unwrap_err();
        assert!(err.contains("refusing to start"), "got: {err}");
        assert!(err.contains("9.9.9"));
    }
}
