// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/backup.rs
//
//! Q1.5 — admin-only backup/restore endpoints.
//!
//! `GET  /api/admin/backup` — streams a tar.gz of the data dir + config
//! `POST /api/admin/restore` — accepts a tar.gz, stages it, signals restart
//!
//! The actual snapshot + swap logic lives in [`crate::install::backup`].
//! These handlers just plumb the bytes between HTTP and disk.

use std::path::PathBuf;
use std::sync::Arc;

use crate::server::handlers::onboarding::DataDir;

use axum::body::Body;
use axum::extract::Multipart;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::{Extension, Json};
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::auth::AdminUser;
use crate::config::MiraConfig;
use crate::install::{backup, backup_crypto, backup_scheduler};
use crate::web::LiveConfig;

/// GET /api/admin/backup
///
/// Generates the backup synchronously into a temp file (the SQLite
/// snapshot API plus the data-dir walk both want sync I/O), then
/// streams the file back as the response body and deletes it when
/// the stream drops. Filename in the Content-Disposition is the
/// canonical `mira-backup-<version>-<utc-timestamp>.tar.gz`.
pub async fn download_backup(
    AdminUser(_caller):  AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(data_dir): Extension<DataDir>,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let _cfg: Arc<MiraConfig> = live_cfg.get().await;
    // Source-of-truth for the operator-chosen config path. MiraConfig
    // doesn't carry it (only what's *inside* the file), so we resolve
    // the same default the binary uses at boot — the operator can set
    // MIRA_CONFIG to override there too.
    let config_path = std::env::var_os("MIRA_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::default_config_path);

    // Build the archive on a blocking pool — synchronous file +
    // sqlite I/O would otherwise stall the tokio runtime.
    let data_dir_c: PathBuf = (*data_dir.0).clone();
    let result = tokio::task::spawn_blocking(move || -> Result<tempfile::NamedTempFile, String> {
        let f = tempfile::NamedTempFile::new()
            .map_err(|e| format!("tempfile: {e}"))?;
        let path = f.path().to_path_buf();
        {
            let mut writer = std::fs::OpenOptions::new()
                .write(true).truncate(true).open(&path)
                .map_err(|e| format!("open tempfile: {e}"))?;
            backup::write_backup(&data_dir_c, &config_path, &mut writer)
                .map_err(|e| format!("backup: {e}"))?;
        }
        Ok(f)
    }).await;

    let archive = match result {
        Ok(Ok(f))  => f,
        Ok(Err(e)) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
        Err(e)     => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}"))),
    };

    // Compose the filename. Version + UTC date give the operator a
    // self-describing artifact name.
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let filename = format!(
        "mira-backup-{}-{}.tar.gz",
        env!("CARGO_PKG_VERSION"),
        now,
    );

    let len = std::fs::metadata(archive.path())
        .map(|m| m.len())
        .unwrap_or(0);

    info!("backup: sending {} ({} bytes)", filename, len);

    // Load the tempfile into memory and ship as a single Body. A
    // typical MIRA backup is well under 50 MB (the bulky models /
    // sandbox aren't included), so the in-memory hit is bounded;
    // simpler than wiring tokio-util's ReaderStream and avoids a
    // new direct dep just for this endpoint.
    let bytes = match std::fs::read(archive.path()) {
        Ok(b)  => b,
        Err(e) => return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read tempfile: {e}"),
        )),
    };
    // `archive` (NamedTempFile) drops here, deleting the file on disk.
    drop(archive);

    let response = Response::builder()
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!(r#"attachment; filename="{filename}""#),
        )
        .body(Body::from(bytes))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("response: {e}")))?;
    Ok(response)
}

/// POST /api/admin/restore (multipart, field name `archive`)
///
/// Accepts the tar.gz returned by `download_backup`, writes it to a
/// temp file, asks `install::backup::stage_restore` to extract it
/// under data_dir, then signals a graceful restart. The startup hook
/// detects the marker on next boot and swaps the staged files in.
pub async fn upload_restore(
    AdminUser(caller):    AdminUser,
    Extension(data_dir):  Extension<DataDir>,
    Extension(shutdown):  Extension<Arc<Notify>>,
    mut multipart:        Multipart,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let data_dir: PathBuf = (*data_dir.0).clone();
    // Drain the multipart looking for the `archive` field; tolerate
    // extra fields (browsers sometimes send a Content-Type marker
    // alongside).
    let mut archive_bytes: Option<Vec<u8>> = None;
    let mut passphrase:    Option<String>  = None;
    while let Some(field) = multipart.next_field().await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("multipart: {e}")))?
    {
        match field.name() {
            Some("archive") => {
                let bytes = field.bytes().await
                    .map_err(|e| err(StatusCode::BAD_REQUEST, format!("read field: {e}")))?;
                archive_bytes = Some(bytes.to_vec());
            }
            Some("passphrase") => {
                let text = field.text().await
                    .map_err(|e| err(StatusCode::BAD_REQUEST, format!("read passphrase: {e}")))?;
                if !text.is_empty() { passphrase = Some(text); }
            }
            _ => {}
        }
    }
    let mut archive_bytes = archive_bytes.ok_or_else(|| err(
        StatusCode::BAD_REQUEST,
        "missing `archive` form field",
    ))?;

    // 100 MiB cap. A typical MIRA backup is single-digit MB; this is
    // a sanity bound, not a real product constraint.
    const MAX_BACKUP_BYTES: usize = 100 * 1024 * 1024;
    if archive_bytes.len() > MAX_BACKUP_BYTES {
        return Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("archive is {} bytes; max {} bytes", archive_bytes.len(), MAX_BACKUP_BYTES),
        ));
    }

    // Auto-detect encrypted backups (MIRABK01 magic at offset 0). If the
    // upload is encrypted, the passphrase form field is required.
    if backup_crypto::is_encrypted(&archive_bytes) {
        let pw = passphrase.as_deref().ok_or_else(|| err(
            StatusCode::BAD_REQUEST,
            "this backup is encrypted — include a `passphrase` form field",
        ))?;
        archive_bytes = backup_crypto::decrypt(&archive_bytes, pw)
            .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        info!(user = %caller.username, "restore: decrypted encrypted backup");
    }

    // Write the bytes to a NamedTempFile so install::backup can
    // stream-decompress without holding everything in memory twice.
    let tmp = tempfile::NamedTempFile::new()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("tempfile: {e}")))?;
    std::fs::write(tmp.path(), &archive_bytes)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("write tempfile: {e}")))?;

    // Stage on the blocking pool — extraction is sync.
    let data_dir_c = data_dir.clone();
    let tmp_path = tmp.path().to_path_buf();
    let stage = tokio::task::spawn_blocking(move || -> Result<(), String> {
        backup::stage_restore(&data_dir_c, &tmp_path)
            .map_err(|e| format!("stage: {e}"))
    }).await;
    match stage {
        Ok(Ok(()))  => {},
        Ok(Err(e))  => return Err(err(StatusCode::BAD_REQUEST, e)),
        Err(e)      => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}"))),
    }

    info!(user = %caller.username, "restore: staged — scheduling restart");

    // Same delayed-notify pattern as restart_handler so the 202
    // response flushes before axum tears down.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        shutdown.notify_waiters();
    });

    Ok(Json(serde_json::json!({
        "status":  "restore_staged",
        "message": "Backup uploaded and staged. The server is restarting; \
                    the swap happens on next startup. If something goes \
                    wrong, your previous data is preserved at \
                    `<data_dir>/.pre_restore_backup/` and you can rename \
                    it back manually."
    })))
}

fn err(s: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    let m = msg.into();
    warn!("backup endpoint: {m}");
    (s, Json(serde_json::json!({ "error": m })))
}

// ── POST /api/admin/backup — encrypted download ─────────────────────────────-

#[derive(serde::Deserialize)]
pub struct EncryptedBackupRequest {
    /// Passphrase used to derive the AES-256-GCM key via argon2id.
    /// Required and non-empty; the endpoint refuses to produce an
    /// "encrypted" archive that's actually plain.
    pub passphrase: String,
}

/// POST /api/admin/backup
///
/// Same content as the GET variant but the tarball is wrapped in
/// AES-256-GCM under a passphrase-derived key (argon2id) before being
/// shipped. Filename suffix is `.tar.gz.enc` so it's obvious on disk.
/// Used for offsite / cloud storage where the plain tarball (containing
/// `master.key`, VAPID private key, and provider API keys in
/// `mira_config.json`) would otherwise be a credentials leak.
pub async fn download_backup_encrypted(
    AdminUser(caller):   AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(data_dir): Extension<DataDir>,
    Json(req):           Json<EncryptedBackupRequest>,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    if req.passphrase.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "passphrase must not be empty"));
    }
    let data_dir: PathBuf = (*data_dir.0).clone();
    let cfg: Arc<MiraConfig> = live_cfg.get().await;
    let cfg_path = cfg.config_path.clone();

    // Build the plain tarball into a tempfile, encrypt it in-memory,
    // ship the encrypted bytes. Encryption is single-shot AES-GCM — no
    // streaming nicety needed at backup sizes (<100 MB).
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let f = tempfile::NamedTempFile::new()
            .map_err(|e| format!("tempfile: {e}"))?;
        {
            let writer = std::fs::File::create(f.path())
                .map_err(|e| format!("open tempfile: {e}"))?;
            backup::write_backup(&data_dir, &cfg_path, &mut std::io::BufWriter::new(writer))
                .map_err(|e| format!("backup: {e}"))?;
        }
        let plain = std::fs::read(f.path())
            .map_err(|e| format!("read tempfile: {e}"))?;
        let enc = backup_crypto::encrypt(&plain, &req.passphrase)?;
        Ok(enc)
    }).await;
    let bytes = match result {
        Ok(Ok(b))   => b,
        Ok(Err(e))  => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
        Err(e)      => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}"))),
    };

    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let filename = format!("mira-backup-{}-{}.tar.gz.enc", env!("CARGO_PKG_VERSION"), now);
    info!(user = %caller.username, "backup: sending encrypted {} ({} bytes)", filename, bytes.len());

    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(header::CONTENT_DISPOSITION, format!(r#"attachment; filename="{filename}""#))
        .body(Body::from(bytes))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("response: {e}")))
}

// ── GET /api/admin/backups — listing of scheduled snapshots ──────────────────

#[derive(serde::Serialize)]
pub struct BackupEntry {
    /// Filename inside `<data_dir>/backups/` (just the leaf name).
    pub name: String,
    pub bytes: u64,
    /// Unix-ms of last modification.
    pub modified_ms: i64,
    /// Convenience flag — UI can show 🔒 next to encrypted entries.
    pub encrypted: bool,
}

pub async fn list_scheduled_backups(
    AdminUser(_):        AdminUser,
    Extension(data_dir): Extension<DataDir>,
) -> Result<Json<Vec<BackupEntry>>, (StatusCode, Json<serde_json::Value>)> {
    let data_dir: PathBuf = (*data_dir.0).clone();
    let entries = backup_scheduler::list_scheduled(&data_dir)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(entries.into_iter().map(|(name, bytes, modified_ms)| {
        let encrypted = name.ends_with(".tar.gz.enc");
        BackupEntry { name, bytes, modified_ms, encrypted }
    }).collect()))
}

// ── POST /api/admin/backups/run-now — trigger one scheduled-style snapshot ──

pub async fn run_scheduled_backup_now(
    AdminUser(caller):   AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(data_dir): Extension<DataDir>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let data_dir: PathBuf = (*data_dir.0).clone();
    let cfg = live_cfg.get().await;
    let cfg_path = cfg.config_path.clone();
    let retention = cfg.backup.scheduled_retention_count as usize;

    let res = tokio::task::spawn_blocking(move || {
        backup_scheduler::run_once(&data_dir, &cfg_path, retention)
    }).await;
    match res {
        Ok(Ok((path, bytes))) => {
            info!(user = %caller.username,
                "scheduled backup (manual): wrote {} ({} bytes)", path.display(), bytes);
            Ok(Json(serde_json::json!({
                "status":    "written",
                "filename":  path.file_name().and_then(|s| s.to_str()),
                "bytes":     bytes,
                "retention": retention,
            })))
        }
        Ok(Err(e)) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
        Err(e)     => Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}"))),
    }
}

// ── POST /api/admin/backups/{name}/restore — restore from local scheduled ───

#[derive(serde::Deserialize, Default)]
pub struct RestoreFromScheduledRequest {
    /// Required when restoring a `.tar.gz.enc` file. Ignored for plain.
    #[serde(default)]
    pub passphrase: Option<String>,
}

pub async fn restore_from_scheduled(
    AdminUser(caller):    AdminUser,
    Extension(data_dir):  Extension<DataDir>,
    Extension(shutdown):  Extension<Arc<Notify>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(req):            Json<RestoreFromScheduledRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Filename sanity — refuse anything that tries to escape the dir.
    if name.contains('/') || name.contains("..") || !name.starts_with("mira-backup-") {
        return Err(err(StatusCode::BAD_REQUEST,
            "invalid backup name (must be a leaf mira-backup-… filename)"));
    }
    let data_dir: PathBuf = (*data_dir.0).clone();
    let path = data_dir.join(backup::SCHEDULED_BACKUPS_DIR).join(&name);
    if !path.exists() {
        return Err(err(StatusCode::NOT_FOUND, format!("no such scheduled backup: {name}")));
    }

    // Read, decrypt-if-needed, stage. Done off the runtime — sync I/O.
    let name_for_log = name.clone();
    let data_dir_c = data_dir.clone();
    let stage = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let bytes = std::fs::read(&path).map_err(|e| format!("read: {e}"))?;
        let final_bytes = if backup_crypto::is_encrypted(&bytes) {
            let pw = req.passphrase.as_deref().ok_or(
                "this scheduled backup is encrypted — include a `passphrase` JSON field")?;
            backup_crypto::decrypt(&bytes, pw)?
        } else {
            bytes
        };
        let tmp = tempfile::NamedTempFile::new()
            .map_err(|e| format!("tempfile: {e}"))?;
        std::fs::write(tmp.path(), &final_bytes)
            .map_err(|e| format!("write tempfile: {e}"))?;
        backup::stage_restore(&data_dir_c, tmp.path()).map_err(|e| format!("stage: {e}"))?;
        Ok(())
    }).await;
    match stage {
        Ok(Ok(()))  => {},
        Ok(Err(e))  => return Err(err(StatusCode::BAD_REQUEST, e)),
        Err(e)      => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}"))),
    }

    info!(user = %caller.username,
        "restore (from scheduled '{name_for_log}'): staged — scheduling restart");
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        shutdown.notify_waiters();
    });

    Ok(Json(serde_json::json!({
        "status":  "restore_staged",
        "from":    name_for_log,
        "message": "Backup staged. Server is restarting; the swap happens on next \
                    startup. If something goes wrong, your previous data is preserved \
                    at `<data_dir>/.pre_restore_backup/` and you can rename it back."
    })))
}
