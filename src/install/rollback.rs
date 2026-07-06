// SPDX-License-Identifier: AGPL-3.0-or-later

// src/install/rollback.rs
//! Upgrade rollback (R1).
//!
//! Every binary upgrade first snapshots the CURRENT binary + config file to
//! `<data_dir>/rollback/<version>/` ([`save_snapshot`]). If the new build
//! misbehaves, [`run_rollback`] restores that snapshot — binary + config — and
//! restarts. `mira rollback` is a standalone CLI so it works even when the new
//! binary crash-loops and the web UI is unreachable.
//!
//! Scope (R1): **binary + config** rollback. Reverting *data* migrations is out
//! of scope here — additive migrations let an older binary read a newer DB, and
//! [`crate::install::data_version`]'s boot guard refuses to start (with clear
//! guidance) when that isn't safe. A full pre-upgrade data snapshot/restore is
//! the R2 follow-up, built on the existing backup subsystem.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use super::binary_upgrade::{atomic_swap, BINARY_NAME};

/// Keep the N most recent snapshots; older ones are pruned after each upgrade.
const KEEP_SNAPSHOTS: usize = 3;

/// The config file the running server uses — `MIRA_CONFIG` override, else the
/// default `~/.mira/config/mira_config.json`.
fn config_file_path() -> PathBuf {
    std::env::var_os("MIRA_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::default_config_path)
}

/// `<data_dir>/rollback` — resolved the same way at snapshot and restore time
/// (`MIRA_DATA_DIR` / config `data_dir` / default) so both agree. If the config
/// can't be loaded (the very case you'd roll back from), we fall back to the
/// default data dir; set `MIRA_DATA_DIR` to override on a relocated install.
pub fn rollback_root() -> PathBuf {
    let data_dir = crate::config::MiraConfig::load(None)
        .map(|c| c.data_dir_path())
        .unwrap_or_else(|_| crate::config::default_data_dir_path());
    data_dir.join("rollback")
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub version: String,
    pub dir:     PathBuf,
    pub binary:  PathBuf,
    /// Snapshotted config, if the config file existed at snapshot time.
    pub config:  Option<PathBuf>,
}

/// Snapshot the current binary + config before an upgrade swaps them out.
pub fn save_snapshot(current_binary: &Path, from_version: &str) -> Result<Snapshot, Box<dyn Error>> {
    let dir = rollback_root().join(from_version);
    fs::create_dir_all(&dir)?;

    let bin_dest = dir.join(BINARY_NAME);
    fs::copy(current_binary, &bin_dest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&bin_dest, fs::Permissions::from_mode(0o755));
    }

    let cfg_src = config_file_path();
    let config = if cfg_src.is_file() {
        let d = dir.join("mira_config.json");
        fs::copy(&cfg_src, &d)?;
        Some(d)
    } else {
        None
    };

    prune_snapshots(KEEP_SNAPSHOTS);
    Ok(Snapshot { version: from_version.to_string(), dir, binary: bin_dest, config })
}

/// Saved snapshots, newest (highest semver) first.
pub fn list_snapshots() -> Vec<Snapshot> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(rollback_root()) else { return out };
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.is_dir() { continue; }
        let binary = dir.join(BINARY_NAME);
        if !binary.is_file() { continue; }
        let version = e.file_name().to_string_lossy().to_string();
        let cfg = dir.join("mira_config.json");
        let config = cfg.is_file().then_some(cfg);
        out.push(Snapshot { version, dir, binary, config });
    }
    out.sort_by(|a, b| cmp_version(&b.version, &a.version));
    out
}

fn cmp_version(a: &str, b: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

fn prune_snapshots(keep: usize) {
    for s in list_snapshots().into_iter().skip(keep) {
        let _ = fs::remove_dir_all(&s.dir);
    }
}

pub struct RollbackOptions {
    /// Target version to restore. `None` = the most recent snapshot.
    pub version:    Option<String>,
    /// Restore the binary/config but don't restart the service.
    pub no_restart: bool,
}

/// Restore a snapshot's binary + config and restart. Standalone-safe.
pub fn run_rollback(opts: RollbackOptions) -> Result<(), Box<dyn Error>> {
    let snaps = list_snapshots();
    if snaps.is_empty() {
        return Err(format!(
            "no rollback snapshots found under {}. Snapshots are created automatically on \
             each upgrade — there's nothing to roll back to yet.", rollback_root().display()
        ).into());
    }

    let target = match opts.version.as_deref() {
        Some(v) => {
            let v = v.trim_start_matches('v');
            snaps.iter().find(|s| s.version == v).cloned().ok_or_else(|| format!(
                "no snapshot for version {v}. Available: {}",
                snaps.iter().map(|s| s.version.as_str()).collect::<Vec<_>>().join(", ")
            ))?
        }
        None => snaps.first().cloned().unwrap(),
    };

    let current = env!("CARGO_PKG_VERSION");
    println!("Rolling back:  {current} → {}", target.version);
    println!("Snapshot:      {}", target.dir.display());

    // Restore the binary.
    let current_binary = std::env::current_exe()?;
    println!("Restoring binary → {}", current_binary.display());
    atomic_swap(&target.binary, &current_binary)?;
    println!("✓ binary restored");

    // Restore the config, keeping a copy of the current one first.
    if let Some(cfg_snap) = &target.config {
        let cfg_dest = config_file_path();
        if cfg_dest.is_file() {
            let mut backup = cfg_dest.clone().into_os_string();
            backup.push(".pre-rollback");
            let _ = fs::copy(&cfg_dest, PathBuf::from(backup));
        }
        if let Some(parent) = cfg_dest.parent() { let _ = fs::create_dir_all(parent); }
        fs::copy(cfg_snap, &cfg_dest)?;
        println!("✓ config restored → {}", cfg_dest.display());
    }

    if opts.no_restart {
        println!("\nSkipping restart per --no-restart. Run `mira restart` when ready.");
        return Ok(());
    }
    let unit_installed = crate::install::supervisor_unit_path().map(|p| p.exists()).unwrap_or(false);
    if !unit_installed {
        println!("\nNo service unit installed — restart MIRA manually to run {}.", target.version);
        return Ok(());
    }
    println!("\nRestarting service…");
    crate::install::run_restart()?;
    println!("✓ rolled back to {}", target.version);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmp_version_orders_newest_first_semantics() {
        assert!(cmp_version("0.293.0", "0.292.9").is_gt());
        assert!(cmp_version("0.292.0", "0.293.0").is_lt());
        // garbage falls back to string compare without panicking
        let _ = cmp_version("weird", "0.293.0");
    }

    #[test]
    fn save_list_and_prune_snapshots() {
        // Point the data dir at a temp dir so we don't touch a real ~/.mira.
        let tmp = tempfile::tempdir().unwrap();
        // Safe in a single-threaded test; resolves rollback_root() → tmp/rollback.
        unsafe { std::env::set_var("MIRA_DATA_DIR", tmp.path()); }

        // A fake "current binary" to snapshot.
        let fake_bin = tmp.path().join("mira-bin");
        fs::write(&fake_bin, b"\x7fELF fake").unwrap();

        for v in ["0.290.0", "0.291.0", "0.292.0", "0.293.0"] {
            save_snapshot(&fake_bin, v).unwrap();
        }
        let snaps = list_snapshots();
        // KEEP_SNAPSHOTS = 3 → oldest (0.290.0) pruned; newest first.
        assert_eq!(snaps.len(), 3, "should keep 3");
        assert_eq!(snaps[0].version, "0.293.0");
        assert!(snaps.iter().all(|s| s.binary.is_file()));
        assert!(!snaps.iter().any(|s| s.version == "0.290.0"), "oldest pruned");

        unsafe { std::env::remove_var("MIRA_DATA_DIR"); }
    }
}
