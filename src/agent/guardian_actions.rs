// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/guardian_actions.rs

//! **MIRA-Guardian action proposals** (P4). When the Guardian (in `active` mode)
//! decides a bounded, reversible remediation is warranted, it does **not**
//! execute — it records a *pending proposal* here and alerts the operator. A
//! human approves (web/channel) and only then does deterministic server code
//! execute the action (P4a-2). The LLM can only ever *propose*; it has no
//! direct restart/requeue tool. This store is the durable record of every
//! proposal + its decision + outcome (complementing the HMAC audit chain).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::MiraError;

/// The bounded, reversible, service-restoration action set (§4.2). Deliberately
/// small; never destructive. Each maps to an existing in-process operation at
/// execution time (P4a-2): re-run/requeue go through the automations scheduler,
/// restart-bridge through the ChannelManager. There is intentionally NO shell,
/// config-write, data-delete, or self-restart kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardianActionKind {
    /// Re-run the health audit now (idempotent).
    RerunAudit,
    /// Restart a wedged channel bridge. `target` = channel account id.
    RestartBridge,
    /// Requeue a stuck/dormant system schedule. `target` = schedule name.
    RequeueAutomation,
    /// Trim already-rotated logs to relieve disk pressure.
    TrimLogs,
}

impl GuardianActionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RerunAudit        => "rerun_audit",
            Self::RestartBridge     => "restart_bridge",
            Self::RequeueAutomation => "requeue_automation",
            Self::TrimLogs          => "trim_logs",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rerun_audit"        => Some(Self::RerunAudit),
            "restart_bridge"     => Some(Self::RestartBridge),
            "requeue_automation" => Some(Self::RequeueAutomation),
            "trim_logs"          => Some(Self::TrimLogs),
            _ => None,
        }
    }
    /// Whether this kind needs a non-empty `target` to act on.
    pub fn needs_target(&self) -> bool {
        matches!(self, Self::RestartBridge | Self::RequeueAutomation)
    }
    pub fn all() -> &'static [&'static str] {
        &["rerun_audit", "restart_bridge", "requeue_automation", "trim_logs"]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardianActionStatus {
    /// Proposed, awaiting an operator decision.
    Pending,
    /// Operator declined — never executed.
    Declined,
    /// Approved + executed successfully.
    Executed,
    /// Approved but execution failed.
    Failed,
}

impl GuardianActionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending  => "pending",
            Self::Declined => "declined",
            Self::Executed => "executed",
            Self::Failed   => "failed",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "declined" => Self::Declined,
            "executed" => Self::Executed,
            "failed"   => Self::Failed,
            _          => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardianAction {
    pub id:         String,
    pub kind:       GuardianActionKind,
    pub target:     Option<String>,
    pub reason:     String,
    pub status:     GuardianActionStatus,
    pub created_at: i64,
    pub decided_at: Option<i64>,
    /// Outcome text once executed/failed (or the decline note).
    pub result:     Option<String>,
}

const MIGRATIONS: &[crate::db::Migration] = &[crate::db::Migration {
    version: 1,
    name: "create guardian_actions",
    up: |tx| {
        tx.execute_batch(
            r#"CREATE TABLE IF NOT EXISTS guardian_actions (
                id          TEXT PRIMARY KEY,
                kind        TEXT NOT NULL,
                target      TEXT,
                reason      TEXT NOT NULL DEFAULT '',
                status      TEXT NOT NULL DEFAULT 'pending',
                created_at  INTEGER NOT NULL,
                decided_at  INTEGER,
                result      TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_guardian_actions_status
                ON guardian_actions(status, created_at DESC);"#,
        )?;
        Ok(())
    },
}];

pub struct GuardianActionStore {
    conn: Arc<Mutex<Connection>>,
}

impl GuardianActionStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MiraError::DatabaseError(format!("create guardian-actions dir: {e}")))?;
        }
        let mut conn = Connection::open(path)
            .map_err(|e| MiraError::DatabaseError(format!("open guardian-actions DB: {e}")))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        crate::db::run(&mut conn, "guardian_actions", MIGRATIONS)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self, MiraError> { Self::open(Path::new(":memory:")) }

    fn now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    /// Record a new pending proposal. Returns its id.
    pub fn create_pending(
        &self, kind: GuardianActionKind, target: Option<&str>, reason: &str,
    ) -> Result<String, MiraError> {
        let id = Uuid::now_v7().to_string();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO guardian_actions (id, kind, target, reason, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![id, kind.as_str(), target, reason, Self::now()],
        ).map_err(|e| MiraError::DatabaseError(format!("create guardian action: {e}")))?;
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Result<Option<GuardianAction>, MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, kind, target, reason, status, created_at, decided_at, result
             FROM guardian_actions WHERE id = ?1",
            params![id], row_to_action,
        ).optional().map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    /// List actions, most-recent first. `status` filters when provided.
    pub fn list(&self, status: Option<GuardianActionStatus>, limit: i64)
        -> Result<Vec<GuardianAction>, MiraError>
    {
        let conn = self.conn.lock().unwrap();
        let (sql, p): (String, Vec<Box<dyn rusqlite::ToSql>>) = match status {
            Some(s) => (
                "SELECT id, kind, target, reason, status, created_at, decided_at, result
                 FROM guardian_actions WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2".into(),
                vec![Box::new(s.as_str().to_string()), Box::new(limit)],
            ),
            None => (
                "SELECT id, kind, target, reason, status, created_at, decided_at, result
                 FROM guardian_actions ORDER BY created_at DESC LIMIT ?1".into(),
                vec![Box::new(limit)],
            ),
        };
        let mut stmt = conn.prepare(&sql).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(p.iter().map(|x| x.as_ref())), row_to_action)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    /// Transition a proposal to a decided/terminal state with an outcome note.
    /// Only a `pending` row may be decided (idempotency / no double-execute).
    pub fn decide(&self, id: &str, status: GuardianActionStatus, result: &str)
        -> Result<bool, MiraError>
    {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE guardian_actions SET status = ?2, decided_at = ?3, result = ?4
             WHERE id = ?1 AND status = 'pending'",
            params![id, status.as_str(), Self::now(), result],
        ).map_err(|e| MiraError::DatabaseError(format!("decide guardian action: {e}")))?;
        Ok(n > 0)
    }
}

/// Deterministic execution of a bounded Guardian action — the single place a
/// proposal becomes a real operation (after human approval, or autonomously
/// under isolation). Shared by the HTTP approve handler and the watch loop.
/// Reversible/safe by construction; never shell/config/data-delete/self-restart.
pub async fn execute_action(
    kind:        GuardianActionKind,
    target:      Option<&str>,
    automations: Option<&Arc<crate::automations::AutomationsStore>>,
    channel_mgr: Option<&Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>>,
) -> Result<String, String> {
    use GuardianActionKind::*;
    match kind {
        RerunAudit => {
            let a = automations.ok_or("automations store unavailable")?;
            match a.force_schedule_next_run("heartbeat.system_audit") {
                Ok(true)  => Ok("queued an immediate health-audit re-run".into()),
                Ok(false) => Err("heartbeat.system_audit is not seeded".into()),
                Err(e)    => Err(format!("db: {e}")),
            }
        }
        RequeueAutomation => {
            let a    = automations.ok_or("automations store unavailable")?;
            let name = target.ok_or("missing schedule name")?;
            match a.force_schedule_next_run(name) {
                Ok(true)  => Ok(format!("requeued schedule '{name}' for immediate run")),
                Ok(false) => Err(format!("schedule '{name}' not found")),
                Err(e)    => Err(format!("db: {e}")),
            }
        }
        TrimLogs => {
            let a = automations.ok_or("automations store unavailable")?;
            match a.force_schedule_next_run("heartbeat.log_cleanup") {
                Ok(true)  => Ok("queued a log-cleanup pass".into()),
                Ok(false) => Err("heartbeat.log_cleanup is not seeded".into()),
                Err(e)    => Err(format!("db: {e}")),
            }
        }
        RestartBridge => {
            let mgr  = channel_mgr.ok_or("channel manager unavailable")?;
            let acct = target.ok_or("missing channel account id")?;
            mgr.write().await.restart_account(acct).await
                .map(|_| format!("restarted channel account '{acct}'"))
                .map_err(|e| format!("restart failed: {e}"))
        }
    }
}

fn row_to_action(row: &rusqlite::Row<'_>) -> rusqlite::Result<GuardianAction> {
    let kind_s: String = row.get(1)?;
    let status_s: String = row.get(4)?;
    Ok(GuardianAction {
        id:         row.get(0)?,
        kind:       GuardianActionKind::parse(&kind_s).unwrap_or(GuardianActionKind::RerunAudit),
        target:     row.get(2)?,
        reason:     row.get(3)?,
        status:     GuardianActionStatus::parse(&status_s),
        created_at: row.get(5)?,
        decided_at: row.get(6)?,
        result:     row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propose_then_decide_lifecycle() {
        let s = GuardianActionStore::open_memory().unwrap();
        let id = s.create_pending(GuardianActionKind::RestartBridge, Some("signal:1"), "bridge down").unwrap();
        let a = s.get(&id).unwrap().unwrap();
        assert_eq!(a.status, GuardianActionStatus::Pending);
        assert_eq!(a.kind, GuardianActionKind::RestartBridge);
        assert_eq!(s.list(Some(GuardianActionStatus::Pending), 10).unwrap().len(), 1);
        // First decide wins; second is a no-op (no double-execute).
        assert!(s.decide(&id, GuardianActionStatus::Executed, "ok").unwrap());
        assert!(!s.decide(&id, GuardianActionStatus::Declined, "late").unwrap());
        assert_eq!(s.get(&id).unwrap().unwrap().status, GuardianActionStatus::Executed);
        assert!(s.list(Some(GuardianActionStatus::Pending), 10).unwrap().is_empty());
    }

    #[test]
    fn kind_roundtrip_and_target_rule() {
        for k in GuardianActionKind::all() {
            assert_eq!(GuardianActionKind::parse(k).unwrap().as_str(), *k);
        }
        assert!(GuardianActionKind::RestartBridge.needs_target());
        assert!(!GuardianActionKind::RerunAudit.needs_target());
        assert!(GuardianActionKind::parse("shell").is_none()); // never a valid kind
    }
}
