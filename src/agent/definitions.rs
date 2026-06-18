// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/definitions.rs

//! **Named agents** (Phase B) — persistent, user-defined agent profiles.
//!
//! Until now an "agent" was an ephemeral UUID worker spawned per task and bound
//! to a Skill. A named agent is instead a *saved* specialist: a name, a persona
//! (system prompt), a tool subset, a model choice, and a budget — configured
//! once, then invoked by name. A user calls one from chat (`@researcher …`), and
//! MIRA itself delegates to one in its automations / proactive activity. The
//! definition is the unit of orchestration; spawning a worker *bound to a
//! definition* is slice B2.
//!
//! This slice ships the definition model + store (CRUD) + the management API.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::MiraError;

/// A saved, reusable agent profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub id: String,
    /// Unique invocation key — a slug (`researcher`, `code-reviewer`). What a
    /// user / MIRA references to call this agent.
    pub name: String,
    pub description: String,
    /// The persona / system prompt the agent runs with.
    pub system_prompt: String,
    /// Tool names this agent may use. Empty = inherit MIRA's default toolset.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// An `agent.llm_aliases` key (`primary`/`coding`/`research`/`cheap`) or
    /// `None` to use the default provider/model.
    #[serde(default)]
    pub model_alias: Option<String>,
    /// Per-invocation USD budget, or `None` for the default task budget.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

fn default_true() -> bool { true }

/// Fields for create/update (the store assigns id + timestamps).
#[derive(Debug, Clone, Deserialize)]
pub struct NewAgentDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub model_alias: Option<String>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Validate a name is a usable invocation slug: 1–40 chars, lowercase
/// alphanumeric + dashes, starting with a letter.
pub fn validate_name(name: &str) -> Result<(), MiraError> {
    let n = name.trim();
    if n.is_empty() || n.len() > 40 {
        return Err(MiraError::ConfigError("agent name must be 1–40 chars".into()));
    }
    let mut chars = n.chars();
    if !chars.next().map(|c| c.is_ascii_lowercase()).unwrap_or(false) {
        return Err(MiraError::ConfigError("agent name must start with a lowercase letter".into()));
    }
    if !n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(MiraError::ConfigError(
            "agent name may only contain lowercase letters, digits, and dashes".into(),
        ));
    }
    // Reserved for the built-in, code-defined system agent — a user agent must
    // not be able to create/rename onto it (and thereby shadow or impersonate
    // it). The Guardian is never a DB row; see `agent::guardian`.
    if n == crate::agent::guardian::RESERVED_NAME {
        return Err(MiraError::ConfigError(format!(
            "{:?} is a reserved built-in agent name", crate::agent::guardian::RESERVED_NAME
        )));
    }
    Ok(())
}

const MIGRATIONS: &[crate::db::Migration] = &[crate::db::Migration {
    version: 1,
    name: "create agent_definitions",
    up: |tx| {
        tx.execute_batch(
            r#"CREATE TABLE IF NOT EXISTS agent_definitions (
                id                 TEXT PRIMARY KEY,
                name               TEXT NOT NULL UNIQUE,
                description        TEXT NOT NULL DEFAULT '',
                system_prompt      TEXT NOT NULL DEFAULT '',
                allowed_tools_json TEXT NOT NULL DEFAULT '[]',
                model_alias        TEXT,
                budget_usd         REAL,
                enabled            INTEGER NOT NULL DEFAULT 1,
                created_at         INTEGER NOT NULL,
                updated_at         INTEGER NOT NULL
            );"#,
        )
    },
}];

pub struct AgentDefinitionStore {
    conn: Arc<Mutex<Connection>>,
}

impl AgentDefinitionStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MiraError::DatabaseError(format!("create agent-defs DB dir: {e}")))?;
        }
        let mut conn = Connection::open(path)
            .map_err(|e| MiraError::DatabaseError(format!("open agent-defs DB: {e}")))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        crate::db::run(&mut conn, "agent_definitions", MIGRATIONS)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self, MiraError> {
        Self::open(Path::new(":memory:"))
    }

    fn now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    pub fn create(&self, new: NewAgentDefinition) -> Result<AgentDefinition, MiraError> {
        validate_name(&new.name)?;
        let id = Uuid::now_v7().to_string();
        let now = Self::now();
        let tools = serde_json::to_string(&new.allowed_tools).unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_definitions
               (id, name, description, system_prompt, allowed_tools_json, model_alias, budget_usd, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![id, new.name.trim(), new.description, new.system_prompt, tools,
                    new.model_alias, new.budget_usd, new.enabled as i64, now],
        )
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                MiraError::ConfigError(format!("an agent named {:?} already exists", new.name))
            } else {
                MiraError::DatabaseError(format!("create agent definition: {e}"))
            }
        })?;
        drop(conn);
        self.get(&id)?.ok_or_else(|| MiraError::DatabaseError("definition vanished after create".into()))
    }

    pub fn list(&self) -> Result<Vec<AgentDefinition>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, description, system_prompt, allowed_tools_json, model_alias, budget_usd, enabled, created_at, updated_at FROM agent_definitions ORDER BY name ASC")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map([], row_to_def).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    pub fn get(&self, id: &str) -> Result<Option<AgentDefinition>, MiraError> {
        self.query_one("WHERE id = ?1", id)
    }

    /// Look up by invocation name (for spawning by name — slice B2).
    pub fn get_by_name(&self, name: &str) -> Result<Option<AgentDefinition>, MiraError> {
        self.query_one("WHERE name = ?1", name)
    }

    fn query_one(&self, clause: &str, key: &str) -> Result<Option<AgentDefinition>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT id, name, description, system_prompt, allowed_tools_json, model_alias, budget_usd, enabled, created_at, updated_at FROM agent_definitions {clause}");
        conn.query_row(&sql, params![key], row_to_def)
            .optional()
            .map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    pub fn update(&self, id: &str, new: NewAgentDefinition) -> Result<AgentDefinition, MiraError> {
        validate_name(&new.name)?;
        let tools = serde_json::to_string(&new.allowed_tools).unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE agent_definitions SET
               name=?2, description=?3, system_prompt=?4, allowed_tools_json=?5,
               model_alias=?6, budget_usd=?7, enabled=?8, updated_at=?9
             WHERE id=?1",
            params![id, new.name.trim(), new.description, new.system_prompt, tools,
                    new.model_alias, new.budget_usd, new.enabled as i64, Self::now()],
        )
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                MiraError::ConfigError(format!("an agent named {:?} already exists", new.name))
            } else {
                MiraError::DatabaseError(format!("update agent definition: {e}"))
            }
        })?;
        if n == 0 {
            return Err(MiraError::NotFound(format!("agent definition {id} not found")));
        }
        drop(conn);
        self.get(id)?.ok_or_else(|| MiraError::DatabaseError("definition vanished after update".into()))
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE agent_definitions SET enabled=?2, updated_at=?3 WHERE id=?1",
            params![id, enabled as i64, Self::now()],
        )
        .map_err(|e| MiraError::DatabaseError(format!("set enabled: {e}")))?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM agent_definitions WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(format!("delete agent definition: {e}")))?;
        Ok(())
    }
}

fn row_to_def(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentDefinition> {
    let tools_json: String = row.get(4)?;
    Ok(AgentDefinition {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        system_prompt: row.get(3)?,
        allowed_tools: serde_json::from_str(&tools_json).unwrap_or_default(),
        model_alias: row.get(5)?,
        budget_usd: row.get(6)?,
        enabled: row.get::<_, i64>(7)? != 0,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(name: &str) -> NewAgentDefinition {
        NewAgentDefinition {
            name: name.into(),
            description: "a specialist".into(),
            system_prompt: "You are focused.".into(),
            allowed_tools: vec!["web_search".into(), "web_fetch".into()],
            model_alias: Some("research".into()),
            budget_usd: Some(3.0),
            enabled: true,
        }
    }

    #[test]
    fn create_get_by_name_update_list_delete() {
        let s = AgentDefinitionStore::open_memory().unwrap();
        let d = s.create(new("researcher")).unwrap();
        assert_eq!(d.name, "researcher");
        assert_eq!(d.allowed_tools, vec!["web_search", "web_fetch"]);
        assert_eq!(s.get_by_name("researcher").unwrap().unwrap().id, d.id);

        let mut upd = new("researcher");
        upd.description = "now broader".into();
        upd.enabled = false;
        let d2 = s.update(&d.id, upd).unwrap();
        assert_eq!(d2.description, "now broader");
        assert!(!d2.enabled);

        assert_eq!(s.list().unwrap().len(), 1);
        s.delete(&d.id).unwrap();
        assert!(s.get(&d.id).unwrap().is_none());
    }

    #[test]
    fn name_must_be_a_slug_and_unique() {
        let s = AgentDefinitionStore::open_memory().unwrap();
        assert!(s.create(new("Bad Name")).is_err());   // spaces/caps
        assert!(s.create(new("9lives")).is_err());      // leading digit
        s.create(new("ok-name")).unwrap();
        assert!(s.create(new("ok-name")).is_err());     // duplicate
    }
}
