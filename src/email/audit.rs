// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/audit.rs
//! Per-account email audit log (slice E1+E3, chunk 5).
//!
//! Every inbound message — Accepted, Quarantined, or Dropped — gets
//! a row written here. Operators can scroll the log on the /email
//! page to confirm what's been happening, or feed an export into a
//! SIEM later. Body content is **not** stored — only an SHA-256 of
//! the raw bytes so a repeated flooded message can be recognised
//! across rows without re-reading the body.
//!
//! Outbound (SMTP, slice E2) will share this table with
//! `direction = "outbound"`. Today every row is inbound.
//!
//! Retention is unbounded in v1. We expect to add a per-account
//! retention setting + a daily-trim job once the table grows on
//! a real deployment; for now the rows are small (no body), so
//! 100k+ rows isn't going to be a problem.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::MiraError;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub id:             String,
    pub account_id:     String,
    pub direction:      String,   // "inbound" today; "outbound" in E2
    pub sender:         String,
    pub recipient:      String,
    pub subject:        String,
    pub action:         String,   // "accepted" | "quarantined" | "dropped" | "approved"
    pub reason:         Option<String>,
    pub body_sha256:    String,
    pub attached_count: i64,
    pub at:             i64,
}

#[derive(Debug, Clone)]
pub struct NewAuditEntry {
    pub account_id:     String,
    pub direction:      String,
    pub sender:         String,
    pub recipient:      String,
    pub subject:        String,
    pub action:         String,
    pub reason:         Option<String>,
    pub body:           Vec<u8>,  // hashed at insert, never stored verbatim
    pub attached_count: usize,
}

pub struct EmailAuditStore {
    conn: Arc<Mutex<Connection>>,
}

impl EmailAuditStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create audit DB dir: {e}"))
            })?;
        }
        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open audit DB: {e}"))
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS email_audit (
                id              TEXT PRIMARY KEY,
                account_id      TEXT NOT NULL,
                direction       TEXT NOT NULL,
                sender          TEXT NOT NULL,
                recipient       TEXT NOT NULL,
                subject         TEXT NOT NULL,
                action          TEXT NOT NULL,
                reason          TEXT,
                body_sha256     TEXT NOT NULL,
                attached_count  INTEGER NOT NULL,
                at              INTEGER NOT NULL,
                FOREIGN KEY (account_id) REFERENCES email_accounts(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_email_audit_account_time
                ON email_audit(account_id, at DESC);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("audit migration: {e}")))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn record(&self, new: NewAuditEntry) -> Result<String, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64).unwrap_or(0);
        let mut hasher = Sha256::new();
        hasher.update(&new.body);
        let sha = format!("{:x}", hasher.finalize());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO email_audit
               (id, account_id, direction, sender, recipient, subject,
                action, reason, body_sha256, attached_count, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id, new.account_id, new.direction, new.sender, new.recipient,
                new.subject, new.action, new.reason, sha,
                new.attached_count as i64, now,
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("audit record: {e}")))?;
        Ok(id)
    }

    /// Most-recent first across the caller's accounts.
    pub fn list_for_accounts(&self, account_ids: &[String], limit: usize)
        -> Result<Vec<AuditEntry>, MiraError>
    {
        if account_ids.is_empty() { return Ok(Vec::new()); }
        let placeholders = account_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let limit = limit.clamp(1, 1000);
        let sql = format!(
            "SELECT id, account_id, direction, sender, recipient, subject,
                    action, reason, body_sha256, attached_count, at
             FROM email_audit
             WHERE account_id IN ({placeholders})
             ORDER BY at DESC
             LIMIT {limit}",
        );
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> = account_ids.iter()
            .map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(params_dyn), |row| {
            Ok(AuditEntry {
                id:             row.get(0)?,
                account_id:     row.get(1)?,
                direction:      row.get(2)?,
                sender:         row.get(3)?,
                recipient:      row.get(4)?,
                subject:        row.get(5)?,
                action:         row.get(6)?,
                reason:         row.get(7)?,
                body_sha256:    row.get(8)?,
                attached_count: row.get(9)?,
                at:             row.get(10)?,
            })
        })
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?); }
        Ok(out)
    }
}
