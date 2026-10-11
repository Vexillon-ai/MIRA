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

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::MiraError;
use crate::config::McpServerConfig;

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
    pub fn to_config(&self) -> Result<McpServerConfig, MiraError> {
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
    pub url:       Option<Option<String>>,
    /// Empty or absent means retain the existing token. A non-empty value
    /// replaces it. Explicit deletion is intentionally not supported yet.
    #[serde(default)]
    pub auth_token: Option<String>,
    pub enabled:   Option<bool>,
    pub sampling_enabled: Option<bool>,
}

// ── Store ────────────────────────────────────────────────────────────────────

pub struct McpServerStore {
    conn: Arc<Mutex<Connection>>,
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

        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
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
        let config_json = serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(
            format!("serialize McpServerConfig: {e}")
        ))?;
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
        let cfg = McpServerConfig {
            name:      new.name.clone(),
            transport: new.transport.clone(),
            command:   new.command.clone(),
            args:      new.args.clone(),
            env:       new.env.clone(),
            url:       new.url.clone(),
            enabled:   new.enabled,
            sampling_enabled: new.sampling_enabled,
        };
        let config_json = serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(
            format!("serialize McpServerConfig: {e}")
        ))?;
        let en = new.enabled as i64;

        let conn = self.conn.lock().unwrap();
        let existing: Result<String, _> = conn.query_row(
            "SELECT id FROM mcp_servers WHERE user_id = ?1 AND name = ?2",
            params![user_id, new.name],
            |r| r.get(0),
        );
        match existing {
            Ok(id) => {
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
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                let id = Uuid::new_v4().to_string();
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
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
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
        let mut cfg = row.to_config()?;

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
        if let Some(url)       = upd.url       { cfg.url = url; }
        if let Some(token)     = upd.auth_token.filter(|v| !v.is_empty()) { cfg.auth_token = Some(token); }
        if let Some(enabled)   = upd.enabled   { cfg.enabled = enabled; }
        if let Some(s)         = upd.sampling_enabled { cfg.sampling_enabled = s; }

        let config_json = serde_json::to_string(&cfg).map_err(|e| MiraError::ConfigError(
            format!("serialize McpServerConfig: {e}")
        ))?;
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
    use super::{McpServerRow, McpServerStore, NewMcpServer, UpdateMcpServer};
    use rusqlite::Connection;
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
        assert_eq!(absent.to_config().unwrap().auth_token.as_deref(), Some("synthetic-old-token"));

        let retained = store.update(&row.id, UpdateMcpServer {
            auth_token: Some(String::new()),
            ..Default::default()
        }).unwrap();
        assert_eq!(retained.to_config().unwrap().auth_token.as_deref(), Some("synthetic-old-token"));

        let replaced = store.update(&row.id, UpdateMcpServer {
            auth_token: Some("synthetic-new-token".to_owned()),
            ..Default::default()
        }).unwrap();
        assert_eq!(replaced.to_config().unwrap().auth_token.as_deref(), Some("synthetic-new-token"));
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
        assert_eq!(updated.to_config().unwrap().env.get("SYNTHETIC_API_KEY").map(String::as_str), Some("synthetic-env-secret"));
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
        let env = updated.to_config().unwrap().env;
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
        assert_eq!(updated.to_config().unwrap().env.get("SYNTHETIC_API_KEY").map(String::as_str), Some("synthetic-old"));
    }
}
