// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel_identity/store.rs
//
// `user_channel_links` — confirmed mappings of (channel, external_id) →
// MIRA user_id. Read on the hot path of every inbound message for bots
// in shared/guest_ok mode, so the lookup is intentionally a single
// indexed SQLite query.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::MiraError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelLink {
    pub id:           String,
    pub user_id:      String,
    /// "signal" / "telegram" / "discord" — kept as a free string in the
    /// table so a new channel doesn't need a schema migration.
    pub channel:      String,
    pub external_id:  String,
    pub created_at:   i64,
    pub verified_at:  i64,
}

pub struct IdentityStore {
    conn: Arc<Mutex<Connection>>,
}

impl IdentityStore {
    /// Open the store at `path` (typically `<data_dir>/auth.db`). Creates
    /// the `user_channel_links` table on first run.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open identity DB: {}", e))
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS user_channel_links (
                id           TEXT PRIMARY KEY,
                user_id      TEXT NOT NULL,
                channel      TEXT NOT NULL,
                external_id  TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                verified_at  INTEGER NOT NULL,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
                UNIQUE(channel, external_id)
            );
            CREATE INDEX IF NOT EXISTS idx_uchanlinks_user
                ON user_channel_links(user_id);
            CREATE INDEX IF NOT EXISTS idx_uchanlinks_lookup
                ON user_channel_links(channel, external_id);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!(
            "user_channel_links migration failed: {}", e
        )))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Hot path — return the MIRA user_id mapped to `(channel,
    /// external_id)`, or None if no link exists. Used by the dispatcher
    /// before every inbound turn for shared/guest_ok bots.
    pub fn lookup(&self, channel: &str, external_id: &str) -> Result<Option<String>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            "SELECT user_id FROM user_channel_links
              WHERE channel = ?1 AND external_id = ?2",
            params![channel, external_id],
            |row| row.get::<_, String>(0),
        );
        match r {
            Ok(uid) => Ok(Some(uid)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// Create a new link. The UNIQUE constraint on `(channel, external_id)`
    /// surfaces as an error if the same external account is already linked
    /// to someone else — the handler maps that to a 409 Conflict.
    pub fn link(&self, user_id: &str, channel: &str, external_id: &str) -> Result<ChannelLink, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user_channel_links
               (id, user_id, channel, external_id, created_at, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, user_id, channel, external_id, now],
        )
        .map_err(|e| {
            // SQLite reports UNIQUE violations as "UNIQUE constraint failed".
            // We surface that as a structured error so the HTTP layer can
            // return 409 rather than a generic 500.
            let s = e.to_string();
            if s.contains("UNIQUE constraint") {
                MiraError::ConfigError(format!(
                    "already linked to another user on this channel"
                ))
            } else {
                MiraError::DatabaseError(format!("create link: {}", e))
            }
        })?;
        Ok(ChannelLink {
            id, user_id: user_id.to_owned(), channel: channel.to_owned(),
            external_id: external_id.to_owned(), created_at: now, verified_at: now,
        })
    }

    /// Bind `(channel, external_id)` to `user_id`, **overwriting** any
    /// existing owner of that identity. Unlike [`link`], which refuses a
    /// re-bind via the UNIQUE constraint, this upserts — the caller must
    /// have already authorised the re-bind (e.g. a Personal Telegram bot
    /// owner who proved ownership with a fresh, single-use LINK code). A
    /// physical channel identity maps to exactly one MIRA user, so taking
    /// it over on proof of ownership is correct; it also self-heals a
    /// stale mapping left over from earlier testing.
    pub fn relink(&self, user_id: &str, channel: &str, external_id: &str) -> Result<ChannelLink, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user_channel_links
               (id, user_id, channel, external_id, created_at, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(channel, external_id) DO UPDATE SET
               user_id     = excluded.user_id,
               verified_at = excluded.verified_at",
            params![id, user_id, channel, external_id, now],
        )
        .map_err(|e| MiraError::DatabaseError(format!("relink: {}", e)))?;
        Ok(ChannelLink {
            id, user_id: user_id.to_owned(), channel: channel.to_owned(),
            external_id: external_id.to_owned(), created_at: now, verified_at: now,
        })
    }

    /// Drop a link by id. The caller is expected to enforce that the
    /// link's owner matches the caller's user_id (or that the caller is
    /// admin) — this method just removes the row.
    pub fn unlink(&self, id: &str) -> Result<bool, MiraError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM user_channel_links WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(n > 0)
    }

    /// Read one link by id — used by the unlink handler to verify
    /// ownership before deletion.
    pub fn get(&self, id: &str) -> Result<Option<ChannelLink>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            "SELECT id, user_id, channel, external_id, created_at, verified_at
               FROM user_channel_links WHERE id = ?1",
            params![id], row_to_link,
        );
        match r {
            Ok(l) => Ok(Some(l)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// All links owned by a MIRA user — drives the Settings → My Channels
    /// list. Sorted by channel then created_at for stable display.
    pub fn list_for_user(&self, user_id: &str) -> Result<Vec<ChannelLink>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, channel, external_id, created_at, verified_at
               FROM user_channel_links WHERE user_id = ?1
               ORDER BY channel, created_at",
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map(params![user_id], row_to_link)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?); }
        Ok(out)
    }
}

fn row_to_link(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelLink> {
    Ok(ChannelLink {
        id:          row.get(0)?,
        user_id:     row.get(1)?,
        channel:     row.get(2)?,
        external_id: row.get(3)?,
        created_at:  row.get(4)?,
        verified_at: row.get(5)?,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::models::{AuthDb, NewUser, Role};
    use tempfile::tempdir;

    fn open_with_users() -> (tempfile::TempDir, IdentityStore, String, String) {
        let dir  = tempdir().unwrap();
        let path = dir.path().join("auth.db");
        // AuthDb seeds the `users` table the FK depends on.
        let auth = AuthDb::open(&path).unwrap();
        let alice = auth.create_user(NewUser {
            username: "alice".into(), display_name: None, email: None,
            password: "p".into(), role: Role::User,
        }, "h".into()).unwrap();
        let bob = auth.create_user(NewUser {
            username: "bob".into(), display_name: None, email: None,
            password: "p".into(), role: Role::User,
        }, "h".into()).unwrap();
        let store = IdentityStore::open(&path).unwrap();
        (dir, store, alice.id, bob.id)
    }

    #[test]
    fn link_then_lookup_roundtrips() {
        let (_d, s, alice, _) = open_with_users();
        s.link(&alice, "discord", "111").unwrap();
        let found = s.lookup("discord", "111").unwrap();
        assert_eq!(found.as_deref(), Some(alice.as_str()));
    }

    #[test]
    fn lookup_misses_return_none_not_err() {
        let (_d, s, _, _) = open_with_users();
        assert_eq!(s.lookup("discord", "nobody").unwrap(), None);
    }

    #[test]
    fn second_user_cannot_steal_an_existing_link() {
        let (_d, s, alice, bob) = open_with_users();
        s.link(&alice, "discord", "111").unwrap();
        let err = s.link(&bob, "discord", "111").unwrap_err();
        assert!(format!("{:?}", err).contains("already linked"));
    }

    #[test]
    fn relink_overwrites_a_stale_mapping() {
        // Telegram chat 111 is stale-mapped to alice (e.g. earlier testing).
        // A plain link by bob is refused — but relink (owner proved ownership
        // with a fresh code) takes the identity over. This is the fix for a
        // Personal bot owner who could never re-claim a chat bound elsewhere.
        let (_d, s, alice, bob) = open_with_users();
        s.link(&alice, "telegram", "111").unwrap();
        assert!(s.link(&bob, "telegram", "111").is_err(), "plain link must still refuse a steal");
        s.relink(&bob, "telegram", "111").unwrap();
        assert_eq!(s.lookup("telegram", "111").unwrap().as_deref(), Some(bob.as_str()));
        // Idempotent: relinking to the same owner again is fine.
        s.relink(&bob, "telegram", "111").unwrap();
        assert_eq!(s.lookup("telegram", "111").unwrap().as_deref(), Some(bob.as_str()));
    }

    #[test]
    fn same_external_id_on_different_channels_is_fine() {
        // A user's Discord snowflake and Signal phone happen to share
        // the same string — that's an unrealistic collision but the
        // schema correctly scopes uniqueness to (channel, external_id).
        let (_d, s, alice, _) = open_with_users();
        s.link(&alice, "discord", "+15551234567").unwrap();
        s.link(&alice, "signal",  "+15551234567").unwrap();
    }

    #[test]
    fn unlink_removes_the_row_and_frees_the_slot() {
        let (_d, s, alice, bob) = open_with_users();
        let l = s.link(&alice, "discord", "111").unwrap();
        assert!(s.unlink(&l.id).unwrap());
        assert!(s.lookup("discord", "111").unwrap().is_none());
        // bob can now claim it.
        s.link(&bob, "discord", "111").unwrap();
    }

    #[test]
    fn list_for_user_sorted_stable() {
        let (_d, s, alice, _) = open_with_users();
        s.link(&alice, "telegram", "tg-1").unwrap();
        s.link(&alice, "discord",  "dc-1").unwrap();
        s.link(&alice, "discord",  "dc-2").unwrap();
        let list = s.list_for_user(&alice).unwrap();
        assert_eq!(list.len(), 3);
        // discord rows come first (alphabetical), then telegram.
        assert_eq!(list[0].channel, "discord");
        assert_eq!(list[1].channel, "discord");
        assert_eq!(list[2].channel, "telegram");
    }
}
