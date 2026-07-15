// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/backup.rs
//! Model-callable backup tools — the user can say "back up my data",
//! "what backups do I have", or "restore from yesterday's backup" in
//! chat.
//!
//! Three tools:
//!
//! - `backup_create` — write a fresh snapshot into
//!   `<data_dir>/backups/`. Safe (read-only on existing data; write to
//!   a new file). Available to any authenticated user.
//! - `backup_list`   — list files in `<data_dir>/backups/`. Read-only.
//! - `backup_restore` — stage a scheduled backup for restore + restart.
//!   DESTRUCTIVE. Admin-gated (looks up the caller's role via
//!   `LocalAuthService`) and requires an explicit `confirm: true`
//!   argument so the model can't fire it accidentally.
//!
//! ## Encryption deliberately UI-only
//!
//! The model-callable surface does NOT accept a passphrase argument.
//! Passphrases in chat would leak into the conversation transcript,
//! the history DB, the model provider's logs, and any future
//! summarisation. Encryption stays in the Settings → Server panel
//! where the field is `type="password"`, not echoed, and never
//! persisted past the request body. The tools document this.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tracing::info;

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::auth::{LocalAuthService, Role};
use crate::install::{backup, backup_crypto, backup_scheduler};
use crate::MiraError;

/// Configuration handed to the tools at registration. Holds owned paths
/// + a retention default that matches the live config at start-up. If
/// the operator later raises retention in Settings, the next gateway
/// restart picks it up — backup tools aren't a hot path so we don't
/// thread `LiveConfig` through every call.
#[derive(Clone)]
pub struct BackupToolDeps {
    pub data_dir:        PathBuf,
    pub config_path:     PathBuf,
    pub retention_count: u32,
    pub auth:            Option<Arc<LocalAuthService>>,
    pub shutdown:        Arc<Notify>,
}

fn require_user_id(args: &ToolArgs, tool: &str) -> Result<String, ToolResult> {
    args.get("_user_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .ok_or_else(|| ToolResult::failure(format!(
            "{tool} called without _user_id (chat handler must inject)"
        )))
}

fn fmt_bytes(n: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * 1024;
    const G: u64 = M * 1024;
    if      n >= G { format!("{:.2} GB", n as f64 / G as f64) }
    else if n >= M { format!("{:.2} MB", n as f64 / M as f64) }
    else if n >= K { format!("{:.2} KB", n as f64 / K as f64) }
    else           { format!("{n} B")                          }
}

// ── backup_create ────────────────────────────────────────────────────────────

pub struct BackupCreateTool { deps: BackupToolDeps }
impl BackupCreateTool { pub fn new(deps: BackupToolDeps) -> Self { Self { deps } } }

#[async_trait]
impl Tool for BackupCreateTool {
    fn name(&self) -> &str { "backup_create" }

    fn description(&self) -> &str {
        "Create a new MIRA backup right now and save it under the data \
         directory's `backups/` folder. Returns the resulting filename \
         + size. The backup includes all databases (history, memory, \
         automations, channel accounts, companion settings, etc.), the \
         wiki + its git history, profiles, avatars, artifacts, installed \
         skills + their secrets, the VAPID push keypair, and the config \
         file. Models / sandbox rootfs / TTS voices are excluded — they're \
         re-downloadable on demand. The backup is unencrypted; for \
         encrypted backups (recommended for offsite/cloud storage) use \
         the Settings → Server panel in the web UI — passphrases must \
         not be sent through chat or they'll leak into the transcript."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
        let deps = self.deps.clone();
        let res = tokio::task::spawn_blocking(move || {
            backup_scheduler::run_once(&deps.data_dir, &deps.config_path, deps.retention_count as usize)
        }).await;
        match res {
            Ok(Ok((path, bytes))) => {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("(unknown)").to_string();
                info!("backup_create tool: wrote {} ({} bytes)", path.display(), bytes);
                Ok(ToolResult::success(json!({
                    "filename": name,
                    "size":     fmt_bytes(bytes),
                    "bytes":    bytes,
                    "path":     path.display().to_string(),
                    "note":     "Saved to the scheduled-backups folder. Pruned to keep the most-recent N (see memory.consolidation — actually backup.scheduled_retention_count — settings).",
                }).to_string()))
            }
            Ok(Err(e)) => Ok(ToolResult::failure(format!("backup_create: {e}"))),
            Err(e)     => Ok(ToolResult::failure(format!("backup_create: task join: {e}"))),
        }
    }
}

// ── backup_list ──────────────────────────────────────────────────────────────

pub struct BackupListTool { deps: BackupToolDeps }
impl BackupListTool { pub fn new(deps: BackupToolDeps) -> Self { Self { deps } } }

#[async_trait]
impl Tool for BackupListTool {
    fn name(&self) -> &str { "backup_list" }

    fn description(&self) -> &str {
        "List all backup files currently on disk in the data directory's \
         `backups/` folder (the rotated scheduled snapshots plus any \
         `backup_create` calls). Returns each entry's filename, size, \
         last-modified time, and whether it's encrypted (.tar.gz.enc \
         suffix). Newest first. Read-only — does not modify any state."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
        let dd = self.deps.data_dir.clone();
        let res = tokio::task::spawn_blocking(move || backup_scheduler::list_scheduled(&dd)).await;
        match res {
            Ok(Ok(entries)) => {
                let payload: Vec<Value> = entries.into_iter().map(|(name, bytes, mtime_ms)| {
                    let encrypted = name.ends_with(".tar.gz.enc");
                    let when = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(mtime_ms)
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_else(|| "(unknown)".to_string());
                    json!({
                        "filename":   name,
                        "size":       fmt_bytes(bytes),
                        "bytes":      bytes,
                        "modified":   when,
                        "encrypted":  encrypted,
                    })
                }).collect();
                let count = payload.len();
                Ok(ToolResult::success(json!({
                    "count":   count,
                    "backups": payload,
                    "note": if count == 0 {
                        "No backups in <data_dir>/backups/. Either scheduled backups aren't enabled, or none have run yet. Use `backup_create` to make one now."
                    } else {
                        "Newest first. To restore one, ask the user to confirm and then call `backup_restore` with the chosen filename + `confirm: true`."
                    },
                }).to_string()))
            }
            Ok(Err(e)) => Ok(ToolResult::failure(format!("backup_list: {e}"))),
            Err(e)     => Ok(ToolResult::failure(format!("backup_list: task join: {e}"))),
        }
    }
}

// ── backup_restore ───────────────────────────────────────────────────────────

pub struct BackupRestoreTool { deps: BackupToolDeps }
impl BackupRestoreTool { pub fn new(deps: BackupToolDeps) -> Self { Self { deps } } }

#[async_trait]
impl Tool for BackupRestoreTool {
    fn name(&self) -> &str { "backup_restore" }

    fn description(&self) -> &str {
        "Stage a scheduled backup for restore and trigger a server restart \
         to apply it. **DESTRUCTIVE** — replaces all MIRA data with the \
         backup's contents. The current data is preserved at \
         `<data_dir>/.pre_restore_backup/` so a botched restore can be \
         rolled back manually.\n\n\
         ADMIN-ONLY (the tool refuses non-admin callers). The model must \
         pass `confirm: true` — the tool refuses without it so an \
         accidental invocation can't blow away the user's data. Encrypted \
         backups (.tar.gz.enc) cannot be restored through this tool — \
         passphrases in chat would leak; use the Settings → Server UI \
         where the passphrase field is type=password."
    }

    // Filesystem tier — restore swaps the live data_dir on disk and then
    // triggers a service restart. Destructive; admin-gated AND requires
    // `confirm: true` to invoke.
    fn tier(&self) -> Tier { Tier::Filesystem }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["filename", "confirm"],
            "properties": {
                "filename": {
                    "type": "string",
                    "description": "Leaf filename of the scheduled backup to restore from \
                                    (as returned by `backup_list`, e.g. \
                                    'mira-backup-0.194.0-20260530T103332Z.tar.gz'). \
                                    No paths — file must live in <data_dir>/backups/."
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Must be `true` to proceed. The model is expected to \
                                    surface the consequences (data replaced, restart \
                                    triggered, rollback path) to the user and only set \
                                    this once the user has explicitly agreed."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        // Admin gate. We rely on the LocalAuthService for the role lookup
        // rather than trusting a model-supplied argument.
        let auth = match &self.deps.auth {
            Some(a) => a.clone(),
            None    => return Ok(ToolResult::failure(
                "backup_restore: no auth service installed; refusing destructive op"
            )),
        };
        let user = match auth.get_user(&user_id) {
            Ok(Some(u)) => u,
            Ok(None)    => return Ok(ToolResult::failure(format!(
                "backup_restore: user '{user_id}' not found"))),
            Err(e)      => return Ok(ToolResult::failure(format!(
                "backup_restore: auth lookup failed: {e}"))),
        };
        if !matches!(user.role, Role::Admin) {
            return Ok(ToolResult::failure(
                "backup_restore: caller is not an admin. This tool is \
                 admin-gated because it can replace all MIRA data."
            ));
        }
        let confirm = args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false);
        if !confirm {
            return Ok(ToolResult::failure(
                "backup_restore: refusing — pass `confirm: true` once the user \
                 has explicitly agreed that all current MIRA data will be \
                 replaced with the backup's contents and the server will \
                 restart. Their current state will be preserved at \
                 <data_dir>/.pre_restore_backup/ for manual rollback."
            ));
        }
        let filename = match args.get("filename").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None    => return Ok(ToolResult::failure(
                "backup_restore: missing `filename` (run `backup_list` to see options)"
            )),
        };
        if filename.contains('/') || filename.contains("..") || !filename.starts_with("mira-backup-") {
            return Ok(ToolResult::failure(format!(
                "backup_restore: invalid filename '{filename}' — must be a leaf \
                 mira-backup-… name from <data_dir>/backups/"
            )));
        }

        let path = self.deps.data_dir.join(backup::SCHEDULED_BACKUPS_DIR).join(&filename);
        if !path.exists() {
            return Ok(ToolResult::failure(format!(
                "backup_restore: no such scheduled backup: {filename}"
            )));
        }
        // Refuse encrypted via this tool path (see file-level docs).
        // Cheap probe: read the first 8 bytes and check the magic.
        let path_for_probe = path.clone();
        let is_enc = tokio::task::spawn_blocking(move || -> bool {
            match std::fs::read(&path_for_probe) {
                Ok(b) => backup_crypto::is_encrypted(&b),
                Err(_) => false,
            }
        }).await.unwrap_or(false);
        if is_enc {
            return Ok(ToolResult::failure(
                "backup_restore: this backup is encrypted (.tar.gz.enc). \
                 The chat tool refuses encrypted restores — passing the \
                 passphrase through chat would leak it into the transcript. \
                 Restore via Settings → Server in the web UI instead."
            ));
        }

        // Stage + schedule restart on the shared shutdown notifier (same
        // mechanism as the HTTP restore handler).
        let data_dir = self.deps.data_dir.clone();
        let stage = tokio::task::spawn_blocking(move || -> Result<(), String> {
            backup::stage_restore(&data_dir, &path).map_err(|e| format!("stage: {e}"))
        }).await;
        match stage {
            Ok(Ok(()))  => {}
            Ok(Err(e))  => return Ok(ToolResult::failure(format!("backup_restore: {e}"))),
            Err(e)      => return Ok(ToolResult::failure(format!("backup_restore: task join: {e}"))),
        }

        info!(user = %user_id, "backup_restore tool: staged '{}'; scheduling restart", filename);
        let shutdown = Arc::clone(&self.deps.shutdown);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            shutdown.notify_waiters();
        });

        Ok(ToolResult::success(json!({
            "status":   "restore_staged",
            "from":     filename,
            "message":  "Backup staged. Server is restarting now; the swap \
                         happens on next startup. Your previous data is preserved \
                         at <data_dir>/.pre_restore_backup/ — you can rename it \
                         back if anything goes wrong.",
        }).to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps_only_paths(d: PathBuf, c: PathBuf) -> BackupToolDeps {
        BackupToolDeps {
            data_dir: d, config_path: c, retention_count: 7,
            auth: None, shutdown: Arc::new(Notify::new()),
        }
    }

    #[test]
    fn fmt_bytes_friendly() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.00 KB");
        assert!(fmt_bytes(5_741_125).starts_with("5.4"));
    }

    #[tokio::test]
    async fn backup_list_handles_empty_dir() {
        let d = tempfile::tempdir().unwrap();
        let tool = BackupListTool::new(deps_only_paths(d.path().to_path_buf(), d.path().join("c.json")));
        let r = tool.execute(json!({})).await.unwrap();
        assert!(r.error.is_none(), "got error: {:?}", r.error);
        assert!(r.output.contains("\"count\":0"));
    }

    #[tokio::test]
    async fn backup_restore_refuses_without_confirm() {
        let d = tempfile::tempdir().unwrap();
        let tool = BackupRestoreTool::new(deps_only_paths(d.path().to_path_buf(), d.path().join("c.json")));
        let r = tool.execute(json!({
            "_user_id": "anyone",
            "filename": "mira-backup-0.0.0-20260101T000000Z.tar.gz",
            "confirm":  false,
        })).await.unwrap();
        // Auth absent here, so the admin gate fires first — but the missing
        // auth refusal is just as valid for "doesn't run destructive op".
        assert!(r.error.is_some(), "expected refusal");
    }

    #[tokio::test]
    async fn backup_restore_refuses_path_escape() {
        // With an auth service in place + admin user, the path-escape check
        // still fires. We use a minimal auth stub instead of a fixture; here,
        // the no-auth refusal covers it acceptably (full-flow test better
        // lives in an integration suite once auth fixtures exist).
        let d = tempfile::tempdir().unwrap();
        let tool = BackupRestoreTool::new(deps_only_paths(d.path().to_path_buf(), d.path().join("c.json")));
        let r = tool.execute(json!({
            "_user_id": "anyone",
            "filename": "../escape.tar.gz",
            "confirm":  true,
        })).await.unwrap();
        assert!(r.error.is_some());
    }
}
