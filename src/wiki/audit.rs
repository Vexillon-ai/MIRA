// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/audit.rs
//! SQLite audit log of every [`WikiOp`].
//!
//! Each wiki gets its own file (`wiki_<user_id>.db` per-user;
//! `wiki_system.db` for the system wiki). The table records the full
//! envelope so we can replay or revert later.

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::wiki::ops::{OpStatus, WikiOpEnvelope};
use crate::wiki::Result;

/// Audit DB handle. Wrap in a `std::sync::Mutex` if shared across
/// threads — `rusqlite::Connection` is `Send` but not `Sync`.
pub struct WikiAuditDb {
    conn: Connection,
}

impl WikiAuditDb {
    /// Open or create. Runs the schema migration idempotently.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init_schema(&conn)?;
        Ok(Self { conn })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS wiki_audit (
                op_id        TEXT PRIMARY KEY,
                user_id      TEXT,
                scope        TEXT NOT NULL,
                op_kind      TEXT NOT NULL,
                target_path  TEXT NOT NULL,
                op_json      TEXT NOT NULL,
                provenance   TEXT NOT NULL,
                status       TEXT NOT NULL,
                failure      TEXT,
                created_at   TEXT NOT NULL,
                applied_at   TEXT,
                reviewed_at  TEXT,
                reviewed_by  TEXT
            );
            CREATE INDEX IF NOT EXISTS wiki_audit_status_idx  ON wiki_audit(status);
            CREATE INDEX IF NOT EXISTS wiki_audit_created_idx ON wiki_audit(created_at DESC);
            CREATE INDEX IF NOT EXISTS wiki_audit_target_idx  ON wiki_audit(target_path);
        "#)?;
        // Additive migration: extractor confidence for tiered auto-apply +
        // "approve all ≥ X". Idempotent — swallow the duplicate-column error
        // when the column already exists. Pre-existing rows carry NULL
        // (treated as "no confidence", so a threshold never auto-approves them).
        if let Err(e) = conn.execute("ALTER TABLE wiki_audit ADD COLUMN confidence REAL", []) {
            if !e.to_string().contains("duplicate column name") {
                return Err(e.into());
            }
        }
        Ok(())
    }

    pub fn insert(&self, env: &WikiOpEnvelope) -> Result<()> {
        let op_json = serde_json::to_string(&env.op)?;
        let prov_json = serde_json::to_string(&env.provenance)?;
        let target = env.op.target_path();
        self.conn.execute(
            "INSERT INTO wiki_audit (
                op_id, user_id, scope, op_kind, target_path, op_json, provenance,
                status, failure, created_at, applied_at, reviewed_at, reviewed_by, confidence
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                env.op_id,
                env.scope.user_id(),
                env.scope.as_str(),
                env.op.kind(),
                target.as_str(),
                op_json,
                prov_json,
                env.status.as_str(),
                env.failure,
                env.created_at.to_rfc3339(),
                env.applied_at.map(|d| d.to_rfc3339()),
                env.reviewed_at.map(|d| d.to_rfc3339()),
                env.reviewed_by,
                env.confidence,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, op_id: &str) -> Result<Option<WikiOpEnvelope>> {
        let mut stmt = self.conn.prepare(
            "SELECT op_id, scope, user_id, op_json, provenance, status, failure,
                    created_at, applied_at, reviewed_at, reviewed_by, confidence
             FROM wiki_audit WHERE op_id = ?1",
        )?;
        let mut rows = stmt.query(params![op_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(envelope_from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn mark_applied(&self, op_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE wiki_audit SET status = 'applied', applied_at = ?1 WHERE op_id = ?2",
            params![Utc::now().to_rfc3339(), op_id],
        )?;
        Ok(())
    }

    pub fn mark_failed(&self, op_id: &str, reason: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE wiki_audit SET status = 'failed', failure = ?1 WHERE op_id = ?2",
            params![reason, op_id],
        )?;
        Ok(())
    }

    pub fn mark_rejected(&self, op_id: &str, reason: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE wiki_audit SET status = 'rejected', failure = ?1 WHERE op_id = ?2",
            params![reason, op_id],
        )?;
        Ok(())
    }

    pub fn mark_reviewed(&self, op_id: &str, reviewer: &str, _approved: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE wiki_audit SET reviewed_at = ?1, reviewed_by = ?2 WHERE op_id = ?3",
            params![Utc::now().to_rfc3339(), reviewer, op_id],
        )?;
        Ok(())
    }

    pub fn list_by_status(&self, status: OpStatus) -> Result<Vec<WikiOpEnvelope>> {
        let mut stmt = self.conn.prepare(
            "SELECT op_id, scope, user_id, op_json, provenance, status, failure,
                    created_at, applied_at, reviewed_at, reviewed_by, confidence
             FROM wiki_audit WHERE status = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![status.as_str()], envelope_from_row)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    pub fn list_recent(&self, since: DateTime<Utc>, limit: usize) -> Result<Vec<WikiOpEnvelope>> {
        let mut stmt = self.conn.prepare(
            "SELECT op_id, scope, user_id, op_json, provenance, status, failure,
                    created_at, applied_at, reviewed_at, reviewed_by, confidence
             FROM wiki_audit WHERE created_at >= ?1
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![since.to_rfc3339(), limit as i64],
            envelope_from_row,
        )?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }
}

fn envelope_from_row(row: &rusqlite::Row) -> rusqlite::Result<WikiOpEnvelope> {
    let op_id: String = row.get(0)?;
    let scope_str: String = row.get(1)?;
    let user_id: Option<String> = row.get(2)?;
    let op_json: String = row.get(3)?;
    let prov_json: String = row.get(4)?;
    let status_str: String = row.get(5)?;
    let failure: Option<String> = row.get(6)?;
    let created_at: String = row.get(7)?;
    let applied_at: Option<String> = row.get(8)?;
    let reviewed_at: Option<String> = row.get(9)?;
    let reviewed_by: Option<String> = row.get(10)?;
    let confidence: Option<f32> = row.get(11)?;

    // Errors from JSON parsing are surfaced as rusqlite::Error::FromSqlConversionFailure
    // so the query_map's iterator can propagate them.
    let op = serde_json::from_str(&op_json).map_err(|e| rusqlite::Error::FromSqlConversionFailure(
        3, rusqlite::types::Type::Text, Box::new(e),
    ))?;
    let provenance = serde_json::from_str(&prov_json).map_err(|e| rusqlite::Error::FromSqlConversionFailure(
        4, rusqlite::types::Type::Text, Box::new(e),
    ))?;
    let scope = match scope_str.as_str() {
        "user" => {
            let uid = user_id.unwrap_or_default();
            crate::wiki::ops::WikiScope::User(uid)
        }
        _ => crate::wiki::ops::WikiScope::System,
    };
    let status = OpStatus::from_str(&status_str).unwrap_or(OpStatus::Pending);
    Ok(WikiOpEnvelope {
        op_id,
        scope,
        op,
        status,
        provenance,
        created_at: parse_rfc3339(&created_at),
        applied_at: applied_at.as_deref().map(parse_rfc3339),
        reviewed_at: reviewed_at.as_deref().map(parse_rfc3339),
        reviewed_by,
        failure,
        confidence,
    })
}

fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::ops::{LogKind, Provenance, WikiOp, WikiScope};
    use tempfile::tempdir;

    fn sample_envelope(user_id: &str) -> WikiOpEnvelope {
        WikiOpEnvelope::new(
            WikiScope::User(user_id.to_string()),
            WikiOp::LogEntry {
                kind: LogKind::Note,
                summary: "hello".into(),
                page_refs: vec![],
            },
            Provenance::user_ui(user_id),
        )
    }

    #[test]
    fn insert_and_get_round_trips() {
        let dir = tempdir().unwrap();
        let db = WikiAuditDb::open(&dir.path().join("wiki_u1.db")).unwrap();
        let env = sample_envelope("u1");
        let op_id = env.op_id.clone();
        db.insert(&env).unwrap();
        let back = db.get(&op_id).unwrap().unwrap();
        assert_eq!(back.op_id, op_id);
        assert_eq!(back.status, OpStatus::Pending);
        assert_eq!(back.op.kind(), "log_entry");
    }

    #[test]
    fn mark_applied_updates_status() {
        let dir = tempdir().unwrap();
        let db = WikiAuditDb::open(&dir.path().join("wiki_u2.db")).unwrap();
        let env = sample_envelope("u2");
        let op_id = env.op_id.clone();
        db.insert(&env).unwrap();
        db.mark_applied(&op_id).unwrap();
        let back = db.get(&op_id).unwrap().unwrap();
        assert_eq!(back.status, OpStatus::Applied);
        assert!(back.applied_at.is_some());
    }

    #[test]
    fn list_by_status_filters() {
        let dir = tempdir().unwrap();
        let db = WikiAuditDb::open(&dir.path().join("wiki_u3.db")).unwrap();
        let e1 = sample_envelope("u3");
        let e2 = sample_envelope("u3");
        db.insert(&e1).unwrap();
        db.insert(&e2).unwrap();
        db.mark_applied(&e1.op_id).unwrap();

        let pending = db.list_by_status(OpStatus::Pending).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op_id, e2.op_id);
    }
}
