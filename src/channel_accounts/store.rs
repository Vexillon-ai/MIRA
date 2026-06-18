// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel_accounts/store.rs
//! SQLite-backed store for per-user channel accounts. Lives in `auth.db`
//! alongside the `users` table — the FK on `user_id` cascades on user delete.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use uuid::Uuid;

use super::models::{
    ChannelAccount, ChannelKind, NewChannelAccount, UpdateChannelAccount,
};
use crate::MiraError;

// ─────────────────────────────────────────────────────────────────────────────

pub struct ChannelAccountStore {
    conn: Arc<Mutex<Connection>>,
}

impl ChannelAccountStore {
    /// Open the store at `path` (typically `<data_dir>/auth.db`). Creates the
    /// `channel_accounts` table on first run. The `users` table must already
    /// exist (open `AuthDb` first) so the FK can resolve.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create channel-accounts DB dir: {}", e))
            })?;
        }

        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open channel-accounts DB: {}", e))
        })?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS channel_accounts (
                id            TEXT PRIMARY KEY,
                user_id       TEXT NOT NULL,
                channel       TEXT NOT NULL,
                account_label TEXT NOT NULL,
                external_id   TEXT,
                config_json   TEXT NOT NULL,
                enabled       INTEGER NOT NULL DEFAULT 1,
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
                UNIQUE(channel, external_id),
                UNIQUE(user_id, channel, account_label)
            );
            CREATE INDEX IF NOT EXISTS idx_chacct_user
                ON channel_accounts(user_id, channel);
            CREATE INDEX IF NOT EXISTS idx_chacct_enabled
                ON channel_accounts(enabled);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!(
            "channel_accounts migration failed: {}", e
        )))?;

        // R1+R2 migration — add the routing_mode column to pre-existing
        // installations. ADD COLUMN is fast, lock-free, and the default
        // value backfills every row to "personal" so old single-user bots
        // keep their trust model unchanged. The `IF NOT EXISTS` dance
        // emulated via a PRAGMA check keeps the migration idempotent on
        // every boot.
        let has_routing_mode: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('channel_accounts') WHERE name = 'routing_mode'")
            .and_then(|mut s| s.exists([]))
            .unwrap_or(false);
        if !has_routing_mode {
            conn.execute(
                "ALTER TABLE channel_accounts ADD COLUMN routing_mode TEXT NOT NULL DEFAULT 'personal'",
                [],
            )
            .map_err(|e| MiraError::DatabaseError(format!(
                "channel_accounts add routing_mode failed: {}", e
            )))?;
        }

        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    // ── Create ────────────────────────────────────────────────────────────────

    pub fn create(&self, new: NewChannelAccount) -> Result<ChannelAccount, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let ch  = new.channel.as_str().to_owned();
        let en  = new.enabled as i64;
        let rm  = new.routing_mode.as_str().to_owned();

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO channel_accounts
               (id, user_id, channel, account_label, external_id, config_json, enabled, routing_mode, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                id, new.user_id, ch, new.account_label, new.external_id,
                new.config_json, en, rm, now,
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("create channel_account: {}", e)))?;

        Ok(ChannelAccount {
            id,
            user_id:       new.user_id,
            channel:       new.channel,
            account_label: new.account_label,
            external_id:   new.external_id,
            config_json:   new.config_json,
            enabled:       new.enabled,
            routing_mode:  new.routing_mode,
            created_at:    now,
            updated_at:    now,
        })
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    pub fn get(&self, id: &str) -> Result<Option<ChannelAccount>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, user_id, channel, account_label, external_id, config_json, enabled, routing_mode, created_at, updated_at
             FROM channel_accounts WHERE id = ?1",
            params![id],
            row_to_account,
        );
        match result {
            Ok(a) => Ok(Some(a)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn list_all(&self) -> Result<Vec<ChannelAccount>, MiraError> {
        self.query_list(
            "SELECT id, user_id, channel, account_label, external_id, config_json, enabled, routing_mode, created_at, updated_at
             FROM channel_accounts ORDER BY created_at ASC",
            params![],
        )
    }

    pub fn list_for_user(&self, user_id: &str) -> Result<Vec<ChannelAccount>, MiraError> {
        self.query_list(
            "SELECT id, user_id, channel, account_label, external_id, config_json, enabled, routing_mode, created_at, updated_at
             FROM channel_accounts WHERE user_id = ?1 ORDER BY created_at ASC",
            params![user_id],
        )
    }

    /// Total number of rows — used by the legacy-config migrator to decide
    /// whether the store is a fresh install (empty) or already populated.
    pub fn count_all(&self) -> Result<u64, MiraError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM channel_accounts",
            params![],
            |r| r.get(0),
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(n.max(0) as u64)
    }

    /// All enabled accounts across all users — what the gateway iterates at
    /// startup to spawn daemons.
    pub fn list_enabled(&self) -> Result<Vec<ChannelAccount>, MiraError> {
        self.query_list(
            "SELECT id, user_id, channel, account_label, external_id, config_json, enabled, routing_mode, created_at, updated_at
             FROM channel_accounts WHERE enabled = 1 ORDER BY created_at ASC",
            params![],
        )
    }

    /// R1+R2 — find an enabled, shared-routing (`shared`/`guest_ok`) bot on
    /// `channel`, regardless of who owns it. Used by outbound dispatchers to
    /// reach a user who has no personal bot of their own but is *linked* to
    /// a shared admin-managed bot: the bot's owner row carries the token we
    /// send through. Oldest-first so the choice is stable when (rarely) more
    /// than one shared bot exists on a channel. `personal` rows are excluded
    /// — they only ever deliver to their own owner.
    pub fn first_shared_bot(&self, channel: ChannelKind) -> Result<Option<ChannelAccount>, MiraError> {
        let rows = self.query_list(
            "SELECT id, user_id, channel, account_label, external_id, config_json, enabled, routing_mode, created_at, updated_at
             FROM channel_accounts
             WHERE enabled = 1 AND channel = ?1 AND routing_mode IN ('shared','guest_ok')
             ORDER BY created_at ASC",
            params![channel.as_str()],
        )?;
        Ok(rows.into_iter().next())
    }

    /// R1+R2 — resolve the Telegram bot token to send an outbound message to
    /// `recipient_user_id` through. Tries the recipient's own enabled
    /// Telegram account first (personal-bot model); failing that, falls back
    /// to a shared admin-managed Telegram bot. `Ok(None)` means neither
    /// exists — the caller turns that into a user-facing "set one up" error.
    pub fn outbound_telegram_token(&self, recipient_user_id: &str) -> Result<Option<String>, MiraError> {
        if let Some(tok) = self.list_for_user(recipient_user_id)?
            .into_iter()
            .filter(|a| a.channel == ChannelKind::Telegram && a.enabled)
            .find_map(|a| a.telegram_config().ok().map(|c| c.bot_token))
        {
            return Ok(Some(tok));
        }
        Ok(self.first_shared_bot(ChannelKind::Telegram)?
            .and_then(|a| a.telegram_config().ok().map(|c| c.bot_token)))
    }

    /// R1+R2 — Discord analogue of `outbound_telegram_token`. Tries the
    /// recipient's own enabled Discord bot first, then falls back to a
    /// shared admin-managed Discord bot. `Ok(None)` means neither exists.
    pub fn outbound_discord_token(&self, recipient_user_id: &str) -> Result<Option<String>, MiraError> {
        if let Some(tok) = self.list_for_user(recipient_user_id)?
            .into_iter()
            .filter(|a| a.channel == ChannelKind::Discord && a.enabled)
            .find_map(|a| a.discord_config().ok().map(|c| c.bot_token))
        {
            return Ok(Some(tok));
        }
        Ok(self.first_shared_bot(ChannelKind::Discord)?
            .and_then(|a| a.discord_config().ok().map(|c| c.bot_token)))
    }

    /// R1+R2 — Matrix analogue. Returns `(homeserver, access_token)` for
    /// the recipient's own enabled Matrix bot, else a shared admin one.
    /// Matrix needs both the homeserver URL and the token to send, so
    /// (unlike the single-string telegram/discord variants) this returns
    /// the pair. `Ok(None)` means neither a personal nor a shared bot exists.
    pub fn outbound_matrix_creds(&self, recipient_user_id: &str) -> Result<Option<(String, String)>, MiraError> {
        if let Some(pair) = self.list_for_user(recipient_user_id)?
            .into_iter()
            .filter(|a| a.channel == ChannelKind::Matrix && a.enabled)
            .find_map(|a| a.matrix_config().ok().map(|c| (c.homeserver, c.access_token)))
        {
            return Ok(Some(pair));
        }
        Ok(self.first_shared_bot(ChannelKind::Matrix)?
            .and_then(|a| a.matrix_config().ok().map(|c| (c.homeserver, c.access_token))))
    }

    /// R1+R2 — WhatsApp analogue. Returns `(phone_number_id, access_token)`
    /// for the recipient's own enabled WhatsApp bot, else a shared one.
    /// `Ok(None)` means neither exists.
    pub fn outbound_whatsapp_creds(&self, recipient_user_id: &str) -> Result<Option<(String, String)>, MiraError> {
        if let Some(pair) = self.list_for_user(recipient_user_id)?
            .into_iter()
            .filter(|a| a.channel == ChannelKind::WhatsApp && a.enabled)
            .find_map(|a| a.whatsapp_config().ok().map(|c| (c.phone_number_id, c.access_token)))
        {
            return Ok(Some(pair));
        }
        Ok(self.first_shared_bot(ChannelKind::WhatsApp)?
            .and_then(|a| a.whatsapp_config().ok().map(|c| (c.phone_number_id, c.access_token))))
    }

    /// R1+R2 — Slack analogue. Returns the bot token for the recipient's
    /// own enabled Slack bot, else a shared one. `Ok(None)` means neither.
    pub fn outbound_slack_token(&self, recipient_user_id: &str) -> Result<Option<String>, MiraError> {
        if let Some(tok) = self.list_for_user(recipient_user_id)?
            .into_iter()
            .filter(|a| a.channel == ChannelKind::Slack && a.enabled)
            .find_map(|a| a.slack_config().ok().map(|c| c.bot_token))
        {
            return Ok(Some(tok));
        }
        Ok(self.first_shared_bot(ChannelKind::Slack)?
            .and_then(|a| a.slack_config().ok().map(|c| c.bot_token)))
    }

    /// R1+R2 — External (CPP) analogue. Returns
    /// `(account_id, send_url, outbound_secret, supports_voice)` for the
    /// recipient's own enabled External account, else a shared one. The
    /// account_id is needed because CPP outbound bodies echo it back to the
    /// provider; `supports_voice` lets the proactive paths (companion +
    /// automations) attach synthesized audio the same way the inbound reply
    /// path does. `Ok(None)` means neither exists.
    pub fn outbound_external_creds(&self, recipient_user_id: &str)
        -> Result<Option<(String, String, String, bool)>, MiraError>
    {
        if let Some(t) = self.list_for_user(recipient_user_id)?
            .into_iter()
            .filter(|a| a.channel == ChannelKind::External && a.enabled)
            .find_map(|a| a.external_config().ok()
                .map(|c| (a.id.clone(), c.send_url, c.outbound_secret, c.supports_voice)))
        {
            return Ok(Some(t));
        }
        Ok(self.first_shared_bot(ChannelKind::External)?
            .and_then(|a| a.external_config().ok()
                .map(|c| (a.id.clone(), c.send_url, c.outbound_secret, c.supports_voice))))
    }

    fn query_list(
        &self,
        sql:    &str,
        p:      impl rusqlite::Params,
    ) -> Result<Vec<ChannelAccount>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map(p, row_to_account)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(out)
    }

    // ── Update / delete ───────────────────────────────────────────────────────

    pub fn update(
        &self,
        id:  &str,
        upd: UpdateChannelAccount,
    ) -> Result<ChannelAccount, MiraError> {
        // Read-modify-write so we can return the full row and avoid the per-
        // field SQL fan-out. Volume here is tiny (a few dozen accounts at
        // most), so the extra round-trip is fine.
        let mut acct = self.get(id)?
            .ok_or_else(|| MiraError::NotFound(format!("channel_account not found: {}", id)))?;

        if let Some(label) = upd.account_label { acct.account_label = label; }
        if let Some(ext)   = upd.external_id   { acct.external_id   = ext;   }
        if let Some(cfg)   = upd.config_json   { acct.config_json   = cfg;   }
        if let Some(en)    = upd.enabled       { acct.enabled       = en;    }
        if let Some(rm)    = upd.routing_mode  { acct.routing_mode  = rm;    }

        let now = Self::now_ms();
        let en  = acct.enabled as i64;
        let ch  = acct.channel.as_str().to_owned();
        let rm  = acct.routing_mode.as_str().to_owned();

        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE channel_accounts
                SET account_label=?1, external_id=?2, config_json=?3, enabled=?4, routing_mode=?5, updated_at=?6
              WHERE id=?7 AND channel=?8",
            params![
                acct.account_label, acct.external_id, acct.config_json,
                en, rm, now, id, ch,
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("update channel_account: {}", e)))?;

        if rows == 0 {
            return Err(MiraError::NotFound(format!("channel_account not found: {}", id)));
        }

        acct.updated_at = now;
        Ok(acct)
    }

    pub fn delete(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute("DELETE FROM channel_accounts WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if rows == 0 {
            return Err(MiraError::NotFound(format!("channel_account not found: {}", id)));
        }
        Ok(())
    }

    /// Convenience for the migrator: returns true when no rows exist at all.
    pub fn is_empty(&self) -> Result<bool, MiraError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM channel_accounts", [], |r| r.get(0),
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(count == 0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────

fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelAccount> {
    use std::str::FromStr;
    let kind_str: String = row.get(2)?;
    let channel = ChannelKind::from_str(&kind_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        ))
    })?;
    let rm_str: String = row.get(7)?;
    let routing_mode = crate::channel_accounts::RoutingMode::from_str(&rm_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        ))
    })?;
    Ok(ChannelAccount {
        id:            row.get(0)?,
        user_id:       row.get(1)?,
        channel,
        account_label: row.get(3)?,
        external_id:   row.get(4)?,
        config_json:   row.get(5)?,
        enabled:       row.get::<_, i64>(6)? != 0,
        routing_mode,
        created_at:    row.get(8)?,
        updated_at:    row.get(9)?,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::models::{AuthDb, NewUser, Role};
    use crate::channel_accounts::RoutingMode;
    use tempfile::tempdir;

    fn open_with_user() -> (tempfile::TempDir, ChannelAccountStore, String) {
        let dir  = tempdir().unwrap();
        let path = dir.path().join("auth.db");

        // The user table must exist before the FK can resolve.
        let auth = AuthDb::open(&path).unwrap();
        let user = auth.create_user(
            NewUser {
                username:     "alice".to_owned(),
                display_name: None,
                email:        None,
                password:     "hunter2".to_owned(),
                role:         Role::User,
            },
            "fake-hash".to_owned(),
        ).unwrap();

        let store = ChannelAccountStore::open(&path).unwrap();
        (dir, store, user.id)
    }

    fn signal_cfg(phone: &str) -> String {
        serde_json::json!({
            "phone_number": phone,
            "cli_binary":   "signal-cli",
            "data_dir":     "/tmp/sig"
        }).to_string()
    }

    fn tg_cfg(token: &str) -> String {
        serde_json::json!({ "bot_token": token, "mode": "webhook" }).to_string()
    }

    fn dc_cfg(token: &str) -> String {
        serde_json::json!({ "bot_token": token, "mention_only": false }).to_string()
    }

    fn mx_cfg(homeserver: &str, token: &str) -> String {
        serde_json::json!({
            "homeserver": homeserver, "access_token": token, "mention_only": false
        }).to_string()
    }

    fn wa_cfg(pnid: &str, token: &str) -> String {
        serde_json::json!({
            "phone_number_id": pnid, "access_token": token,
            "verify_token": "vt", "mention_only": false
        }).to_string()
    }

    fn sl_cfg(token: &str) -> String {
        serde_json::json!({
            "bot_token": token, "signing_secret": "ss", "mention_only": false
        }).to_string()
    }

    fn ext_cfg(kind: &str, send_url: &str, out_secret: &str) -> String {
        ext_cfg_v(kind, send_url, out_secret, false)
    }

    fn ext_cfg_v(kind: &str, send_url: &str, out_secret: &str, supports_voice: bool) -> String {
        serde_json::json!({
            "provider_kind": kind, "send_url": send_url,
            "inbound_secret": "in", "outbound_secret": out_secret,
            "mention_only": false, "supports_voice": supports_voice
        }).to_string()
    }

    /// Like `open_with_user` but also returns the `AuthDb` so a test can
    /// create extra users (e.g. a shared-bot owner + a separate recipient).
    fn open_with_db() -> (tempfile::TempDir, ChannelAccountStore, AuthDb) {
        let dir  = tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let auth = AuthDb::open(&path).unwrap();
        let store = ChannelAccountStore::open(&path).unwrap();
        (dir, store, auth)
    }

    fn mk_user(auth: &AuthDb, username: &str) -> String {
        auth.create_user(
            NewUser {
                username:     username.to_owned(),
                display_name: None,
                email:        None,
                password:     "pw".to_owned(),
                role:         Role::User,
            },
            "fake-hash".to_owned(),
        ).unwrap().id
    }

    #[test]
    fn create_and_fetch_round_trip() {
        let (_d, store, uid) = open_with_user();

        let created = store.create(NewChannelAccount {
            user_id:       uid.clone(),
            channel:       ChannelKind::Signal,
            account_label: "Personal".to_owned(),
            external_id:   Some("+15551234567".to_owned()),
            config_json:   signal_cfg("+15551234567"),
            enabled:       true,
            routing_mode:  Default::default(),

        }).unwrap();

        let fetched = store.get(&created.id).unwrap().unwrap();
        assert_eq!(fetched.user_id,       uid);
        assert_eq!(fetched.channel,       ChannelKind::Signal);
        assert_eq!(fetched.account_label, "Personal");
        assert_eq!(fetched.external_id.as_deref(), Some("+15551234567"));
        assert!(fetched.enabled);
    }

    #[test]
    fn list_for_user_scopes_to_owner() {
        let (_d, store, alice) = open_with_user();
        // Add a second user via the same DB so the FK on a sibling row resolves.
        let auth = AuthDb::open(&_d.path().join("auth.db")).unwrap();
        let bob  = auth.create_user(
            NewUser {
                username: "bob".to_owned(), display_name: None, email: None,
                password: "x".to_owned(), role: Role::User,
            },
            "fake-hash".to_owned(),
        ).unwrap();

        store.create(NewChannelAccount {
            user_id: alice.clone(), channel: ChannelKind::Signal,
            account_label: "p".to_owned(), external_id: Some("+1".to_owned()),
            config_json: signal_cfg("+1"), enabled: true,
            routing_mode:  Default::default(),

        }).unwrap();
        store.create(NewChannelAccount {
            user_id: bob.id.clone(), channel: ChannelKind::Signal,
            account_label: "p".to_owned(), external_id: Some("+2".to_owned()),
            config_json: signal_cfg("+2"), enabled: true,
            routing_mode:  Default::default(),

        }).unwrap();

        let alice_list = store.list_for_user(&alice).unwrap();
        assert_eq!(alice_list.len(), 1);
        assert_eq!(alice_list[0].user_id, alice);

        assert_eq!(store.list_all().unwrap().len(), 2);
    }

    #[test]
    fn unique_external_id_per_channel() {
        let (_d, store, uid) = open_with_user();

        store.create(NewChannelAccount {
            user_id:       uid.clone(),
            channel:       ChannelKind::Signal,
            account_label: "p".to_owned(),
            external_id:   Some("+15551234567".to_owned()),
            config_json:   signal_cfg("+15551234567"),
            enabled:       true,
            routing_mode:  Default::default(),

        }).unwrap();

        // Same number under a different label — must collide.
        let dup = store.create(NewChannelAccount {
            user_id:       uid.clone(),
            channel:       ChannelKind::Signal,
            account_label: "Work".to_owned(),
            external_id:   Some("+15551234567".to_owned()),
            config_json:   signal_cfg("+15551234567"),
            enabled:       true,
            routing_mode:  Default::default(),

        });
        assert!(dup.is_err(), "duplicate external_id must be rejected");
    }

    #[test]
    fn unique_label_per_user_and_channel() {
        let (_d, store, uid) = open_with_user();

        store.create(NewChannelAccount {
            user_id:       uid.clone(),
            channel:       ChannelKind::Signal,
            account_label: "Personal".to_owned(),
            external_id:   Some("+1".to_owned()),
            config_json:   signal_cfg("+1"),
            enabled:       true,
            routing_mode:  Default::default(),

        }).unwrap();

        // Same label under the same channel — must collide even with a
        // different external_id.
        let dup = store.create(NewChannelAccount {
            user_id:       uid.clone(),
            channel:       ChannelKind::Signal,
            account_label: "Personal".to_owned(),
            external_id:   Some("+2".to_owned()),
            config_json:   signal_cfg("+2"),
            enabled:       true,
            routing_mode:  Default::default(),

        });
        assert!(dup.is_err(), "duplicate label per (user, channel) must be rejected");

        // Same label under a *different* channel is allowed.
        let ok = store.create(NewChannelAccount {
            user_id:       uid,
            channel:       ChannelKind::Telegram,
            account_label: "Personal".to_owned(),
            external_id:   Some("@bot".to_owned()),
            config_json:   r#"{"bot_token":"xyz","mode":"webhook"}"#.to_owned(),
            enabled:       true,
            routing_mode:  Default::default(),
        });
        assert!(ok.is_ok());
    }

    #[test]
    fn update_persists_changes() {
        let (_d, store, uid) = open_with_user();
        let created = store.create(NewChannelAccount {
            user_id:       uid,
            channel:       ChannelKind::Signal,
            account_label: "Personal".to_owned(),
            external_id:   Some("+1".to_owned()),
            config_json:   signal_cfg("+1"),
            enabled:       true,
            routing_mode:  Default::default(),

        }).unwrap();

        let updated = store.update(&created.id, UpdateChannelAccount {
            account_label: Some("Renamed".to_owned()),
            enabled:       Some(false),
            ..Default::default()
        }).unwrap();

        assert_eq!(updated.account_label, "Renamed");
        assert!(!updated.enabled);

        let fetched = store.get(&created.id).unwrap().unwrap();
        assert_eq!(fetched.account_label, "Renamed");
        assert!(!fetched.enabled);
    }

    #[test]
    fn delete_removes_row() {
        let (_d, store, uid) = open_with_user();
        let created = store.create(NewChannelAccount {
            user_id:       uid,
            channel:       ChannelKind::Telegram,
            account_label: "p".to_owned(),
            external_id:   None,
            config_json:   r#"{"bot_token":"x","mode":"webhook"}"#.to_owned(),
            enabled:       true,
            routing_mode:  Default::default(),
        }).unwrap();

        store.delete(&created.id).unwrap();
        assert!(store.get(&created.id).unwrap().is_none());
    }

    #[test]
    fn list_enabled_filters_disabled() {
        let (_d, store, uid) = open_with_user();
        let on = store.create(NewChannelAccount {
            user_id: uid.clone(), channel: ChannelKind::Signal,
            account_label: "on".to_owned(), external_id: Some("+1".to_owned()),
            config_json: signal_cfg("+1"), enabled: true,
            routing_mode:  Default::default(),

        }).unwrap();
        let _off = store.create(NewChannelAccount {
            user_id: uid, channel: ChannelKind::Signal,
            account_label: "off".to_owned(), external_id: Some("+2".to_owned()),
            config_json: signal_cfg("+2"), enabled: false,
            routing_mode:  Default::default(),

        }).unwrap();

        let enabled = store.list_enabled().unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, on.id);
    }

    // ── R1+R2 shared-bot outbound resolution ──────────────────────────

    #[test]
    fn first_shared_bot_ignores_personal_and_disabled() {
        let (_d, store, auth) = open_with_db();
        let owner = mk_user(&auth, "owner");

        // A personal telegram bot — must NOT be returned.
        store.create(NewChannelAccount {
            user_id: owner.clone(), channel: ChannelKind::Telegram,
            account_label: "personal".into(), external_id: None,
            config_json: tg_cfg("PERSONAL"), enabled: true,
            routing_mode: RoutingMode::Personal,
        }).unwrap();
        assert!(store.first_shared_bot(ChannelKind::Telegram).unwrap().is_none());

        // A disabled shared bot — still must NOT be returned.
        store.create(NewChannelAccount {
            user_id: owner.clone(), channel: ChannelKind::Telegram,
            account_label: "shared-off".into(), external_id: None,
            config_json: tg_cfg("SHARED_OFF"), enabled: false,
            routing_mode: RoutingMode::Shared,
        }).unwrap();
        assert!(store.first_shared_bot(ChannelKind::Telegram).unwrap().is_none());

        // An enabled shared bot — now returned.
        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Telegram,
            account_label: "shared-on".into(), external_id: None,
            config_json: tg_cfg("SHARED_ON"), enabled: true,
            routing_mode: RoutingMode::Shared,
        }).unwrap();
        let got = store.first_shared_bot(ChannelKind::Telegram).unwrap().unwrap();
        assert_eq!(got.account_label, "shared-on");
        assert_eq!(got.telegram_config().unwrap().bot_token, "SHARED_ON");
    }

    #[test]
    fn first_shared_bot_accepts_guest_ok() {
        let (_d, store, auth) = open_with_db();
        let owner = mk_user(&auth, "owner");
        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Telegram,
            account_label: "guest".into(), external_id: None,
            config_json: tg_cfg("GUEST"), enabled: true,
            routing_mode: RoutingMode::GuestOk,
        }).unwrap();
        assert!(store.first_shared_bot(ChannelKind::Telegram).unwrap().is_some());
    }

    #[test]
    fn outbound_token_prefers_personal_over_shared() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient");

        // Shared admin bot owned by `owner`.
        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Telegram,
            account_label: "shared".into(), external_id: None,
            config_json: tg_cfg("SHARED"), enabled: true,
            routing_mode: RoutingMode::Shared,
        }).unwrap();
        // Recipient's OWN personal bot.
        store.create(NewChannelAccount {
            user_id: recipient.clone(), channel: ChannelKind::Telegram,
            account_label: "mine".into(), external_id: None,
            config_json: tg_cfg("MINE"), enabled: true,
            routing_mode: RoutingMode::Personal,
        }).unwrap();

        // Recipient has their own bot → use it, not the shared one.
        assert_eq!(
            store.outbound_telegram_token(&recipient).unwrap().as_deref(),
            Some("MINE"),
        );
    }

    #[test]
    fn outbound_token_falls_back_to_shared_for_linked_user() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient"); // owns no bot

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Telegram,
            account_label: "shared".into(), external_id: None,
            config_json: tg_cfg("SHARED"), enabled: true,
            routing_mode: RoutingMode::Shared,
        }).unwrap();

        // Recipient owns nothing → fall back to the shared bot's token.
        assert_eq!(
            store.outbound_telegram_token(&recipient).unwrap().as_deref(),
            Some("SHARED"),
        );
    }

    #[test]
    fn outbound_token_none_when_no_bot_anywhere() {
        let (_d, store, auth) = open_with_db();
        let recipient = mk_user(&auth, "recipient");
        assert!(store.outbound_telegram_token(&recipient).unwrap().is_none());
    }

    // ── D3 Discord outbound resolution (mirrors the telegram tests) ────

    #[test]
    fn outbound_discord_prefers_personal_over_shared() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient");

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Discord,
            account_label: "shared".into(), external_id: None,
            config_json: dc_cfg("SHARED"), enabled: true,
            routing_mode: RoutingMode::Shared,
        }).unwrap();
        store.create(NewChannelAccount {
            user_id: recipient.clone(), channel: ChannelKind::Discord,
            account_label: "mine".into(), external_id: None,
            config_json: dc_cfg("MINE"), enabled: true,
            routing_mode: RoutingMode::Personal,
        }).unwrap();

        assert_eq!(
            store.outbound_discord_token(&recipient).unwrap().as_deref(),
            Some("MINE"),
        );
    }

    #[test]
    fn outbound_discord_falls_back_to_shared_for_linked_user() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient"); // owns no bot

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Discord,
            account_label: "shared".into(), external_id: None,
            config_json: dc_cfg("SHARED"), enabled: true,
            routing_mode: RoutingMode::GuestOk,
        }).unwrap();

        assert_eq!(
            store.outbound_discord_token(&recipient).unwrap().as_deref(),
            Some("SHARED"),
        );
    }

    #[test]
    fn outbound_discord_none_when_no_bot_anywhere() {
        let (_d, store, auth) = open_with_db();
        let recipient = mk_user(&auth, "recipient");
        assert!(store.outbound_discord_token(&recipient).unwrap().is_none());
    }

    #[test]
    fn outbound_discord_ignores_telegram_bots() {
        // A recipient with only a Telegram bot must NOT resolve a Discord
        // token — channels are independent.
        let (_d, store, auth) = open_with_db();
        let recipient = mk_user(&auth, "recipient");
        store.create(NewChannelAccount {
            user_id: recipient.clone(), channel: ChannelKind::Telegram,
            account_label: "tg".into(), external_id: None,
            config_json: tg_cfg("TG"), enabled: true,
            routing_mode: RoutingMode::Personal,
        }).unwrap();
        assert!(store.outbound_discord_token(&recipient).unwrap().is_none());
    }

    // ── Matrix outbound resolution (returns (homeserver, token) pair) ──

    #[test]
    fn outbound_matrix_prefers_personal_over_shared() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient");

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Matrix,
            account_label: "shared".into(), external_id: None,
            config_json: mx_cfg("https://shared.hs", "SHARED"), enabled: true,
            routing_mode: RoutingMode::Shared,
        }).unwrap();
        store.create(NewChannelAccount {
            user_id: recipient.clone(), channel: ChannelKind::Matrix,
            account_label: "mine".into(), external_id: None,
            config_json: mx_cfg("https://mine.hs", "MINE"), enabled: true,
            routing_mode: RoutingMode::Personal,
        }).unwrap();

        let (hs, tok) = store.outbound_matrix_creds(&recipient).unwrap().unwrap();
        assert_eq!(hs, "https://mine.hs");
        assert_eq!(tok, "MINE");
    }

    #[test]
    fn outbound_matrix_falls_back_to_shared_for_linked_user() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient"); // owns no bot

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Matrix,
            account_label: "shared".into(), external_id: None,
            config_json: mx_cfg("https://shared.hs", "SHARED"), enabled: true,
            routing_mode: RoutingMode::GuestOk,
        }).unwrap();

        let (hs, tok) = store.outbound_matrix_creds(&recipient).unwrap().unwrap();
        assert_eq!(hs, "https://shared.hs");
        assert_eq!(tok, "SHARED");
    }

    #[test]
    fn outbound_matrix_none_when_no_bot_anywhere() {
        let (_d, store, auth) = open_with_db();
        let recipient = mk_user(&auth, "recipient");
        assert!(store.outbound_matrix_creds(&recipient).unwrap().is_none());
    }

    // ── WhatsApp outbound resolution ((phone_number_id, token) pair) ──

    #[test]
    fn outbound_whatsapp_prefers_personal_over_shared() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient");

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::WhatsApp,
            account_label: "shared".into(), external_id: None,
            config_json: wa_cfg("SHARED_PNID", "SHARED_TOK"), enabled: true,
            routing_mode: RoutingMode::Shared,
        }).unwrap();
        store.create(NewChannelAccount {
            user_id: recipient.clone(), channel: ChannelKind::WhatsApp,
            account_label: "mine".into(), external_id: None,
            config_json: wa_cfg("MINE_PNID", "MINE_TOK"), enabled: true,
            routing_mode: RoutingMode::Personal,
        }).unwrap();

        let (pnid, tok) = store.outbound_whatsapp_creds(&recipient).unwrap().unwrap();
        assert_eq!(pnid, "MINE_PNID");
        assert_eq!(tok, "MINE_TOK");
    }

    #[test]
    fn outbound_whatsapp_falls_back_to_shared_for_linked_user() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient"); // owns no bot

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::WhatsApp,
            account_label: "shared".into(), external_id: None,
            config_json: wa_cfg("SHARED_PNID", "SHARED_TOK"), enabled: true,
            routing_mode: RoutingMode::GuestOk,
        }).unwrap();

        let (pnid, tok) = store.outbound_whatsapp_creds(&recipient).unwrap().unwrap();
        assert_eq!(pnid, "SHARED_PNID");
        assert_eq!(tok, "SHARED_TOK");
    }

    #[test]
    fn outbound_whatsapp_none_when_no_bot_anywhere() {
        let (_d, store, auth) = open_with_db();
        let recipient = mk_user(&auth, "recipient");
        assert!(store.outbound_whatsapp_creds(&recipient).unwrap().is_none());
    }

    // ── Slack outbound resolution (single bot-token string) ──

    #[test]
    fn outbound_slack_prefers_personal_over_shared() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient");

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Slack,
            account_label: "shared".into(), external_id: None,
            config_json: sl_cfg("xoxb-SHARED"), enabled: true,
            routing_mode: RoutingMode::Shared,
        }).unwrap();
        store.create(NewChannelAccount {
            user_id: recipient.clone(), channel: ChannelKind::Slack,
            account_label: "mine".into(), external_id: None,
            config_json: sl_cfg("xoxb-MINE"), enabled: true,
            routing_mode: RoutingMode::Personal,
        }).unwrap();

        assert_eq!(
            store.outbound_slack_token(&recipient).unwrap().as_deref(),
            Some("xoxb-MINE"),
        );
    }

    #[test]
    fn outbound_slack_falls_back_to_shared_for_linked_user() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient"); // owns no bot

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::Slack,
            account_label: "shared".into(), external_id: None,
            config_json: sl_cfg("xoxb-SHARED"), enabled: true,
            routing_mode: RoutingMode::GuestOk,
        }).unwrap();

        assert_eq!(
            store.outbound_slack_token(&recipient).unwrap().as_deref(),
            Some("xoxb-SHARED"),
        );
    }

    #[test]
    fn outbound_slack_none_when_no_bot_anywhere() {
        let (_d, store, auth) = open_with_db();
        let recipient = mk_user(&auth, "recipient");
        assert!(store.outbound_slack_token(&recipient).unwrap().is_none());
    }

    // ── External (CPP) outbound resolution: (account_id, send_url, secret) ──

    #[test]
    fn outbound_external_prefers_personal_and_returns_account_id() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient");

        store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::External,
            account_label: "shared".into(), external_id: None,
            config_json: ext_cfg("nctalk", "https://shared/send", "SH"), enabled: true,
            routing_mode: RoutingMode::Shared,
        }).unwrap();
        let mine = store.create(NewChannelAccount {
            user_id: recipient.clone(), channel: ChannelKind::External,
            account_label: "mine".into(), external_id: None,
            config_json: ext_cfg_v("nctalk", "https://mine/send", "MY", true), enabled: true,
            routing_mode: RoutingMode::Personal,
        }).unwrap();

        let (acc, url, secret, voice) = store.outbound_external_creds(&recipient).unwrap().unwrap();
        assert_eq!(acc, mine.id);           // the recipient's own account
        assert_eq!(url, "https://mine/send");
        assert_eq!(secret, "MY");
        assert!(voice, "supports_voice must flow through the resolver");
    }

    #[test]
    fn outbound_external_falls_back_to_shared() {
        let (_d, store, auth) = open_with_db();
        let owner     = mk_user(&auth, "owner");
        let recipient = mk_user(&auth, "recipient"); // owns none

        let shared = store.create(NewChannelAccount {
            user_id: owner, channel: ChannelKind::External,
            account_label: "shared".into(), external_id: None,
            config_json: ext_cfg("nctalk", "https://shared/send", "SH"), enabled: true,
            routing_mode: RoutingMode::GuestOk,
        }).unwrap();

        let (acc, url, secret, voice) = store.outbound_external_creds(&recipient).unwrap().unwrap();
        assert_eq!(acc, shared.id);
        assert_eq!(url, "https://shared/send");
        assert_eq!(secret, "SH");
        assert!(!voice, "shared account left supports_voice off → false");
    }

    #[test]
    fn outbound_external_none_when_no_provider_anywhere() {
        let (_d, store, auth) = open_with_db();
        let recipient = mk_user(&auth, "recipient");
        assert!(store.outbound_external_creds(&recipient).unwrap().is_none());
    }
}
