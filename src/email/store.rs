// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/store.rs
//! SQLite-backed store for per-user email accounts (slice E1+E3,
//! chunk 1). Lives in `auth.db` next to `users`, `channel_accounts`
//! and `mcp_servers` — the FK on `user_id` cascades on user delete,
//! same posture as everything else in the per-user channel family.
//!
//! Credential storage: the secret credential fields — `imap_password`,
//! `smtp_password`, `oauth_access_token`, `oauth_refresh_token` — are
//! AES-256-GCM encrypted at rest under the instance master key (the same
//! `master.key` the calendar store and skill-secrets vault use), bound to
//! the account id as AAD. They are stored as the envelope
//! `enc:v1:<base64(nonce||ciphertext)>` in their existing TEXT columns and
//! decrypted transparently on read, so in-process consumers (poller,
//! sender, OAuth refresh) keep seeing plaintext. Legacy plaintext rows are
//! upgraded in place on open ([`EmailAccountStore::encrypt_legacy_plaintext`]).
//! The `webhook_secret` is intentionally *not* encrypted: it is a
//! capability token the user must read back to configure their provider's
//! webhook URL, and is matched against the inbound public webhook path.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng, Payload},
    Aes256Gcm, Key, Nonce,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::RngCore;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::MiraError;

/// Envelope prefix marking an AES-256-GCM-encrypted credential value. A
/// stored value without this prefix is treated as legacy plaintext.
const ENC_PREFIX: &str = "enc:v1:";

// ── Models ───────────────────────────────────────────────────────────────────

// One row in `email_accounts`. The full security-knob blob is held
// in `security_json` so the schema doesn't need a new column for
// every per-account toggle that lands across E3 — same pattern the
// MCP store uses with `config_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailAccountRow {
    pub id:        String,
    pub user_id:   String,
    pub label:     String,
    pub address:   String,
    pub auth_mode: String,            // "password" | "oauth_google" | "oauth_microsoft"

    // ── Password-auth fields (E1) ───────────────────────────────────
    pub imap_host:     Option<String>,
    pub imap_port:     Option<u16>,
    pub imap_use_tls:  bool,
    pub imap_username: Option<String>,
    pub imap_password: Option<String>,
    pub smtp_host:     Option<String>,
    pub smtp_port:     Option<u16>,
    pub smtp_use_tls:  bool,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,

    // ── OAuth fields (E4 — present but unused until that slice) ─────
    #[serde(default)]
    pub oauth_access_token:  Option<String>,
    #[serde(default)]
    pub oauth_refresh_token: Option<String>,
    #[serde(default)]
    pub oauth_expires_at:    Option<i64>,

    // ── Webhook fields (E6) — populated only when auth_mode=webhook ─
    // Provider format MIRA's webhook handler expects to receive:
    // `"postmark"`, `"resend"`, or `"mailgun"`. Drives the JSON
    // parser pick. `None` for non-webhook auth modes.
    #[serde(default)]
    pub webhook_provider: Option<String>,
    // Per-account random secret. The provider POSTs to
    // `/webhook/email/{account_id}/{webhook_secret}`; the secret
    // path segment is what authenticates the call (no JWT). Generated
    // at account creation when auth_mode="webhook".
    #[serde(default)]
    pub webhook_secret:   Option<String>,

    // ── Security overrides (E3) ─────────────────────────────────────
    // Per-account overrides for the system-wide email defaults plus
    // the allowlist/denylist. Parsed lazily via [`EmailSecurity::from_json`].
    pub security_json: String,

    // ── Lifecycle ───────────────────────────────────────────────────
    pub enabled:       bool,
    // IMAP UID watermark — anything ≤ this UID has already been
    // fetched. Advanced by the poller after each successful turn,
    // persisted so we don't re-process on restart.
    pub last_uid_seen: i64,
    pub created_at:    i64,
    pub updated_at:    i64,
}

impl EmailAccountRow {
    // Hydrate the parsed security blob; defaults when JSON is empty
    // or malformed so a botched edit can't lock the operator out of
    // the row.
    pub fn security(&self) -> EmailSecurity {
        EmailSecurity::from_json(&self.security_json).unwrap_or_default()
    }

    /// A copy safe to hand to API clients: secret credential fields are
    /// nulled so we never echo a password or OAuth token back over the
    /// wire (mirrors the calendar store, which never returns a CalDAV
    /// password). `webhook_secret` is deliberately retained — the user
    /// needs it to configure their provider's webhook URL.
    pub fn redacted(mut self) -> Self {
        self.imap_password       = None;
        self.smtp_password       = None;
        self.oauth_access_token  = None;
        self.oauth_refresh_token = None;
        self
    }
}

// Per-account security configuration. Lives inside
// `email_accounts.security_json` so we can extend it across E3
// chunks without DDL. Defaults are deliberately conservative — see
// `design-docs/email-channel.md` §6 for the rationale per field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailSecurity {
    // Exact emails or `*@domain` wildcards. Senders matching any
    // entry bypass the quarantine queue. Empty when nothing
    // allowed — combined with `accept_from_unknown_senders=false`,
    // every inbound is quarantined.
    #[serde(default)]
    pub allowed_senders: Vec<String>,

    // Hard-block list — checked before allowed_senders, so a
    // blanket allow can be punched through for known bad addresses.
    #[serde(default)]
    pub denied_senders: Vec<String>,

    // When false (default), senders not on the allowlist are
    // quarantined rather than processed.
    #[serde(default)]
    pub accept_from_unknown_senders: bool,

    // Per-account overrides; when None the system-wide default
    // applies. Resolution lives in chunk 3's security pipeline.
    #[serde(default)]
    pub accept_html:             Option<bool>,
    #[serde(default)]
    pub accept_inline_images:    Option<bool>,
    #[serde(default)]
    pub accept_attachments:      Option<bool>,
    #[serde(default)]
    pub max_message_size_kb:     Option<u32>,
    #[serde(default)]
    pub inbound_rate_per_sender_per_hour: Option<u32>,
    #[serde(default)]
    pub inbound_rate_per_account_per_day: Option<u32>,

    // The narrow tool allowlist applied to email-initiated turns.
    // Reuses the `TurnContext.allowed_tool_names` mechanism. Empty
    // = "use the system default subset" (set in chunk 4).
    #[serde(default)]
    pub allowed_tools_for_email_turn: Vec<String>,
}

impl Default for EmailSecurity {
    fn default() -> Self {
        Self {
            allowed_senders: Vec::new(),
            denied_senders:  Vec::new(),
            accept_from_unknown_senders: false,
            accept_html:             None,
            accept_inline_images:    None,
            accept_attachments:      None,
            max_message_size_kb:     None,
            inbound_rate_per_sender_per_hour: None,
            inbound_rate_per_account_per_day: None,
            allowed_tools_for_email_turn: Vec::new(),
        }
    }
}

impl EmailSecurity {
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        if s.trim().is_empty() { return Ok(Self::default()); }
        serde_json::from_str(s)
    }
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

// ── CRUD input types ────────────────────────────────────────────────────────

// POST body for `/api/email/accounts`. The `user_id` comes from the
// auth context, never the body — same posture as MCP CRUD. Password
// auth fields are required when `auth_mode = "password"`; OAuth
// fields are populated by the OAuth flow in E4 and never accepted
// raw from a client.
#[derive(Debug, Clone, Deserialize)]
pub struct NewEmailAccount {
    pub label:     String,
    pub address:   String,
    #[serde(default = "default_auth_mode")]
    pub auth_mode: String,

    #[serde(default)]
    pub imap_host:     Option<String>,
    #[serde(default)]
    pub imap_port:     Option<u16>,
    #[serde(default = "default_true")]
    pub imap_use_tls:  bool,
    #[serde(default)]
    pub imap_username: Option<String>,
    #[serde(default)]
    pub imap_password: Option<String>,
    #[serde(default)]
    pub smtp_host:     Option<String>,
    #[serde(default)]
    pub smtp_port:     Option<u16>,
    #[serde(default = "default_true")]
    pub smtp_use_tls:  bool,
    #[serde(default)]
    pub smtp_username: Option<String>,
    #[serde(default)]
    pub smtp_password: Option<String>,

    // E6 — provider format when `auth_mode = "webhook"`.
    // `"postmark"` / `"resend"` / `"mailgun"`. Ignored otherwise.
    #[serde(default)]
    pub webhook_provider: Option<String>,

    #[serde(default)]
    pub security: EmailSecurity,

    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool { true }
fn default_auth_mode() -> String { "password".to_string() }

// PUT body for `/api/email/accounts/{id}`. Every field is optional
// so the UI can patch one knob at a time. `Option<Option<String>>`
// distinguishes "leave alone" (outer None) from "clear" (inner
// None), matching the MCP store's convention.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UpdateEmailAccount {
    pub label:     Option<String>,
    pub address:   Option<String>,
    pub auth_mode: Option<String>,

    pub imap_host:     Option<Option<String>>,
    pub imap_port:     Option<Option<u16>>,
    pub imap_use_tls:  Option<bool>,
    pub imap_username: Option<Option<String>>,
    pub imap_password: Option<Option<String>>,
    pub smtp_host:     Option<Option<String>>,
    pub smtp_port:     Option<Option<u16>>,
    pub smtp_use_tls:  Option<bool>,
    pub smtp_username: Option<Option<String>>,
    pub smtp_password: Option<Option<String>>,

    pub security: Option<EmailSecurity>,
    pub enabled:  Option<bool>,
}

// ── Store ────────────────────────────────────────────────────────────────────

pub struct EmailAccountStore {
    conn: Arc<Mutex<Connection>>,
    /// AES-256-GCM under the instance master key — encrypts the secret
    /// credential fields at rest. Shares the same `master.key` as the
    /// calendar store and the skill-secrets vault.
    cipher: Aes256Gcm,
}

impl EmailAccountStore {
    // Open the store at `path` (typically `<data_dir>/auth.db`).
    // Creates `email_accounts` on first run. The `users` table must
    // already exist so the FK can resolve.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create email_accounts DB dir: {e}"))
            })?;
        }
        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open email_accounts DB: {e}"))
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS email_accounts (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                label           TEXT NOT NULL,
                address         TEXT NOT NULL,
                auth_mode       TEXT NOT NULL,
                imap_host       TEXT,
                imap_port       INTEGER,
                imap_use_tls    INTEGER NOT NULL DEFAULT 1,
                imap_username   TEXT,
                imap_password   TEXT,
                smtp_host       TEXT,
                smtp_port       INTEGER,
                smtp_use_tls    INTEGER NOT NULL DEFAULT 1,
                smtp_username   TEXT,
                smtp_password   TEXT,
                oauth_access_token  TEXT,
                oauth_refresh_token TEXT,
                oauth_expires_at    INTEGER,
                security_json   TEXT NOT NULL DEFAULT '{}',
                enabled         INTEGER NOT NULL DEFAULT 1,
                last_uid_seen   INTEGER NOT NULL DEFAULT 0,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
                UNIQUE(user_id, address)
            );
            CREATE INDEX IF NOT EXISTS idx_email_accounts_user
                ON email_accounts(user_id);
            CREATE INDEX IF NOT EXISTS idx_email_accounts_enabled
                ON email_accounts(enabled);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("email_accounts migration failed: {e}")))?;

        // E6 — additive columns for webhook inbound. Idempotent
        // ALTER TABLE pattern: SQLite returns a "duplicate column"
        // error if the column already exists; we eat that error
        // and continue. Anything else is real.
        for stmt in [
            "ALTER TABLE email_accounts ADD COLUMN webhook_provider TEXT",
            "ALTER TABLE email_accounts ADD COLUMN webhook_secret   TEXT",
        ] {
            if let Err(e) = conn.execute(stmt, []) {
                if !e.to_string().contains("duplicate column name") {
                    return Err(MiraError::DatabaseError(format!(
                        "email_accounts E6 migration ({stmt}): {e}"
                    )));
                }
            }
        }

        // Encrypt the secret credential fields under the instance master
        // key (sibling `master.key`, same one the calendar store and the
        // skill-secrets vault use).
        let key_path = path.parent().unwrap_or_else(|| Path::new(".")).join("master.key");
        let key = crate::skills::secrets::load_or_create_master_key(&key_path)
            .map_err(|e| MiraError::DatabaseError(format!("email master key: {e}")))?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));

        let store = Self { conn: Arc::new(Mutex::new(conn)), cipher };
        // One-time, idempotent upgrade of any rows written before
        // encryption-at-rest landed.
        store.encrypt_legacy_plaintext()?;
        Ok(store)
    }

    // ── Encryption-at-rest helpers ──────────────────────────────────────────────

    /// Encrypt a plaintext credential into the `enc:v1:` envelope, bound to
    /// `aad` (the account id) so a row copied to another account fails to
    /// decrypt.
    fn encrypt_field(&self, aad: &str, plain: &str) -> Result<String, MiraError> {
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let ct = self.cipher.encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload { msg: plain.as_bytes(), aad: aad.as_bytes() },
        ).map_err(|_| MiraError::DatabaseError("email credential encrypt failed".into()))?;
        let mut blob = Vec::with_capacity(12 + ct.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ct);
        Ok(format!("{ENC_PREFIX}{}", B64.encode(&blob)))
    }

    /// Decrypt an `enc:v1:` envelope; a value without the prefix is returned
    /// verbatim (legacy plaintext that predates the migration, or a value
    /// written out-of-band).
    fn decrypt_field(&self, aad: &str, stored: &str) -> Result<String, MiraError> {
        let Some(b64) = stored.strip_prefix(ENC_PREFIX) else {
            return Ok(stored.to_string());
        };
        let blob = B64.decode(b64)
            .map_err(|_| MiraError::DatabaseError("email credential base64 corrupt".into()))?;
        if blob.len() < 13 {
            return Err(MiraError::DatabaseError("email credential blob too short".into()));
        }
        let (nonce_bytes, ct) = blob.split_at(12);
        let plain = self.cipher.decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload { msg: ct, aad: aad.as_bytes() },
        ).map_err(|_| MiraError::DatabaseError("email credential decrypt failed".into()))?;
        String::from_utf8(plain)
            .map_err(|_| MiraError::DatabaseError("email credential not utf-8".into()))
    }

    /// Encrypt an optional credential field (None stays None).
    fn enc_opt(&self, aad: &str, v: &Option<String>) -> Result<Option<String>, MiraError> {
        match v {
            Some(s) => Ok(Some(self.encrypt_field(aad, s)?)),
            None    => Ok(None),
        }
    }

    /// Decrypt the secret fields of a freshly-read row in place (AAD = id).
    fn decrypt_row(&self, row: &mut EmailAccountRow) -> Result<(), MiraError> {
        let id = row.id.clone();
        for f in [
            &mut row.imap_password,
            &mut row.smtp_password,
            &mut row.oauth_access_token,
            &mut row.oauth_refresh_token,
        ] {
            if let Some(s) = f {
                *s = self.decrypt_field(&id, s)?;
            }
        }
        Ok(())
    }

    /// Idempotent one-time migration: encrypt any secret field still stored
    /// as plaintext (no `enc:v1:` prefix). Runs at open; a no-op once every
    /// row is enveloped.
    fn encrypt_legacy_plaintext(&self) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, imap_password, smtp_password, oauth_access_token, oauth_refresh_token
               FROM email_accounts",
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        type Row = (String, Option<String>, Option<String>, Option<String>, Option<String>);
        let rows: Vec<Row> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        drop(stmt);

        let is_plain = |v: &Option<String>| matches!(v, Some(s) if !s.starts_with(ENC_PREFIX));
        let enc_if_plain = |aad: &str, v: &Option<String>| -> Result<Option<String>, MiraError> {
            match v {
                Some(s) if !s.starts_with(ENC_PREFIX) => Ok(Some(self.encrypt_field(aad, s)?)),
                other => Ok(other.clone()),
            }
        };

        let mut count = 0usize;
        for (id, imap_pw, smtp_pw, oauth_at, oauth_rt) in &rows {
            if !(is_plain(imap_pw) || is_plain(smtp_pw) || is_plain(oauth_at) || is_plain(oauth_rt)) {
                continue;
            }
            conn.execute(
                "UPDATE email_accounts SET
                    imap_password       = ?1,
                    smtp_password       = ?2,
                    oauth_access_token  = ?3,
                    oauth_refresh_token = ?4
                  WHERE id = ?5",
                params![
                    enc_if_plain(id, imap_pw)?,
                    enc_if_plain(id, smtp_pw)?,
                    enc_if_plain(id, oauth_at)?,
                    enc_if_plain(id, oauth_rt)?,
                    id,
                ],
            ).map_err(|e| MiraError::DatabaseError(format!("encrypt legacy email creds: {e}")))?;
            count += 1;
        }
        if count > 0 {
            tracing::info!("email store: encrypted credentials at rest for {count} legacy account(s)");
        }
        Ok(())
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    // ── Create ────────────────────────────────────────────────────────────────

    pub fn create(&self, user_id: &str, new: NewEmailAccount) -> Result<EmailAccountRow, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let security_json = new.security.to_json()
            .map_err(|e| MiraError::ConfigError(format!("serialize EmailSecurity: {e}")))?;

        // E6 — auto-generate the webhook secret on creation when
        // auth_mode=webhook. UUID-v4 hex (no dashes) gives 32 chars
        // of entropy — fine for a path-segment secret.
        let webhook_secret: Option<String> = if new.auth_mode == "webhook" {
            Some(Uuid::new_v4().simple().to_string())
        } else {
            None
        };
        let webhook_provider = if new.auth_mode == "webhook" {
            new.webhook_provider.clone()
        } else {
            None
        };

        // Encrypt the password fields for storage (AAD = account id). The
        // returned row keeps the plaintext the caller passed in.
        let enc_imap_pw = self.enc_opt(&id, &new.imap_password)?;
        let enc_smtp_pw = self.enc_opt(&id, &new.smtp_password)?;

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO email_accounts
               (id, user_id, label, address, auth_mode,
                imap_host, imap_port, imap_use_tls, imap_username, imap_password,
                smtp_host, smtp_port, smtp_use_tls, smtp_username, smtp_password,
                webhook_provider, webhook_secret,
                security_json, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5,
                     ?6, ?7, ?8, ?9, ?10,
                     ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17,
                     ?18, ?19, ?20, ?20)",
            params![
                id, user_id, new.label, new.address, new.auth_mode,
                new.imap_host, new.imap_port, new.imap_use_tls as i64,
                new.imap_username, enc_imap_pw,
                new.smtp_host, new.smtp_port, new.smtp_use_tls as i64,
                new.smtp_username, enc_smtp_pw,
                webhook_provider, webhook_secret,
                security_json, new.enabled as i64, now,
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("create email_account: {e}")))?;

        Ok(EmailAccountRow {
            id,
            user_id:       user_id.to_owned(),
            label:         new.label,
            address:       new.address,
            auth_mode:     new.auth_mode,
            imap_host:     new.imap_host,
            imap_port:     new.imap_port,
            imap_use_tls:  new.imap_use_tls,
            imap_username: new.imap_username,
            imap_password: new.imap_password,
            smtp_host:     new.smtp_host,
            smtp_port:     new.smtp_port,
            smtp_use_tls:  new.smtp_use_tls,
            smtp_username: new.smtp_username,
            smtp_password: new.smtp_password,
            oauth_access_token:  None,
            oauth_refresh_token: None,
            oauth_expires_at:    None,
            webhook_provider,
            webhook_secret,
            security_json,
            enabled:       new.enabled,
            last_uid_seen: 0,
            created_at:    now,
            updated_at:    now,
        })
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    pub fn get(&self, id: &str) -> Result<Option<EmailAccountRow>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            SELECT_COLS_AND_FROM,
            params![id],
            row_to_account,
        );
        match r {
            Ok(mut a) => { self.decrypt_row(&mut a)?; Ok(Some(a)) }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn list_for_user(&self, user_id: &str) -> Result<Vec<EmailAccountRow>, MiraError> {
        self.query_list(LIST_FOR_USER_SQL, params![user_id])
    }

    pub fn list_all_enabled(&self) -> Result<Vec<EmailAccountRow>, MiraError> {
        self.query_list(LIST_ALL_ENABLED_SQL, params![])
    }

    fn query_list(
        &self,
        sql: &str,
        p:   impl rusqlite::Params,
    ) -> Result<Vec<EmailAccountRow>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map(p, row_to_account)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            let mut row = r.map_err(|e| MiraError::DatabaseError(e.to_string()))?;
            self.decrypt_row(&mut row)?;
            out.push(row);
        }
        Ok(out)
    }

    // ── Update / delete ───────────────────────────────────────────────────────

    pub fn update(&self, id: &str, upd: UpdateEmailAccount) -> Result<EmailAccountRow, MiraError> {
        let mut row = self.get(id)?
            .ok_or_else(|| MiraError::NotFound(format!("email_account not found: {id}")))?;

        if let Some(v) = upd.label     { row.label = v; }
        if let Some(v) = upd.address   { row.address = v; }
        if let Some(v) = upd.auth_mode { row.auth_mode = v; }
        if let Some(v) = upd.imap_host     { row.imap_host = v; }
        if let Some(v) = upd.imap_port     { row.imap_port = v; }
        if let Some(v) = upd.imap_use_tls  { row.imap_use_tls = v; }
        if let Some(v) = upd.imap_username { row.imap_username = v; }
        if let Some(v) = upd.imap_password { row.imap_password = v; }
        if let Some(v) = upd.smtp_host     { row.smtp_host = v; }
        if let Some(v) = upd.smtp_port     { row.smtp_port = v; }
        if let Some(v) = upd.smtp_use_tls  { row.smtp_use_tls = v; }
        if let Some(v) = upd.smtp_username { row.smtp_username = v; }
        if let Some(v) = upd.smtp_password { row.smtp_password = v; }
        if let Some(v) = upd.security      {
            row.security_json = v.to_json()
                .map_err(|e| MiraError::ConfigError(format!("serialize EmailSecurity: {e}")))?;
        }
        if let Some(v) = upd.enabled { row.enabled = v; }

        // Re-encrypt the password fields for storage (AAD = account id);
        // `row` still carries plaintext, which is what we return.
        let enc_imap_pw = self.enc_opt(id, &row.imap_password)?;
        let enc_smtp_pw = self.enc_opt(id, &row.smtp_password)?;

        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE email_accounts SET
                label=?1, address=?2, auth_mode=?3,
                imap_host=?4, imap_port=?5, imap_use_tls=?6, imap_username=?7, imap_password=?8,
                smtp_host=?9, smtp_port=?10, smtp_use_tls=?11, smtp_username=?12, smtp_password=?13,
                security_json=?14, enabled=?15, updated_at=?16
              WHERE id=?17",
            params![
                row.label, row.address, row.auth_mode,
                row.imap_host, row.imap_port, row.imap_use_tls as i64,
                row.imap_username, enc_imap_pw,
                row.smtp_host, row.smtp_port, row.smtp_use_tls as i64,
                row.smtp_username, enc_smtp_pw,
                row.security_json, row.enabled as i64, now, id,
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("update email_account: {e}")))?;
        if n == 0 {
            return Err(MiraError::NotFound(format!("email_account not found: {id}")));
        }
        row.updated_at = now;
        Ok(row)
    }

    pub fn delete(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM email_accounts WHERE id=?1", params![id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if n == 0 {
            return Err(MiraError::NotFound(format!("email_account not found: {id}")));
        }
        Ok(())
    }

    // E4 — write OAuth tokens onto a row after a successful
    // authorize/refresh exchange. Targeted update — leaves IMAP/
    // SMTP password fields, security_json, and unrelated flags
    // alone. Also stamps `auth_mode` so the IMAP poller picks
    // the XOAUTH2 path next cycle.
    pub fn set_oauth_tokens(
        &self,
        id:             &str,
        auth_mode:      &str,
        access_token:   &str,
        refresh_token:  Option<&str>,
        expires_at_ms:  Option<i64>,
    ) -> Result<(), MiraError> {
        // Encrypt the tokens for storage (AAD = account id).
        let enc_access = self.encrypt_field(id, access_token)?;
        let enc_refresh = match refresh_token {
            Some(rt) => Some(self.encrypt_field(id, rt)?),
            None     => None,
        };

        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        // Two-form UPDATE: keep the existing refresh_token when the
        // refresh response didn't include a new one (Google never
        // rotates; Microsoft sometimes does).
        let n = if let Some(rt) = enc_refresh {
            conn.execute(
                "UPDATE email_accounts SET
                    auth_mode = ?1,
                    oauth_access_token  = ?2,
                    oauth_refresh_token = ?3,
                    oauth_expires_at    = ?4,
                    updated_at          = ?5
                  WHERE id = ?6",
                params![auth_mode, enc_access, rt, expires_at_ms, now, id],
            )
            .map_err(|e| MiraError::DatabaseError(format!("set_oauth_tokens: {e}")))?
        } else {
            conn.execute(
                "UPDATE email_accounts SET
                    auth_mode = ?1,
                    oauth_access_token = ?2,
                    oauth_expires_at   = ?3,
                    updated_at         = ?4
                  WHERE id = ?5",
                params![auth_mode, enc_access, expires_at_ms, now, id],
            )
            .map_err(|e| MiraError::DatabaseError(format!("set_oauth_tokens: {e}")))?
        };
        if n == 0 {
            return Err(MiraError::NotFound(format!("email_account not found: {id}")));
        }
        Ok(())
    }

    // Advance the IMAP UID watermark after a successful poll cycle.
    // Idempotent; only writes when `new_uid > last_uid_seen` so a
    // retry can't rewind.
    pub fn advance_uid(&self, id: &str, new_uid: i64) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE email_accounts
                SET last_uid_seen = MAX(last_uid_seen, ?1), updated_at = ?2
              WHERE id = ?3",
            params![new_uid, Self::now_ms(), id],
        )
        .map_err(|e| MiraError::DatabaseError(format!("advance_uid: {e}")))?;
        Ok(())
    }
}

// ── SQL constants ────────────────────────────────────────────────────────────

const SELECT_COLS: &str = "id, user_id, label, address, auth_mode,
    imap_host, imap_port, imap_use_tls, imap_username, imap_password,
    smtp_host, smtp_port, smtp_use_tls, smtp_username, smtp_password,
    oauth_access_token, oauth_refresh_token, oauth_expires_at,
    webhook_provider, webhook_secret,
    security_json, enabled, last_uid_seen, created_at, updated_at";

const SELECT_COLS_AND_FROM: &str = "SELECT id, user_id, label, address, auth_mode,
    imap_host, imap_port, imap_use_tls, imap_username, imap_password,
    smtp_host, smtp_port, smtp_use_tls, smtp_username, smtp_password,
    oauth_access_token, oauth_refresh_token, oauth_expires_at,
    webhook_provider, webhook_secret,
    security_json, enabled, last_uid_seen, created_at, updated_at
    FROM email_accounts WHERE id = ?1";

const LIST_FOR_USER_SQL: &str = "SELECT id, user_id, label, address, auth_mode,
    imap_host, imap_port, imap_use_tls, imap_username, imap_password,
    smtp_host, smtp_port, smtp_use_tls, smtp_username, smtp_password,
    oauth_access_token, oauth_refresh_token, oauth_expires_at,
    webhook_provider, webhook_secret,
    security_json, enabled, last_uid_seen, created_at, updated_at
    FROM email_accounts WHERE user_id = ?1 ORDER BY created_at ASC";

const LIST_ALL_ENABLED_SQL: &str = "SELECT id, user_id, label, address, auth_mode,
    imap_host, imap_port, imap_use_tls, imap_username, imap_password,
    smtp_host, smtp_port, smtp_use_tls, smtp_username, smtp_password,
    oauth_access_token, oauth_refresh_token, oauth_expires_at,
    webhook_provider, webhook_secret,
    security_json, enabled, last_uid_seen, created_at, updated_at
    FROM email_accounts WHERE enabled = 1 ORDER BY user_id, created_at ASC";

// Silence unused-const warning if a future refactor stops using it.
#[allow(dead_code)]
const _SELECT_COLS_STILL_REFERENCED_FOR_GREP: &str = SELECT_COLS;

fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<EmailAccountRow> {
    Ok(EmailAccountRow {
        id:            row.get(0)?,
        user_id:       row.get(1)?,
        label:         row.get(2)?,
        address:       row.get(3)?,
        auth_mode:     row.get(4)?,
        imap_host:     row.get(5)?,
        imap_port:     row.get::<_, Option<i64>>(6)?.map(|n| n as u16),
        imap_use_tls:  row.get::<_, i64>(7)? != 0,
        imap_username: row.get(8)?,
        imap_password: row.get(9)?,
        smtp_host:     row.get(10)?,
        smtp_port:     row.get::<_, Option<i64>>(11)?.map(|n| n as u16),
        smtp_use_tls:  row.get::<_, i64>(12)? != 0,
        smtp_username: row.get(13)?,
        smtp_password: row.get(14)?,
        oauth_access_token:  row.get(15)?,
        oauth_refresh_token: row.get(16)?,
        oauth_expires_at:    row.get(17)?,
        webhook_provider:    row.get(18)?,
        webhook_secret:      row.get(19)?,
        security_json: row.get(20)?,
        enabled:       row.get::<_, i64>(21)? != 0,
        last_uid_seen: row.get(22)?,
        created_at:    row.get(23)?,
        updated_at:    row.get(24)?,
    })
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // A db file with the minimal `users` table the FK needs, plus one user.
    // The store's `open` puts `master.key` alongside it in the temp dir.
    fn db_with_user() -> (std::path::PathBuf, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id TEXT PRIMARY KEY);
             INSERT INTO users (id) VALUES ('u1');",
        ).unwrap();
        (path, dir)
    }

    fn password_account(label: &str, address: &str) -> NewEmailAccount {
        NewEmailAccount {
            label:         label.to_string(),
            address:       address.to_string(),
            auth_mode:     "password".to_string(),
            imap_host:     Some("imap.example.com".to_string()),
            imap_port:     Some(993),
            imap_use_tls:  true,
            imap_username: Some("u".to_string()),
            imap_password: Some("imap-secret".to_string()),
            smtp_host:     Some("smtp.example.com".to_string()),
            smtp_port:     Some(465),
            smtp_use_tls:  true,
            smtp_username: Some("u".to_string()),
            smtp_password: Some("smtp-secret".to_string()),
            webhook_provider: None,
            security:      EmailSecurity::default(),
            enabled:       true,
        }
    }

    // Read a column's raw stored bytes, bypassing the store's decryption.
    fn raw_col(path: &std::path::Path, id: &str, col: &str) -> Option<String> {
        let conn = Connection::open(path).unwrap();
        conn.query_row(
            &format!("SELECT {col} FROM email_accounts WHERE id = ?1"),
            params![id], |r| r.get::<_, Option<String>>(0),
        ).unwrap()
    }

    #[test]
    fn passwords_encrypted_at_rest_decrypt_on_read() {
        let (path, _dir) = db_with_user();
        let store = EmailAccountStore::open(&path).unwrap();

        // create() returns the plaintext the caller passed in.
        let created = store.create("u1", password_account("Work", "me@example.com")).unwrap();
        assert_eq!(created.imap_password.as_deref(), Some("imap-secret"));
        assert_eq!(created.smtp_password.as_deref(), Some("smtp-secret"));

        // The DB column is an encrypted envelope, not the plaintext.
        let raw_imap = raw_col(&path, &created.id, "imap_password").unwrap();
        assert!(raw_imap.starts_with(ENC_PREFIX), "stored value should be enc-enveloped");
        assert!(!raw_imap.contains("imap-secret"));

        // get()/list() transparently decrypt.
        let got = store.get(&created.id).unwrap().unwrap();
        assert_eq!(got.imap_password.as_deref(), Some("imap-secret"));
        assert_eq!(got.smtp_password.as_deref(), Some("smtp-secret"));
        let listed = store.list_for_user("u1").unwrap();
        assert_eq!(listed[0].imap_password.as_deref(), Some("imap-secret"));
    }

    #[test]
    fn update_re_encrypts_and_oauth_tokens_encrypted() {
        let (path, _dir) = db_with_user();
        let store = EmailAccountStore::open(&path).unwrap();
        let created = store.create("u1", password_account("Work", "me@example.com")).unwrap();

        // Rotate the IMAP password.
        let mut upd = UpdateEmailAccount::default();
        upd.imap_password = Some(Some("new-imap".to_string()));
        let updated = store.update(&created.id, upd).unwrap();
        assert_eq!(updated.imap_password.as_deref(), Some("new-imap"));
        let raw = raw_col(&path, &created.id, "imap_password").unwrap();
        assert!(raw.starts_with(ENC_PREFIX) && !raw.contains("new-imap"));
        assert_eq!(store.get(&created.id).unwrap().unwrap().imap_password.as_deref(), Some("new-imap"));

        // OAuth tokens land encrypted, decrypt on read.
        store.set_oauth_tokens(&created.id, "oauth_google", "at-123", Some("rt-456"), Some(9_999)).unwrap();
        let raw_at = raw_col(&path, &created.id, "oauth_access_token").unwrap();
        assert!(raw_at.starts_with(ENC_PREFIX) && !raw_at.contains("at-123"));
        let got = store.get(&created.id).unwrap().unwrap();
        assert_eq!(got.oauth_access_token.as_deref(), Some("at-123"));
        assert_eq!(got.oauth_refresh_token.as_deref(), Some("rt-456"));
    }

    #[test]
    fn legacy_plaintext_rows_upgraded_on_open() {
        let (path, _dir) = db_with_user();
        // First open creates the schema; insert a plaintext row out-of-band,
        // then a second open runs the migration.
        { let _ = EmailAccountStore::open(&path).unwrap(); }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO email_accounts
                   (id, user_id, label, address, auth_mode, imap_password, smtp_password,
                    security_json, enabled, last_uid_seen, created_at, updated_at)
                 VALUES ('acc1','u1','L','a@b','password','plain-imap','plain-smtp','{}',1,0,0,0)",
                [],
            ).unwrap();
        }
        let store = EmailAccountStore::open(&path).unwrap(); // triggers migration
        let raw = raw_col(&path, "acc1", "imap_password").unwrap();
        assert!(raw.starts_with(ENC_PREFIX), "legacy plaintext should be encrypted after open");
        let got = store.get("acc1").unwrap().unwrap();
        assert_eq!(got.imap_password.as_deref(), Some("plain-imap"));
        assert_eq!(got.smtp_password.as_deref(), Some("plain-smtp"));

        // Migration is idempotent: a third open is a no-op (still decrypts).
        let store2 = EmailAccountStore::open(&path).unwrap();
        assert_eq!(store2.get("acc1").unwrap().unwrap().imap_password.as_deref(), Some("plain-imap"));
    }

    #[test]
    fn crypto_helpers_bind_aad_and_pass_through_plaintext() {
        let (path, _dir) = db_with_user();
        let store = EmailAccountStore::open(&path).unwrap();
        let enc = store.encrypt_field("acc1", "secret").unwrap();
        assert!(enc.starts_with(ENC_PREFIX));
        assert_eq!(store.decrypt_field("acc1", &enc).unwrap(), "secret");
        // Wrong AAD (row copied to another account) fails to decrypt.
        assert!(store.decrypt_field("other", &enc).is_err());
        // A value without the envelope prefix passes through unchanged.
        assert_eq!(store.decrypt_field("acc1", "legacy-plain").unwrap(), "legacy-plain");
    }

    #[test]
    fn redacted_strips_secrets_but_keeps_webhook_secret() {
        let mut row = EmailAccountRow {
            id: "x".into(), user_id: "u1".into(), label: "L".into(), address: "a@b".into(),
            auth_mode: "password".into(),
            imap_host: None, imap_port: None, imap_use_tls: true, imap_username: None,
            imap_password: Some("p".into()),
            smtp_host: None, smtp_port: None, smtp_use_tls: true, smtp_username: None,
            smtp_password: Some("p".into()),
            oauth_access_token: Some("at".into()), oauth_refresh_token: Some("rt".into()),
            oauth_expires_at: None,
            webhook_provider: Some("postmark".into()), webhook_secret: Some("wh-secret".into()),
            security_json: "{}".into(), enabled: true, last_uid_seen: 0,
            created_at: 0, updated_at: 0,
        };
        row = row.redacted();
        assert!(row.imap_password.is_none());
        assert!(row.smtp_password.is_none());
        assert!(row.oauth_access_token.is_none());
        assert!(row.oauth_refresh_token.is_none());
        // Webhook secret is retained — the user needs it to wire up their provider.
        assert_eq!(row.webhook_secret.as_deref(), Some("wh-secret"));
    }
}
