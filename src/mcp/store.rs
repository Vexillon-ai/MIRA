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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl McpServerRow {
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
#[derive(Debug, Clone, Deserialize)]
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
    #[serde(default = "default_enabled")]
    pub enabled:   bool,
    // opt-in to sampling. Defaults false so a freshly-
    // created row never accepts server-initiated LLM calls until
    // the operator explicitly enables it.
    #[serde(default)]
    pub sampling_enabled: bool,
}

fn default_transport() -> String { "stdio".to_string() }
fn default_enabled() -> bool { true }

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UpdateMcpServer {
    pub name:      Option<String>,
    pub transport: Option<String>,
    pub command:   Option<Option<String>>,
    pub args:      Option<Vec<String>>,
    pub env:       Option<HashMap<String, String>>,
    pub url:       Option<Option<String>>,
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
        if let Some(env)       = upd.env       { cfg.env = env; }
        if let Some(url)       = upd.url       { cfg.url = url; }
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
