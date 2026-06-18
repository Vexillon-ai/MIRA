// SPDX-License-Identifier: AGPL-3.0-or-later

//! SQLite-backed record of installed packages + the **provisioning ledger**.
//!
//! The ledger is the heart of reversibility (design-docs/plugin-packages.md,
//! "Uninstall & teardown"): as a package installs, we record exactly what it
//! created (an `mcp_servers` row, a stored secret, …). Uninstall and cancel
//! both reverse the ledger — so a partial/failed install reverses precisely
//! what it managed to provision.
//!
//! Lives in `auth.db` alongside the other per-feature stores. Packages are
//! admin-installed and identified by their reverse-DNS `id` (one install per
//! id; re-installing the same id is an upsert).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::MiraError;

// One thing an install provisioned. Teardown reverses these in reverse order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerEntry {
    // An `mcp_servers` row this package created (id = that row's uuid).
    McpServer { id: String },
    // A directory of extracted payload files (removed on uninstall).
    Files { dir: String },
    // A secret this package stored in the vault under its package id.
    Secret { key: String },
    // A CPP/External channel account this package created (id = its uuid).
    // (`cpp_provider`); teardown deletes the account row.
    ChannelAccount { id: String },
    // A MIRA-managed provider service this package installed (`mira.write_service`).
    // `unit` is the systemd unit name; teardown stops + removes it.
    Service { unit: String },
}

pub type Ledger = Vec<LedgerEntry>;

// A row in `installed_packages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub id: String,
    pub version: String,
    pub name: String,
    // Trust level label recorded at install time (e.g. "verified", "unsigned").
    pub trust: String,
    pub installed_by: String,
    pub installed_at: i64,
    pub updated_at: i64,
    // What this install provisioned — drives teardown.
    pub ledger: Ledger,
    // The full manifest, kept for re-display and future update diffs.
    pub manifest: serde_json::Value,
    // The install's resolved **non-secret** config values (secrets live in the
    // vault). Seeds an update so the admin re-enters nothing. JSON object.
    #[serde(default)]
    pub config: serde_json::Value,
    // Lifecycle state: `active` (default) or `disabled`. A disabled package's
    // channel account is off + its managed service stopped; the record stays.
    #[serde(default = "default_state")]
    pub state: String,
}

fn default_state() -> String {
    "active".to_string()
}

// Fields needed to record (or re-record) an install.
#[derive(Debug, Clone)]
pub struct NewInstall {
    pub id: String,
    pub version: String,
    pub name: String,
    pub trust: String,
    pub installed_by: String,
    pub ledger: Ledger,
    pub manifest: serde_json::Value,
    // Resolved non-secret config (defaults to `{}` for one-shot installs).
    pub config: serde_json::Value,
}

// Schema migrations for `installed_packages`. v1 is the idempotent baseline
// (safe on DBs that already got the pre-migrator ad-hoc `ALTER`s); v2/v3 add
// the Phase-3 columns via `add_column_if_missing`.
const MIGRATIONS: &[crate::db::Migration] = &[
    crate::db::Migration {
        version: 1,
        name: "baseline installed_packages",
        up: |tx| {
            tx.execute_batch(
                r#"CREATE TABLE IF NOT EXISTS installed_packages (
                    id            TEXT PRIMARY KEY,
                    version       TEXT NOT NULL,
                    name          TEXT NOT NULL,
                    trust         TEXT NOT NULL,
                    manifest_json TEXT NOT NULL,
                    ledger_json   TEXT NOT NULL,
                    installed_by  TEXT NOT NULL,
                    installed_at  INTEGER NOT NULL,
                    updated_at    INTEGER NOT NULL
                );"#,
            )
        },
    },
    crate::db::Migration {
        version: 2,
        name: "add config_json (Phase 3 — seed updates from prior config)",
        up: |tx| crate::db::add_column_if_missing(tx, "installed_packages", "config_json TEXT NOT NULL DEFAULT '{}'"),
    },
    crate::db::Migration {
        version: 3,
        name: "add state (Phase 3 — lifecycle active|disabled)",
        up: |tx| crate::db::add_column_if_missing(tx, "installed_packages", "state TEXT NOT NULL DEFAULT 'active'"),
    },
];

pub struct PackageStore {
    conn: Arc<Mutex<Connection>>,
}

impl PackageStore {
    // Open the store at `path` (typically `<data_dir>/auth.db`), creating the
    // table if needed.
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("create packages DB dir: {e}"))
            })?;
        }
        let mut conn = Connection::open(path)
            .map_err(|e| MiraError::DatabaseError(format!("open packages DB: {e}")))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        crate::db::run(&mut conn, "installed_packages", MIGRATIONS)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }

    // Insert or replace the record for a package id. `installed_at` is
    // preserved across a re-install (upsert) when one already exists.
    pub fn upsert(&self, new: NewInstall) -> Result<InstalledPackage, MiraError> {
        let now = Self::now_ms();
        let ledger_json = serde_json::to_string(&new.ledger)
            .map_err(|e| MiraError::ConfigError(format!("serialise ledger: {e}")))?;
        let manifest_json = serde_json::to_string(&new.manifest)
            .map_err(|e| MiraError::ConfigError(format!("serialise manifest: {e}")))?;
        let config_json = serde_json::to_string(&new.config)
            .map_err(|e| MiraError::ConfigError(format!("serialise config: {e}")))?;

        let conn = self.conn.lock().unwrap();
        let installed_at: i64 = conn
            .query_row(
                "SELECT installed_at FROM installed_packages WHERE id = ?1",
                params![new.id],
                |r| r.get(0),
            )
            .unwrap_or(now);
        conn.execute(
            "INSERT INTO installed_packages
               (id, version, name, trust, manifest_json, ledger_json, config_json, installed_by, installed_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
               version=?2, name=?3, trust=?4, manifest_json=?5, ledger_json=?6,
               config_json=?7, installed_by=?8, updated_at=?10",
            params![
                new.id, new.version, new.name, new.trust, manifest_json, ledger_json,
                config_json, new.installed_by, installed_at, now
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("upsert package: {e}")))?;
        drop(conn);
        self.get(&new.id)?.ok_or_else(|| MiraError::DatabaseError("package vanished after upsert".into()))
    }

    pub fn get(&self, id: &str) -> Result<Option<InstalledPackage>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            "SELECT id, version, name, trust, manifest_json, ledger_json, config_json, installed_by, installed_at, updated_at, state
             FROM installed_packages WHERE id = ?1",
            params![id],
            row_to_pkg,
        );
        match r {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn list(&self) -> Result<Vec<InstalledPackage>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, version, name, trust, manifest_json, ledger_json, config_json, installed_by, installed_at, updated_at, state
                 FROM installed_packages ORDER BY name ASC",
            )
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt
            .query_map([], row_to_pkg)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(out)
    }

    // Set a package's lifecycle state (`active` | `disabled`). No-op if absent.
    pub fn set_state(&self, id: &str, state: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE installed_packages SET state=?1, updated_at=?2 WHERE id=?3",
            params![state, Self::now_ms(), id],
        )
        .map_err(|e| MiraError::DatabaseError(format!("set package state: {e}")))?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM installed_packages WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(format!("delete package: {e}")))?;
        Ok(())
    }
}

fn row_to_pkg(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstalledPackage> {
    let manifest_json: String = row.get(4)?;
    let ledger_json: String = row.get(5)?;
    let config_json: String = row.get(6)?;
    Ok(InstalledPackage {
        id: row.get(0)?,
        version: row.get(1)?,
        name: row.get(2)?,
        trust: row.get(3)?,
        manifest: serde_json::from_str(&manifest_json).unwrap_or(serde_json::Value::Null),
        ledger: serde_json::from_str(&ledger_json).unwrap_or_default(),
        config: serde_json::from_str(&config_json).unwrap_or_else(|_| serde_json::json!({})),
        installed_by: row.get(7)?,
        installed_at: row.get(8)?,
        updated_at: row.get(9)?,
        state: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PackageStore {
        PackageStore::open(Path::new(":memory:")).unwrap()
    }

    fn sample(id: &str) -> NewInstall {
        NewInstall {
            id: id.into(),
            version: "1.0.0".into(),
            name: "Sample".into(),
            trust: "unsigned".into(),
            installed_by: "admin-1".into(),
            ledger: vec![LedgerEntry::McpServer { id: "row-1".into() }],
            manifest: serde_json::json!({ "id": id, "format": "1" }),
            config: serde_json::json!({ "SEND_URL": "https://x" }),
        }
    }

    #[test]
    fn upsert_get_list_delete() {
        let s = store();
        let p = s.upsert(sample("com.x.one")).unwrap();
        assert_eq!(p.id, "com.x.one");
        assert_eq!(p.ledger, vec![LedgerEntry::McpServer { id: "row-1".into() }]);
        // Resolved config persists; state defaults active.
        assert_eq!(p.config.get("SEND_URL").and_then(|v| v.as_str()), Some("https://x"));
        assert_eq!(p.state, "active");
        assert!(s.get("com.x.one").unwrap().is_some());
        assert_eq!(s.list().unwrap().len(), 1);
        s.delete("com.x.one").unwrap();
        assert!(s.get("com.x.one").unwrap().is_none());
    }

    #[test]
    fn set_state_toggles_and_survives_reinstall() {
        let s = store();
        s.upsert(sample("com.x.st")).unwrap();
        s.set_state("com.x.st", "disabled").unwrap();
        assert_eq!(s.get("com.x.st").unwrap().unwrap().state, "disabled");
        // An update (re-upsert) doesn't silently re-enable a disabled package.
        let mut again = sample("com.x.st");
        again.version = "2.0.0".into();
        s.upsert(again).unwrap();
        assert_eq!(s.get("com.x.st").unwrap().unwrap().state, "disabled");
    }

    #[test]
    fn reinstall_preserves_installed_at() {
        let s = store();
        let first = s.upsert(sample("com.x.two")).unwrap();
        // upsert again with a different version + ledger
        let mut again = sample("com.x.two");
        again.version = "2.0.0".into();
        again.ledger = vec![LedgerEntry::Secret { key: "k".into() }];
        let second = s.upsert(again).unwrap();
        assert_eq!(second.version, "2.0.0");
        assert_eq!(second.installed_at, first.installed_at); // preserved
        assert!(second.updated_at >= first.updated_at);
        assert_eq!(second.ledger, vec![LedgerEntry::Secret { key: "k".into() }]);
    }
}
