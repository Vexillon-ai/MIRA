// SPDX-License-Identifier: AGPL-3.0-or-later

// src/db.rs

//! A tiny, dependency-free SQLite **migration runner** shared by every store.
//!
//! Until now each store hand-rolled its schema as `CREATE TABLE IF NOT EXISTS`
//! plus ad-hoc `ALTER TABLE … ADD COLUMN` (with the duplicate-column error
//! swallowed on re-open). That works but is unversioned, un-ordered, and can't
//! express anything beyond add-column (no data backfills, no table rebuilds,
//! no down-grade guard). This module gives stores an ordered, versioned,
//! transactional migration list instead.
//!
//! ## How it works
//!
//! Applied migrations are tracked in a shared `schema_migrations(namespace,
//! version, …)` table, **not** `PRAGMA user_version` — because MIRA packs many
//! stores' tables into one file (`auth.db`), and `user_version` is per *file*,
//! so a single counter would make stores collide. Each store passes a unique
//! `namespace` (e.g. `"installed_packages"`) + its own ordered list; [`run`]
//! applies every [`Migration`] not yet recorded for that namespace, each in its
//! own transaction. Re-running is a no-op. Versions are dense + 1-based.
//!
//! ## Adopting it on a database that already has an ad-hoc schema
//!
//! An existing DB has no `schema_migrations` rows even though its tables exist.
//! Make migration **v1 the idempotent baseline** — `CREATE TABLE IF NOT EXISTS`
//! for the original schema, and [`add_column_if_missing`] for any column a
//! prior ad-hoc `ALTER` already shipped to live DBs. Running v1 on an existing
//! DB is then a safe no-op that simply records it; fresh DBs get the full
//! schema. Subsequent migrations (v2+) are clean, run-once steps.

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

use crate::MiraError;

/// One ordered schema step. `up` runs inside a transaction; on any error the
/// transaction rolls back and [`run`] aborts (leaving `user_version` at the
/// last fully-applied step).
pub struct Migration {
    /// 1-based, dense, increasing. The applied watermark is stored in
    /// `PRAGMA user_version`.
    pub version: u32,
    /// Human label for logs (e.g. `"add config_json"`).
    pub name: &'static str,
    /// The schema change. Use `tx.execute_batch(...)` or the helpers below.
    pub up: fn(&Transaction<'_>) -> rusqlite::Result<()>,
}

/// Apply every migration for `namespace` not yet recorded in
/// `schema_migrations`. Takes `&mut Connection` so each step runs in a real
/// transaction — call at store-open time, before wrapping in `Arc<Mutex<…>>`.
/// Multiple stores sharing one DB file each pass their own `namespace`.
pub fn run(conn: &mut Connection, namespace: &str, migrations: &[Migration]) -> Result<(), MiraError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            namespace  TEXT NOT NULL,
            version    INTEGER NOT NULL,
            name       TEXT NOT NULL,
            applied_at INTEGER NOT NULL,
            PRIMARY KEY (namespace, version)
        )",
    )
    .map_err(|e| MiraError::DatabaseError(format!("{namespace}: ensure schema_migrations: {e}")))?;

    for (i, m) in migrations.iter().enumerate() {
        debug_assert_eq!(m.version, (i as u32) + 1, "{namespace}: migrations must be dense + 1-based");
        let already: Option<bool> = conn
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE namespace = ?1 AND version = ?2",
                params![namespace, m.version],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| MiraError::DatabaseError(format!("{namespace}: check migration: {e}")))?;
        if already.is_some() {
            continue;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let tx = conn
            .transaction()
            .map_err(|e| MiraError::DatabaseError(format!("{namespace}: begin tx: {e}")))?;
        (m.up)(&tx)
            .map_err(|e| MiraError::DatabaseError(format!("{namespace}: migration v{} ({}): {e}", m.version, m.name)))?;
        tx.execute(
            "INSERT INTO schema_migrations (namespace, version, name, applied_at) VALUES (?1, ?2, ?3, ?4)",
            params![namespace, m.version, m.name, now],
        )
        .map_err(|e| MiraError::DatabaseError(format!("{namespace}: record migration: {e}")))?;
        tx.commit()
            .map_err(|e| MiraError::DatabaseError(format!("{namespace}: commit v{}: {e}", m.version)))?;
        info!("db migrate: {namespace} → v{} ({})", m.version, m.name);
    }
    Ok(())
}

/// The highest applied version for a namespace (0 if none) — for tests/introspection.
pub fn applied_version(conn: &Connection, namespace: &str) -> u32 {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations WHERE namespace = ?1",
        params![namespace],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0) as u32
}

/// `ALTER TABLE <table> ADD COLUMN <column_def>`, tolerating the
/// "duplicate column name" error so a baseline migration is safe on a DB where
/// a prior ad-hoc `ALTER` already added the column. `column_def` is the full
/// definition, e.g. `"config_json TEXT NOT NULL DEFAULT '{}'"`.
pub fn add_column_if_missing(
    tx: &Transaction<'_>,
    table: &str,
    column_def: &str,
) -> rusqlite::Result<()> {
    match tx.execute(&format!("ALTER TABLE {table} ADD COLUMN {column_def}"), []) {
        Ok(_) => Ok(()),
        // SQLite returns this exact message for a duplicate column.
        Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(conn: &Connection) -> u32 {
        applied_version(conn, "t")
    }

    const MIGRATIONS: &[Migration] = &[
        Migration {
            version: 1,
            name: "baseline",
            up: |tx| tx.execute_batch("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, a TEXT NOT NULL)"),
        },
        Migration {
            version: 2,
            name: "add b",
            up: |tx| add_column_if_missing(tx, "t", "b TEXT NOT NULL DEFAULT 'x'"),
        },
    ];

    #[test]
    fn fresh_db_applies_all_and_records_version() {
        let mut c = Connection::open_in_memory().unwrap();
        run(&mut c, "t", MIGRATIONS).unwrap();
        assert_eq!(version(&c), 2);
        // Both columns exist.
        c.execute("INSERT INTO t (a, b) VALUES ('1','2')", []).unwrap();
    }

    #[test]
    fn rerun_is_a_noop() {
        let mut c = Connection::open_in_memory().unwrap();
        run(&mut c, "t", MIGRATIONS).unwrap();
        run(&mut c, "t", MIGRATIONS).unwrap(); // no error, no double-apply
        assert_eq!(version(&c), 2);
    }

    #[test]
    fn baseline_is_safe_on_a_preexisting_adhoc_schema() {
        // Simulate a DB that got the ad-hoc treatment: table + column already
        // there, but no schema_migrations rows (never tracked).
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT NOT NULL);
             ALTER TABLE t ADD COLUMN b TEXT NOT NULL DEFAULT 'x';",
        )
        .unwrap();
        assert_eq!(version(&c), 0);
        // Adopting the migrator must NOT fail on the already-present column.
        run(&mut c, "t", MIGRATIONS).unwrap();
        assert_eq!(version(&c), 2);
    }

    #[test]
    fn partial_then_extended_migration_set() {
        let mut c = Connection::open_in_memory().unwrap();
        run(&mut c, "t", &MIGRATIONS[..1]).unwrap(); // only v1
        assert_eq!(version(&c), 1);
        run(&mut c, "t", MIGRATIONS).unwrap(); // now v1+v2; only v2 runs
        assert_eq!(version(&c), 2);
        c.execute("INSERT INTO t (a, b) VALUES ('1','2')", []).unwrap();
    }

    #[test]
    fn two_namespaces_in_one_file_dont_collide() {
        // The reason we use a table, not PRAGMA user_version: many stores share
        // auth.db. Each namespace tracks its own versions independently.
        let mut c = Connection::open_in_memory().unwrap();
        let other: &[Migration] = &[Migration {
            version: 1,
            name: "other baseline",
            up: |tx| tx.execute_batch("CREATE TABLE other (id INTEGER PRIMARY KEY)"),
        }];
        run(&mut c, "t", MIGRATIONS).unwrap();
        run(&mut c, "other", other).unwrap();
        assert_eq!(applied_version(&c, "t"), 2);
        assert_eq!(applied_version(&c, "other"), 1);
        // Both stores' tables coexist.
        c.execute("INSERT INTO t (a, b) VALUES ('1','2')", []).unwrap();
        c.execute("INSERT INTO other (id) VALUES (1)", []).unwrap();
    }

    #[test]
    fn add_column_if_missing_tolerates_duplicate() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
        let tx = c.transaction().unwrap();
        add_column_if_missing(&tx, "t", "x TEXT").unwrap();
        add_column_if_missing(&tx, "t", "x TEXT").unwrap(); // duplicate → Ok
        tx.commit().unwrap();
    }
}
