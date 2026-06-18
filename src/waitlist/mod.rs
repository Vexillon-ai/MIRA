// SPDX-License-Identifier: AGPL-3.0-or-later

//! Q1.7 — Hosted-MIRA waitlist.
//!
//! Small SQLite-backed list of email addresses captured from the
//! landing page's signup form. Public POST endpoint, admin-only read
//! + delete + export. No third-party dependency; the operator can
//! migrate to ConvertKit / Buttondown later by exporting the CSV.
//!
//! Schema is intentionally minimal — adding columns later is cheap
//! (idempotent ALTER), removing them isn't. Notes / source / ref
//! tags can grow as the operator learns what they need.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::MiraError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitlistEntry {
    pub id:         String,
    pub email:      String,
    pub created_at: DateTime<Utc>,
    /// User-Agent header of the signup request — useful for spotting
    /// bot floods. Truncated to 200 chars.
    pub user_agent: Option<String>,
    /// Free-text label the form posted (e.g. "personal" / "family" /
    /// "team"). The form ships without it for v1 but the column is
    /// here so adding a "what's this for?" radio later doesn't need
    /// a migration.
    pub source:     Option<String>,
}

pub struct WaitlistStore {
    conn: Mutex<Connection>,
}

impl WaitlistStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MiraError::ConfigError(format!("waitlist dir: {e}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| MiraError::ConfigError(format!("waitlist db open: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS waitlist (
               id          TEXT PRIMARY KEY,
               email       TEXT NOT NULL UNIQUE COLLATE NOCASE,
               created_at  INTEGER NOT NULL,
               user_agent  TEXT,
               source      TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_waitlist_created
               ON waitlist(created_at DESC);"
        ).map_err(|e| MiraError::ConfigError(format!("waitlist schema: {e}")))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Upsert by email. Re-signups don't error — they refresh the
    /// `created_at` (so a follow-up form submission is visible as
    /// "active interest") but keep the original id so the admin's
    /// "remove this entry" UX stays stable.
    pub fn signup(
        &self,
        email:      &str,
        user_agent: Option<&str>,
        source:     Option<&str>,
    ) -> Result<WaitlistEntry, MiraError> {
        let email = email.trim().to_lowercase();
        if !is_valid_email(&email) {
            return Err(MiraError::ConfigError(format!("invalid email: {email}")));
        }
        let ua = user_agent.map(|s| s.chars().take(200).collect::<String>());
        let conn = self.conn.lock().expect("waitlist store poisoned");
        let now = Utc::now();

        // Look up existing (case-insensitive thanks to COLLATE NOCASE).
        let existing: Option<String> = conn.query_row(
            "SELECT id FROM waitlist WHERE email = ?1",
            params![email],
            |r| r.get(0),
        ).optional().map_err(|e| MiraError::ConfigError(format!("lookup: {e}")))?;

        let id = match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE waitlist
                     SET created_at = ?2, user_agent = ?3, source = ?4
                     WHERE id = ?1",
                    params![id, now.timestamp_millis(), ua, source],
                ).map_err(|e| MiraError::ConfigError(format!("update: {e}")))?;
                id
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO waitlist (id, email, created_at, user_agent, source)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, email, now.timestamp_millis(), ua, source],
                ).map_err(|e| MiraError::ConfigError(format!("insert: {e}")))?;
                id
            }
        };
        Ok(WaitlistEntry {
            id,
            email,
            created_at: now,
            user_agent: ua,
            source: source.map(String::from),
        })
    }

    pub fn list(&self, limit: usize) -> Result<Vec<WaitlistEntry>, MiraError> {
        let conn = self.conn.lock().expect("waitlist store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, email, created_at, user_agent, source
             FROM waitlist ORDER BY created_at DESC LIMIT ?1",
        ).map_err(|e| MiraError::ConfigError(format!("list prep: {e}")))?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let created_ms: i64 = r.get(2)?;
            Ok(WaitlistEntry {
                id:         r.get(0)?,
                email:      r.get(1)?,
                created_at: DateTime::from_timestamp_millis(created_ms)
                    .unwrap_or_else(Utc::now),
                user_agent: r.get(3)?,
                source:     r.get(4)?,
            })
        }).map_err(|e| MiraError::ConfigError(format!("list query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::ConfigError(format!("row: {e}")))?);
        }
        Ok(out)
    }

    pub fn count(&self) -> Result<u64, MiraError> {
        let conn = self.conn.lock().expect("waitlist store poisoned");
        conn.query_row("SELECT COUNT(*) FROM waitlist", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(|e| MiraError::ConfigError(format!("count: {e}")))
    }

    pub fn delete(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().expect("waitlist store poisoned");
        conn.execute("DELETE FROM waitlist WHERE id = ?1", params![id])
            .map_err(|e| MiraError::ConfigError(format!("delete: {e}")))?;
        Ok(())
    }
}

/// Cheap shape check — not RFC-5322 compliant; the goal is "did the
/// user type something that vaguely looks like an email" not full
/// validation. Real verification happens on the first welcome email
/// the operator sends.
fn is_valid_email(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 3 || s.len() > 254 { return false; }
    let mut parts = s.split('@');
    let local  = parts.next().unwrap_or("");
    let domain = parts.next().unwrap_or("");
    if parts.next().is_some() { return false; } // more than one @
    if local.is_empty() || domain.is_empty() { return false; }
    if !domain.contains('.') { return false; }
    // No whitespace anywhere.
    if s.chars().any(char::is_whitespace) { return false; }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh() -> (tempfile::TempDir, WaitlistStore) {
        let d = tempdir().unwrap();
        let s = WaitlistStore::open(&d.path().join("waitlist.db")).unwrap();
        (d, s)
    }

    #[test]
    fn signup_inserts_and_lists() {
        let (_d, s) = fresh();
        s.signup("tarek@example.com", Some("Mozilla/5.0"), Some("personal")).unwrap();
        let list = s.list(10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].email, "tarek@example.com");
        assert_eq!(list[0].source.as_deref(), Some("personal"));
    }

    #[test]
    fn signup_is_idempotent_case_insensitive() {
        let (_d, s) = fresh();
        let a = s.signup("Tarek@Example.com", None, None).unwrap();
        let b = s.signup("tarek@example.com", None, None).unwrap();
        // Same id — second signup refreshed the first row, not a duplicate.
        assert_eq!(a.id, b.id);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn invalid_emails_rejected() {
        let (_d, s) = fresh();
        assert!(s.signup("not-an-email", None, None).is_err());
        assert!(s.signup("", None, None).is_err());
        assert!(s.signup("two@@signs.com", None, None).is_err());
        assert!(s.signup("no-domain@", None, None).is_err());
        assert!(s.signup("with space@example.com", None, None).is_err());
    }

    #[test]
    fn user_agent_truncated_to_200() {
        let (_d, s) = fresh();
        let big = "x".repeat(500);
        let e = s.signup("a@b.com", Some(&big), None).unwrap();
        assert_eq!(e.user_agent.unwrap().len(), 200);
    }
}
