// SPDX-License-Identifier: AGPL-3.0-or-later

// src/mcp/store.rs
//! SQLite-backed store for per-user MCP server entries.
//!
//! Lives in `auth.db` alongside `users` and `channel_accounts` — the
//! FK on `user_id` cascades on user delete, same posture as channel
//! accounts. The store replaces the old `config.mcp.servers` array
//! as the source of truth at runtime; `legacy_migrate` seeds the
//! admin user's rows from the config block on first boot so existing
//! deployments don't lose their entries.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::{aead::{Aead, KeyInit, OsRng, Payload}, Aes256Gcm, Key, Nonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::RngCore;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::MiraError;
use crate::config::McpServerConfig;

const ENC_PREFIX: &str = "enc:v1:";

// ── Models ───────────────────────────────────────────────────────────────────

// One row in `mcp_servers`. The runtime fields (`command`, `args`,
// `env`, `url`) are serialised into `config_json` to keep the schema
// flat — the registry deserialises back into a [`McpServerConfig`]
// at connect time. `transport` is hoisted so it can be queried/sorted
// without parsing JSON.
#[derive(Clone, Serialize, Deserialize)]
pub struct McpServerRow {
    pub id:         String,
    pub user_id:    String,
    pub name:       String,
    pub transport:  String,
    pub enabled:    bool,
    pub config_json: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl fmt::Debug for McpServerRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerRow")
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("enabled", &self.enabled)
            .field("config_json", &redacted_config_json(&self.config_json))
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerResponse {
    pub id:         String,
    pub user_id:    String,
    pub name:       String,
    pub transport:  String,
    pub enabled:    bool,
    pub config_json: String,
    pub auth_token_configured: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl McpServerRow {
    pub fn response(&self) -> McpServerResponse {
        let token_configured = serde_json::from_str::<serde_json::Value>(&self.config_json)
            .ok()
            .and_then(|v| v.get("auth_token").and_then(|v| v.as_str()).map(|v| !v.is_empty()))
            .unwrap_or(false);
        McpServerResponse {
            id: self.id.clone(), user_id: self.user_id.clone(), name: self.name.clone(),
            transport: self.transport.clone(), enabled: self.enabled,
            config_json: redacted_config_json(&self.config_json),
            auth_token_configured: token_configured,
            created_at: self.created_at, updated_at: self.updated_at,
        }
    }

    #[cfg(test)]
    pub fn from_config_for_test(config: McpServerConfig) -> Self {
        let config_json = serde_json::to_string(&config).unwrap();
        Self {
            id: "test-id".to_owned(), user_id: "test-user".to_owned(),
            name: config.name, transport: config.transport, enabled: config.enabled,
            config_json, created_at: 0, updated_at: 0,
        }
    }

    // Hydrate back into the [`McpServerConfig`] the registry knows
    // how to connect with. The owner's `user_id` is **not** part of
    // the config — it's tracked separately by the registry so a
    // single adapter set can serve multiple users' tools.
    fn to_config_with_token(&self, token: Option<String>) -> Result<McpServerConfig, MiraError> {
        // The stored JSON is the result of serialising an
        // McpServerConfig, so the round-trip is symmetric. Any
        // failure here is a corrupted row — surface clearly.
        let mut cfg: McpServerConfig = serde_json::from_str(&self.config_json)
            .map_err(|e| MiraError::ConfigError(format!(
                "mcp_servers row {}: bad config_json: {e}", self.id
            )))?;
        // Authoritative copies — the row's columns are canonical and
        // the JSON blob is a convenience.
        cfg.name      = self.name.clone();
        cfg.transport = self.transport.clone();
        cfg.enabled   = self.enabled;
        cfg.auth_token = token;
        Ok(cfg)
    }
}

// CRUD input — same shape as [`McpServerConfig`] minus the
// computed/server-owned fields (id, timestamps). `user_id` is taken
// from the authenticated caller, never accepted from the request
// body, so an account can't create rows owned by someone else.
#[derive(Clone, Deserialize)]
pub struct NewMcpServer {
    pub name:      String,
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default)]
    pub command:   Option<String>,
    #[serde(default)]
    pub args:      Vec<String>,
    #[serde(default)]
    pub env:       HashMap<String, String>,
    #[serde(default)]
    pub url:       Option<String>,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled:   bool,
    // opt-in to sampling. Defaults false so a freshly-
    // created row never accepts server-initiated LLM calls until
    // the operator explicitly enables it.
    #[serde(default)]
    pub sampling_enabled: bool,
}

impl fmt::Debug for NewMcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env = self.env.keys()
            .map(|key| (key, "***"))
            .collect::<HashMap<_, _>>();
        f.debug_struct("NewMcpServer")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &env)
            .field("url", &self.url)
            .field("auth_token_configured", &self.auth_token.as_ref().is_some_and(|v| !v.is_empty()))
            .field("enabled", &self.enabled)
            .field("sampling_enabled", &self.sampling_enabled)
            .finish()
    }
}

fn default_transport() -> String { "stdio".to_string() }
fn default_enabled() -> bool { true }

#[derive(Clone, Deserialize, Default)]
pub struct UpdateMcpServer {
    pub name:      Option<String>,
    pub transport: Option<String>,
    pub command:   Option<Option<String>>,
    pub args:      Option<Vec<String>>,
    pub env:       Option<HashMap<String, String>>,
    /// Explicitly removes these environment keys. This is separate from
    /// `env` so omitted/redacted values keep the existing secret.
    #[serde(default)]
    pub remove_env_keys: Vec<String>,
    /// Explicit values, including the literal `***`. This is intentionally
    /// separate from `env`, where `***` retains a redacted existing value.
    #[serde(default)]
    pub set_env: HashMap<String, String>,
    pub url:       Option<Option<String>>,
    /// Empty or absent means retain the existing token. A non-empty value
    /// replaces it. `clear_auth_token` takes precedence over a new token.
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub clear_auth_token: bool,
    pub enabled:   Option<bool>,
    pub sampling_enabled: Option<bool>,
}

// ── Store ────────────────────────────────────────────────────────────────────

pub struct McpServerStore {
    conn: Arc<Mutex<Connection>>,
    cipher: Aes256Gcm,
}

impl McpServerStore {
    // Open the store at `path` (typically `<data_dir>/auth.db`).
    // Creates `mcp_servers` on first run. The `users` table must
    // already exist so the FK can resolve.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create mcp_servers DB dir: {e}"))
            })?;
        }
        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open mcp_servers DB: {e}"))
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS mcp_servers (
                id          TEXT PRIMARY KEY,
                user_id     TEXT NOT NULL,
                name        TEXT NOT NULL,
                transport   TEXT NOT NULL,
                enabled     INTEGER NOT NULL DEFAULT 1,
                config_json TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
                UNIQUE(user_id, name)
            );
            CREATE INDEX IF NOT EXISTS idx_mcp_servers_user
                ON mcp_servers(user_id);
            CREATE INDEX IF NOT EXISTS idx_mcp_servers_enabled
                ON mcp_servers(enabled);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("mcp_servers migration failed: {e}")))?;

        let key_path = path.parent().unwrap_or_else(|| Path::new(".")).join("master.key");
        let key = crate::skills::secrets::load_or_create_master_key(&key_path)
            .map_err(|e| MiraError::DatabaseError(format!("mcp master key: {e}")))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
        };
        store.encrypt_legacy_plaintext()?;
        Ok(store)
    }

    fn encrypt_field(&self, aad: &str, plain: &str) -> Result<String, MiraError> {
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let ciphertext = self.cipher.encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload { msg: plain.as_bytes(), aad: aad.as_bytes() },
        ).map_err(|_| MiraError::DatabaseError("mcp token encrypt failed".into()))?;
        let mut blob = Vec::with_capacity(12 + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(format!("{ENC_PREFIX}{}", B64.encode(blob)))
    }

    fn decrypt_field(&self, aad: &str, stored: &str) -> Result<String, MiraError> {
        let Some(encoded) = stored.strip_prefix(ENC_PREFIX) else {
            return Ok(stored.to_owned());
        };
        let blob = B64.decode(encoded)
            .map_err(|_| MiraError::DatabaseError("mcp token base64 corrupt".into()))?;
        if blob.len() < 13 {
            return Err(MiraError::DatabaseError("mcp token blob too short".into()));
        }
        let (nonce, ciphertext) = blob.split_at(12);
        let plaintext = self.cipher.decrypt(
            Nonce::from_slice(nonce),
            Payload { msg: ciphertext, aad: aad.as_bytes() },
        ).map_err(|_| MiraError::DatabaseError("mcp token decrypt failed".into()))?;
        String::from_utf8(plaintext)
            .map_err(|_| MiraError::DatabaseError("mcp token not utf-8".into()))
    }

    // AAD binding a single env value to its row + key, so an encrypted value
    // can't be swapped between keys or between servers.
    fn env_aad(id: &str, key: &str) -> String { format!("{id}:env:{key}") }

    fn config_json(&self, id: &str, cfg: &McpServerConfig) -> Result<String, MiraError> {
        let mut value = serde_json::to_value(cfg)
            .map_err(|e| MiraError::ConfigError(format!("serialize McpServerConfig: {e}")))?;
        if let Some(token) = cfg.auth_token.as_deref() {
            value["auth_token"] = serde_json::Value::String(self.encrypt_field(id, token)?);
        }
        // Encrypt env values at rest too — they're frequently API keys. Bound to
        // (row id, key) via AAD. Idempotent: an already-encrypted value is left
        // as-is (cfg env is plaintext on the normal write path, so this guard
        // only matters for a value that round-tripped undecrypted).
        if let Some(env) = value.get_mut("env").and_then(|e| e.as_object_mut()) {
            let keys: Vec<String> = env.keys().cloned().collect();
            for key in keys {
                let plain = match env.get(&key).and_then(|v| v.as_str()) {
                    Some(s) if !s.starts_with(ENC_PREFIX) => s.to_owned(),
                    _ => continue,
                };
                let enc = self.encrypt_field(&Self::env_aad(id, &key), &plain)?;
                env.insert(key, serde_json::Value::String(enc));
            }
        }
        serde_json::to_string(&value)
            .map_err(|e| MiraError::ConfigError(format!("serialize McpServerConfig: {e}")))
    }

    fn token_from_config(&self, row: &McpServerRow) -> Result<Option<String>, MiraError> {
        let value: serde_json::Value = serde_json::from_str(&row.config_json)
            .map_err(|e| MiraError::ConfigError(format!("mcp_servers row {}: bad config_json: {e}", row.id)))?;
        value.get("auth_token").and_then(|v| v.as_str()).map(|v| self.decrypt_field(&row.id, v)).transpose()
    }

    // Decrypt every env value in place (strict — a value that claims to be
    // encrypted but won't decrypt is a corrupted/undecryptable row and surfaces
    // as an error). A plaintext value (no `enc:v1:` prefix) passes through.
    fn decrypt_env_in_place(&self, id: &str, cfg: &mut McpServerConfig) -> Result<(), MiraError> {
        for (key, value) in cfg.env.iter_mut() {
            let plain = self.decrypt_field(&Self::env_aad(id, key), value)?;
            *value = plain;
        }
        Ok(())
    }

    fn to_config(&self, row: &McpServerRow) -> Result<McpServerConfig, MiraError> {
        let mut cfg = row.to_config_with_token(self.token_from_config(row)?)?;
        self.decrypt_env_in_place(&row.id, &mut cfg)?;
        Ok(cfg)
    }

    pub fn config_for_row(&self, row: &McpServerRow) -> Result<McpServerConfig, MiraError> {
        self.to_config(row)
    }

    fn encrypt_legacy_plaintext(&self) -> Result<(), MiraError> {
        let mut conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, config_json FROM mcp_servers")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        drop(stmt);
        let tx = conn.transaction()
            .map_err(|e| MiraError::DatabaseError(format!("mcp token migration transaction failed: {e}")))?;
        for (id, raw) in rows {
            let mut value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| MiraError::ConfigError(format!("mcp_servers row {id}: bad config_json: {e}")))?;
            let mut changed = false;

            // auth_token (as before).
            if let Some(token) = value.get("auth_token").and_then(|v| v.as_str())
                .filter(|t| !t.starts_with(ENC_PREFIX)).map(str::to_owned)
            {
                value["auth_token"] = serde_json::Value::String(self.encrypt_field(&id, &token)?);
                changed = true;
            }

            // env values — same treatment, bound to (row id, key).
            let plain_env: Vec<(String, String)> = value.get("env").and_then(|e| e.as_object())
                .map(|obj| obj.iter().filter_map(|(k, v)| {
                    let s = v.as_str()?;
                    (!s.starts_with(ENC_PREFIX)).then(|| (k.clone(), s.to_owned()))
                }).collect())
                .unwrap_or_default();
            if !plain_env.is_empty() {
                if let Some(env) = value.get_mut("env").and_then(|e| e.as_object_mut()) {
                    for (k, s) in plain_env {
                        let enc = self.encrypt_field(&Self::env_aad(&id, &k), &s)?;
                        env.insert(k, serde_json::Value::String(enc));
                    }
                    changed = true;
                }
            }

            if !changed { continue; }
            let encoded = serde_json::to_string(&value)
                .map_err(|e| MiraError::ConfigError(format!("serialize MCP config: {e}")))?;
            tx.execute("UPDATE mcp_servers SET config_json = ?1 WHERE id = ?2", params![encoded, id])
                .map_err(|e| MiraError::DatabaseError(format!("mcp secret migration failed: {e}")))?;
        }
        tx.commit()
            .map_err(|e| MiraError::DatabaseError(format!("mcp token migration commit failed: {e}")))?;
        Ok(())
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    // ── Create ────────────────────────────────────────────────────────────────

    pub fn create(&self, user_id: &str, new: NewMcpServer) -> Result<McpServerRow, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let cfg = McpServerConfig {
            name:      new.name.clone(),
            transport: new.transport.clone(),
            command:   new.command.clone(),
            args:      new.args.clone(),
            env:       new.env.clone(),
            url:       new.url.clone(),
            auth_token: new.auth_token.filter(|v| !v.is_empty()),
            enabled:   new.enabled,
            sampling_enabled: new.sampling_enabled,
        };
        let config_json = self.config_json(&id, &cfg)?;
        let en = new.enabled as i64;

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO mcp_servers
               (id, user_id, name, transport, enabled, config_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![id, user_id, new.name, new.transport, en, config_json, now],
        )
        .map_err(|e| MiraError::DatabaseError(format!("create mcp_server: {e}")))?;

        Ok(McpServerRow {
            id,
            user_id:    user_id.to_owned(),
            name:       new.name,
            transport:  new.transport,
            enabled:    new.enabled,
            config_json,
            created_at: now,
            updated_at: now,
        })
    }

    /// Like [`create`], but if a row already exists for this `(user_id, name)`
    /// it is updated in place (same id) instead of violating the
    /// `UNIQUE(user_id, name)` constraint. Used by the package installer so a
    /// re-install whose `mcp_server` row was left behind — e.g. an earlier
    /// install by this admin that a later uninstall didn't fully reverse
    /// (`installed_packages` is one global row per package, but each install
    /// creates a per-installer `mcp_servers` row) — succeeds as a reinstall
    /// rather than failing with a UNIQUE conflict.
    pub fn create_or_replace(&self, user_id: &str, new: NewMcpServer) -> Result<McpServerRow, MiraError> {
        let now = Self::now_ms();
        let mut cfg = McpServerConfig {
            name:      new.name.clone(),
            transport: new.transport.clone(),
            command:   new.command.clone(),
            args:      new.args.clone(),
            env:       new.env.clone(),
            url:       new.url.clone(),
            auth_token: new.auth_token.filter(|v| !v.is_empty()),
            enabled:   new.enabled,
            sampling_enabled: new.sampling_enabled,
        };
        let en = new.enabled as i64;

        match self.existing_id(user_id, &new.name)? {
            Some(id) => {
                // Reinstall over an existing row: keep operator-added secrets the
                // incoming spec doesn't carry (a bearer token, env keys not in the
                // spec) rather than clobbering them. Best-effort — if the existing
                // row can't be decrypted (e.g. master key rotated) we proceed with
                // the spec alone rather than block the reinstall. (Package specs
                // are stdio today, so this is mostly forward-looking.)
                if let Ok(Some(existing_row)) = self.get(&id) {
                    if let Ok(existing_cfg) = self.to_config(&existing_row) {
                        if cfg.auth_token.is_none() {
                            cfg.auth_token = existing_cfg.auth_token;
                        }
                        for (key, value) in existing_cfg.env {
                            cfg.env.entry(key).or_insert(value);
                        }
                    }
                }
                let config_json = self.config_json(&id, &cfg)?;
                let conn = self.conn.lock().unwrap();
                conn.execute(
                    "UPDATE mcp_servers SET transport = ?1, enabled = ?2, config_json = ?3, updated_at = ?4
                     WHERE id = ?5",
                    params![new.transport, en, config_json, now, id],
                )
                .map_err(|e| MiraError::DatabaseError(format!("update mcp_server: {e}")))?;
                Ok(McpServerRow {
                    id, user_id: user_id.to_owned(), name: new.name, transport: new.transport,
                    enabled: new.enabled, config_json, created_at: now, updated_at: now,
                })
            }
            None => {
                let id = Uuid::new_v4().to_string();
                let config_json = self.config_json(&id, &cfg)?;
                let conn = self.conn.lock().unwrap();
                conn.execute(
                    "INSERT INTO mcp_servers
                       (id, user_id, name, transport, enabled, config_json, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    params![id, user_id, new.name, new.transport, en, config_json, now],
                )
                .map_err(|e| MiraError::DatabaseError(format!("create mcp_server: {e}")))?;
                Ok(McpServerRow {
                    id, user_id: user_id.to_owned(), name: new.name, transport: new.transport,
                    enabled: new.enabled, config_json, created_at: now, updated_at: now,
                })
            }
        }
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    pub fn get(&self, id: &str) -> Result<Option<McpServerRow>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            "SELECT id, user_id, name, transport, enabled, config_json, created_at, updated_at
             FROM mcp_servers WHERE id = ?1",
            params![id],
            row_to_server,
        );
        match r {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    fn existing_id(&self, user_id: &str, name: &str) -> Result<Option<String>, MiraError> {
        let conn = self.conn.lock().unwrap();
        match conn.query_row(
            "SELECT id FROM mcp_servers WHERE user_id = ?1 AND name = ?2",
            params![user_id, name],
            |r| r.get(0),
        ) {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn list_for_user(&self, user_id: &str) -> Result<Vec<McpServerRow>, MiraError> {
        self.query_list(
            "SELECT id, user_id, name, transport, enabled, config_json, created_at, updated_at
             FROM mcp_servers WHERE user_id = ?1 ORDER BY created_at ASC",
            params![user_id],
        )
    }

    // Every enabled row across every user — what the gateway iterates
    // on startup to connect.
    pub fn list_all_enabled(&self) -> Result<Vec<McpServerRow>, MiraError> {
        self.query_list(
            "SELECT id, user_id, name, transport, enabled, config_json, created_at, updated_at
             FROM mcp_servers WHERE enabled = 1 ORDER BY user_id ASC, created_at ASC",
            params![],
        )
    }

    // Every row across every user — enabled or disabled. Used by
    // the registry's status snapshot so the UI can show disabled
    // entries greyed out rather than hiding them entirely.
    pub fn list_all(&self) -> Result<Vec<McpServerRow>, MiraError> {
        self.query_list(
            "SELECT id, user_id, name, transport, enabled, config_json, created_at, updated_at
             FROM mcp_servers ORDER BY user_id ASC, created_at ASC",
            params![],
        )
    }

    // Used by the legacy-migrator: nonzero means we've already seeded.
    pub fn count_all(&self) -> Result<u64, MiraError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM mcp_servers",
            params![],
            |r| r.get(0),
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(n.max(0) as u64)
    }

    fn query_list(
        &self,
        sql: &str,
        p:   impl rusqlite::Params,
    ) -> Result<Vec<McpServerRow>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map(p, row_to_server)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(out)
    }

    // ── Update / delete ───────────────────────────────────────────────────────

    pub fn update(&self, id: &str, upd: UpdateMcpServer) -> Result<McpServerRow, MiraError> {
        // Read-modify-write so we can return the full row and avoid
        // the per-field SQL fan-out. Volume here is tiny (a few rows
        // per user), so the extra round-trip is fine.
        let row = self.get(id)?
            .ok_or_else(|| MiraError::NotFound(format!("mcp_server not found: {id}")))?;
        // Hydrate WITHOUT the stored token: a token that can't be decrypted
        // (master key lost/rotated, tampered row) must not block clearing or
        // replacing it. The token is resolved below, and only *retaining* it
        // requires a successful decrypt. (env is decrypted strictly — for the
        // bearer-token case the server is http with no env, so this doesn't
        // block the common recovery.)
        let mut cfg = row.to_config_with_token(None)?;
        self.decrypt_env_in_place(&row.id, &mut cfg)?;

        if let Some(name)      = upd.name      { cfg.name = name; }
        if let Some(transport) = upd.transport { cfg.transport = transport; }
        if let Some(command)   = upd.command   { cfg.command = command; }
        if let Some(args)      = upd.args      { cfg.args = args; }
        if let Some(env)       = upd.env       {
            // API responses redact values as `***`. Merge rather than replace
            // so omitted keys cannot delete existing environment secrets.
            // An explicitly entered literal `***` remains ambiguous with the
            // sentinel and is therefore treated as "retain" (see the test).
            for (key, value) in env {
                if value != "***" {
                    cfg.env.insert(key, value);
                }
            }
        }
        for key in upd.remove_env_keys {
            cfg.env.remove(&key);
        }
        // `set_env` is applied after removals, so an explicit value wins if
        // the same key appears in both update fields.
        for (key, value) in upd.set_env {
            cfg.env.insert(key, value);
        }
        if let Some(url)       = upd.url       { cfg.url = url; }
        if upd.clear_auth_token {
            cfg.auth_token = None;
        } else if let Some(token) = upd.auth_token.filter(|v| !v.is_empty()) {
            cfg.auth_token = Some(token);
        } else {
            // Retain the existing token — this is the only path that needs it
            // decrypted. Fail closed with actionable guidance if it can't be,
            // so an unrelated edit can't silently drop the token.
            cfg.auth_token = self.token_from_config(&row).map_err(|_| MiraError::ConfigError(format!(
                "mcp_server {id}: stored token can't be decrypted; set a new token or clear it"
            )))?;
        }
        if let Some(enabled)   = upd.enabled   { cfg.enabled = enabled; }
        if let Some(s)         = upd.sampling_enabled { cfg.sampling_enabled = s; }

        let config_json = self.config_json(id, &cfg)?;
        let now = Self::now_ms();
        let en  = cfg.enabled as i64;

        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE mcp_servers
                SET name=?1, transport=?2, enabled=?3, config_json=?4, updated_at=?5
              WHERE id=?6",
            params![cfg.name, cfg.transport, en, config_json, now, id],
        )
        .map_err(|e| MiraError::DatabaseError(format!("update mcp_server: {e}")))?;
        if n == 0 {
            return Err(MiraError::NotFound(format!("mcp_server not found: {id}")));
        }

        Ok(McpServerRow {
            id:          row.id,
            user_id:     row.user_id,
            name:        cfg.name,
            transport:   cfg.transport,
            enabled:     cfg.enabled,
            config_json,
            created_at:  row.created_at,
            updated_at:  now,
        })
    }

    pub fn delete(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM mcp_servers WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if n == 0 {
            return Err(MiraError::NotFound(format!("mcp_server not found: {id}")));
        }
        Ok(())
    }
}

fn row_to_server(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpServerRow> {
    Ok(McpServerRow {
        id:          row.get(0)?,
        user_id:     row.get(1)?,
        name:        row.get(2)?,
        transport:   row.get(3)?,
        enabled:     row.get::<_, i64>(4)? != 0,
        config_json: row.get(5)?,
        created_at:  row.get(6)?,
        updated_at:  row.get(7)?,
    })
}

fn redacted_config_json(raw: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return "{}".to_owned();
    };
    if let Some(object) = value.as_object_mut() {
        object.remove("auth_token");
        if let Some(env) = object.get_mut("env").and_then(|env| env.as_object_mut()) {
            for value in env.values_mut() {
                *value = serde_json::Value::String("***".to_owned());
            }
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{McpServerRow, McpServerStore, NewMcpServer, UpdateMcpServer, ENC_PREFIX};
    use rusqlite::{params, Connection};
    use tempfile::tempdir;

    fn store() -> (McpServerStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE users (id TEXT PRIMARY KEY); INSERT INTO users VALUES ('synthetic-user');").unwrap();
        drop(conn);
        let store = McpServerStore::open(&path).unwrap();
        (store, dir)
    }

    fn new_server(token: Option<&str>) -> NewMcpServer {
        NewMcpServer {
            name: "synthetic-server".to_owned(),
            transport: "http".to_owned(),
            command: None,
            args: vec![],
            env: Default::default(),
            url: Some("https://mcp.example.test/v1".to_owned()),
            auth_token: token.map(str::to_owned),
            enabled: true,
            sampling_enabled: false,
        }
    }

    #[test]
    fn response_redacts_token_and_reports_only_configuration_status() {
        let row = McpServerRow::from_config_for_test(crate::config::McpServerConfig {
            name: "synthetic-server".to_owned(),
            transport: "http".to_owned(),
            command: None,
            args: vec![],
            env: Default::default(),
            url: Some("https://mcp.example.test/v1".to_owned()),
            auth_token: Some("synthetic-response-token".to_owned()),
            enabled: true,
            sampling_enabled: false,
        });

        let response = row.response();
        let serialized = serde_json::to_string(&response).unwrap();
        assert!(response.auth_token_configured);
        assert!(!response.config_json.contains("auth_token"));
        assert!(!serialized.contains("synthetic-response-token"));
    }

    #[test]
    fn response_redacts_environment_values() {
        let mut env = std::collections::HashMap::new();
        env.insert("SYNTHETIC_API_KEY".to_owned(), "synthetic-env-secret".to_owned());
        let row = McpServerRow::from_config_for_test(crate::config::McpServerConfig {
            name: "synthetic-server".to_owned(), transport: "stdio".to_owned(),
            command: Some("synthetic".to_owned()), args: vec![], env,
            url: None, auth_token: None, enabled: true, sampling_enabled: false,
        });
        let response = row.response();
        assert!(response.config_json.contains("SYNTHETIC_API_KEY"));
        assert!(response.config_json.contains("***"));
        assert!(!response.config_json.contains("synthetic-env-secret"));
    }

    #[test]
    fn update_empty_token_retains_existing_token_and_new_token_replaces_it() {
        let (store, _dir) = store();
        let row = store.create("synthetic-user", new_server(Some("synthetic-old-token"))).unwrap();

        let absent = store.update(&row.id, UpdateMcpServer::default()).unwrap();
        assert_eq!(store.config_for_row(&absent).unwrap().auth_token.as_deref(), Some("synthetic-old-token"));

        let retained = store.update(&row.id, UpdateMcpServer {
            auth_token: Some(String::new()),
            ..Default::default()
        }).unwrap();
        assert_eq!(store.config_for_row(&retained).unwrap().auth_token.as_deref(), Some("synthetic-old-token"));

        let replaced = store.update(&row.id, UpdateMcpServer {
            auth_token: Some("synthetic-new-token".to_owned()),
            ..Default::default()
        }).unwrap();
        assert_eq!(store.config_for_row(&replaced).unwrap().auth_token.as_deref(), Some("synthetic-new-token"));
    }

    #[test]
    fn explicit_token_clear_removes_token_and_wins_over_new_token() {
        let (store, _dir) = store();
        let row = store.create("synthetic-user", new_server(Some("synthetic-old-token"))).unwrap();

        let cleared = store.update(&row.id, UpdateMcpServer {
            clear_auth_token: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(store.config_for_row(&cleared).unwrap().auth_token, None);
        assert!(!cleared.config_json.contains("auth_token"));

        let restored = store.update(&cleared.id, UpdateMcpServer {
            auth_token: Some("synthetic-restored-token".to_owned()),
            ..Default::default()
        }).unwrap();
        let conflicted = store.update(&restored.id, UpdateMcpServer {
            auth_token: Some("synthetic-ignored-token".to_owned()),
            clear_auth_token: true,
            ..Default::default()
        }).unwrap();
        assert_eq!(store.config_for_row(&conflicted).unwrap().auth_token, None);
        assert!(!conflicted.config_json.contains("synthetic-ignored-token"));
    }

    #[test]
    fn auth_token_is_encrypted_at_rest_and_bound_to_row_id() {
        let (store, _dir) = store();
        let row = store.create("synthetic-user", new_server(Some("synthetic-db-token"))).unwrap();
        assert!(row.config_json.contains(ENC_PREFIX));
        assert!(!row.config_json.contains("synthetic-db-token"));
        assert_eq!(store.config_for_row(&row).unwrap().auth_token.as_deref(), Some("synthetic-db-token"));

        let stored = serde_json::from_str::<serde_json::Value>(&row.config_json).unwrap();
        let ciphertext = stored["auth_token"].as_str().unwrap();
        assert!(store.decrypt_field(&row.id, ciphertext).is_ok());
        assert!(store.decrypt_field("different-row-id", ciphertext).is_err());
    }

    #[test]
    fn invalid_encrypted_token_fails_closed() {
        let (store, _dir) = store();
        let row = store.create("synthetic-user", new_server(Some("synthetic-db-token"))).unwrap();
        let mut value = serde_json::from_str::<serde_json::Value>(&row.config_json).unwrap();
        value["auth_token"] = serde_json::Value::String(format!("{ENC_PREFIX}invalid"));
        let raw = serde_json::to_string(&value).unwrap();
        store.conn.lock().unwrap()
            .execute("UPDATE mcp_servers SET config_json = ?1 WHERE id = ?2", params![raw, row.id])
            .unwrap();
        let tampered = store.get(&row.id).unwrap().unwrap();
        assert!(store.config_for_row(&tampered).is_err());
    }

    #[test]
    fn legacy_plaintext_token_is_migrated_on_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE users (id TEXT PRIMARY KEY); INSERT INTO users VALUES ('synthetic-user');").unwrap();
        conn.execute_batch("CREATE TABLE mcp_servers (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, transport TEXT NOT NULL, enabled INTEGER NOT NULL, config_json TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);").unwrap();
        conn.execute(
            "INSERT INTO mcp_servers VALUES (?1, ?2, ?3, ?4, 1, ?5, 0, 0)",
            params![
                "legacy-row", "synthetic-user", "legacy", "http",
                serde_json::json!({
                    "name": "legacy", "transport": "http", "url": "https://mcp.example.test/v1",
                    "auth_token": "synthetic-legacy-token", "enabled": true,
                    "args": [], "env": {}, "sampling_enabled": false
                }).to_string()
            ],
        ).unwrap();
        drop(conn);

        let store = McpServerStore::open(&path).unwrap();
        let row = store.get("legacy-row").unwrap().unwrap();
        assert!(row.config_json.contains(ENC_PREFIX));
        assert!(!row.config_json.contains("synthetic-legacy-token"));
        assert_eq!(store.config_for_row(&row).unwrap().auth_token.as_deref(), Some("synthetic-legacy-token"));
    }

    #[test]
    fn update_environment_sentinel_retains_existing_value() {
        let (store, _dir) = store();
        let mut server = new_server(None);
        server.transport = "stdio".to_owned();
        server.command = Some("synthetic".to_owned());
        server.env.insert("SYNTHETIC_API_KEY".to_owned(), "synthetic-env-secret".to_owned());
        let row = store.create("synthetic-user", server).unwrap();
        let mut env = std::collections::HashMap::new();
        env.insert("SYNTHETIC_API_KEY".to_owned(), "***".to_owned());

        let updated = store.update(&row.id, UpdateMcpServer { env: Some(env), ..Default::default() }).unwrap();
        assert_eq!(store.config_for_row(&updated).unwrap().env.get("SYNTHETIC_API_KEY").map(String::as_str), Some("synthetic-env-secret"));
    }

    #[test]
    fn explicit_env_removal_preserves_other_keys() {
        let (store, _dir) = store();
        let mut server = new_server(None);
        server.transport = "stdio".to_owned();
        server.command = Some("synthetic".to_owned());
        server.env.insert("SYNTHETIC_REMOVE".to_owned(), "remove-me".to_owned());
        server.env.insert("SYNTHETIC_KEEP".to_owned(), "keep-me".to_owned());
        let row = store.create("synthetic-user", server).unwrap();

        let updated = store.update(&row.id, UpdateMcpServer {
            remove_env_keys: vec!["SYNTHETIC_REMOVE".to_owned()],
            ..Default::default()
        }).unwrap();
        let env = store.config_for_row(&updated).unwrap().env;
        assert!(!env.contains_key("SYNTHETIC_REMOVE"));
        assert_eq!(env.get("SYNTHETIC_KEEP").map(String::as_str), Some("keep-me"));
    }

    #[test]
    fn set_env_can_store_literal_sentinel_and_wins_over_removal() {
        let (store, _dir) = store();
        let mut server = new_server(None);
        server.transport = "stdio".to_owned();
        server.command = Some("synthetic".to_owned());
        server.env.insert("SYNTHETIC_VALUE".to_owned(), "old".to_owned());
        let row = store.create("synthetic-user", server).unwrap();

        let mut set_env = std::collections::HashMap::new();
        set_env.insert("SYNTHETIC_VALUE".to_owned(), "***".to_owned());
        let updated = store.update(&row.id, UpdateMcpServer {
            remove_env_keys: vec!["SYNTHETIC_VALUE".to_owned()],
            set_env,
            ..Default::default()
        }).unwrap();
        assert_eq!(store.config_for_row(&updated).unwrap().env.get("SYNTHETIC_VALUE").map(String::as_str), Some("***"));
    }

    #[test]
    fn update_environment_omission_retains_existing_values() {
        let (store, _dir) = store();
        let mut server = new_server(None);
        server.transport = "stdio".to_owned();
        server.command = Some("synthetic".to_owned());
        server.env.insert("SYNTHETIC_API_KEY".to_owned(), "synthetic-env-secret".to_owned());
        server.env.insert("SYNTHETIC_OTHER".to_owned(), "synthetic-other".to_owned());
        let row = store.create("synthetic-user", server).unwrap();
        let mut env = std::collections::HashMap::new();
        env.insert("SYNTHETIC_API_KEY".to_owned(), "synthetic-updated".to_owned());

        let updated = store.update(&row.id, UpdateMcpServer { env: Some(env), ..Default::default() }).unwrap();
        let env = store.config_for_row(&updated).unwrap().env;
        assert_eq!(env.get("SYNTHETIC_API_KEY").map(String::as_str), Some("synthetic-updated"));
        assert_eq!(env.get("SYNTHETIC_OTHER").map(String::as_str), Some("synthetic-other"));
    }

    #[test]
    fn literal_sentinel_value_is_documented_as_ambiguous_and_retains_old_value() {
        let (store, _dir) = store();
        let mut server = new_server(None);
        server.transport = "stdio".to_owned();
        server.command = Some("synthetic".to_owned());
        server.env.insert("SYNTHETIC_API_KEY".to_owned(), "synthetic-old".to_owned());
        let row = store.create("synthetic-user", server).unwrap();
        let mut env = std::collections::HashMap::new();
        env.insert("SYNTHETIC_API_KEY".to_owned(), "***".to_owned());

        let updated = store.update(&row.id, UpdateMcpServer { env: Some(env), ..Default::default() }).unwrap();
        assert_eq!(store.config_for_row(&updated).unwrap().env.get("SYNTHETIC_API_KEY").map(String::as_str), Some("synthetic-old"));
    }

    // Fix 1 — env values are encrypted at rest, bound to (row id, key).
    #[test]
    fn env_values_are_encrypted_at_rest_and_bound_to_row_and_key() {
        let (store, _dir) = store();
        let mut server = new_server(None);
        server.transport = "stdio".to_owned();
        server.command = Some("synthetic".to_owned());
        server.env.insert("SYNTHETIC_API_KEY".to_owned(), "synthetic-env-secret".to_owned());
        let row = store.create("synthetic-user", server).unwrap();

        assert!(row.config_json.contains(ENC_PREFIX), "env stored encrypted");
        assert!(!row.config_json.contains("synthetic-env-secret"), "no plaintext env at rest");
        assert_eq!(store.config_for_row(&row).unwrap().env.get("SYNTHETIC_API_KEY").map(String::as_str),
                   Some("synthetic-env-secret"));

        // AAD binds the ciphertext to (id, key) — it won't decrypt under another key name.
        let stored = serde_json::from_str::<serde_json::Value>(&row.config_json).unwrap();
        let ciphertext = stored["env"]["SYNTHETIC_API_KEY"].as_str().unwrap();
        assert!(store.decrypt_field(&McpServerStore::env_aad(&row.id, "SYNTHETIC_API_KEY"), ciphertext).is_ok());
        assert!(store.decrypt_field(&McpServerStore::env_aad(&row.id, "OTHER_KEY"), ciphertext).is_err());
    }

    // Fix 1 — a legacy row with plaintext env is migrated on open, idempotently.
    #[test]
    fn legacy_plaintext_env_is_migrated_on_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE users (id TEXT PRIMARY KEY); INSERT INTO users VALUES ('synthetic-user');").unwrap();
        conn.execute_batch("CREATE TABLE mcp_servers (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, transport TEXT NOT NULL, enabled INTEGER NOT NULL, config_json TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);").unwrap();
        conn.execute(
            "INSERT INTO mcp_servers VALUES (?1, ?2, ?3, ?4, 1, ?5, 0, 0)",
            params![
                "legacy-env", "synthetic-user", "legacy", "stdio",
                serde_json::json!({
                    "name": "legacy", "transport": "stdio", "command": "synthetic",
                    "enabled": true, "args": [], "sampling_enabled": false,
                    "env": { "SYNTHETIC_API_KEY": "synthetic-legacy-env" }
                }).to_string()
            ],
        ).unwrap();
        drop(conn);

        let store = McpServerStore::open(&path).unwrap();
        let row = store.get("legacy-env").unwrap().unwrap();
        assert!(row.config_json.contains(ENC_PREFIX));
        assert!(!row.config_json.contains("synthetic-legacy-env"));
        assert_eq!(store.config_for_row(&row).unwrap().env.get("SYNTHETIC_API_KEY").map(String::as_str),
                   Some("synthetic-legacy-env"));
        // Idempotent: re-opening doesn't re-encrypt / corrupt it.
        let store2 = McpServerStore::open(&path).unwrap();
        assert_eq!(store2.config_for_row(&store2.get("legacy-env").unwrap().unwrap()).unwrap()
                   .env.get("SYNTHETIC_API_KEY").map(String::as_str), Some("synthetic-legacy-env"));
    }

    // Fix 2 — an undecryptable token can still be cleared or replaced; only
    // retaining it fails closed.
    #[test]
    fn undecryptable_token_can_be_cleared_or_replaced_but_retain_fails() {
        let (store, _dir) = store();
        let corrupt = |store: &McpServerStore, id: &str| {
            let row = store.get(id).unwrap().unwrap();
            let mut value = serde_json::from_str::<serde_json::Value>(&row.config_json).unwrap();
            value["auth_token"] = serde_json::Value::String(format!("{ENC_PREFIX}invalid"));
            let raw = serde_json::to_string(&value).unwrap();
            store.conn.lock().unwrap()
                .execute("UPDATE mcp_servers SET config_json = ?1 WHERE id = ?2", params![raw, id]).unwrap();
        };

        // clear succeeds despite a corrupt stored token.
        let mut a = new_server(Some("synthetic-token")); a.name = "srv-a".into();
        let a = store.create("synthetic-user", a).unwrap();
        corrupt(&store, &a.id);
        let cleared = store.update(&a.id, UpdateMcpServer { clear_auth_token: true, ..Default::default() }).unwrap();
        assert_eq!(store.config_for_row(&cleared).unwrap().auth_token, None);

        // replace succeeds despite a corrupt stored token.
        let mut b = new_server(Some("synthetic-token")); b.name = "srv-b".into();
        let b = store.create("synthetic-user", b).unwrap();
        corrupt(&store, &b.id);
        let replaced = store.update(&b.id, UpdateMcpServer {
            auth_token: Some("synthetic-fresh".into()), ..Default::default() }).unwrap();
        assert_eq!(store.config_for_row(&replaced).unwrap().auth_token.as_deref(), Some("synthetic-fresh"));

        // an unrelated edit that would retain the token fails closed (no silent drop).
        let mut c = new_server(Some("synthetic-token")); c.name = "srv-c".into();
        let c = store.create("synthetic-user", c).unwrap();
        corrupt(&store, &c.id);
        let err = store.update(&c.id, UpdateMcpServer { enabled: Some(false), ..Default::default() }).unwrap_err();
        assert!(err.to_string().contains("can't be decrypted"), "{err}");
    }

    // Fix 3 — reinstalling (create_or_replace) over an existing row keeps an
    // operator-added token and env keys the incoming spec doesn't carry.
    #[test]
    fn create_or_replace_preserves_operator_added_token_and_env() {
        let (store, _dir) = store();
        // Operator installed/edited: a token + two env keys.
        let mut first = new_server(Some("synthetic-operator-token"));
        first.env.insert("OPERATOR_KEY".to_owned(), "operator-value".to_owned());
        first.env.insert("SHARED_KEY".to_owned(), "original".to_owned());
        store.create_or_replace("synthetic-user", first).unwrap();

        // Reinstall: spec carries no token, overrides SHARED_KEY, omits OPERATOR_KEY.
        let mut reinstall = new_server(None);
        reinstall.env.insert("SHARED_KEY".to_owned(), "from-spec".to_owned());
        let row = store.create_or_replace("synthetic-user", reinstall).unwrap();

        let cfg = store.config_for_row(&row).unwrap();
        assert_eq!(cfg.auth_token.as_deref(), Some("synthetic-operator-token"), "token retained");
        assert_eq!(cfg.env.get("OPERATOR_KEY").map(String::as_str), Some("operator-value"), "user env retained");
        assert_eq!(cfg.env.get("SHARED_KEY").map(String::as_str), Some("from-spec"), "spec overrides shared key");
    }
}
