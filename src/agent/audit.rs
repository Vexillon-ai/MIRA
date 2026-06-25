// SPDX-License-Identifier: AGPL-3.0-or-later

//! Agent audit log (slice B9) — append-only HMAC-chained event log.
//!
//! Every interesting state change in the multi-agent runtime lands as
//! an [`AuditEvent`] row in `agent_audit`. Each row carries the HMAC
//! of `prev_hmac || row_json`, so removing or mutating any past row
//! invalidates the chain from that point forward — `verify_chain`
//! detects either tampering or a missing row.
//!
//! What's recorded today (extends as more layers wire in):
//!   - Spawn requested / approved / denied
//!   - Status changes (Pending → Running → Paused / Completed / …)
//!   - Budget exceeded (per-agent + session)
//!   - Interrupt issued
//!   - Generic policy decisions (granted / denied with reason)
//!
//! The HMAC key is generated on first DB open and stored in a sidecar
//! `meta` table. An attacker with read access can forge new rows
//! consistent with the chain, but any *retroactive* edit / deletion
//! breaks subsequent HMACs and is detected on verify. v1 trade-off
//! that's plenty for the threat model — losing the key locally
//! means we can't sign new rows but can still verify the existing
//! chain (the prev_hmac field is plain content).

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use hmac::{Hmac, Mac};
use rand::RngCore;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::agent::instance::AgentId;
use crate::MiraError;

type HmacSha256 = Hmac<Sha256>;

/// All zeros — sentinel for "this is the first row in the chain."
const GENESIS_HMAC: [u8; 32] = [0u8; 32];

/// What happened. Each variant is JSON-serialised into the `event_json`
/// column; the variant tag drives the indexed `event_kind` column for
/// fast filtering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditEvent {
    /// A worker requested a child spawn — *before* the depth/budget/
    /// resolver checks run. Useful for "who tried to spawn what."
    SpawnRequested {
        skill_id:   String,
        budget_usd: f64,
    },
    /// Spawn approved by the manager loop. `child_id` is the new agent.
    SpawnApproved {
        skill_id: String,
        child_id: AgentId,
    },
    /// Spawn denied. Reason mirrors the SpawnDecision message.
    SpawnDenied {
        skill_id: String,
        reason:   String,
    },
    /// Agent transitioned through the lifecycle. `from`/`to` are the
    /// AgentStatus snake_case strings.
    StatusChange {
        from: String,
        to:   String,
    },
    /// Per-agent budget tripped. `spent` is what the agent had
    /// accumulated when the cap fired.
    AgentBudgetExceeded {
        spent_usd: f64,
        cap_usd:   f64,
    },
    /// Session-wide budget tripped. The agent that observes the kill
    /// switch records this — usually a sibling of whoever pushed the
    /// total over.
    SessionBudgetExceeded {
        session_spent_usd: f64,
        session_cap_usd:   f64,
    },
    /// Interrupt issued (by user, supervisor, or policy).
    Interrupted {
        reason: String,
    },
    /// Forensic record for any policy gate (Phase D) — granted or
    /// denied with a human-readable rationale. v1 uses this only for
    /// the existing budget kills + ad-hoc supervisor decisions.
    PolicyDecision {
        granted: bool,
        rule:    String,
        detail:  Option<String>,
    },
    /// MIRA-Guardian action lifecycle (P4): proposal → operator decision →
    /// execution. Tamper-evident record that the Guardian only ever proposed
    /// and a human approved before anything ran. Recorded under the sentinel
    /// nil `AgentId` (the Guardian isn't a spawned worker).
    GuardianAction {
        action_id:   String,
        /// The bounded action kind (rerun_audit / restart_bridge / …).
        action_kind: String,
        /// proposed | approved | executed | failed | declined.
        decision:    String,
        detail:      Option<String>,
    },
}

impl AuditEvent {
    /// Wire-level kind tag (matches the serde rename for filtering).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SpawnRequested        { .. } => "spawn_requested",
            Self::SpawnApproved         { .. } => "spawn_approved",
            Self::SpawnDenied           { .. } => "spawn_denied",
            Self::StatusChange          { .. } => "status_change",
            Self::AgentBudgetExceeded   { .. } => "agent_budget_exceeded",
            Self::SessionBudgetExceeded { .. } => "session_budget_exceeded",
            Self::Interrupted           { .. } => "interrupted",
            Self::PolicyDecision        { .. } => "policy_decision",
            Self::GuardianAction        { .. } => "guardian_action",
        }
    }
}

/// Sentinel `AgentId` for MIRA-Guardian audit records (the Guardian is a
/// built-in agent, not a spawned worker with a real id).
pub fn guardian_agent_id() -> crate::agent::instance::AgentId {
    crate::agent::instance::AgentId(uuid::Uuid::nil())
}

/// One row read back from storage.
#[derive(Debug, Clone, Serialize)]
pub struct AuditRecord {
    pub id:        i64,
    pub ts_ms:     i64,
    pub agent_id:  AgentId,
    pub event:     AuditEvent,
    /// Initiating user (the agent's `user_id`). `None` for system-initiated
    /// agents (and pre-migration rows). Used as a per-user visibility filter
    /// only — deliberately NOT part of the HMAC chain (see `record`).
    pub user_id:   Option<String>,
    pub prev_hmac: String, // hex
    pub hmac:      String, // hex
}

/// Filters for `AuditStore::query`. Empty = "everything since forever",
/// capped by `limit` (the SQL query always sets one — default 200).
#[derive(Debug, Default)]
pub struct AuditFilter {
    pub agent_id:  Option<AgentId>,
    pub kinds:     Vec<&'static str>,
    pub since_ms:  Option<i64>,
    pub until_ms:  Option<i64>,
    pub limit:     Option<usize>,
    /// Restrict to rows owned by this user_id. When set, system-initiated
    /// rows (`user_id IS NULL`) are excluded — non-admins only see their own
    /// agents' events. `None` = no user filter (admin / unfiltered view).
    pub user_id:   Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit DB error: {0}")]
    Db(String),
    #[error("audit chain broken at row {row}: {reason}")]
    ChainBroken { row: i64, reason: String },
}

impl From<rusqlite::Error> for AuditError {
    fn from(e: rusqlite::Error) -> Self { Self::Db(e.to_string()) }
}

/// SQLite-backed audit store. Cheap to clone via Arc; thread-safe.
pub struct AuditStore {
    conn:     Arc<Mutex<Connection>>,
    hmac_key: [u8; 32],
}

impl AuditStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MiraError::DatabaseError(
                format!("create audit dir {}: {e}", parent.display()),
            ))?;
        }
        let conn = Connection::open(path).map_err(|e| MiraError::DatabaseError(
            format!("open audit DB {}: {e}", path.display()),
        ))?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS agent_audit (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms       INTEGER NOT NULL,
                agent_id    TEXT NOT NULL,
                event_kind  TEXT NOT NULL,
                event_json  TEXT NOT NULL,
                user_id     TEXT,
                prev_hmac   TEXT NOT NULL,
                hmac        TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_agent_audit_agent ON agent_audit(agent_id);
            CREATE INDEX IF NOT EXISTS idx_agent_audit_ts    ON agent_audit(ts_ms);
            CREATE INDEX IF NOT EXISTS idx_agent_audit_kind  ON agent_audit(event_kind);

            CREATE TABLE IF NOT EXISTS agent_audit_meta (
                key         TEXT PRIMARY KEY,
                value_blob  BLOB NOT NULL
            );
            "#,
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        // Idempotent migration for DBs created before the per-user column
        // existed. SQLite has no "ADD COLUMN IF NOT EXISTS", so we add it and
        // swallow the duplicate-column error if it already ran (e.g. a fresh DB
        // that got `user_id` from the CREATE above). Pre-migration rows keep
        // `user_id = NULL` (admin-only visibility).
        if let Err(e) = conn.execute("ALTER TABLE agent_audit ADD COLUMN user_id TEXT", []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(MiraError::DatabaseError(format!("add user_id column: {msg}")));
            }
        }

        let hmac_key = load_or_init_hmac_key(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)), hmac_key })
    }

    /// Test convenience — stores everything in memory, generates a
    /// fresh key. Don't use in production.
    #[cfg(test)]
    pub fn open_in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            r#"
            CREATE TABLE agent_audit (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms       INTEGER NOT NULL,
                agent_id    TEXT NOT NULL,
                event_kind  TEXT NOT NULL,
                event_json  TEXT NOT NULL,
                user_id     TEXT,
                prev_hmac   TEXT NOT NULL,
                hmac        TEXT NOT NULL
            );
            CREATE TABLE agent_audit_meta (
                key TEXT PRIMARY KEY, value_blob BLOB NOT NULL
            );
            "#,
        ).unwrap();
        let hmac_key = load_or_init_hmac_key(&conn).unwrap();
        Self { conn: Arc::new(Mutex::new(conn)), hmac_key }
    }

    /// Append one row. Computes the HMAC against the most recent row's
    /// HMAC (or `GENESIS_HMAC` for the first ever row). Returns the
    /// new row id.
    ///
    /// `user_id` is the initiating user (the agent's `user_id`); `None` for
    /// system-initiated agents. It is stored for per-user visibility filtering
    /// only and is deliberately **NOT** part of the HMAC chain — the chain
    /// inputs (`prev_hmac || ts_ms || agent_id | kind | event_json`) are
    /// unchanged so existing chains keep verifying. `user_id` is a
    /// non-tamper-protected filter field.
    pub fn record(
        &self,
        agent_id: AgentId,
        user_id:  Option<&str>,
        event:    AuditEvent,
    ) -> Result<i64, AuditError> {
        let event_json = serde_json::to_string(&event)
            .map_err(|e| AuditError::Db(format!("serialise event: {e}")))?;
        let kind = event.kind();
        let ts_ms = Utc::now().timestamp_millis();

        let conn = self.conn.lock().expect("audit lock");
        let prev_hmac_hex: String = conn.query_row(
            "SELECT hmac FROM agent_audit ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        ).unwrap_or_else(|_| hex::encode(GENESIS_HMAC));

        let prev_hmac_bytes = hex::decode(&prev_hmac_hex)
            .map_err(|e| AuditError::Db(format!("decode prev_hmac: {e}")))?;

        // NB: user_id is intentionally excluded from the HMAC inputs (it is a
        // filter field, not tamper-protected content).
        let hmac_hex = compute_chain_hmac(
            &self.hmac_key, &prev_hmac_bytes,
            ts_ms, &agent_id, kind, &event_json,
        );

        conn.execute(
            "INSERT INTO agent_audit
               (ts_ms, agent_id, event_kind, event_json, user_id, prev_hmac, hmac)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![ts_ms, agent_id.to_string(), kind, event_json, user_id, prev_hmac_hex, hmac_hex],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Query with optional filters. Newest rows first (id DESC) so the
    /// UI shows the latest activity at the top.
    pub fn query(&self, filter: &AuditFilter) -> Result<Vec<AuditRecord>, AuditError> {
        let mut sql = String::from(
            "SELECT id, ts_ms, agent_id, event_kind, event_json, user_id, prev_hmac, hmac \
             FROM agent_audit WHERE 1=1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(id) = filter.agent_id {
            sql.push_str(" AND agent_id = ?");
            args.push(Box::new(id.to_string()));
        }
        // Per-user scope: when set, only this user's rows (system rows with
        // NULL user_id are excluded — admins query without this filter).
        if let Some(uid) = &filter.user_id {
            sql.push_str(" AND user_id = ?");
            args.push(Box::new(uid.clone()));
        }
        if !filter.kinds.is_empty() {
            let qs = filter.kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND event_kind IN ({qs})"));
            for k in &filter.kinds { args.push(Box::new(k.to_string())); }
        }
        if let Some(since) = filter.since_ms {
            sql.push_str(" AND ts_ms >= ?");
            args.push(Box::new(since));
        }
        if let Some(until) = filter.until_ms {
            sql.push_str(" AND ts_ms <= ?");
            args.push(Box::new(until));
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        args.push(Box::new(filter.limit.unwrap_or(200) as i64));

        let conn = self.conn.lock().expect("audit lock");
        let mut stmt = conn.prepare(&sql)?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(params_dyn), |r| {
            let id:           i64    = r.get(0)?;
            let ts_ms:        i64    = r.get(1)?;
            let agent_id_str: String = r.get(2)?;
            let _kind:        String = r.get(3)?;
            let event_json:   String = r.get(4)?;
            let user_id:      Option<String> = r.get(5)?;
            let prev_hmac:    String = r.get(6)?;
            let hmac:         String = r.get(7)?;
            Ok((id, ts_ms, agent_id_str, event_json, user_id, prev_hmac, hmac))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, ts_ms, agent_id_str, event_json, user_id, prev_hmac, hmac) = row?;
            let agent_uuid = uuid::Uuid::parse_str(&agent_id_str)
                .map_err(|e| AuditError::Db(format!("bad agent_id: {e}")))?;
            let event: AuditEvent = serde_json::from_str(&event_json)
                .map_err(|e| AuditError::Db(format!("deserialise event: {e}")))?;
            out.push(AuditRecord {
                id, ts_ms,
                agent_id: AgentId(agent_uuid),
                event, user_id, prev_hmac, hmac,
            });
        }
        Ok(out)
    }

    /// Walk every row in chain order and verify each row's HMAC links
    /// to the previous. Returns Ok on a clean chain; the first
    /// detected break stops verification with `ChainBroken`.
    pub fn verify_chain(&self) -> Result<(), AuditError> {
        let conn = self.conn.lock().expect("audit lock");
        let mut stmt = conn.prepare(
            "SELECT id, ts_ms, agent_id, event_kind, event_json, prev_hmac, hmac \
             FROM agent_audit ORDER BY id ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut expected_prev_hex = hex::encode(GENESIS_HMAC);

        while let Some(row) = rows.next()? {
            let id:         i64    = row.get(0)?;
            let ts_ms:      i64    = row.get(1)?;
            let agent_str:  String = row.get(2)?;
            let kind:       String = row.get(3)?;
            let event_json: String = row.get(4)?;
            let prev_hmac:  String = row.get(5)?;
            let hmac_hex:   String = row.get(6)?;

            if prev_hmac != expected_prev_hex {
                return Err(AuditError::ChainBroken {
                    row: id,
                    reason: format!("prev_hmac mismatch: row says {prev_hmac}, chain expected {expected_prev_hex}"),
                });
            }

            let agent_uuid = uuid::Uuid::parse_str(&agent_str)
                .map_err(|e| AuditError::Db(format!("bad agent_id: {e}")))?;
            let prev_hmac_bytes = hex::decode(&prev_hmac)
                .map_err(|e| AuditError::Db(format!("decode prev_hmac: {e}")))?;

            let recomputed = compute_chain_hmac(
                &self.hmac_key, &prev_hmac_bytes,
                ts_ms, &AgentId(agent_uuid), &kind, &event_json,
            );
            if recomputed != hmac_hex {
                return Err(AuditError::ChainBroken {
                    row: id,
                    reason: "hmac doesn't match recomputed value (row was tampered with)".into(),
                });
            }
            expected_prev_hex = hmac_hex;
        }
        Ok(())
    }
}

// ─── helpers ───────────────────────────────────────────────────────────

fn load_or_init_hmac_key(conn: &Connection) -> Result<[u8; 32], MiraError> {
    let existing: Option<Vec<u8>> = conn.query_row(
        "SELECT value_blob FROM agent_audit_meta WHERE key = 'hmac_key'",
        [], |r| r.get(0),
    ).ok();
    if let Some(bytes) = existing {
        let arr: [u8; 32] = bytes.as_slice().try_into()
            .map_err(|_| MiraError::DatabaseError("hmac_key in meta is not 32 bytes".into()))?;
        return Ok(arr);
    }

    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    conn.execute(
        "INSERT INTO agent_audit_meta (key, value_blob) VALUES ('hmac_key', ?)",
        params![&key[..]],
    ).map_err(|e| MiraError::DatabaseError(format!("save hmac_key: {e}")))?;
    Ok(key)
}

fn compute_chain_hmac(
    key:        &[u8; 32],
    prev_hmac:  &[u8],
    ts_ms:      i64,
    agent_id:   &AgentId,
    kind:       &str,
    event_json: &str,
) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key length");
    mac.update(prev_hmac);
    mac.update(&ts_ms.to_be_bytes());
    mac.update(agent_id.to_string().as_bytes());
    mac.update(b"|");
    mac.update(kind.as_bytes());
    mac.update(b"|");
    mac.update(event_json.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> AgentId { AgentId::new() }

    #[test]
    fn record_and_query_round_trip() {
        let store = AuditStore::open_in_memory();
        let a = id();
        store.record(a, None, AuditEvent::SpawnRequested {
            skill_id: "com.example.x".into(), budget_usd: 1.0,
        }).unwrap();
        store.record(a, None, AuditEvent::StatusChange {
            from: "pending".into(), to: "running".into(),
        }).unwrap();

        let rows = store.query(&AuditFilter::default()).unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first.
        assert!(matches!(rows[0].event, AuditEvent::StatusChange { .. }));
        assert!(matches!(rows[1].event, AuditEvent::SpawnRequested { .. }));
    }

    #[test]
    fn first_row_chains_to_genesis() {
        let store = AuditStore::open_in_memory();
        store.record(id(), None, AuditEvent::Interrupted { reason: "user".into() }).unwrap();
        let rows = store.query(&AuditFilter::default()).unwrap();
        assert_eq!(rows[0].prev_hmac, hex::encode([0u8; 32]));
    }

    #[test]
    fn each_subsequent_row_chains_to_prior_hmac() {
        let store = AuditStore::open_in_memory();
        store.record(id(), None, AuditEvent::Interrupted { reason: "a".into() }).unwrap();
        store.record(id(), None, AuditEvent::Interrupted { reason: "b".into() }).unwrap();
        store.record(id(), None, AuditEvent::Interrupted { reason: "c".into() }).unwrap();

        let rows = store.query(&AuditFilter::default()).unwrap();
        // Sort oldest-first so we can compare row[i].prev_hmac == row[i-1].hmac.
        let mut rows = rows; rows.sort_by_key(|r| r.id);
        assert_eq!(rows[1].prev_hmac, rows[0].hmac);
        assert_eq!(rows[2].prev_hmac, rows[1].hmac);
    }

    #[test]
    fn verify_chain_passes_on_clean_log() {
        let store = AuditStore::open_in_memory();
        for i in 0..5 {
            store.record(id(), None, AuditEvent::Interrupted { reason: format!("r{i}") }).unwrap();
        }
        store.verify_chain().expect("clean chain verifies");
    }

    #[test]
    fn verify_chain_detects_tampering() {
        let store = AuditStore::open_in_memory();
        store.record(id(), None, AuditEvent::Interrupted { reason: "a".into() }).unwrap();
        store.record(id(), None, AuditEvent::Interrupted { reason: "b".into() }).unwrap();
        // Corrupt row 1 in place.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE agent_audit SET event_json = ? WHERE id = 1",
                params![r#"{"kind":"interrupted","reason":"NOT WHAT WAS SIGNED"}"#],
            ).unwrap();
        }
        let err = store.verify_chain().unwrap_err();
        assert!(matches!(err, AuditError::ChainBroken { .. }), "got {err:?}");
    }

    #[test]
    fn verify_chain_detects_deletion() {
        let store = AuditStore::open_in_memory();
        store.record(id(), None, AuditEvent::Interrupted { reason: "a".into() }).unwrap();
        store.record(id(), None, AuditEvent::Interrupted { reason: "b".into() }).unwrap();
        store.record(id(), None, AuditEvent::Interrupted { reason: "c".into() }).unwrap();
        // Remove row 2 — row 3's prev_hmac no longer matches the row
        // before it (row 1's hmac).
        {
            let conn = store.conn.lock().unwrap();
            conn.execute("DELETE FROM agent_audit WHERE id = 2", []).unwrap();
        }
        let err = store.verify_chain().unwrap_err();
        assert!(matches!(err, AuditError::ChainBroken { .. }), "got {err:?}");
    }

    #[test]
    fn filter_by_agent_id() {
        let store = AuditStore::open_in_memory();
        let a = id();
        let b = id();
        store.record(a, None, AuditEvent::Interrupted { reason: "x".into() }).unwrap();
        store.record(b, None, AuditEvent::Interrupted { reason: "y".into() }).unwrap();
        store.record(a, None, AuditEvent::Interrupted { reason: "z".into() }).unwrap();

        let only_a = store.query(&AuditFilter { agent_id: Some(a), ..Default::default() }).unwrap();
        assert_eq!(only_a.len(), 2);
        for r in only_a { assert_eq!(r.agent_id, a); }
    }

    #[test]
    fn filter_by_kind() {
        let store = AuditStore::open_in_memory();
        let a = id();
        store.record(a, None, AuditEvent::Interrupted { reason: "x".into() }).unwrap();
        store.record(a, None, AuditEvent::StatusChange { from: "running".into(), to: "completed".into() }).unwrap();
        store.record(a, None, AuditEvent::Interrupted { reason: "y".into() }).unwrap();

        let only_status = store.query(&AuditFilter {
            kinds: vec!["status_change"], ..Default::default()
        }).unwrap();
        assert_eq!(only_status.len(), 1);
    }
}
