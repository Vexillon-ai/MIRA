// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/audit.rs

//! SQLite-backed audit log for tool invocations.
//!
//! One row per `ToolRegistry::execute` call, recording the caller, tool name,
//! a digest of the arguments, timing, outcome, and a truncated snapshot of
//! the output. Lives in its own `tools.db` file so retention and pruning can
//! be managed independently of memory/history/auth.
//!
//! This is the same table Tier 4 (sandboxed code tools) will write into —
//! the schema is deliberately neutral about tier.

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::debug;

use crate::MiraError;

// ─────────────────────────────────────────────────────────────────────────────

/// Outcome categorisation for a tool call. Kept as a small enum so the UI /
/// admin list can filter by category without parsing free-form error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Tool returned `ToolResult { success: true, .. }`.
    Success,
    /// Tool returned `ToolResult { success: false, .. }`.
    Failure,
    /// `ToolRegistry::execute` itself returned `Err(..)` — unknown tool,
    /// dispatch error, etc.
    Error,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
            Outcome::Error   => "error",
        }
    }
}

/// Truncation limit for the recorded output snippet. Enough to read at a
/// glance in an admin list without bloating the audit table.
pub const MAX_OUTPUT_SNIPPET: usize = 512;

/// Hex-encoded SHA-256 of the canonical JSON representation of `args`.
///
/// Identity-injection keys (`_user_id`, `_conversation_id`) are stripped
/// before hashing so the digest reflects the *model's* chosen call shape,
/// not who dispatched it — otherwise every row per-user would have a
/// distinct digest and the signal of "which calls does the model make"
/// would be lost. The caller identity is already stored in its own column.
pub fn args_digest(args: &serde_json::Value) -> String {
    let mut stripped = args.clone();
    if let Some(obj) = stripped.as_object_mut() {
        obj.remove("_user_id");
        obj.remove("_conversation_id");
    }
    let canonical = serde_json::to_string(&stripped).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

/// Clip `text` to at most `MAX_OUTPUT_SNIPPET` bytes on a UTF-8 char boundary.
pub fn truncate_output(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_SNIPPET {
        return text.to_string();
    }
    let mut end = MAX_OUTPUT_SNIPPET;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

// ─────────────────────────────────────────────────────────────────────────────

pub struct ToolAuditStore {
    conn: Arc<Mutex<Connection>>,
}

impl ToolAuditStore {
    /// Open the store at `path` (typically `<data_dir>/tools.db`). Creates
    /// the `tool_audit` table on first run.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create tool-audit DB dir: {}", e))
            })?;
        }

        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open tool-audit DB: {}", e))
        })?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tool_audit (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                actor            TEXT NOT NULL,
                tool             TEXT NOT NULL,
                args_digest      TEXT NOT NULL,
                started_at       INTEGER NOT NULL,
                duration_ms      INTEGER NOT NULL,
                outcome          TEXT NOT NULL,
                truncated_output TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_tool_audit_started ON tool_audit(started_at);
            CREATE INDEX IF NOT EXISTS idx_tool_audit_actor   ON tool_audit(actor);
            CREATE INDEX IF NOT EXISTS idx_tool_audit_tool    ON tool_audit(tool);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("tool_audit migration failed: {}", e)))?;

        debug!("tool_audit schema ready at {}", path.display());
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Write one audit row. Errors are returned so callers can log — the
    /// registry treats a failed write as non-fatal.
    pub fn record(
        &self,
        actor:            &str,
        tool:             &str,
        args_digest:      &str,
        started_at_ms:    i64,
        duration_ms:      i64,
        outcome:          Outcome,
        truncated_output: Option<&str>,
    ) -> Result<(), MiraError> {
        let conn = self.conn.lock()
            .map_err(|e| MiraError::DatabaseError(format!("tool_audit lock: {}", e)))?;
        conn.execute(
            "INSERT INTO tool_audit
                (actor, tool, args_digest, started_at, duration_ms, outcome, truncated_output)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                actor,
                tool,
                args_digest,
                started_at_ms,
                duration_ms,
                outcome.as_str(),
                truncated_output,
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("tool_audit insert: {}", e)))?;
        Ok(())
    }

    /// Row count — used by tests and the admin list endpoint.
    pub fn count(&self) -> Result<i64, MiraError> {
        let conn = self.conn.lock()
            .map_err(|e| MiraError::DatabaseError(format!("tool_audit lock: {}", e)))?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM tool_audit", [], |r| r.get(0))
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(n)
    }

    /// List rows, most recent first. Filters are AND'd; `None` means "any".
    pub fn list(
        &self,
        limit:       i64,
        offset:      i64,
        actor:       Option<&str>,
        tool:        Option<&str>,
        outcome:     Option<&str>,
    ) -> Result<Vec<AuditRow>, MiraError> {
        let conn = self.conn.lock()
            .map_err(|e| MiraError::DatabaseError(format!("tool_audit lock: {}", e)))?;

        let mut sql = String::from(
            "SELECT id, actor, tool, args_digest, started_at, duration_ms, outcome, truncated_output
             FROM tool_audit WHERE 1=1"
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(a) = actor   { sql.push_str(" AND actor   = ?"); params_vec.push(Box::new(a.to_string())); }
        if let Some(t) = tool    { sql.push_str(" AND tool    = ?"); params_vec.push(Box::new(t.to_string())); }
        if let Some(o) = outcome { sql.push_str(" AND outcome = ?"); params_vec.push(Box::new(o.to_string())); }
        sql.push_str(" ORDER BY started_at DESC LIMIT ? OFFSET ?");
        params_vec.push(Box::new(limit));
        params_vec.push(Box::new(offset));

        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok(AuditRow {
                id:               r.get(0)?,
                actor:            r.get(1)?,
                tool:             r.get(2)?,
                args_digest:      r.get(3)?,
                started_at:       r.get(4)?,
                duration_ms:      r.get(5)?,
                outcome:          r.get(6)?,
                truncated_output: r.get(7)?,
            })
        })
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(out)
    }
}

/// One row returned by [`ToolAuditStore::list`]. Matches the SQLite schema
/// 1:1 so the admin endpoint can serialise it directly.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditRow {
    pub id:               i64,
    pub actor:            String,
    pub tool:             String,
    pub args_digest:      String,
    pub started_at:       i64,
    pub duration_ms:      i64,
    pub outcome:          String,
    pub truncated_output: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn digest_ignores_injected_identity_keys() {
        let a = json!({"url": "https://x", "_user_id": "u1", "_conversation_id": "c1"});
        let b = json!({"url": "https://x", "_user_id": "u2", "_conversation_id": "c9"});
        let c = json!({"url": "https://y"});
        assert_eq!(args_digest(&a), args_digest(&b), "identity keys must not affect digest");
        assert_ne!(args_digest(&a), args_digest(&c));
    }

    #[test]
    fn truncate_preserves_utf8_boundary() {
        let long = "é".repeat(MAX_OUTPUT_SNIPPET);
        let out = truncate_output(&long);
        assert!(out.len() <= MAX_OUTPUT_SNIPPET + "…".len());
        assert!(out.ends_with('…') || out == long);
    }

    #[test]
    fn open_and_record_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ToolAuditStore::open(&tmp.path().join("tools.db")).unwrap();
        assert_eq!(store.count().unwrap(), 0);

        store.record("u1", "web_fetch", "abc", 123, 42, Outcome::Success, Some("ok")).unwrap();
        store.record("u1", "shell",     "def", 124, 10, Outcome::Failure, Some("err")).unwrap();
        store.record("u2", "web_fetch", "abc", 125,  5, Outcome::Error,   None).unwrap();

        assert_eq!(store.count().unwrap(), 3);
    }

    #[test]
    fn outcome_strings_are_stable() {
        // These strings are stored in SQLite; changing them is a breaking
        // change for any later admin queries.
        assert_eq!(Outcome::Success.as_str(), "success");
        assert_eq!(Outcome::Failure.as_str(), "failure");
        assert_eq!(Outcome::Error.as_str(),   "error");
    }
}
