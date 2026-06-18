// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/quarantine.rs
//! Held inbound emails awaiting operator review (slice E1+E3, chunk 5).
//!
//! When the security pipeline returns `Verdict::Quarantine`, the
//! poller writes the full RFC822 body here instead of running the
//! agent. The operator reviews the queue in the UI and either:
//!
//!   * **Approves** — the body is re-parsed + re-dispatched as if
//!     the verdict had been `Accept`, and the sender is (by
//!     default) added to the per-account allowlist so future mail
//!     from them bypasses quarantine. The row is then deleted.
//!   * **Rejects** — the row is deleted; the operator can optionally
//!     add the sender to the per-account denylist in the same call.
//!
//! Raw bodies are stored as `BLOB` in the same `auth.db` rather than
//! spilled to disk. The 1 MB-per-message size cap (enforced before
//! we'd even consider quarantine) bounds the table's worst case
//! comfortably. If that ever becomes a real problem we'll move to
//! file-backed storage; for now inline keeps the lifecycle simple
//! (delete the row = the data is gone).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::Serialize;
use uuid::Uuid;

use crate::MiraError;

#[derive(Debug, Clone, Serialize)]
pub struct QuarantineEntry {
    pub id:          String,
    pub account_id:  String,
    pub sender:      String,
    pub subject:     String,
    /// First 500 chars of the parsed text body. Exposed in the UI
    /// list so the operator can decide without loading the full
    /// body — and so a quarantine flood doesn't make us ship MB of
    /// JSON to the browser.
    pub preview:     String,
    pub message_id:  String,
    pub reason:      String,
    pub received_at: i64,
    pub uid:         i64,
    /// Excluded from the list JSON via `Serialize` skip — only the
    /// /quarantine/{id} detail endpoint surfaces this when present.
    #[serde(skip)]
    pub raw_body:    Vec<u8>,
}

/// Row written by the poller — the `id` + timestamps are filled by
/// the store at insert time.
#[derive(Debug, Clone)]
pub struct NewQuarantineEntry {
    pub account_id:  String,
    pub sender:      String,
    pub subject:     String,
    pub preview:     String,
    pub message_id:  String,
    pub reason:      String,
    pub uid:         i64,
    pub raw_body:    Vec<u8>,
}

pub struct EmailQuarantineStore {
    conn: Arc<Mutex<Connection>>,
}

impl EmailQuarantineStore {
    /// Open at `<data_dir>/auth.db`, creating the table on first
    /// run. The `email_accounts` FK cascade means deleting an
    /// account scrubs its quarantine too.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create quarantine DB dir: {e}"))
            })?;
        }
        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open quarantine DB: {e}"))
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS email_quarantine (
                id          TEXT PRIMARY KEY,
                account_id  TEXT NOT NULL,
                sender      TEXT NOT NULL,
                subject     TEXT NOT NULL,
                preview     TEXT NOT NULL,
                message_id  TEXT NOT NULL DEFAULT '',
                reason      TEXT NOT NULL,
                raw_body    BLOB NOT NULL,
                received_at INTEGER NOT NULL,
                uid         INTEGER NOT NULL,
                FOREIGN KEY (account_id) REFERENCES email_accounts(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_email_quarantine_account
                ON email_quarantine(account_id, received_at DESC);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("quarantine migration: {e}")))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }

    /// Insert one held message. Called by the poller on
    /// `Verdict::Quarantine`. Returns the generated row id.
    pub fn put(&self, new: NewQuarantineEntry) -> Result<String, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO email_quarantine
               (id, account_id, sender, subject, preview, message_id,
                reason, raw_body, received_at, uid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id, new.account_id, new.sender, new.subject, new.preview,
                new.message_id, new.reason, new.raw_body, now, new.uid,
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("quarantine put: {e}")))?;
        Ok(id)
    }

    /// One row by id. Used by approve/reject before the actual
    /// state-change op so the handler can return 404 cleanly when
    /// the row doesn't exist.
    pub fn get(&self, id: &str) -> Result<Option<QuarantineEntry>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            "SELECT id, account_id, sender, subject, preview, message_id,
                    reason, raw_body, received_at, uid
             FROM email_quarantine WHERE id = ?1",
            params![id],
            row_to_entry,
        );
        match r {
            Ok(e) => Ok(Some(e)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// All held messages for the caller's accounts, newest first.
    /// `account_ids` is the set of accounts owned by the caller —
    /// the handler resolves this from `EmailAccountStore::list_for_user`
    /// before calling here, so we never leak entries across users.
    pub fn list_for_accounts(&self, account_ids: &[String])
        -> Result<Vec<QuarantineEntry>, MiraError>
    {
        if account_ids.is_empty() { return Ok(Vec::new()); }
        let placeholders = account_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, account_id, sender, subject, preview, message_id,
                    reason, raw_body, received_at, uid
             FROM email_quarantine
             WHERE account_id IN ({placeholders})
             ORDER BY received_at DESC
             LIMIT 200",
        );
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> = account_ids.iter()
            .map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(params_dyn), row_to_entry)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(out)
    }

    /// Delete a row by id. Used by approve (after dispatch) and by
    /// reject. Errors when no row matched so the handler can
    /// distinguish "already gone" from "couldn't reach DB".
    pub fn delete(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM email_quarantine WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if n == 0 {
            return Err(MiraError::NotFound(format!("quarantine entry not found: {id}")));
        }
        Ok(())
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<QuarantineEntry> {
    Ok(QuarantineEntry {
        id:          row.get(0)?,
        account_id:  row.get(1)?,
        sender:      row.get(2)?,
        subject:     row.get(3)?,
        preview:     row.get(4)?,
        message_id:  row.get(5)?,
        reason:      row.get(6)?,
        raw_body:    row.get(7)?,
        received_at: row.get(8)?,
        uid:         row.get(9)?,
    })
}
