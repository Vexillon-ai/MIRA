// SPDX-License-Identifier: AGPL-3.0-or-later

// src/mcp/catalog.rs
//! Admin-managed catalog of recommended MCP servers.
//!
//! The `/mcp` page lets any user pick an entry from this catalog to
//! **pre-fill** the add-server form (they review/edit, then save — nothing
//! is spawned until they confirm). The catalog itself is curated by admins:
//! they can add, edit, enable/disable, or remove entries via the admin API,
//! so the list a non-admin sees is whatever the operator has approved.
//!
//! Seeded on first run with a default set (no-key servers + popular keyed
//! ones). Entries marked `requires_key`/with empty `env` values or
//! placeholder paths are expected to be edited in the form before saving.
//!
//! Stored in `auth.db` (the same database as users + mcp_servers). The
//! connection details live in `config_json` exactly like `mcp_servers`, so
//! a catalog entry maps cleanly onto a `NewMcpServer` on the client.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::MiraError;

/// One catalog entry. `name` is the suggested server name; the connection
/// fields mirror `NewMcpServer` so the frontend can drop them straight into
/// the add form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCatalogEntry {
    pub id:          String,
    /// Suggested server name when added (e.g. "everything").
    pub name:        String,
    /// Display title in the catalog (e.g. "Everything — reference / test").
    pub title:       String,
    pub description: String,
    pub transport:   String,            // "stdio" | "http"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command:     Option<String>,
    #[serde(default)]
    pub args:        Vec<String>,
    #[serde(default)]
    pub env:         HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url:         Option<String>,
    /// Hint for the UI: this server needs a credential (env value to fill)
    /// or a path/connection-string edited before it'll work.
    #[serde(default)]
    pub requires_setup: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage:    Option<String>,
    /// Admin toggle — only enabled entries are offered to non-admin users.
    pub enabled:     bool,
    pub sort_order:  i64,
    pub created_at:  i64,
    pub updated_at:  i64,
}

/// What the connection half serialises to in `config_json` (everything
/// except the catalog-management columns).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogConfig {
    transport:      String,
    #[serde(default)]
    command:        Option<String>,
    #[serde(default)]
    args:           Vec<String>,
    #[serde(default)]
    env:            HashMap<String, String>,
    #[serde(default)]
    url:            Option<String>,
    #[serde(default)]
    requires_setup: bool,
    #[serde(default)]
    homepage:       Option<String>,
    #[serde(default)]
    description:    String,
}

/// Fields an admin can submit to create/update an entry.
#[derive(Debug, Clone, Deserialize)]
pub struct UpsertCatalogEntry {
    pub name:        String,
    pub title:       String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_transport")]
    pub transport:   String,
    #[serde(default)]
    pub command:     Option<String>,
    #[serde(default)]
    pub args:        Vec<String>,
    #[serde(default)]
    pub env:         HashMap<String, String>,
    #[serde(default)]
    pub url:         Option<String>,
    #[serde(default)]
    pub requires_setup: bool,
    #[serde(default)]
    pub homepage:    Option<String>,
    #[serde(default = "default_true")]
    pub enabled:     bool,
    #[serde(default)]
    pub sort_order:  i64,
}

fn default_transport() -> String { "stdio".into() }
fn default_true() -> bool { true }

pub struct McpCatalogStore {
    conn: Arc<Mutex<Connection>>,
}

impl McpCatalogStore {
    /// Open at `path` (typically `<data_dir>/auth.db`), create the table,
    /// and seed the default catalog on first run.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create mcp_catalog DB dir: {e}"))
            })?;
        }
        let conn = Connection::open(path)
            .map_err(|e| MiraError::DatabaseError(format!("Cannot open mcp_catalog DB: {e}")))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS mcp_catalog (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                title       TEXT NOT NULL,
                config_json TEXT NOT NULL,
                enabled     INTEGER NOT NULL DEFAULT 1,
                sort_order  INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_mcp_catalog_enabled ON mcp_catalog(enabled);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("mcp_catalog migration failed: {e}")))?;

        let store = Self { conn: Arc::new(Mutex::new(conn)) };
        store.seed_if_empty()?;
        Ok(store)
    }

    fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }

    fn count(&self) -> Result<u64, MiraError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM mcp_catalog", params![], |r| r.get(0))
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(n.max(0) as u64)
    }

    /// Seed the default catalog if the table is empty. Idempotent — a
    /// non-empty table (operator already curated it) is left untouched.
    fn seed_if_empty(&self) -> Result<(), MiraError> {
        if self.count()? > 0 {
            return Ok(());
        }
        for (i, e) in default_catalog().into_iter().enumerate() {
            self.insert(&UpsertCatalogEntry {
                name: e.name, title: e.title, description: e.description,
                transport: e.transport, command: e.command, args: e.args,
                env: e.env, url: e.url, requires_setup: e.requires_setup,
                homepage: e.homepage, enabled: true, sort_order: i as i64,
            })?;
        }
        Ok(())
    }

    fn insert(&self, e: &UpsertCatalogEntry) -> Result<McpCatalogEntry, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let cfg = CatalogConfig {
            transport: e.transport.clone(), command: e.command.clone(), args: e.args.clone(),
            env: e.env.clone(), url: e.url.clone(), requires_setup: e.requires_setup,
            homepage: e.homepage.clone(), description: e.description.clone(),
        };
        let config_json = serde_json::to_string(&cfg)
            .map_err(|e| MiraError::ConfigError(format!("serialize catalog cfg: {e}")))?;
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO mcp_catalog (id, name, title, config_json, enabled, sort_order, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![id, e.name, e.title, config_json, e.enabled as i64, e.sort_order, now],
            ).map_err(|err| MiraError::DatabaseError(format!("insert catalog entry: {err}")))?;
        }
        self.get(&id)?.ok_or_else(|| MiraError::DatabaseError("entry vanished after insert".into()))
    }

    pub fn create(&self, e: UpsertCatalogEntry) -> Result<McpCatalogEntry, MiraError> {
        self.insert(&e)
    }

    pub fn get(&self, id: &str) -> Result<Option<McpCatalogEntry>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            "SELECT id, name, title, config_json, enabled, sort_order, created_at, updated_at
             FROM mcp_catalog WHERE id = ?1",
            params![id], row_to_entry,
        );
        match r {
            Ok(e) => Ok(Some(e)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// Entries offered to a non-admin user (enabled only).
    pub fn list_enabled(&self) -> Result<Vec<McpCatalogEntry>, MiraError> {
        self.query("SELECT id, name, title, config_json, enabled, sort_order, created_at, updated_at
                    FROM mcp_catalog WHERE enabled = 1 ORDER BY sort_order ASC, title ASC")
    }

    /// Every entry, for admin management.
    pub fn list_all(&self) -> Result<Vec<McpCatalogEntry>, MiraError> {
        self.query("SELECT id, name, title, config_json, enabled, sort_order, created_at, updated_at
                    FROM mcp_catalog ORDER BY sort_order ASC, title ASC")
    }

    fn query(&self, sql: &str) -> Result<Vec<McpCatalogEntry>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map(params![], row_to_entry)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?); }
        Ok(out)
    }

    pub fn update(&self, id: &str, e: UpsertCatalogEntry) -> Result<McpCatalogEntry, MiraError> {
        let cfg = CatalogConfig {
            transport: e.transport.clone(), command: e.command.clone(), args: e.args.clone(),
            env: e.env.clone(), url: e.url.clone(), requires_setup: e.requires_setup,
            homepage: e.homepage.clone(), description: e.description.clone(),
        };
        let config_json = serde_json::to_string(&cfg)
            .map_err(|e| MiraError::ConfigError(format!("serialize catalog cfg: {e}")))?;
        {
            let conn = self.conn.lock().unwrap();
            let n = conn.execute(
                "UPDATE mcp_catalog SET name=?2, title=?3, config_json=?4, enabled=?5, sort_order=?6, updated_at=?7
                 WHERE id=?1",
                params![id, e.name, e.title, config_json, e.enabled as i64, e.sort_order, Self::now_ms()],
            ).map_err(|err| MiraError::DatabaseError(format!("update catalog entry: {err}")))?;
            if n == 0 {
                return Err(MiraError::NotFound(format!("catalog entry {id}")));
            }
        }
        self.get(id)?.ok_or_else(|| MiraError::NotFound(format!("catalog entry {id}")))
    }

    /// Toggle just the enabled flag (admin show/hide without a full edit).
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE mcp_catalog SET enabled=?2, updated_at=?3 WHERE id=?1",
            params![id, enabled as i64, Self::now_ms()],
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if n == 0 { return Err(MiraError::NotFound(format!("catalog entry {id}"))); }
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM mcp_catalog WHERE id=?1", params![id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(())
    }
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<McpCatalogEntry> {
    let config_json: String = row.get(3)?;
    let cfg: CatalogConfig = serde_json::from_str(&config_json).unwrap_or(CatalogConfig {
        transport: "stdio".into(), command: None, args: vec![], env: HashMap::new(),
        url: None, requires_setup: false, homepage: None, description: String::new(),
    });
    Ok(McpCatalogEntry {
        id:          row.get(0)?,
        name:        row.get(1)?,
        title:       row.get(2)?,
        description: cfg.description,
        transport:   cfg.transport,
        command:     cfg.command,
        args:        cfg.args,
        env:         cfg.env,
        url:         cfg.url,
        requires_setup: cfg.requires_setup,
        homepage:    cfg.homepage,
        enabled:     row.get::<_, i64>(4)? != 0,
        sort_order:  row.get(5)?,
        created_at:  row.get(6)?,
        updated_at:  row.get(7)?,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Default seed
// ─────────────────────────────────────────────────────────────────────────────

fn entry(
    name: &str, title: &str, desc: &str, transport: &str,
    command: Option<&str>, args: &[&str], env: &[(&str, &str)], url: Option<&str>,
    requires_setup: bool, homepage: &str,
) -> McpCatalogEntry {
    McpCatalogEntry {
        id: String::new(), name: name.into(), title: title.into(), description: desc.into(),
        transport: transport.into(), command: command.map(String::from),
        args: args.iter().map(|s| s.to_string()).collect(),
        env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        url: url.map(String::from), requires_setup,
        homepage: if homepage.is_empty() { None } else { Some(homepage.into()) },
        enabled: true, sort_order: 0, created_at: 0, updated_at: 0,
    }
}

/// The default catalog. npx/uvx resolve via the service PATH (a systemd
/// drop-in adds the nvm + ~/.local/bin dirs); operators on a different
/// layout can edit `command` to an absolute path.
fn default_catalog() -> Vec<McpCatalogEntry> {
    vec![
        // ── No-key ──────────────────────────────────────────────────────────
        entry("everything", "Everything — reference / test",
            "Official reference server. Exercises tools, resources, prompts, sampling, and image content — the best end-to-end test of MIRA's MCP host. Enable sampling on the saved server to test the sampleLLM tool.",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-everything"], &[], None,
            false, "https://github.com/modelcontextprotocol/servers/tree/main/src/everything"),
        entry("filesystem", "Filesystem",
            "Read/write files within a directory you grant. Edit the path argument to the folder you want MIRA to access before saving.",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/dir"], &[], None,
            true, "https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem"),
        entry("memory", "Memory (knowledge graph)",
            "A persistent knowledge-graph memory the agent can read and write entities/relations to.",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-memory"], &[], None,
            false, "https://github.com/modelcontextprotocol/servers/tree/main/src/memory"),
        entry("sequential-thinking", "Sequential Thinking",
            "A structured step-by-step reasoning tool the model can call to think through a problem.",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-sequential-thinking"], &[], None,
            false, "https://github.com/modelcontextprotocol/servers/tree/main/src/sequentialthinking"),
        entry("fetch", "Fetch (URL → markdown)",
            "Fetch a web page and return it as clean markdown. Python — runs via uvx.",
            "stdio", Some("uvx"), &["mcp-server-fetch"], &[], None,
            false, "https://github.com/modelcontextprotocol/servers/tree/main/src/fetch"),
        entry("time", "Time / timezones",
            "Current time and timezone conversion tools. Python — runs via uvx.",
            "stdio", Some("uvx"), &["mcp-server-time"], &[], None,
            false, "https://github.com/modelcontextprotocol/servers/tree/main/src/time"),
        entry("git", "Git",
            "Inspect a local git repository (log, diff, status, show). Edit the --repository path before saving. Python — runs via uvx.",
            "stdio", Some("uvx"), &["mcp-server-git", "--repository", "/path/to/repo"], &[], None,
            true, "https://github.com/modelcontextprotocol/servers/tree/main/src/git"),
        entry("mira-tts", "MIRA Voice (text-to-speech)",
            "MIRA's own voice as an MCP tool: `synthesize` turns text into an audio clip using your configured TTS backend (Kokoro/Piper). No API key. Requires the `mira` binary on PATH; on a dev build edit the command to its absolute path.",
            "stdio", Some("mira"), &["tts", "mcp-serve"], &[], None,
            false, "https://github.com/tarekedOz"),
        entry("puppeteer", "Puppeteer (browser)",
            "Headless-browser automation: navigate, screenshot, click, fill, evaluate JS. MIRA provisions a managed Chrome (~150 MB) into ~/.mira/deps/puppeteer on first connect and points the server at it — so it works on Linux, macOS and Windows (incl. as a service) without a system Chrome. The download runs in the background; browser tools become available once it finishes (a few minutes on first use). Set PUPPETEER_EXECUTABLE_PATH in the env to use your own Chrome instead. Launch args preset for WSL2/containers (--no-sandbox).",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-puppeteer"],
            &[
                ("PUPPETEER_LAUNCH_OPTIONS", "{\"headless\":true,\"args\":[\"--no-sandbox\",\"--disable-dev-shm-usage\"]}"),
                ("ALLOW_DANGEROUS", "true"),
            ], None,
            false, "https://github.com/modelcontextprotocol/servers/tree/main/src/puppeteer"),
        // ── Keyed (fill the credential / connection string before saving) ─────
        entry("github", "GitHub",
            "Repos, issues, PRs, code search. Needs a GitHub personal access token.",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-github"],
            &[("GITHUB_PERSONAL_ACCESS_TOKEN", "")], None,
            true, "https://github.com/modelcontextprotocol/servers/tree/main/src/github"),
        entry("brave-search", "Brave Search",
            "Web + local search via the Brave Search API. Needs a Brave API key.",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-brave-search"],
            &[("BRAVE_API_KEY", "")], None,
            true, "https://github.com/modelcontextprotocol/servers/tree/main/src/brave-search"),
        entry("slack", "Slack",
            "Read channels and post messages. Needs a Slack bot token + team id.",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-slack"],
            &[("SLACK_BOT_TOKEN", ""), ("SLACK_TEAM_ID", "")], None,
            true, "https://github.com/modelcontextprotocol/servers/tree/main/src/slack"),
        entry("gitlab", "GitLab",
            "Projects, issues, MRs. Needs a GitLab personal access token (and API URL for self-hosted).",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-gitlab"],
            &[("GITLAB_PERSONAL_ACCESS_TOKEN", ""), ("GITLAB_API_URL", "https://gitlab.com/api/v4")], None,
            true, "https://github.com/modelcontextprotocol/servers/tree/main/src/gitlab"),
        entry("postgres", "PostgreSQL (read-only)",
            "Run read-only SQL against a Postgres database. Edit the connection-string argument before saving.",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-postgres", "postgresql://user:pass@host:5432/db"], &[], None,
            true, "https://github.com/modelcontextprotocol/servers/tree/main/src/postgres"),
        entry("google-maps", "Google Maps",
            "Geocoding, places, directions. Needs a Google Maps API key.",
            "stdio", Some("npx"), &["-y", "@modelcontextprotocol/server-google-maps"],
            &[("GOOGLE_MAPS_API_KEY", "")], None,
            true, "https://github.com/modelcontextprotocol/servers/tree/main/src/google-maps"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> (tempfile::TempDir, McpCatalogStore) {
        let d = tempdir().unwrap();
        let s = McpCatalogStore::open(&d.path().join("auth.db")).unwrap();
        (d, s)
    }

    #[test]
    fn seeds_defaults_on_first_open() {
        let (_d, s) = store();
        let all = s.list_all().unwrap();
        assert!(all.len() >= 10, "expected the default catalog, got {}", all.len());
        assert!(all.iter().any(|e| e.name == "everything"));
        assert!(all.iter().all(|e| e.enabled));
    }

    #[test]
    fn reopen_does_not_reseed() {
        let d = tempdir().unwrap();
        let path = d.path().join("auth.db");
        let n1 = McpCatalogStore::open(&path).unwrap().list_all().unwrap().len();
        let n2 = McpCatalogStore::open(&path).unwrap().list_all().unwrap().len();
        assert_eq!(n1, n2, "second open must not re-seed");
    }

    #[test]
    fn enabled_filter_and_toggle() {
        let (_d, s) = store();
        let first = s.list_all().unwrap().into_iter().next().unwrap();
        s.set_enabled(&first.id, false).unwrap();
        assert!(s.list_enabled().unwrap().iter().all(|e| e.id != first.id));
        assert!(s.list_all().unwrap().iter().any(|e| e.id == first.id));
    }

    #[test]
    fn crud_round_trip() {
        let (_d, s) = store();
        let created = s.create(UpsertCatalogEntry {
            name: "custom".into(), title: "Custom".into(), description: "x".into(),
            transport: "http".into(), command: None, args: vec![], env: HashMap::new(),
            url: Some("http://localhost:9000/mcp".into()), requires_setup: false,
            homepage: None, enabled: true, sort_order: 99,
        }).unwrap();
        assert_eq!(created.url.as_deref(), Some("http://localhost:9000/mcp"));
        s.delete(&created.id).unwrap();
        assert!(s.get(&created.id).unwrap().is_none());
    }
}
