// SPDX-License-Identifier: AGPL-3.0-or-later

//! Persistence for in-flight `cpp_provider` install sessions.
//!
//! A [`super::engine::ProvisionSession`] is the resumable state of a guided
//! install. The wizard is web-driven and pauses on human steps (run an `occ`
//! command, paste an app-password) that can take minutes or hours — across page
//! reloads and browser sessions. This store keeps the session blob in `auth.db`
//! so "resume where you left off" survives all of that, and a server restart.
//!
//! One in-flight session per package id (re-beginning an install for the same
//! id replaces it). The row is deleted on finalize (the package record in
//! `installed_packages` takes over) or on cancel.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

use crate::MiraError;

use super::engine::ProvisionSession;

// Metadata for listing in-flight sessions without deserializing the blob.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub package_id: String,
    pub status: String,
    pub updated_at: i64,
}

pub struct ProvisionSessionStore {
    conn: Arc<Mutex<Connection>>,
}

impl ProvisionSessionStore {
    // Open the store at `path` (typically `<data_dir>/auth.db`), creating the
    // table if needed.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MiraError::DatabaseError(format!("create sessions DB dir: {e}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| MiraError::DatabaseError(format!("open sessions DB: {e}")))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS provision_sessions (
                package_id   TEXT PRIMARY KEY,
                status       TEXT NOT NULL,
                session_json TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                updated_at   INTEGER NOT NULL
            );
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("provision_sessions migration: {e}")))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    // Open against an in-memory DB (tests).
    #[cfg(test)]
    pub fn open_memory() -> Result<Self, MiraError> {
        Self::open(Path::new(":memory:"))
    }

    fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }

    // Insert or replace the in-flight session for a package id. `created_at` is
    // preserved across saves so the original start time survives.
    pub fn put(&self, session: &ProvisionSession) -> Result<(), MiraError> {
        let now = Self::now_ms();
        let status = serde_json::to_value(&session.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "in_progress".into());
        let blob = serde_json::to_string(session)
            .map_err(|e| MiraError::ConfigError(format!("serialise session: {e}")))?;

        let conn = self.conn.lock().unwrap();
        let created_at: i64 = conn
            .query_row(
                "SELECT created_at FROM provision_sessions WHERE package_id = ?1",
                params![session.package_id],
                |r| r.get(0),
            )
            .unwrap_or(now);
        conn.execute(
            "INSERT INTO provision_sessions (package_id, status, session_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(package_id) DO UPDATE SET
               status=?2, session_json=?3, updated_at=?5",
            params![session.package_id, status, blob, created_at, now],
        )
        .map_err(|e| MiraError::DatabaseError(format!("upsert session: {e}")))?;
        Ok(())
    }

    pub fn get(&self, package_id: &str) -> Result<Option<ProvisionSession>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            "SELECT session_json FROM provision_sessions WHERE package_id = ?1",
            params![package_id],
            |row| row.get::<_, String>(0),
        );
        match r {
            Ok(blob) => serde_json::from_str(&blob)
                .map(Some)
                .map_err(|e| MiraError::ConfigError(format!("deserialise session: {e}"))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn list(&self) -> Result<Vec<SessionSummary>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT package_id, status, updated_at FROM provision_sessions ORDER BY updated_at DESC",
            )
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionSummary {
                    package_id: row.get(0)?,
                    status: row.get(1)?,
                    updated_at: row.get(2)?,
                })
            })
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(out)
    }

    pub fn delete(&self, package_id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM provision_sessions WHERE package_id = ?1",
            params![package_id],
        )
        .map_err(|e| MiraError::DatabaseError(format!("delete session: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::engine::{ProvisionSession, SessionStatus};
    use std::collections::BTreeMap;

    fn sample(id: &str, status: SessionStatus) -> ProvisionSession {
        ProvisionSession {
            package_id: id.into(),
            admin_id: "admin1".into(),
            config: BTreeMap::new(),
            outputs: BTreeMap::new(),
            steps: vec![],
            ledger: vec![],
            status,
            manifest: serde_json::Value::Null,
            trust: String::new(),
            version: String::new(),
            name: String::new(),
            install_dir: String::new(),
        }
    }

    #[test]
    fn put_get_roundtrip_and_status_column() {
        let store = ProvisionSessionStore::open_memory().unwrap();
        let s = sample("com.x.talk", SessionStatus::AwaitingInput);
        store.put(&s).unwrap();
        let got = store.get("com.x.talk").unwrap().unwrap();
        assert_eq!(got.package_id, "com.x.talk");
        assert_eq!(got.status, SessionStatus::AwaitingInput);
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, "awaiting_input");
    }

    #[test]
    fn put_is_upsert_and_delete_clears() {
        let store = ProvisionSessionStore::open_memory().unwrap();
        store.put(&sample("p", SessionStatus::InProgress)).unwrap();
        store.put(&sample("p", SessionStatus::Complete)).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.get("p").unwrap().unwrap().status, SessionStatus::Complete);
        store.delete("p").unwrap();
        assert!(store.get("p").unwrap().is_none());
    }

    #[test]
    fn missing_session_is_none() {
        let store = ProvisionSessionStore::open_memory().unwrap();
        assert!(store.get("nope").unwrap().is_none());
    }
}
