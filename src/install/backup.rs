// SPDX-License-Identifier: AGPL-3.0-or-later

//! Q1.5 — data-dir + config backup/restore.
//!
//! The goal: a single tar.gz the operator can keep somewhere safe
//! that, applied to a fresh MIRA install, reproduces their
//! conversations, memories, wiki, automations, channel accounts,
//! companion settings, and per-user voice prefs. Models / sandbox
//! rootfs / TTS voices are excluded — they're re-downloadable on demand
//! and would blow the tarball past gigabytes for no operational gain.
//!
//! ## SQLite consistency
//!
//! Naively `tar`-ing the data dir while MIRA is running risks a
//! torn-write snapshot (the WAL might be mid-checkpoint). We sidestep
//! that by using rusqlite's online backup API for every `.db` file —
//! it produces a consistent point-in-time copy even with active
//! writers. The `.db-wal` / `.db-shm` sidecars are then excluded
//! from the archive because the backed-up `.db` already contains
//! everything they would have replayed.
//!
//! ## On-disk files
//!
//! Walked verbatim:
//!   - `wikis/`                   (markdown knowledge base)
//!   - `avatars/`                 (uploaded user images)
//!   - `artifacts/`               (per-task outputs)
//!   - `skills/`                  (installed skill bundles + secrets)
//!   - `web_push_vapid.key`       (VAPID keypair — losing this
//!                                 invalidates every browser
//!                                 subscription, so include it)
//!
//! Plus the **config file** (typically `~/.mira/config/mira_config.json`),
//! which lives outside `data_dir` but carries the provider API keys
//! and security settings.
//!
//! ## Excluded
//!
//! These are huge / regeneratable / runtime-only:
//!   - `tts/`, `stt/`, `sandbox/`, `models/`, `cache/`, `deps/`
//!   - `boot_history.json`, `local.token`
//!   - all `.db-wal` / `.db-shm` sidecars
//!
//! ## Restore semantics
//!
//! The restore endpoint accepts the same tar.gz, extracts it to a
//! staging dir under `data_dir/.restore_staged/`, writes a marker
//! file, and triggers a restart. On startup we detect the marker,
//! move the current data dir aside (`.pre_restore_backup/`), promote
//! the staged files into place, then start normally. If the new
//! state is broken the operator can rename `.pre_restore_backup/`
//! back to `data/` and try again.

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::{backup::Backup, Connection, OpenFlags};
use tar::Builder;
use tracing::{info, warn};

/// Files / dirs at the top level of data_dir we always include in a
/// backup (in addition to all *.db files which are snapshotted via
/// the SQLite backup API). Plain on-disk files / markdown / blobs.
const INCLUDE_DIRS: &[&str] = &[
    "wikis",
    "avatars",
    "artifacts",
    "skills",
];

/// Standalone files at the top level of data_dir we always include.
const INCLUDE_FILES: &[&str] = &[
    "web_push_vapid.key",
];

/// Substrings used to filter the data_dir walk. Any entry whose name
/// (or path) starts with / ends with one of these is skipped.
const EXCLUDE_DIRS: &[&str] = &[
    "tts", "stt", "sandbox", "models", "cache", "deps",
    // Restore staging + the rollback safety copy — including them
    // would either cause a self-recursion or restore stale state.
    ".restore_staged", ".pre_restore_backup",
    // Scheduled-backup rotation lives here; including it would
    // recursively grow each backup by all prior backups.
    "backups",
];

/// Marker file at `data_dir/.restore_pending` written by the restore
/// endpoint. Startup hook checks for it; presence triggers the swap.
pub const RESTORE_PENDING_MARKER: &str = ".restore_pending";
/// Staging dir under data_dir where the uploaded archive's extracted
/// contents land until the next startup applies them.
pub const RESTORE_STAGING_DIR:    &str = ".restore_staged";
/// Where the previous data dir is moved before the staged files
/// promote into place. Kept around so the operator can roll back.
pub const PRE_RESTORE_BACKUP_DIR: &str = ".pre_restore_backup";
/// Subdir of `data_dir` where the scheduled backup loop writes its
/// rotated snapshots. Excluded from `write_backup` to prevent the
/// obvious self-recursion.
pub const SCHEDULED_BACKUPS_DIR:  &str = "backups";

/// Current on-disk backup archive format. Bumped when the archive's
/// internal layout changes incompatibly. Restore refuses any backup
/// whose `manifest.json` declares a different value — the data is
/// safer behind a "please use a compatible MIRA version" error than
/// a half-decoded restore.
pub const BACKUP_FORMAT_VERSION: u32 = 1;

#[derive(Debug, serde::Deserialize)]
struct BackupManifest {
    #[serde(default)]
    format_version: u32,
    #[serde(default)]
    mira_version: String,
    #[serde(default)]
    created_at: String,
}

/// Parse the staged `manifest.json` and refuse the restore when the
/// archive isn't safely applicable to this MIRA. Two checks:
///   1. `format_version` must equal `BACKUP_FORMAT_VERSION` — different
///      values mean the on-disk layout has changed and an old/new
///      restorer would mis-route files.
///   2. `mira_version`'s major component must match the running MIRA's.
///      Same-major backups are guaranteed compatible by SemVer; cross-
///      major is refused since DB schema migrations have shipped in
///      minor bumps historically and might not roll forward cleanly.
/// Returns a human-readable error message on incompatibility.
pub fn check_backup_compatibility(manifest_json: &str) -> Result<(), String> {
    let manifest: BackupManifest = serde_json::from_str(manifest_json)
        .map_err(|e| format!("manifest.json parse failed: {e} — not a MIRA backup or corrupted"))?;
    if manifest.format_version != BACKUP_FORMAT_VERSION {
        return Err(format!(
            "backup format_version {} not supported by this MIRA \
             (expected {}). Restore with a matching MIRA version.",
            manifest.format_version, BACKUP_FORMAT_VERSION,
        ));
    }
    // Major-version match. Empty / unparseable `mira_version` is
    // accepted (forward compat for backups missing the field).
    let this_major = env!("CARGO_PKG_VERSION").split('.').next().unwrap_or("0");
    if let Some(backup_major) = manifest.mira_version.split('.').next() {
        if !backup_major.is_empty() && backup_major != this_major {
            return Err(format!(
                "backup is from MIRA {} (major {}); this MIRA is {} (major {}). \
                 Cross-major restore is not supported.",
                manifest.mira_version, backup_major,
                env!("CARGO_PKG_VERSION"), this_major,
            ));
        }
    }
    let when = if manifest.created_at.is_empty() { "(unknown)" } else { &manifest.created_at };
    info!("restore: backup version-check passed (mira={}, created={})",
        manifest.mira_version, when);
    Ok(())
}

// ── Create a backup ──────────────────────────────────────────────────────────

/// Stream a complete backup of `data_dir` + `config_path` into `out`
/// as a gzip-compressed tar archive. `out` is whatever the caller
/// hands us — a file, a Vec<u8>, an axum response body, etc.
///
/// Returns the number of bytes written before compression.
pub fn write_backup<W: Write>(
    data_dir:    &Path,
    config_path: &Path,
    out:         W,
) -> Result<(), Box<dyn Error>> {
    // Compression level 6 — default; good size/CPU trade-off for
    // mostly-text databases + markdown.
    let gz = GzEncoder::new(out, Compression::default());
    let mut tar = Builder::new(BufWriter::new(gz));
    // Don't follow symlinks — preserve them as links instead. Avoids
    // accidentally backing up something the user pointed into their
    // data dir.
    tar.follow_symlinks(false);

    // 1. Per-database snapshots via SQLite's backup API. Walks every
    //    *.db file at the top of data_dir (subdir DBs are rare in this
    //    codebase but easy to add if they appear).
    let snapshot_dir = tempfile::tempdir()?;
    let snap_root = snapshot_dir.path().to_path_buf();
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".db") { continue; }
        let src = entry.path();
        let snap_path = snap_root.join(&name_str.as_ref());
        snapshot_db(&src, &snap_path)?;
        info!("backup: snapshotted {} -> staging", name_str);
        tar.append_path_with_name(
            &snap_path,
            Path::new("data").join(name_str.as_ref()),
        )?;
    }

    // 2. The whitelisted dirs (markdown wiki, avatars, artifacts,
    //    skills). Walk recursively; tar's append_dir_all handles the
    //    layout. Missing dirs are silently skipped — a fresh install
    //    may not have written `artifacts/` yet.
    for dir in INCLUDE_DIRS {
        let src = data_dir.join(dir);
        if !src.exists() { continue; }
        info!("backup: including dir {}", dir);
        tar.append_dir_all(Path::new("data").join(dir), &src)?;
    }

    // 3. Standalone include files (VAPID keypair, …).
    for fname in INCLUDE_FILES {
        let src = data_dir.join(fname);
        if !src.exists() { continue; }
        tar.append_path_with_name(&src, Path::new("data").join(fname))?;
    }

    // 4. Anything else at the top level of data_dir that doesn't
    //    match an exclude — catches future state we forgot to enumerate.
    //    Smaller surprises beat unintentional data loss.
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().into_owned();

        if EXCLUDE_DIRS.iter().any(|e| name_str == *e) { continue; }
        if name_str.ends_with(".db-wal") || name_str.ends_with(".db-shm") { continue; }
        if name_str.ends_with(".db")     { continue; } // already snapshotted
        if INCLUDE_DIRS.iter().any(|e| name_str == *e) { continue; } // already added
        if INCLUDE_FILES.iter().any(|e| name_str == *e) { continue; } // already added
        // Runtime-only files we deliberately drop on restore so the
        // new boot mints fresh values rather than colliding with the
        // backed-up host.
        if name_str == "local.token" || name_str == "boot_history.json" { continue; }

        let src = entry.path();
        let dst = Path::new("data").join(&name_str);
        if src.is_dir() {
            // Best-effort — log + skip on permission errors instead of
            // bailing the whole backup.
            if let Err(e) = tar.append_dir_all(&dst, &src) {
                warn!("backup: skipping {}: {}", name_str, e);
            }
        } else if src.is_file() {
            if let Err(e) = tar.append_path_with_name(&src, &dst) {
                warn!("backup: skipping {}: {}", name_str, e);
            }
        }
    }

    // 5. Config file — lives outside data_dir; placed at
    //    `config/<filename>` in the archive so restore can route it
    //    back to wherever the operator's config path was set.
    if config_path.exists() {
        let cfg_name = config_path.file_name()
            .ok_or_else(|| -> Box<dyn Error> { "config path has no file name".into() })?;
        tar.append_path_with_name(
            config_path,
            Path::new("config").join(cfg_name),
        )?;
        info!("backup: included config {}", config_path.display());
    }

    // 6. Manifest — a tiny JSON describing the source so a restore can
    //    sanity-check (mira version, timestamp, what's inside).
    let manifest = serde_json::json!({
        "mira_version":  env!("CARGO_PKG_VERSION"),
        "created_at":    chrono::Utc::now().to_rfc3339(),
        "data_dir":      data_dir.display().to_string(),
        "config_path":   config_path.display().to_string(),
        "format_version": 1,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_path("manifest.json")?;
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, manifest_bytes.as_slice())?;

    // Finish closes the gzip footer + flushes to the underlying writer.
    tar.finish()?;
    drop(tar);
    // Hold the tempdir alive until after `finish` (so the snapshot
    // files are still on disk when tar reads them).
    snapshot_dir.close().ok();
    Ok(())
}

/// Use SQLite's online backup API to copy `src` to `dst` as a
/// consistent snapshot. Works while another connection is actively
/// writing — the API takes a read-lock per page batch.
fn snapshot_db(src: &Path, dst: &Path) -> Result<(), Box<dyn Error>> {
    let src_conn = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| -> Box<dyn Error> {
            format!("open source db {}: {e}", src.display()).into()
        })?;
    let mut dst_conn = Connection::open(dst)
        .map_err(|e| -> Box<dyn Error> {
            format!("create snapshot {}: {e}", dst.display()).into()
        })?;
    let backup = Backup::new(&src_conn, &mut dst_conn)
        .map_err(|e| -> Box<dyn Error> { format!("backup init: {e}").into() })?;
    backup.run_to_completion(
        // 100 pages per step; enough to make progress on a busy DB
        // without holding the read lock too long per slice.
        100,
        std::time::Duration::from_millis(10),
        None,
    ).map_err(|e| -> Box<dyn Error> { format!("backup run: {e}").into() })?;
    Ok(())
}

// ── Restore staging + startup swap ───────────────────────────────────────────

/// Called from the HTTP restore handler after the uploaded tar.gz has
/// been written to disk. Extracts it under `data_dir/.restore_staged/`
/// and writes a marker file the startup hook will see on the next
/// boot. Does NOT swap files in — that happens at startup so the
/// restore takes effect on a quiesced data dir.
pub fn stage_restore(
    data_dir:    &Path,
    archive:     &Path,
) -> Result<(), Box<dyn Error>> {
    let staging = data_dir.join(RESTORE_STAGING_DIR);
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    let f = File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut tar = tar::Archive::new(gz);
    tar.unpack(&staging)?;

    // Sanity-check — every restorable backup carries a manifest.json
    // at the archive root.
    let manifest_path = staging.join("manifest.json");
    if !manifest_path.exists() {
        // Roll back the staging dir so a subsequent restore attempt
        // doesn't see a half-populated layout.
        let _ = std::fs::remove_dir_all(&staging);
        return Err(
            "uploaded archive doesn't look like a MIRA backup — \
             no manifest.json found. Make sure it was produced by \
             /api/admin/backup."
            .into(),
        );
    }
    // Version-compat guard — refuse archives this MIRA can't safely
    // promote (different format_version or cross-major mira_version).
    let manifest_bytes = std::fs::read_to_string(&manifest_path)?;
    if let Err(msg) = check_backup_compatibility(&manifest_bytes) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(msg.into());
    }

    // Write the marker — atomic via rename so a crashed restore
    // doesn't leave a partial marker that the startup hook acts on.
    let marker = data_dir.join(RESTORE_PENDING_MARKER);
    let tmp = marker.with_extension("pending.tmp");
    std::fs::write(&tmp, chrono::Utc::now().to_rfc3339())?;
    std::fs::rename(&tmp, &marker)?;
    info!(
        "restore: staged + marker written ({} bytes archive)",
        std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0),
    );
    Ok(())
}

/// Called from main.rs at startup, before any DB connection opens.
/// When the marker exists, moves the current data dir aside,
/// promotes the staged files into place, removes the marker, and
/// returns Ok(true) to indicate the swap ran (caller logs it). When
/// the marker is absent, no-ops with Ok(false).
pub fn apply_pending_restore(
    data_dir:    &Path,
    config_path: &Path,
) -> Result<bool, Box<dyn Error>> {
    let marker = data_dir.join(RESTORE_PENDING_MARKER);
    if !marker.exists() { return Ok(false); }
    let staging = data_dir.join(RESTORE_STAGING_DIR);
    if !staging.exists() {
        // Marker but no staging — clean it up so we don't loop.
        let _ = std::fs::remove_file(&marker);
        return Err("restore marker present but no staging dir — aborting".into());
    }

    info!("restore: applying staged backup from {}", staging.display());

    // 1. Move the current data dir contents (except the staging /
    //    rollback dirs themselves) into a sibling .pre_restore_backup/.
    //    If the operator wants to roll back, they rename it back.
    let rollback = data_dir.join(PRE_RESTORE_BACKUP_DIR);
    if rollback.exists() {
        // Stale rollback from a previous restore — keep the most
        // recent one only, since multiple historical snapshots
        // weren't requested.
        std::fs::remove_dir_all(&rollback)?;
    }
    std::fs::create_dir(&rollback)?;
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().into_owned();
        if name_str == RESTORE_STAGING_DIR
            || name_str == PRE_RESTORE_BACKUP_DIR
            || name_str == RESTORE_PENDING_MARKER
        {
            continue;
        }
        let dst = rollback.join(&name_str);
        std::fs::rename(entry.path(), &dst)?;
    }

    // 2. Promote staged/data/* into data_dir/.
    let staged_data = staging.join("data");
    if staged_data.exists() {
        for entry in std::fs::read_dir(&staged_data)? {
            let entry = entry?;
            let dst = data_dir.join(entry.file_name());
            std::fs::rename(entry.path(), &dst)?;
        }
    }

    // 3. Restore the config file when the archive carried one. We
    //    write to the operator-configured config_path (not whatever
    //    path the source MIRA had), so a cross-host restore lands at
    //    the right place.
    let staged_config = staging.join("config");
    if staged_config.exists() {
        for entry in std::fs::read_dir(&staged_config)? {
            let entry = entry?;
            // Only one file expected; copy it to config_path
            // regardless of original name.
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), config_path)?;
            break;
        }
    }

    // 4. Clean up.
    std::fs::remove_dir_all(&staging)?;
    std::fs::remove_file(&marker)?;

    info!("restore: complete (previous data archived at {})", rollback.display());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE kv(k TEXT PRIMARY KEY, v TEXT);
             INSERT INTO kv VALUES ('hello', 'world');"
        ).unwrap();
    }

    #[test]
    fn snapshot_db_produces_a_consistent_copy() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.db");
        let dst = dir.path().join("dst.db");
        make_db(&src);
        snapshot_db(&src, &dst).unwrap();

        let conn = Connection::open(&dst).unwrap();
        let v: String = conn.query_row(
            "SELECT v FROM kv WHERE k = 'hello'", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(v, "world");
    }

    #[test]
    fn write_backup_includes_db_wiki_and_manifest() {
        let dir = tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(data.join("wikis/pages")).unwrap();
        std::fs::write(data.join("wikis/pages/test.md"), "# hello").unwrap();
        make_db(&data.join("history.db"));

        let cfg = dir.path().join("config.json");
        std::fs::write(&cfg, r#"{"server":{}}"#).unwrap();

        let mut buf = Vec::new();
        write_backup(&data, &cfg, &mut buf).unwrap();
        assert!(!buf.is_empty());

        // Round-trip: gunzip + tar inspect.
        let gz = flate2::read::GzDecoder::new(&buf[..]);
        let mut tar = tar::Archive::new(gz);
        let names: Vec<String> = tar.entries().unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().display().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "manifest.json"));
        assert!(names.iter().any(|n| n == "data/history.db"));
        assert!(names.iter().any(|n| n.starts_with("data/wikis/")));
        assert!(names.iter().any(|n| n == "config/config.json"));
        // WAL/SHM and excluded dirs must NOT be in the archive.
        assert!(!names.iter().any(|n| n.contains(".db-wal")));
        assert!(!names.iter().any(|n| n.contains(".db-shm")));
    }

    #[test]
    fn apply_pending_restore_swaps_when_marker_present() {
        let dir = tempdir().unwrap();
        let data = dir.path().join("data");
        let cfg = dir.path().join("config.json");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(&cfg, "old config").unwrap();
        // Pre-existing content to be moved aside.
        std::fs::write(data.join("old.txt"), "OLD").unwrap();

        // Stage a "new" payload (manifest + a known new file).
        let staging = data.join(RESTORE_STAGING_DIR);
        std::fs::create_dir_all(staging.join("data")).unwrap();
        std::fs::create_dir_all(staging.join("config")).unwrap();
        std::fs::write(staging.join("manifest.json"), "{}").unwrap();
        std::fs::write(staging.join("data/new.txt"), "NEW").unwrap();
        std::fs::write(staging.join("config/whatever.json"), "new config").unwrap();
        std::fs::write(data.join(RESTORE_PENDING_MARKER), "now").unwrap();

        let applied = apply_pending_restore(&data, &cfg).unwrap();
        assert!(applied);

        // New file is in place; old file moved aside.
        assert_eq!(std::fs::read_to_string(data.join("new.txt")).unwrap(), "NEW");
        assert!(!data.join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(data.join(PRE_RESTORE_BACKUP_DIR).join("old.txt")).unwrap(),
            "OLD"
        );
        // Config promoted regardless of source filename.
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "new config");
        // Marker + staging cleaned up.
        assert!(!data.join(RESTORE_PENDING_MARKER).exists());
        assert!(!staging.exists());
    }

    #[test]
    fn apply_pending_restore_noop_when_marker_absent() {
        let dir = tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let cfg = dir.path().join("config.json");

        let applied = apply_pending_restore(&data, &cfg).unwrap();
        assert!(!applied);
    }
}
