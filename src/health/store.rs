// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/store.rs
//! Persistence for [`HealthSnapshot`] rows.
//!
//! Snapshots live in a dedicated `health.db` rather than piggybacking
//! on `automations.db` so health audit data stays cleanly separable
//! from automations forensics. Schema is intentionally minimal — one
//! row per audit run with the full snapshot serialised as JSON.
//!
//! Retention: 30 days. Pruned by the same heartbeat that writes new
//! rows; old snapshots are useful for trend analysis but unbounded
//! growth would defeat the purpose.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};

use crate::MiraError;

use super::HealthSnapshot;

/// How long to keep snapshot rows. 30 days × 24 hourly snapshots = 720
/// rows; well within SQLite's comfort zone, and enough trend depth for
/// "is RSS climbing?" style questions on the LLM analysis path.
const SNAPSHOT_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone)]
pub struct HealthStore {
    conn: Arc<Mutex<Connection>>,
}

impl HealthStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MiraError::DatabaseError(
                format!("create health dir {}: {e}", parent.display()),
            ))?;
        }
        let conn = Connection::open(path).map_err(|e| MiraError::DatabaseError(
            format!("open health DB {}: {e}", path.display()),
        ))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS health_snapshots (
                id                       INTEGER PRIMARY KEY AUTOINCREMENT,
                taken_at                 INTEGER NOT NULL,
                duration_ms              INTEGER NOT NULL,
                snapshot_json            TEXT    NOT NULL,
                triggered_signal_count   INTEGER NOT NULL,
                worst_level              TEXT    NOT NULL,
                -- FK to watchdog_incidents.id when the audit filed an
                -- incident. NULL when nothing tripped (most common).
                -- Multiple incidents per audit is allowed — this stores
                -- the first one for quick "what triggered?" lookup;
                -- query watchdog_incidents by source='system_health' for
                -- the full set.
                incident_id              TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_snapshots_taken_at
                ON health_snapshots(taken_at DESC);

            -- 0.107.0 — per-detector policy override. Detectors absent
            -- from this table fall back to NotifyOnly (the slice 1+2
            -- default). One row per detector_name; updated_by tracks
            -- which admin made the change for forensics.
            CREATE TABLE IF NOT EXISTS health_signal_config (
                detector_name  TEXT PRIMARY KEY,
                policy         TEXT NOT NULL,
                note           TEXT,
                updated_at     INTEGER NOT NULL,
                updated_by     TEXT NOT NULL
            );
            "#,
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        // 0.109.0 — additive `snooze_until` column for time-bounded
        // mutes. ALTER TABLE ADD COLUMN with a NULL default is the
        // idempotent migration shape (SQLite has no IF NOT EXISTS).
        let _ = conn.execute(
            "ALTER TABLE health_signal_config ADD COLUMN snooze_until INTEGER",
            [],
        );

        // 0.110.0 — slice 5 tables. Three additive tables, all
        // independently optional — empty rows mean the feature is unused.
        conn.execute_batch(
            r#"
            -- Per-detector threshold overrides. When a row is present,
            -- the collector re-levels the detector's report based on
            -- the value field vs these thresholds (yellow_at + red_at).
            -- Detectors without a numeric value can't be overridden.
            CREATE TABLE IF NOT EXISTS health_thresholds (
                detector_name  TEXT PRIMARY KEY,
                yellow_at      REAL,
                red_at         REAL,
                direction      TEXT NOT NULL DEFAULT 'above',
                updated_at     INTEGER NOT NULL,
                updated_by     TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS health_custom_detectors (
                name           TEXT PRIMARY KEY,
                description    TEXT,
                target_db      TEXT NOT NULL,
                sql            TEXT NOT NULL,
                yellow_at      REAL,
                red_at         REAL,
                direction      TEXT NOT NULL DEFAULT 'above',
                enabled        INTEGER NOT NULL DEFAULT 1,
                created_at     INTEGER NOT NULL,
                updated_at     INTEGER NOT NULL,
                updated_by     TEXT NOT NULL
            );

            -- Outbound webhooks. POST per non-green report (or always
            -- when `levels_csv` includes 'green'). HMAC-SHA256 signs
            -- the body; receiver verifies via `secret`. `enabled=0` skips.
            CREATE TABLE IF NOT EXISTS health_webhooks (
                id             TEXT PRIMARY KEY,
                url            TEXT NOT NULL,
                secret         TEXT,
                levels_csv     TEXT,
                enabled        INTEGER NOT NULL DEFAULT 1,
                description    TEXT,
                created_at     INTEGER NOT NULL,
                updated_at     INTEGER NOT NULL,
                updated_by     TEXT NOT NULL,
                last_fire_at   INTEGER,
                last_status    INTEGER,
                last_error     TEXT
            );

            -- 0.110.0 — per-charge LLM cost ledger. Replaces the
            -- "current burn from running agents" proxy. One row per
            -- non-zero delta from supervisor's worker-progress loop.
            CREATE TABLE IF NOT EXISTS llm_charges (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id    TEXT NOT NULL,
                user_id     TEXT,
                usd         REAL NOT NULL,
                charged_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_llm_charges_at ON llm_charges(charged_at DESC);
            CREATE INDEX IF NOT EXISTS idx_llm_charges_user ON llm_charges(user_id, charged_at DESC);
            "#,
        ).map_err(|e| MiraError::DatabaseError(format!("0.110 schema: {e}")))?;

        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE health_snapshots (
                id                       INTEGER PRIMARY KEY AUTOINCREMENT,
                taken_at                 INTEGER NOT NULL,
                duration_ms              INTEGER NOT NULL,
                snapshot_json            TEXT    NOT NULL,
                triggered_signal_count   INTEGER NOT NULL,
                worst_level              TEXT    NOT NULL,
                incident_id              TEXT
            );
            CREATE TABLE health_signal_config (
                detector_name  TEXT PRIMARY KEY,
                policy         TEXT NOT NULL,
                note           TEXT,
                updated_at     INTEGER NOT NULL,
                updated_by     TEXT NOT NULL,
                snooze_until   INTEGER
            );
            CREATE TABLE health_thresholds (
                detector_name  TEXT PRIMARY KEY,
                yellow_at      REAL,
                red_at         REAL,
                direction      TEXT NOT NULL DEFAULT 'above',
                updated_at     INTEGER NOT NULL,
                updated_by     TEXT NOT NULL
            );
            CREATE TABLE health_custom_detectors (
                name           TEXT PRIMARY KEY,
                description    TEXT,
                target_db      TEXT NOT NULL,
                sql            TEXT NOT NULL,
                yellow_at      REAL,
                red_at         REAL,
                direction      TEXT NOT NULL DEFAULT 'above',
                enabled        INTEGER NOT NULL DEFAULT 1,
                created_at     INTEGER NOT NULL,
                updated_at     INTEGER NOT NULL,
                updated_by     TEXT NOT NULL
            );
            CREATE TABLE health_webhooks (
                id             TEXT PRIMARY KEY,
                url            TEXT NOT NULL,
                secret         TEXT,
                levels_csv     TEXT,
                enabled        INTEGER NOT NULL DEFAULT 1,
                description    TEXT,
                created_at     INTEGER NOT NULL,
                updated_at     INTEGER NOT NULL,
                updated_by     TEXT NOT NULL,
                last_fire_at   INTEGER,
                last_status    INTEGER,
                last_error     TEXT
            );
            CREATE TABLE llm_charges (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id    TEXT NOT NULL,
                user_id     TEXT,
                usd         REAL NOT NULL,
                charged_at  INTEGER NOT NULL
            );
            "#,
        ).unwrap();
        Self { conn: Arc::new(Mutex::new(conn)) }
    }

    /// Insert a fresh snapshot row. `incident_id` is the first incident
    /// the collector filed for this run, or None when nothing tripped.
    pub fn record(
        &self, snap: &HealthSnapshot, incident_id: Option<&str>,
    ) -> Result<i64, MiraError> {
        let json = serde_json::to_string(snap)
            .map_err(|e| MiraError::DatabaseError(format!("serialise snapshot: {e}")))?;
        let triggered = snap.triggered_count() as i64;
        let worst = snap.worst_level().as_str();
        let conn = self.conn.lock().expect("health lock");
        conn.execute(
            "INSERT INTO health_snapshots
               (taken_at, duration_ms, snapshot_json, triggered_signal_count, worst_level, incident_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![snap.taken_at, snap.duration_ms as i64, json, triggered, worst, incident_id],
        ).map_err(|e| MiraError::DatabaseError(format!("insert snapshot: {e}")))?;
        Ok(conn.last_insert_rowid())
    }

    /// Most recent N snapshots, newest first. Used by the (future) UI.
    pub fn list_recent(&self, limit: usize) -> Result<Vec<HealthSnapshot>, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let mut stmt = conn.prepare(
            "SELECT snapshot_json FROM health_snapshots
              ORDER BY taken_at DESC LIMIT ?1",
        ).map_err(|e| MiraError::DatabaseError(format!("list_recent prep: {e}")))?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let s: String = r.get(0)?;
            Ok(s)
        }).map_err(|e| MiraError::DatabaseError(format!("list_recent q: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            let s = row.map_err(|e| MiraError::DatabaseError(e.to_string()))?;
            let snap: HealthSnapshot = serde_json::from_str(&s)
                .map_err(|e| MiraError::DatabaseError(format!("parse snapshot: {e}")))?;
            out.push(snap);
        }
        Ok(out)
    }

    /// Whether the collector should suppress filing a fresh incident
    /// for this detector. Returns true when an incident with the
    /// signal-fingerprint was already filed within the last
    /// `dedup_window_secs`. Same idea as the watchdog's in-memory
    /// fingerprint dedup, but cross-restart by going through the
    /// `watchdog_incidents` table on `automations.db`.
    pub fn was_recently_filed(
        automations: &crate::automations::AutomationsStore,
        fingerprint: &str,
        dedup_window_secs: i64,
        now: i64,
    ) -> Result<bool, MiraError> {
        let n = automations.count_incidents_by_fingerprint_since(
            fingerprint, now - dedup_window_secs,
        )?;
        Ok(n > 0)
    }

    /// Drop snapshot rows older than the retention window. Returns the
    /// number of rows pruned. Called from the heartbeat after each
    /// successful insert so the table stays roughly bounded.
    pub fn prune_old(&self, now: i64) -> Result<usize, MiraError> {
        let cutoff = now - SNAPSHOT_RETENTION_SECS;
        let conn = self.conn.lock().expect("health lock");
        let n = conn.execute(
            "DELETE FROM health_snapshots WHERE taken_at < ?1",
            params![cutoff],
        ).map_err(|e| MiraError::DatabaseError(format!("prune snapshots: {e}")))?;
        Ok(n)
    }

    // ── 0.107.0 — snapshot summaries (sparkline data) ────────────────

    /// One row per snapshot since `since`, oldest first. Lightweight —
    /// no full snapshot_json payload. Used by the dashboard's history
    /// endpoint to render trend lines without shipping the full
    /// per-detector blob for every point.
    pub fn list_summaries_since(
        &self, since: i64,
    ) -> Result<Vec<SnapshotSummary>, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let mut stmt = conn.prepare(
            "SELECT taken_at, duration_ms, triggered_signal_count, worst_level, incident_id
               FROM health_snapshots
              WHERE taken_at >= ?1
              ORDER BY taken_at ASC",
        ).map_err(|e| MiraError::DatabaseError(format!("list_summaries prep: {e}")))?;
        let rows = stmt.query_map(params![since], |r| {
            Ok(SnapshotSummary {
                taken_at:               r.get(0)?,
                duration_ms:            r.get::<_, i64>(1)? as u64,
                triggered_signal_count: r.get::<_, i64>(2)? as u64,
                worst_level:            r.get(3)?,
                incident_id:            r.get(4)?,
            })
        }).map_err(|e| MiraError::DatabaseError(format!("list_summaries q: {e}")))?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(|e| MiraError::DatabaseError(format!("list_summaries rows: {e}")))?;
        Ok(rows)
    }

    /// Latest snapshot or None if the heartbeat has never run.
    pub fn latest(&self) -> Result<Option<HealthSnapshot>, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let row: Option<String> = conn.query_row(
            "SELECT snapshot_json FROM health_snapshots
              ORDER BY taken_at DESC LIMIT 1",
            [], |r| r.get(0),
        ).ok();
        match row {
            None    => Ok(None),
            Some(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(|e| MiraError::DatabaseError(format!("parse latest snapshot: {e}"))),
        }
    }

    // ── 0.107.0 — per-signal policy config ───────────────────────────

    /// Read the configured policy for one detector. Returns None when
    /// no override exists (collector falls back to NotifyOnly).
    pub fn get_signal_policy(
        &self, detector_name: &str,
    ) -> Result<Option<super::ActionPolicy>, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let row: Option<String> = conn.query_row(
            "SELECT policy FROM health_signal_config WHERE detector_name = ?1",
            params![detector_name],
            |r| r.get(0),
        ).ok();
        Ok(row.and_then(|s| parse_policy(&s)))
    }

    /// Read every configured override. Used by the GET /config handler
    /// (UI builds a row per known detector by merging this map with the
    /// runtime detector list).
    pub fn list_signal_configs(&self) -> Result<Vec<HealthSignalConfigRow>, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let mut stmt = conn.prepare(
            "SELECT detector_name, policy, note, updated_at, updated_by, snooze_until
               FROM health_signal_config
              ORDER BY detector_name ASC",
        ).map_err(|e| MiraError::DatabaseError(format!("list_signal_configs prep: {e}")))?;
        let rows = stmt.query_map([], |r| {
            Ok(HealthSignalConfigRow {
                detector_name: r.get(0)?,
                policy:        r.get(1)?,
                note:          r.get(2)?,
                updated_at:    r.get(3)?,
                updated_by:    r.get(4)?,
                snooze_until:  r.get(5)?,
            })
        }).map_err(|e| MiraError::DatabaseError(format!("list_signal_configs q: {e}")))?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(|e| MiraError::DatabaseError(format!("list_signal_configs rows: {e}")))?;
        Ok(rows)
    }

    /// Set or update one detector's policy. Idempotent — re-applying
    /// the same value is harmless. `updated_by` should be the admin
    /// user's id for audit. `snooze_until=Some(t)` sets a time-bounded
    /// mute (collector treats the policy as `Disabled` until `t`, then
    /// reverts to `policy`); `snooze_until=None` clears the snooze.
    pub fn upsert_signal_config(
        &self, detector_name: &str, policy: super::ActionPolicy,
        note: Option<&str>, updated_by: &str, snooze_until: Option<i64>,
    ) -> Result<(), MiraError> {
        let now = chrono::Utc::now().timestamp();
        let policy_str = match policy {
            super::ActionPolicy::Disabled    => "disabled",
            super::ActionPolicy::NotifyOnly  => "notify_only",
            super::ActionPolicy::AutoCleanup => "auto_cleanup",
        };
        let conn = self.conn.lock().expect("health lock");
        conn.execute(
            "INSERT INTO health_signal_config (detector_name, policy, note, updated_at, updated_by, snooze_until)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(detector_name) DO UPDATE
                SET policy       = excluded.policy,
                    note         = excluded.note,
                    updated_at   = excluded.updated_at,
                    updated_by   = excluded.updated_by,
                    snooze_until = excluded.snooze_until",
            params![detector_name, policy_str, note, now, updated_by, snooze_until],
        ).map_err(|e| MiraError::DatabaseError(format!("upsert_signal_config: {e}")))?;
        Ok(())
    }

    /// 0.109.0 — Sweep expired snoozes, returning the count cleared.
    /// Called by the collector at the start of each audit so a snoozed
    /// detector reverts to its declared policy without a separate
    /// heartbeat. Cheap: single conditional UPDATE.
    pub fn clear_expired_snoozes(&self) -> Result<usize, MiraError> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().expect("health lock");
        let n = conn.execute(
            "UPDATE health_signal_config
                SET snooze_until = NULL
              WHERE snooze_until IS NOT NULL AND snooze_until <= ?1",
            params![now],
        ).map_err(|e| MiraError::DatabaseError(format!("clear_expired_snoozes: {e}")))?;
        Ok(n)
    }

    /// Delete one override row, reverting the detector to default
    /// (NotifyOnly) without leaving an explicit `notify_only` record.
    pub fn clear_signal_config(&self, detector_name: &str) -> Result<bool, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let n = conn.execute(
            "DELETE FROM health_signal_config WHERE detector_name = ?1",
            params![detector_name],
        ).map_err(|e| MiraError::DatabaseError(format!("clear_signal_config: {e}")))?;
        Ok(n > 0)
    }
}

/// Lightweight summary row — just enough to render a sparkline chart
/// without shipping the full snapshot blob.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapshotSummary {
    pub taken_at:               i64,
    pub duration_ms:            u64,
    pub triggered_signal_count: u64,
    pub worst_level:            String,
    pub incident_id:            Option<String>,
}

// ── 0.110.0 — slice 5 row types ──────────────────────────────────────────────

/// Per-detector threshold override.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThresholdRow {
    pub detector_name: String,
    pub yellow_at:     Option<f64>,
    pub red_at:        Option<f64>,
    /// "above" or "below". `above` is the default (most detectors —
    /// bigger value = worse).
    pub direction:     String,
    pub updated_at:    i64,
    pub updated_by:    String,
}

/// One custom SQL detector. Evaluated by the collector at audit time.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CustomDetectorRow {
    pub name:        String,
    pub description: Option<String>,
    pub target_db:   String,
    pub sql:         String,
    pub yellow_at:   Option<f64>,
    pub red_at:      Option<f64>,
    pub direction:   String,
    pub enabled:     bool,
    pub created_at:  i64,
    pub updated_at:  i64,
    pub updated_by:  String,
}

/// Outbound webhook target.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebhookRow {
    pub id:          String,
    pub url:         String,
    pub secret:      Option<String>,
    pub levels_csv:  Option<String>,
    pub enabled:     bool,
    pub description: Option<String>,
    pub created_at:  i64,
    pub updated_at:  i64,
    pub updated_by:  String,
    pub last_fire_at: Option<i64>,
    pub last_status:  Option<i64>,
    pub last_error:   Option<String>,
}

impl HealthStore {
    // ── thresholds ─────────────────────────────────────────────────

    pub fn list_thresholds(&self) -> Result<Vec<ThresholdRow>, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let mut stmt = conn.prepare(
            "SELECT detector_name, yellow_at, red_at, direction, updated_at, updated_by
               FROM health_thresholds ORDER BY detector_name",
        ).map_err(|e| MiraError::DatabaseError(format!("list_thresholds prep: {e}")))?;
        let rows = stmt.query_map([], |r| Ok(ThresholdRow {
            detector_name: r.get(0)?,
            yellow_at:     r.get(1)?,
            red_at:        r.get(2)?,
            direction:     r.get(3)?,
            updated_at:    r.get(4)?,
            updated_by:    r.get(5)?,
        })).map_err(|e| MiraError::DatabaseError(format!("list_thresholds q: {e}")))?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(|e| MiraError::DatabaseError(format!("list_thresholds rows: {e}")))?;
        Ok(rows)
    }

    pub fn upsert_threshold(
        &self, detector_name: &str, yellow_at: Option<f64>, red_at: Option<f64>,
        direction: &str, updated_by: &str,
    ) -> Result<(), MiraError> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().expect("health lock");
        conn.execute(
            "INSERT INTO health_thresholds (detector_name, yellow_at, red_at, direction, updated_at, updated_by)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(detector_name) DO UPDATE
                SET yellow_at  = excluded.yellow_at,
                    red_at     = excluded.red_at,
                    direction  = excluded.direction,
                    updated_at = excluded.updated_at,
                    updated_by = excluded.updated_by",
            params![detector_name, yellow_at, red_at, direction, now, updated_by],
        ).map_err(|e| MiraError::DatabaseError(format!("upsert_threshold: {e}")))?;
        Ok(())
    }

    pub fn clear_threshold(&self, detector_name: &str) -> Result<bool, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let n = conn.execute(
            "DELETE FROM health_thresholds WHERE detector_name = ?1",
            params![detector_name],
        ).map_err(|e| MiraError::DatabaseError(format!("clear_threshold: {e}")))?;
        Ok(n > 0)
    }

    // ── custom detectors ──────────────────────────────────────────

    pub fn list_custom_detectors(&self) -> Result<Vec<CustomDetectorRow>, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let mut stmt = conn.prepare(
            "SELECT name, description, target_db, sql, yellow_at, red_at,
                    direction, enabled, created_at, updated_at, updated_by
               FROM health_custom_detectors ORDER BY name",
        ).map_err(|e| MiraError::DatabaseError(format!("list_custom prep: {e}")))?;
        let rows = stmt.query_map([], |r| Ok(CustomDetectorRow {
            name:        r.get(0)?,
            description: r.get(1)?,
            target_db:   r.get(2)?,
            sql:         r.get(3)?,
            yellow_at:   r.get(4)?,
            red_at:      r.get(5)?,
            direction:   r.get(6)?,
            enabled:     r.get::<_, i64>(7)? != 0,
            created_at:  r.get(8)?,
            updated_at:  r.get(9)?,
            updated_by:  r.get(10)?,
        })).map_err(|e| MiraError::DatabaseError(format!("list_custom q: {e}")))?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(|e| MiraError::DatabaseError(format!("list_custom rows: {e}")))?;
        Ok(rows)
    }

    pub fn upsert_custom_detector(
        &self, row: &CustomDetectorRow,
    ) -> Result<(), MiraError> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().expect("health lock");
        conn.execute(
            "INSERT INTO health_custom_detectors
               (name, description, target_db, sql, yellow_at, red_at,
                direction, enabled, created_at, updated_at, updated_by)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, ?10), ?10, ?11)
             ON CONFLICT(name) DO UPDATE
                SET description = excluded.description,
                    target_db   = excluded.target_db,
                    sql         = excluded.sql,
                    yellow_at   = excluded.yellow_at,
                    red_at      = excluded.red_at,
                    direction   = excluded.direction,
                    enabled     = excluded.enabled,
                    updated_at  = excluded.updated_at,
                    updated_by  = excluded.updated_by",
            params![
                row.name, row.description, row.target_db, row.sql,
                row.yellow_at, row.red_at, row.direction,
                if row.enabled { 1 } else { 0 },
                if row.created_at == 0 { None } else { Some(row.created_at) }, now, row.updated_by,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("upsert_custom: {e}")))?;
        Ok(())
    }

    pub fn delete_custom_detector(&self, name: &str) -> Result<bool, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let n = conn.execute(
            "DELETE FROM health_custom_detectors WHERE name = ?1",
            params![name],
        ).map_err(|e| MiraError::DatabaseError(format!("delete_custom: {e}")))?;
        Ok(n > 0)
    }

    // ── webhooks ──────────────────────────────────────────────────

    pub fn list_webhooks(&self) -> Result<Vec<WebhookRow>, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let mut stmt = conn.prepare(
            "SELECT id, url, secret, levels_csv, enabled, description,
                    created_at, updated_at, updated_by,
                    last_fire_at, last_status, last_error
               FROM health_webhooks ORDER BY created_at DESC",
        ).map_err(|e| MiraError::DatabaseError(format!("list_webhooks prep: {e}")))?;
        let rows = stmt.query_map([], |r| Ok(WebhookRow {
            id:          r.get(0)?,
            url:         r.get(1)?,
            secret:      r.get(2)?,
            levels_csv:  r.get(3)?,
            enabled:     r.get::<_, i64>(4)? != 0,
            description: r.get(5)?,
            created_at:  r.get(6)?,
            updated_at:  r.get(7)?,
            updated_by:  r.get(8)?,
            last_fire_at: r.get(9)?,
            last_status:  r.get(10)?,
            last_error:   r.get(11)?,
        })).map_err(|e| MiraError::DatabaseError(format!("list_webhooks q: {e}")))?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(|e| MiraError::DatabaseError(format!("list_webhooks rows: {e}")))?;
        Ok(rows)
    }

    pub fn upsert_webhook(&self, row: &WebhookRow) -> Result<(), MiraError> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().expect("health lock");
        conn.execute(
            "INSERT INTO health_webhooks
               (id, url, secret, levels_csv, enabled, description,
                created_at, updated_at, updated_by)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(?7, ?8), ?8, ?9)
             ON CONFLICT(id) DO UPDATE
                SET url = excluded.url, secret = excluded.secret,
                    levels_csv = excluded.levels_csv, enabled = excluded.enabled,
                    description = excluded.description,
                    updated_at = excluded.updated_at, updated_by = excluded.updated_by",
            params![
                row.id, row.url, row.secret, row.levels_csv,
                if row.enabled { 1 } else { 0 }, row.description,
                if row.created_at == 0 { None } else { Some(row.created_at) }, now, row.updated_by,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("upsert_webhook: {e}")))?;
        Ok(())
    }

    pub fn delete_webhook(&self, id: &str) -> Result<bool, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let n = conn.execute(
            "DELETE FROM health_webhooks WHERE id = ?1",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("delete_webhook: {e}")))?;
        Ok(n > 0)
    }

    /// Stamp a webhook's most recent delivery result. Called from the
    /// fan-out task; failures recorded but never bubbled out (a broken
    /// webhook shouldn't break the audit).
    pub fn record_webhook_fire(
        &self, id: &str, http_status: Option<i64>, error: Option<&str>,
    ) -> Result<(), MiraError> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().expect("health lock");
        conn.execute(
            "UPDATE health_webhooks
                SET last_fire_at = ?1, last_status = ?2, last_error = ?3
              WHERE id = ?4",
            params![now, http_status, error, id],
        ).map_err(|e| MiraError::DatabaseError(format!("record_webhook_fire: {e}")))?;
        Ok(())
    }

    // ── 0.110.0 — LLM cost ledger ─────────────────────────────────

    /// Record one charge. `delta_usd` should be > 0; zero/negative
    /// charges are silently dropped to keep the ledger meaningful.
    pub fn record_llm_charge(
        &self, agent_id: &str, user_id: Option<&str>, delta_usd: f64,
    ) -> Result<(), MiraError> {
        if delta_usd <= 0.0 { return Ok(()); }
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().expect("health lock");
        conn.execute(
            "INSERT INTO llm_charges (agent_id, user_id, usd, charged_at)
                  VALUES (?1, ?2, ?3, ?4)",
            params![agent_id, user_id, delta_usd, now],
        ).map_err(|e| MiraError::DatabaseError(format!("record_llm_charge: {e}")))?;
        Ok(())
    }

    /// Sum of charges since `since`. Used by the cost detector. None
    /// when the table is empty for the window.
    pub fn sum_llm_charges_since(&self, since: i64) -> Result<f64, MiraError> {
        let conn = self.conn.lock().expect("health lock");
        let total: Option<f64> = conn.query_row(
            "SELECT SUM(usd) FROM llm_charges WHERE charged_at >= ?1",
            params![since], |r| r.get(0),
        ).ok();
        Ok(total.unwrap_or(0.0))
    }

    /// Drop ledger rows older than 30 days. Called from the existing
    /// snapshot prune path so the ledger doesn't grow without bound.
    pub fn prune_old_charges(&self, now: i64) -> Result<usize, MiraError> {
        let cutoff = now - 30 * 24 * 60 * 60;
        let conn = self.conn.lock().expect("health lock");
        let n = conn.execute(
            "DELETE FROM llm_charges WHERE charged_at < ?1",
            params![cutoff],
        ).map_err(|e| MiraError::DatabaseError(format!("prune_old_charges: {e}")))?;
        Ok(n)
    }
}

/// One row of `health_signal_config`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthSignalConfigRow {
    pub detector_name: String,
    pub policy:        String,
    pub note:          Option<String>,
    pub updated_at:    i64,
    pub updated_by:    String,
    /// 0.109.0 — when set and in the future, the collector overrides
    /// `policy` to `Disabled` until this unix timestamp passes.
    pub snooze_until:  Option<i64>,
}

fn parse_policy(s: &str) -> Option<super::ActionPolicy> {
    match s {
        "disabled"     => Some(super::ActionPolicy::Disabled),
        "notify_only"  => Some(super::ActionPolicy::NotifyOnly),
        "auto_cleanup" => Some(super::ActionPolicy::AutoCleanup),
        _              => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{DetectorReport, HealthLevel};

    fn snap(level: HealthLevel) -> HealthSnapshot {
        HealthSnapshot {
            taken_at: 100,
            duration_ms: 5,
            reports: vec![DetectorReport {
                name: "test.x".into(), level,
                message: "ok".into(), value: None,
                payload: serde_json::Value::Null,
                auto_action_eligible: false,
                analytics: None,
            }],
        }
    }

    #[test]
    fn record_round_trips() {
        let s = HealthStore::open_in_memory();
        let id = s.record(&snap(HealthLevel::Green), None).unwrap();
        assert!(id > 0);
        let recent = s.list_recent(5).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].reports[0].name, "test.x");
    }

    #[test]
    fn prune_old_removes_aged_rows() {
        let s = HealthStore::open_in_memory();
        let mut old = snap(HealthLevel::Green);
        old.taken_at = 0;
        s.record(&old, None).unwrap();
        let mut fresh = snap(HealthLevel::Green);
        fresh.taken_at = SNAPSHOT_RETENTION_SECS + 10;
        s.record(&fresh, None).unwrap();
        let pruned = s.prune_old(SNAPSHOT_RETENTION_SECS + 10).unwrap();
        assert_eq!(pruned, 1);
        let recent = s.list_recent(10).unwrap();
        assert_eq!(recent.len(), 1);
    }
}
