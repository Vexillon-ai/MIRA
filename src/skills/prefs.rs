// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-user Skill preferences (slice A5).
//!
//! The data model is intentionally **opt-out**: a Skill is enabled for
//! every user by default the moment it loads. The only thing we persist
//! is explicit "this user disabled this skill" rows. Two reasons:
//!
//!   1. New Skills shouldn't require every user to click a button before
//!      the agent can use them — that's bad UX and creates a dead-on-
//!      arrival impression.
//!   2. If we stored an explicit "enabled" row for every (user, skill)
//!      pair, the table would balloon with the number of users *and*
//!      skills, and we'd have to backfill rows whenever a Skill is
//!      installed. Tracking only the disable list keeps the table small
//!      and the install path zero-touch.
//!
//! Schema:
//! ```sql
//! CREATE TABLE user_skill_disabled (
//!   user_id     TEXT NOT NULL,
//!   skill_id    TEXT NOT NULL,
//!   disabled_at INTEGER NOT NULL,   -- unix ms, audit
//!   PRIMARY KEY (user_id, skill_id)
//! );
//! ```

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rusqlite::{params, Connection};

use crate::MiraError;

pub struct SkillPrefsStore {
    conn: Arc<Mutex<Connection>>,
}

impl SkillPrefsStore {
    /// Open (and create-if-missing) the prefs DB at `path`.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create skill-prefs DB dir: {e}"))
            })?;
        }

        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open skill-prefs DB: {e}"))
        })?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS user_skill_disabled (
                user_id     TEXT NOT NULL,
                skill_id    TEXT NOT NULL,
                disabled_at INTEGER NOT NULL,
                PRIMARY KEY (user_id, skill_id)
            );
            CREATE INDEX IF NOT EXISTS idx_user_skill_disabled_user
                ON user_skill_disabled(user_id);
            "#,
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Convenience constructor for tests — opens an in-memory database.
    #[cfg(test)]
    pub fn open_in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            r#"
            CREATE TABLE user_skill_disabled (
                user_id     TEXT NOT NULL,
                skill_id    TEXT NOT NULL,
                disabled_at INTEGER NOT NULL,
                PRIMARY KEY (user_id, skill_id)
            );
            "#,
        ).unwrap();
        Self { conn: Arc::new(Mutex::new(conn)) }
    }

    /// Has `user_id` explicitly disabled `skill_id`?
    /// Default answer (no row) is **false** — Skills are enabled by default.
    pub fn is_disabled(&self, user_id: &str, skill_id: &str) -> bool {
        let conn = self.conn.lock().expect("lock");
        let mut stmt = match conn.prepare(
            "SELECT 1 FROM user_skill_disabled WHERE user_id = ? AND skill_id = ?",
        ) {
            Ok(s) => s,
            Err(_) => return false, // fail-open here — runtime check has its own
                                    // guard; we'd rather lose a disable than
                                    // unconditionally block the user.
        };
        stmt.exists(params![user_id, skill_id]).unwrap_or(false)
    }

    /// Inverse of `is_disabled` for readability at call sites.
    pub fn is_enabled(&self, user_id: &str, skill_id: &str) -> bool {
        !self.is_disabled(user_id, skill_id)
    }

    /// Set the enabled state for one (user, skill). `enabled = false`
    /// inserts (or updates the timestamp on) a disable row; `enabled =
    /// true` removes any existing disable row.
    pub fn set_enabled(&self, user_id: &str, skill_id: &str, enabled: bool) -> Result<(), MiraError> {
        let conn = self.conn.lock().expect("lock");
        if enabled {
            conn.execute(
                "DELETE FROM user_skill_disabled WHERE user_id = ? AND skill_id = ?",
                params![user_id, skill_id],
            ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        } else {
            let now = Utc::now().timestamp_millis();
            conn.execute(
                "INSERT INTO user_skill_disabled (user_id, skill_id, disabled_at)
                 VALUES (?, ?, ?)
                 ON CONFLICT(user_id, skill_id) DO UPDATE SET disabled_at = excluded.disabled_at",
                params![user_id, skill_id, now],
            ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        }
        Ok(())
    }

    /// All Skill ids `user_id` has disabled. Used by the `/api/skills`
    /// handler to project an `enabled` flag per Skill in one query.
    pub fn disabled_for_user(&self, user_id: &str) -> HashSet<String> {
        let conn = self.conn.lock().expect("lock");
        let mut stmt = match conn.prepare(
            "SELECT skill_id FROM user_skill_disabled WHERE user_id = ?",
        ) {
            Ok(s) => s,
            Err(_) => return HashSet::new(),
        };
        let rows = stmt.query_map(params![user_id], |row| row.get::<_, String>(0));
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_)   => HashSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_enabled_by_default() {
        let store = SkillPrefsStore::open_in_memory();
        assert!(store.is_enabled("alice", "com.mira.research"));
        assert!(!store.is_disabled("alice", "com.mira.research"));
    }

    #[test]
    fn disable_persists() {
        let store = SkillPrefsStore::open_in_memory();
        store.set_enabled("alice", "com.mira.research", false).unwrap();
        assert!(store.is_disabled("alice", "com.mira.research"));
        assert!(!store.is_enabled("alice", "com.mira.research"));
    }

    #[test]
    fn re_enable_clears_disable_row() {
        let store = SkillPrefsStore::open_in_memory();
        store.set_enabled("alice", "com.mira.research", false).unwrap();
        store.set_enabled("alice", "com.mira.research", true).unwrap();
        assert!(store.is_enabled("alice", "com.mira.research"));
        assert_eq!(store.disabled_for_user("alice").len(), 0);
    }

    #[test]
    fn disabled_for_user_lists_only_that_user() {
        let store = SkillPrefsStore::open_in_memory();
        store.set_enabled("alice", "com.a.x", false).unwrap();
        store.set_enabled("alice", "com.a.y", false).unwrap();
        store.set_enabled("bob",   "com.a.x", false).unwrap();

        let alice = store.disabled_for_user("alice");
        assert_eq!(alice.len(), 2);
        assert!(alice.contains("com.a.x"));
        assert!(alice.contains("com.a.y"));

        let bob = store.disabled_for_user("bob");
        assert_eq!(bob.len(), 1);
        assert!(bob.contains("com.a.x"));

        // Bob's disabling com.a.x doesn't affect Alice's view of com.a.x.
        assert!(!alice.contains("not-there"));
    }

    #[test]
    fn double_disable_is_idempotent() {
        let store = SkillPrefsStore::open_in_memory();
        store.set_enabled("alice", "com.a.x", false).unwrap();
        store.set_enabled("alice", "com.a.x", false).unwrap();
        // No panic, no duplicate row (PK conflict path handled).
        assert!(store.is_disabled("alice", "com.a.x"));
    }
}
