// SPDX-License-Identifier: AGPL-3.0-or-later

// src/calendar/store.rs

//! SQLite-backed calendar event store.
//!
//! One DB file (`<data_dir>/calendar.db`) holds events plus per-provider
//! OAuth tokens. Events are scoped by `owner_user_id` so the same MIRA
//! install can serve multiple users without cross-leakage.

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng, Payload},
    Aes256Gcm, Key, Nonce,
};
use chrono::Utc;
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::debug;
use uuid::Uuid;

use crate::MiraError;

use super::models::{CalendarEvent, EventInput, EventKind, EventSource};

/// Owner sentinel for **shared / organisation** events — owned by the instance,
/// not a person. Every user sees them (folded into `list_events`); only admins
/// create/edit/delete them (gated in the HTTP handlers). Chosen to never collide
/// with a real user id (UUIDs / usernames).
pub const SHARED_OWNER: &str = "__org__";

/// Owner prefix for **group-scoped** shared events: `grp:<group_id>`. Only that
/// group's members see them. Built from a group id via [`group_owner`].
pub const GROUP_OWNER_PREFIX: &str = "grp:";

/// The owner sentinel for a group-scoped calendar: `grp:<group_id>`.
pub fn group_owner(group_id: &str) -> String {
    format!("{GROUP_OWNER_PREFIX}{group_id}")
}

/// Per-user CalDAV credentials (decrypted). The password is stored encrypted at
/// rest (AES-256-GCM under the instance master key); only ever held in plaintext
/// transiently here to drive a sync.
#[derive(Debug, Clone)]
pub struct CalDavCreds {
    pub url:      String,
    pub username: String,
    pub password: String,
}

/// Opaque token-store row used by sync adapters to persist OAuth state across
/// restarts.
#[derive(Debug, Clone)]
pub struct OAuthTokens {
    pub user_id:       String,
    pub provider:      String, // "google" | "outlook"
    pub access_token:  String,
    pub refresh_token: Option<String>,
    pub expires_at:    Option<i64>,  // ms since epoch
    pub scope:         Option<String>,
}

pub struct CalendarStore {
    conn:   Arc<Mutex<Connection>>,
    /// AES-256-GCM under the instance master key — encrypts per-user CalDAV
    /// passwords at rest. Shares the same `master.key` as the skill-secrets vault.
    cipher: Aes256Gcm,
}

impl CalendarStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create calendar DB dir: {}", e))
            })?;
        }

        // Encrypt per-user CalDAV passwords under the instance master key
        // (sibling `master.key`, same one the skill-secrets vault uses).
        let key_path = path.parent().unwrap_or_else(|| Path::new(".")).join("master.key");
        let key = crate::skills::secrets::load_or_create_master_key(&key_path)
            .map_err(|e| MiraError::DatabaseError(format!("calendar master key: {e}")))?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));

        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open calendar DB: {}", e))
        })?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS calendar_events (
                id              TEXT PRIMARY KEY,
                owner_user_id   TEXT NOT NULL,
                summary         TEXT NOT NULL,
                description     TEXT,
                starts_at       INTEGER NOT NULL,
                ends_at         INTEGER NOT NULL,
                all_day         INTEGER NOT NULL DEFAULT 0,
                location        TEXT,
                rrule           TEXT,
                status          TEXT,
                source          TEXT NOT NULL DEFAULT 'native',
                external_id     TEXT,
                last_synced_at  INTEGER,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_calendar_owner
                ON calendar_events(owner_user_id, starts_at);
            CREATE INDEX IF NOT EXISTS idx_calendar_external
                ON calendar_events(owner_user_id, source, external_id);

            CREATE TABLE IF NOT EXISTS calendar_oauth_tokens (
                user_id        TEXT NOT NULL,
                provider       TEXT NOT NULL,
                access_token   TEXT NOT NULL,
                refresh_token  TEXT,
                expires_at     INTEGER,
                scope          TEXT,
                updated_at     INTEGER NOT NULL,
                PRIMARY KEY (user_id, provider)
            );

            -- Per-user CalDAV accounts (Nextcloud etc.). One row per user; the
            -- password is AES-256-GCM encrypted (nonce + ciphertext), never plain.
            CREATE TABLE IF NOT EXISTS calendar_caldav_accounts (
                user_id        TEXT PRIMARY KEY,
                url            TEXT NOT NULL,
                username       TEXT NOT NULL,
                pw_nonce       BLOB NOT NULL,
                pw_ciphertext  BLOB NOT NULL,
                updated_at     INTEGER NOT NULL
            );
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("calendar migration failed: {}", e)))?;

        // Idempotent migration: add `kind` column for v0.23 (event vs note).
        // SQLite has no `IF NOT EXISTS` for ADD COLUMN, so swallow the
        // duplicate-column error.
        if let Err(e) = conn.execute_batch(
            "ALTER TABLE calendar_events ADD COLUMN kind TEXT NOT NULL DEFAULT 'event';"
        ) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                return Err(MiraError::DatabaseError(format!("kind migration: {}", msg)));
            }
        }

        debug!("calendar schema ready at {}", path.display());
        Ok(Self { conn: Arc::new(Mutex::new(conn)), cipher })
    }

    // ── Per-user CalDAV credentials (encrypted password) ──────────────────────

    /// Store (or replace) a user's CalDAV account. The password is encrypted with
    /// AES-256-GCM under the instance master key, bound to the user_id as AAD so a
    /// row copied to another user fails to decrypt.
    pub fn save_caldav(&self, user_id: &str, url: &str, username: &str, password: &str)
        -> Result<(), MiraError>
    {
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self.cipher.encrypt(
            nonce,
            Payload { msg: password.as_bytes(), aad: user_id.as_bytes() },
        ).map_err(|_| MiraError::DatabaseError("caldav password encrypt failed".into()))?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO calendar_caldav_accounts (user_id, url, username, pw_nonce, pw_ciphertext, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(user_id) DO UPDATE SET
               url = excluded.url, username = excluded.username,
               pw_nonce = excluded.pw_nonce, pw_ciphertext = excluded.pw_ciphertext,
               updated_at = excluded.updated_at",
            params![user_id, url, username, &nonce_bytes[..], &ciphertext, Utc::now().timestamp()],
        ).map_err(|e| MiraError::DatabaseError(format!("save_caldav: {e}")))?;
        Ok(())
    }

    /// Decrypt and return a user's CalDAV credentials, or None if not connected.
    pub fn get_caldav(&self, user_id: &str) -> Result<Option<CalDavCreds>, MiraError> {
        let conn = self.lock()?;
        let row = conn.query_row(
            "SELECT url, username, pw_nonce, pw_ciphertext FROM calendar_caldav_accounts WHERE user_id = ?1",
            params![user_id],
            |r| Ok((
                r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?, r.get::<_, Vec<u8>>(3)?,
            )),
        ).optional().map_err(|e| MiraError::DatabaseError(format!("get_caldav: {e}")))?;
        let Some((url, username, nonce_bytes, ciphertext)) = row else { return Ok(None) };
        if nonce_bytes.len() != 12 {
            return Err(MiraError::DatabaseError("caldav nonce corrupt".into()));
        }
        let plain = self.cipher.decrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload { msg: &ciphertext, aad: user_id.as_bytes() },
        ).map_err(|_| MiraError::DatabaseError("caldav password decrypt failed".into()))?;
        let password = String::from_utf8(plain)
            .map_err(|_| MiraError::DatabaseError("caldav password not utf-8".into()))?;
        Ok(Some(CalDavCreds { url, username, password }))
    }

    /// Whether a user has connected a CalDAV account (no decryption).
    pub fn has_caldav(&self, user_id: &str) -> Result<bool, MiraError> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM calendar_caldav_accounts WHERE user_id = ?1",
            params![user_id], |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("has_caldav: {e}")))?;
        Ok(n > 0)
    }

    /// Remove a user's CalDAV account.
    pub fn delete_caldav(&self, user_id: &str) -> Result<(), MiraError> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM calendar_caldav_accounts WHERE user_id = ?1", params![user_id])
            .map_err(|e| MiraError::DatabaseError(format!("delete_caldav: {e}")))?;
        Ok(())
    }

    // ── Events ────────────────────────────────────────────────────────────────

    pub fn create_event(
        &self,
        owner_user_id: &str,
        input:         &EventInput,
    ) -> Result<CalendarEvent, MiraError> {
        let conn = self.lock()?;
        let now  = Utc::now().timestamp_millis();
        let id   = Uuid::new_v4().to_string();

        conn.execute(
            "INSERT INTO calendar_events
                (id, owner_user_id, summary, description, starts_at, ends_at,
                 all_day, location, rrule, status, source, external_id,
                 last_synced_at, created_at, updated_at, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'native', NULL, NULL, ?11, ?11, ?12)",
            params![
                id, owner_user_id, input.summary, input.description,
                input.starts_at, input.ends_at,
                input.all_day as i32,
                input.location, input.rrule, input.status,
                now, input.kind.as_str(),
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("create_event: {}", e)))?;

        Ok(CalendarEvent {
            id,
            owner_user_id:  owner_user_id.to_string(),
            summary:        input.summary.clone(),
            description:    input.description.clone(),
            starts_at:      input.starts_at,
            ends_at:        input.ends_at,
            all_day:        input.all_day,
            location:       input.location.clone(),
            rrule:          input.rrule.clone(),
            status:         input.status.clone(),
            source:         EventSource::Native,
            kind:           input.kind,
            external_id:    None,
            last_synced_at: None,
            created_at:     now,
            updated_at:     now,
        })
    }

    pub fn update_event(
        &self,
        owner_user_id: &str,
        id:            &str,
        input:         &EventInput,
    ) -> Result<Option<CalendarEvent>, MiraError> {
        let conn = self.lock()?;
        let now  = Utc::now().timestamp_millis();

        let n = conn.execute(
            "UPDATE calendar_events
                SET summary = ?1, description = ?2, starts_at = ?3, ends_at = ?4,
                    all_day = ?5, location = ?6, rrule = ?7, status = ?8,
                    kind = ?9, updated_at = ?10
              WHERE id = ?11 AND owner_user_id = ?12 AND source = 'native'",
            params![
                input.summary, input.description,
                input.starts_at, input.ends_at,
                input.all_day as i32,
                input.location, input.rrule, input.status,
                input.kind.as_str(), now, id, owner_user_id,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("update_event: {}", e)))?;

        if n == 0 { return Ok(None); }
        drop(conn);
        self.get_event(owner_user_id, id)
    }

    pub fn delete_event(
        &self,
        owner_user_id: &str,
        id:            &str,
    ) -> Result<bool, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM calendar_events
              WHERE id = ?1 AND owner_user_id = ?2 AND source = 'native'",
            params![id, owner_user_id],
        ).map_err(|e| MiraError::DatabaseError(format!("delete_event: {}", e)))?;
        Ok(n > 0)
    }

    pub fn get_event(
        &self,
        owner_user_id: &str,
        id:            &str,
    ) -> Result<Option<CalendarEvent>, MiraError> {
        let conn = self.lock()?;
        let row = conn.query_row(
            "SELECT id, owner_user_id, summary, description, starts_at, ends_at,
                    all_day, location, rrule, status, source, external_id,
                    last_synced_at, created_at, updated_at, kind
               FROM calendar_events
              WHERE id = ?1 AND owner_user_id = ?2",
            params![id, owner_user_id],
            row_to_event,
        ).optional()
         .map_err(|e| MiraError::DatabaseError(format!("get_event: {}", e)))?;
        Ok(row)
    }

    /// List events for a user within `[from_ms, to_ms)`. If both bounds are
    /// `None`, returns the next `limit` events ordered by start time.
    /// List a user's own events plus every shared owner in `shared_owners`
    /// (e.g. `SHARED_OWNER` for org-wide events and `grp:<id>` for each group the
    /// user belongs to). The HTTP handler computes `shared_owners` from the
    /// caller's group memberships; `list_events` is the convenience wrapper for
    /// callers that only need own + org-wide.
    pub fn list_events_scoped(
        &self,
        owner_user_id: &str,
        shared_owners: &[String],
        from_ms:       Option<i64>,
        to_ms:         Option<i64>,
        limit:         i64,
    ) -> Result<Vec<CalendarEvent>, MiraError> {
        use rusqlite::types::Value;
        let conn = self.lock()?;
        let (from, to) = (from_ms.unwrap_or(i64::MIN / 2), to_ms.unwrap_or(i64::MAX / 2));

        // owner_user_id = ?1, plus an IN (...) over the shared owners (?5, ?6, …).
        let owner_clause = if shared_owners.is_empty() {
            "owner_user_id = ?1".to_string()
        } else {
            let ph: Vec<String> = (0..shared_owners.len()).map(|i| format!("?{}", i + 5)).collect();
            format!("(owner_user_id = ?1 OR owner_user_id IN ({}))", ph.join(","))
        };
        let sql = format!(
            "SELECT id, owner_user_id, summary, description, starts_at, ends_at,
                    all_day, location, rrule, status, source, external_id,
                    last_synced_at, created_at, updated_at, kind
               FROM calendar_events
              WHERE {owner_clause}
                AND ends_at   >= ?2
                AND starts_at <= ?3
              ORDER BY starts_at ASC
              LIMIT ?4"
        );

        let mut bind: Vec<Value> = vec![
            Value::Text(owner_user_id.to_string()),
            Value::Integer(from),
            Value::Integer(to),
            Value::Integer(limit),
        ];
        for o in shared_owners { bind.push(Value::Text(o.clone())); }

        let mut stmt = conn.prepare(&sql).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind.iter()), row_to_event)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(out)
    }

    /// Own + org-wide shared events. (Group scoping goes through
    /// [`Self::list_events_scoped`] with the caller's group owners.)
    pub fn list_events(
        &self,
        owner_user_id: &str,
        from_ms:       Option<i64>,
        to_ms:         Option<i64>,
        limit:         i64,
    ) -> Result<Vec<CalendarEvent>, MiraError> {
        self.list_events_scoped(owner_user_id, &[SHARED_OWNER.to_string()], from_ms, to_ms, limit)
    }

    /// Upsert an externally-sourced event. Keyed on `(owner_user_id, source,
    /// external_id)` so a second sync replaces the previous mirror row
    /// in-place and preserves the stable MIRA `id`.
    pub fn upsert_external(
        &self,
        owner_user_id: &str,
        source:        EventSource,
        external_id:   &str,
        input:         &EventInput,
    ) -> Result<(), MiraError> {
        if matches!(source, EventSource::Native) {
            return Err(MiraError::DatabaseError(
                "upsert_external called with Native source".into(),
            ));
        }
        let conn = self.lock()?;
        let now  = Utc::now().timestamp_millis();

        let existing: Option<String> = conn.query_row(
            "SELECT id FROM calendar_events
              WHERE owner_user_id = ?1 AND source = ?2 AND external_id = ?3",
            params![owner_user_id, source.as_str(), external_id],
            |r| r.get::<_, String>(0),
        ).optional().map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE calendar_events
                        SET summary = ?1, description = ?2, starts_at = ?3, ends_at = ?4,
                            all_day = ?5, location = ?6, rrule = ?7, status = ?8,
                            kind = ?9, last_synced_at = ?10, updated_at = ?10
                      WHERE id = ?11",
                    params![
                        input.summary, input.description,
                        input.starts_at, input.ends_at,
                        input.all_day as i32,
                        input.location, input.rrule, input.status,
                        input.kind.as_str(), now, id,
                    ],
                ).map_err(|e| MiraError::DatabaseError(format!("upsert_external: {}", e)))?;
            }
            None => {
                let id = Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO calendar_events
                        (id, owner_user_id, summary, description, starts_at, ends_at,
                         all_day, location, rrule, status, source, external_id,
                         last_synced_at, created_at, updated_at, kind)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, ?13, ?14)",
                    params![
                        id, owner_user_id, input.summary, input.description,
                        input.starts_at, input.ends_at,
                        input.all_day as i32,
                        input.location, input.rrule, input.status,
                        source.as_str(), external_id, now, input.kind.as_str(),
                    ],
                ).map_err(|e| MiraError::DatabaseError(format!("upsert_external: {}", e)))?;
            }
        }
        Ok(())
    }

    /// Remove every external event for a given user+source whose external_id
    /// is not in `keep_ids`. Used to prune deletions during a full-refresh
    /// sync pass.
    pub fn prune_external(
        &self,
        owner_user_id: &str,
        source:        EventSource,
        keep_ids:      &[String],
    ) -> Result<usize, MiraError> {
        let conn = self.lock()?;
        if keep_ids.is_empty() {
            let n = conn.execute(
                "DELETE FROM calendar_events
                  WHERE owner_user_id = ?1 AND source = ?2",
                params![owner_user_id, source.as_str()],
            ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
            return Ok(n);
        }
        let placeholders = std::iter::repeat("?")
            .take(keep_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "DELETE FROM calendar_events
              WHERE owner_user_id = ? AND source = ?
                AND external_id NOT IN ({})",
            placeholders
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        params_vec.push(Box::new(owner_user_id.to_string()));
        params_vec.push(Box::new(source.as_str().to_string()));
        for k in keep_ids { params_vec.push(Box::new(k.clone())); }
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
        let n = conn.execute(&sql, refs.as_slice())
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(n)
    }

    // ── OAuth tokens ──────────────────────────────────────────────────────────

    pub fn get_tokens(
        &self,
        user_id:  &str,
        provider: &str,
    ) -> Result<Option<OAuthTokens>, MiraError> {
        let conn = self.lock()?;
        let row = conn.query_row(
            "SELECT user_id, provider, access_token, refresh_token, expires_at, scope
               FROM calendar_oauth_tokens
              WHERE user_id = ?1 AND provider = ?2",
            params![user_id, provider],
            |r| Ok(OAuthTokens {
                user_id:       r.get(0)?,
                provider:      r.get(1)?,
                access_token:  r.get(2)?,
                refresh_token: r.get(3)?,
                expires_at:    r.get(4)?,
                scope:         r.get(5)?,
            }),
        ).optional()
         .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(row)
    }

    pub fn save_tokens(&self, tokens: &OAuthTokens) -> Result<(), MiraError> {
        let conn = self.lock()?;
        let now  = Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO calendar_oauth_tokens
                (user_id, provider, access_token, refresh_token, expires_at, scope, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(user_id, provider) DO UPDATE SET
                access_token  = excluded.access_token,
                refresh_token = COALESCE(excluded.refresh_token, calendar_oauth_tokens.refresh_token),
                expires_at    = excluded.expires_at,
                scope         = excluded.scope,
                updated_at    = excluded.updated_at",
            params![
                tokens.user_id, tokens.provider, tokens.access_token,
                tokens.refresh_token, tokens.expires_at, tokens.scope, now,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("save_tokens: {}", e)))?;
        Ok(())
    }

    pub fn delete_tokens(&self, user_id: &str, provider: &str) -> Result<(), MiraError> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM calendar_oauth_tokens WHERE user_id = ?1 AND provider = ?2",
            params![user_id, provider],
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, MiraError> {
        self.conn.lock()
            .map_err(|e| MiraError::DatabaseError(format!("calendar lock: {}", e)))
    }
}

fn row_to_event(r: &rusqlite::Row) -> rusqlite::Result<CalendarEvent> {
    let all_day_i: i32 = r.get(6)?;
    let source_s: String = r.get(10)?;
    let kind_s: String = r.get(15)?;
    Ok(CalendarEvent {
        id:             r.get(0)?,
        owner_user_id:  r.get(1)?,
        summary:        r.get(2)?,
        description:    r.get(3)?,
        starts_at:      r.get(4)?,
        ends_at:        r.get(5)?,
        all_day:        all_day_i != 0,
        location:       r.get(7)?,
        rrule:          r.get(8)?,
        status:         r.get(9)?,
        source:         EventSource::parse(&source_s),
        kind:           EventKind::parse(&kind_s),
        external_id:    r.get(11)?,
        last_synced_at: r.get(12)?,
        created_at:     r.get(13)?,
        updated_at:     r.get(14)?,
    })
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_store() -> (CalendarStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = CalendarStore::open(&dir.path().join("cal.db")).unwrap();
        (store, dir)
    }

    fn sample_input(summary: &str, start: i64) -> EventInput {
        EventInput {
            summary:     summary.to_string(),
            description: Some("desc".to_string()),
            starts_at:   start,
            ends_at:     start + 3_600_000,
            all_day:     false,
            location:    Some("home".to_string()),
            rrule:       None,
            status:      None,
            kind:        EventKind::Event,
            shared:      false,
            group_id:    None,
        }
    }

    #[test]
    fn native_crud_roundtrip() {
        let (store, _dir) = open_store();
        let ev = store.create_event("u1", &sample_input("lunch", 1_000)).unwrap();
        assert_eq!(ev.source, EventSource::Native);
        assert_eq!(ev.summary, "lunch");

        let got = store.get_event("u1", &ev.id).unwrap().unwrap();
        assert_eq!(got.id, ev.id);

        let mut upd = sample_input("renamed", 1_000);
        upd.location = Some("office".to_string());
        let after = store.update_event("u1", &ev.id, &upd).unwrap().unwrap();
        assert_eq!(after.summary, "renamed");
        assert_eq!(after.location.as_deref(), Some("office"));

        assert!(store.delete_event("u1", &ev.id).unwrap());
        assert!(store.get_event("u1", &ev.id).unwrap().is_none());
    }

    #[test]
    fn cross_user_isolation() {
        let (store, _dir) = open_store();
        let ev = store.create_event("u1", &sample_input("private", 42)).unwrap();
        assert!(store.get_event("u2", &ev.id).unwrap().is_none(),
                "u2 must not see u1's events");
        assert!(!store.delete_event("u2", &ev.id).unwrap(),
                "u2 must not delete u1's events");
    }

    #[test]
    fn list_events_range_filter() {
        let (store, _dir) = open_store();
        store.create_event("u1", &sample_input("a", 1_000)).unwrap();
        store.create_event("u1", &sample_input("b", 5_000_000_000)).unwrap();
        store.create_event("u1", &sample_input("c", 10_000_000_000)).unwrap();

        let all = store.list_events("u1", None, None, 100).unwrap();
        assert_eq!(all.len(), 3);

        let mid = store.list_events("u1", Some(4_000_000_000), Some(6_000_000_000), 100).unwrap();
        assert_eq!(mid.len(), 1);
        assert_eq!(mid[0].summary, "b");
    }

    #[test]
    fn upsert_external_keeps_stable_id() {
        let (store, _dir) = open_store();
        store.upsert_external("u1", EventSource::Google, "g-1",
                              &sample_input("meeting", 1_000)).unwrap();
        let v1 = store.list_events("u1", None, None, 100).unwrap();
        assert_eq!(v1.len(), 1);
        let id1 = v1[0].id.clone();

        store.upsert_external("u1", EventSource::Google, "g-1",
                              &sample_input("meeting v2", 2_000)).unwrap();
        let v2 = store.list_events("u1", None, None, 100).unwrap();
        assert_eq!(v2.len(), 1, "second upsert must replace, not duplicate");
        assert_eq!(v2[0].id, id1, "MIRA id is stable across syncs");
        assert_eq!(v2[0].summary, "meeting v2");
    }

    #[test]
    fn update_and_delete_refuse_to_touch_external_events() {
        let (store, _dir) = open_store();
        store.upsert_external("u1", EventSource::Caldav, "c-1",
                              &sample_input("mirror", 1_000)).unwrap();
        let id = store.list_events("u1", None, None, 1).unwrap()[0].id.clone();
        assert!(store.update_event("u1", &id, &sample_input("nope", 1_000)).unwrap().is_none());
        assert!(!store.delete_event("u1", &id).unwrap());
    }

    #[test]
    fn prune_external_removes_stale_rows() {
        let (store, _dir) = open_store();
        store.upsert_external("u1", EventSource::Google, "g-1",
                              &sample_input("keep", 1)).unwrap();
        store.upsert_external("u1", EventSource::Google, "g-2",
                              &sample_input("drop", 2)).unwrap();
        let kept = vec!["g-1".to_string()];
        let n = store.prune_external("u1", EventSource::Google, &kept).unwrap();
        assert_eq!(n, 1);
        let rows = store.list_events("u1", None, None, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].external_id.as_deref(), Some("g-1"));
    }

    #[test]
    fn kind_roundtrips_event_and_note() {
        let (store, _dir) = open_store();
        let mut note_in = sample_input("buy bread", 1_000);
        note_in.kind = EventKind::Note;
        let note = store.create_event("u1", &note_in).unwrap();
        assert_eq!(note.kind, EventKind::Note);

        let got = store.get_event("u1", &note.id).unwrap().unwrap();
        assert_eq!(got.kind, EventKind::Note);

        // Promote to event via update
        let mut upd = sample_input("buy bread", 1_000);
        upd.kind = EventKind::Event;
        let after = store.update_event("u1", &note.id, &upd).unwrap().unwrap();
        assert_eq!(after.kind, EventKind::Event);
    }

    #[test]
    fn oauth_tokens_roundtrip_and_refresh_preserves() {
        let (store, _dir) = open_store();
        store.save_tokens(&OAuthTokens {
            user_id:       "u1".to_string(),
            provider:      "google".to_string(),
            access_token:  "a".to_string(),
            refresh_token: Some("r".to_string()),
            expires_at:    Some(1_000),
            scope:         Some("scope".to_string()),
        }).unwrap();
        let got = store.get_tokens("u1", "google").unwrap().unwrap();
        assert_eq!(got.access_token, "a");

        // refresh_token omitted on subsequent update → previous one preserved.
        store.save_tokens(&OAuthTokens {
            user_id:       "u1".to_string(),
            provider:      "google".to_string(),
            access_token:  "a2".to_string(),
            refresh_token: None,
            expires_at:    Some(2_000),
            scope:         Some("scope".to_string()),
        }).unwrap();
        let got = store.get_tokens("u1", "google").unwrap().unwrap();
        assert_eq!(got.access_token, "a2");
        assert_eq!(got.refresh_token.as_deref(), Some("r"));

        store.delete_tokens("u1", "google").unwrap();
        assert!(store.get_tokens("u1", "google").unwrap().is_none());
    }
}
