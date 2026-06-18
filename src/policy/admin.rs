// SPDX-License-Identifier: AGPL-3.0-or-later

//! Admin-defined rules — slice D3.
//!
//! Lets an operator add custom Deny rules on top of D2's hard-coded
//! built-ins. Rules are persisted in SQLite (so they survive restart
//! and admins can tune without redeploying), evaluated by an
//! [`AdminRulesEngine`] that implements [`PolicyEngine`], and
//! composed with the built-in engine through [`ChainedEngine`].
//!
//! Composition: `Supervisor::with_policy_engine(ChainedEngine::new([
//!     Arc::new(BuiltinRulesEngine::standard(...)),
//!     Arc::new(AdminRulesEngine::new(store)),
//! ]))`. First-deny-wins **across** engines too.
//!
//! # Predicate model
//!
//! Each [`AdminRule`] has:
//!   - An `event_kind` (`"spawn_worker"` / `"tool_invocation"` / …)
//!     — the rule only fires for matching events.
//!   - A list of [`Predicate`]s combined with AND semantics.
//!     Admins write OR by adding multiple rules with the same
//!     `reason`.
//!
//! Predicates cover concrete patterns observed in the design doc and
//! in real audit traces — exact matches on skill_id / tool / provider
//! / model, host equality + suffix matching ("\*.evil.com" expressed
//! as `HostHasSuffix("evil.com")`), path-prefix, fs-mode equality,
//! and threshold predicates for cost + depth. Deliberately NOT a
//! general DSL: admin-supplied code is the wrong threat surface.
//! When the predicate set proves too narrow, more variants get added
//! (non-breaking — old rules keep deserialising).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tracing::warn;

use crate::policy::engine::{PolicyDecision, PolicyEngine};
use crate::policy::event::PolicyEvent;
use crate::MiraError;

/// One admin-defined rule. Persisted as a row in `admin_rules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminRule {
    /// Stable identifier — surfaces into `PolicyDecision::Deny.rule`
    /// so the audit log groups identical denies. Convention:
    /// `"admin/<short-kebab-name>"` to distinguish from built-in rules
    /// (which use bare kebab-case ids like `"max-recursion-depth"`).
    pub id:         String,
    /// Human-readable display name for the admin UI. Doesn't affect
    /// matching.
    pub name:       String,
    /// When false, the rule is skipped during evaluation. Lets admins
    /// stage / test rules without deleting them.
    pub enabled:    bool,
    /// Snake-case event kind this rule applies to (matches
    /// `PolicyEvent::kind()`). Rules ignore events of other kinds
    /// rather than matching unrelated payloads.
    pub event_kind: String,
    /// Predicates combined with AND. Empty list = match every event
    /// of `event_kind` (useful for "deny all secret_read for skill X"
    /// when paired with one `SkillIdEquals` predicate).
    pub predicates: Vec<Predicate>,
    /// Human-readable reason surfaced to the agents UI / audit log.
    pub reason:     String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl AdminRule {
    /// Construct a fresh rule with timestamps stamped now. The caller
    /// supplies the id, name, kind, predicates, reason; `enabled`
    /// defaults to true.
    pub fn new(
        id:         impl Into<String>,
        name:       impl Into<String>,
        event_kind: impl Into<String>,
        predicates: Vec<Predicate>,
        reason:     impl Into<String>,
    ) -> Self {
        let now = Utc::now().timestamp_millis();
        Self {
            id: id.into(), name: name.into(),
            enabled: true, event_kind: event_kind.into(),
            predicates, reason: reason.into(),
            created_at_ms: now, updated_at_ms: now,
        }
    }

    /// Returns true if `event` should be denied by this rule.
    /// `false` if disabled, kind doesn't match, or any predicate fails.
    pub fn matches(&self, event: &PolicyEvent) -> bool {
        if !self.enabled { return false; }
        if event.kind() != self.event_kind { return false; }
        self.predicates.iter().all(|p| p.matches(event))
    }
}

/// One condition in an admin rule. Each variant maps to a specific
/// field on a specific event variant. A predicate that doesn't apply
/// to the event variant returns `false` — the rule won't match.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Predicate {
    // ── identity matchers ──────────────────────────────────────────
    /// Exact match on `skill_id` (works for any event variant that has one).
    SkillIdEquals    { value: String },
    /// Exact match on `ToolInvocation.tool`.
    ToolNameEquals   { value: String },
    /// Exact match on `LlmCall.provider`.
    ProviderEquals   { value: String },
    /// Exact match on `LlmCall.model`.
    ModelEquals      { value: String },
    /// Exact match on `SecretRead.secret_name`.
    SecretNameEquals { value: String },

    // ── network matchers ───────────────────────────────────────────
    /// Exact match on `NetworkEgress.host`.
    HostEquals       { value: String },
    /// `NetworkEgress.host` ends with the given suffix. Pass
    /// `"evil.com"` to match `evil.com` and any subdomain. The leading
    /// `*.` from glob-style patterns is implied — DON'T include it.
    HostHasSuffix    { value: String },

    // ── filesystem matchers ────────────────────────────────────────
    /// `FilesystemAccess.path` is `value` or a descendant.
    PathUnder        { value: PathBuf },
    /// Exact match on `FilesystemAccess.mode`.
    FsModeEquals     { value: String },

    // ── threshold predicates ───────────────────────────────────────
    /// `LlmCall.running_cost_usd > value`.
    RunningCostExceedsUsd { value: f64 },
    /// `LlmCall.session_cost_usd` OR `SpawnWorker.session_spent_usd`
    /// `> value`. Same predicate covers both event types — admins
    /// don't need to write two rules to cap "session spend in USD."
    SessionCostExceedsUsd { value: f64 },
    /// `SpawnWorker.child_depth > value`.
    DepthExceeds          { value: u8  },
}

impl Predicate {
    /// Does this predicate hold for `event`? Returns false when the
    /// predicate variant simply doesn't apply (e.g. `HostEquals`
    /// against a `ToolInvocation` event).
    pub fn matches(&self, event: &PolicyEvent) -> bool {
        match (self, event) {
            // Identity — skill_id can apply to many event variants.
            (Predicate::SkillIdEquals { value }, e) => skill_id_of(e).map(|s| s == value).unwrap_or(false),

            // Tool / provider / model / secret — variant-specific.
            (Predicate::ToolNameEquals { value },
                PolicyEvent::ToolInvocation { tool, .. }) => tool == value,
            (Predicate::ProviderEquals { value },
                PolicyEvent::LlmCall { provider, .. }) => provider == value,
            (Predicate::ModelEquals { value },
                PolicyEvent::LlmCall { model, .. }) => model == value,
            (Predicate::SecretNameEquals { value },
                PolicyEvent::SecretRead { secret_name, .. }) => secret_name == value,

            // Network host.
            (Predicate::HostEquals { value },
                PolicyEvent::NetworkEgress { host, .. }) => host == value,
            (Predicate::HostHasSuffix { value },
                PolicyEvent::NetworkEgress { host, .. }) => {
                // host == suffix OR host ends with ".suffix" — the
                // dot prevents "evil.com" matching "notevil.com".
                host == value || host.ends_with(&format!(".{value}"))
            }

            // Filesystem.
            (Predicate::PathUnder { value },
                PolicyEvent::FilesystemAccess { path, .. }) => path.starts_with(value),
            (Predicate::FsModeEquals { value },
                PolicyEvent::FilesystemAccess { mode, .. }) => mode == value,

            // Thresholds.
            (Predicate::RunningCostExceedsUsd { value },
                PolicyEvent::LlmCall { running_cost_usd, .. }) => running_cost_usd > value,
            (Predicate::SessionCostExceedsUsd { value }, e) => match e {
                PolicyEvent::LlmCall { session_cost_usd, .. }    => session_cost_usd > value,
                PolicyEvent::SpawnWorker { session_spent_usd, .. } => session_spent_usd > value,
                _ => false,
            },
            (Predicate::DepthExceeds { value },
                PolicyEvent::SpawnWorker { child_depth, .. }) => child_depth > value,

            // Predicate variant doesn't apply to this event variant.
            _ => false,
        }
    }
}

/// Extract `skill_id` from any event variant that has one.
fn skill_id_of(e: &PolicyEvent) -> Option<&str> {
    match e {
        PolicyEvent::ToolInvocation   { skill_id, .. } => skill_id.as_deref(),
        PolicyEvent::LlmCall          { skill_id, .. } => skill_id.as_deref(),
        PolicyEvent::NetworkEgress    { skill_id, .. } => skill_id.as_deref(),
        PolicyEvent::FilesystemAccess { skill_id, .. } => skill_id.as_deref(),
        PolicyEvent::SecretRead       { skill_id, .. } => skill_id.as_deref(),
        PolicyEvent::SpawnWorker      { skill_id, .. } => Some(skill_id),
    }
}

// ─── Persistence ──────────────────────────────────────────────────────

/// SQLite-backed persistence for [`AdminRule`]s. Cheap to clone
/// (`Arc<Mutex<Connection>>` inside).
pub struct AdminRulesStore {
    conn: Arc<Mutex<Connection>>,
}

impl AdminRulesStore {
    pub fn open(path: &std::path::Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MiraError::DatabaseError(
                format!("create admin rules dir {}: {e}", parent.display()),
            ))?;
        }
        let conn = Connection::open(path).map_err(|e| MiraError::DatabaseError(
            format!("open admin rules DB {}: {e}", path.display()),
        ))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS admin_rules (
                id              TEXT PRIMARY KEY,
                name            TEXT NOT NULL,
                enabled         INTEGER NOT NULL,
                event_kind      TEXT NOT NULL,
                predicates_json TEXT NOT NULL,
                reason          TEXT NOT NULL,
                created_at_ms   INTEGER NOT NULL,
                updated_at_ms   INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_admin_rules_kind
                ON admin_rules(event_kind);
            "#,
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Test convenience — in-memory store, fresh schema.
    #[cfg(test)]
    pub fn open_in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("in-mem db");
        conn.execute_batch(
            r#"CREATE TABLE admin_rules (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, enabled INTEGER NOT NULL,
                event_kind TEXT NOT NULL, predicates_json TEXT NOT NULL,
                reason TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
            );"#,
        ).unwrap();
        Self { conn: Arc::new(Mutex::new(conn)) }
    }

    /// Insert or replace a rule by id. Updates `updated_at_ms` to now;
    /// preserves `created_at_ms` if the row already exists.
    pub fn upsert(&self, rule: &AdminRule) -> Result<(), MiraError> {
        let conn = self.conn.lock().expect("admin lock");
        let now = Utc::now().timestamp_millis();
        let existing_created: Option<i64> = conn.query_row(
            "SELECT created_at_ms FROM admin_rules WHERE id = ?",
            params![rule.id], |r| r.get(0),
        ).ok();
        let created_at = existing_created.unwrap_or(rule.created_at_ms);
        let predicates_json = serde_json::to_string(&rule.predicates)
            .map_err(|e| MiraError::DatabaseError(format!("serialise predicates: {e}")))?;

        conn.execute(
            "INSERT INTO admin_rules
                (id, name, enabled, event_kind, predicates_json, reason,
                 created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                name            = excluded.name,
                enabled         = excluded.enabled,
                event_kind      = excluded.event_kind,
                predicates_json = excluded.predicates_json,
                reason          = excluded.reason,
                updated_at_ms   = excluded.updated_at_ms",
            params![
                rule.id, rule.name, rule.enabled as i64,
                rule.event_kind, predicates_json, rule.reason,
                created_at, now,
            ],
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    /// Fetch one rule by id. Returns Ok(None) if absent.
    pub fn get(&self, id: &str) -> Result<Option<AdminRule>, MiraError> {
        let conn = self.conn.lock().expect("admin lock");
        match conn.query_row(
            "SELECT id, name, enabled, event_kind, predicates_json, reason,
                    created_at_ms, updated_at_ms
             FROM admin_rules WHERE id = ?",
            params![id],
            row_to_rule,
        ) {
            Ok(rule)  => Ok(Some(rule)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)    => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// All rules, ordered by id for stable display.
    pub fn list(&self) -> Result<Vec<AdminRule>, MiraError> {
        let conn = self.conn.lock().expect("admin lock");
        let mut stmt = conn.prepare(
            "SELECT id, name, enabled, event_kind, predicates_json, reason,
                    created_at_ms, updated_at_ms
             FROM admin_rules ORDER BY id ASC",
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map([], row_to_rule)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(out)
    }

    /// Just the rules for one event kind. Used by the engine to skip
    /// SQL filter work in the hot eval path.
    pub fn list_for_kind(&self, kind: &str) -> Result<Vec<AdminRule>, MiraError> {
        let conn = self.conn.lock().expect("admin lock");
        let mut stmt = conn.prepare(
            "SELECT id, name, enabled, event_kind, predicates_json, reason,
                    created_at_ms, updated_at_ms
             FROM admin_rules WHERE event_kind = ? AND enabled = 1
             ORDER BY id ASC",
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map(params![kind], row_to_rule)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(out)
    }

    /// Delete by id. No error if the row didn't exist — admins
    /// shouldn't have to check before delete.
    pub fn delete(&self, id: &str) -> Result<bool, MiraError> {
        let conn = self.conn.lock().expect("admin lock");
        let n = conn.execute("DELETE FROM admin_rules WHERE id = ?", params![id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(n > 0)
    }
}

fn row_to_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<AdminRule> {
    let id:              String = row.get(0)?;
    let name:            String = row.get(1)?;
    let enabled:         i64    = row.get(2)?;
    let event_kind:      String = row.get(3)?;
    let predicates_json: String = row.get(4)?;
    let reason:          String = row.get(5)?;
    let created_at_ms:   i64    = row.get(6)?;
    let updated_at_ms:   i64    = row.get(7)?;
    let predicates: Vec<Predicate> = serde_json::from_str(&predicates_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            4, rusqlite::types::Type::Text, Box::new(e),
        ))?;
    Ok(AdminRule {
        id, name, enabled: enabled != 0, event_kind,
        predicates, reason, created_at_ms, updated_at_ms,
    })
}

// ─── Engine impl ──────────────────────────────────────────────────────

/// PolicyEngine that consults an [`AdminRulesStore`] on every event.
/// Reads the store on each call; for v1 the SQL cost is negligible
/// (typical rule count is tens, indexed by event_kind). When that
/// becomes hot, swap in an in-memory cache + cache-bust on writes.
pub struct AdminRulesEngine {
    store: Arc<AdminRulesStore>,
}

impl AdminRulesEngine {
    pub fn new(store: Arc<AdminRulesStore>) -> Self { Self { store } }
}

#[async_trait]
impl PolicyEngine for AdminRulesEngine {
    async fn evaluate(&self, event: &PolicyEvent) -> PolicyDecision {
        let rules = match self.store.list_for_kind(event.kind()) {
            Ok(r) => r,
            Err(e) => {
                // Fail-OPEN on store errors. The alternative — fail-
                // closed — would block all spawns/tools the moment
                // the admin DB hiccups, which is worse than briefly
                // running without admin policy. Loud warn so it's
                // visible in logs.
                warn!("admin rules store unavailable: {e} — admin rules SKIPPED");
                return PolicyDecision::Allow;
            }
        };
        for rule in &rules {
            if rule.matches(event) {
                return PolicyDecision::deny(&rule.id, &rule.reason);
            }
        }
        PolicyDecision::Allow
    }
}

// ─── Combinator ───────────────────────────────────────────────────────

/// Evaluate a list of engines in order and return the first Deny.
/// Used in production to compose the built-in engine + the admin
/// engine: built-ins evaluate first, admin rules layer on top.
pub struct ChainedEngine {
    engines: Vec<Arc<dyn PolicyEngine>>,
}

impl ChainedEngine {
    pub fn new(engines: Vec<Arc<dyn PolicyEngine>>) -> Self {
        Self { engines }
    }
}

#[async_trait]
impl PolicyEngine for ChainedEngine {
    async fn evaluate(&self, event: &PolicyEvent) -> PolicyDecision {
        for eng in &self.engines {
            let d = eng.evaluate(event).await;
            if d.is_deny() { return d; }
        }
        PolicyDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::instance::AgentId;
    use crate::policy::{
        AllowAllEngine, DenyAllEngine,
        event::NetworkEgressDirection,
    };

    fn id() -> AgentId { AgentId::new() }

    fn rule(id: &str, kind: &str, predicates: Vec<Predicate>, reason: &str) -> AdminRule {
        AdminRule::new(id, format!("{id} display"), kind, predicates, reason)
    }

    // ── Predicate matching ─────────────────────────────────────────────

    #[test]
    fn skill_id_equals_matches_each_event_variant_with_skill_id() {
        let p = Predicate::SkillIdEquals { value: "com.x".into() };
        // Tool invocation
        assert!(p.matches(&PolicyEvent::ToolInvocation {
            agent_id: id(), skill_id: Some("com.x".into()),
            tool: "t".into(), args_summary: "".into(),
            running_cost_usd: 0.0, session_cost_usd: 0.0,
        }));
        // Spawn worker (skill_id is required, not Option)
        assert!(p.matches(&PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "com.x".into(),
            child_depth: 1, budget_usd: 1.0, session_spent_usd: 0.0,
        }));
        // Wrong skill
        assert!(!p.matches(&PolicyEvent::ToolInvocation {
            agent_id: id(), skill_id: Some("com.OTHER".into()),
            tool: "t".into(), args_summary: "".into(),
            running_cost_usd: 0.0, session_cost_usd: 0.0,
        }));
        // No skill_id (root agent action)
        assert!(!p.matches(&PolicyEvent::ToolInvocation {
            agent_id: id(), skill_id: None,
            tool: "t".into(), args_summary: "".into(),
            running_cost_usd: 0.0, session_cost_usd: 0.0,
        }));
    }

    #[test]
    fn host_has_suffix_matches_subdomains_only_via_dot_boundary() {
        let p = Predicate::HostHasSuffix { value: "evil.com".into() };
        let e = |host: &str| PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: Some("x".into()),
            url: format!("https://{host}/"),
            host: host.into(), scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        };
        assert!( p.matches(&e("evil.com")));
        assert!( p.matches(&e("foo.evil.com")));
        assert!( p.matches(&e("a.b.evil.com")));
        // Critical: don't false-match prefixes that share the suffix
        // string but cross a different word boundary.
        assert!(!p.matches(&e("notevil.com")));
        assert!(!p.matches(&e("evil.com.fake")));
    }

    #[test]
    fn path_under_matches_descendants_only() {
        let p = Predicate::PathUnder { value: "/etc".into() };
        let e = |path: &str| PolicyEvent::FilesystemAccess {
            agent_id: id(), skill_id: Some("x".into()),
            path: path.into(), mode: "read".into(),
        };
        assert!( p.matches(&e("/etc")));
        assert!( p.matches(&e("/etc/passwd")));
        assert!( p.matches(&e("/etc/ssh/sshd_config")));
        assert!(!p.matches(&e("/var/log")));
        // PathBuf::starts_with is component-aware so /etcd doesn't
        // match /etc — the implementation uses it for that reason.
        assert!(!p.matches(&e("/etcd")));
    }

    #[test]
    fn cost_predicates_use_strictly_greater_than() {
        let p = Predicate::SessionCostExceedsUsd { value: 1.00 };
        let e = |cost: f64| PolicyEvent::LlmCall {
            agent_id: id(), skill_id: None,
            provider: "p".into(), model: "m".into(),
            running_cost_usd: 0.0, session_cost_usd: cost,
            agent_budget_usd: None,
        };
        assert!(!p.matches(&e(0.99)));
        assert!(!p.matches(&e(1.00)), "exactly at threshold should NOT trigger");
        assert!( p.matches(&e(1.01)));
    }

    #[test]
    fn session_cost_predicate_works_for_both_llm_and_spawn_events() {
        let p = Predicate::SessionCostExceedsUsd { value: 1.00 };
        // SpawnWorker carries `session_spent_usd` — same predicate
        // should fire on it.
        let spawn = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 1, budget_usd: 1.0, session_spent_usd: 5.0,
        };
        assert!(p.matches(&spawn));
    }

    #[test]
    fn predicate_returns_false_when_event_variant_does_not_apply() {
        // ToolNameEquals against a SpawnWorker — the predicate just
        // doesn't apply, so it's `false` (the rule won't match, but
        // doesn't crash).
        let p = Predicate::ToolNameEquals { value: "web_fetch".into() };
        let e = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 1, budget_usd: 1.0, session_spent_usd: 0.0,
        };
        assert!(!p.matches(&e));
    }

    // ── AdminRule.matches() ────────────────────────────────────────────

    #[test]
    fn rule_matches_when_kind_and_all_predicates_match() {
        let r = rule("admin/no-aws-secrets",
            "secret_read",
            vec![
                Predicate::SkillIdEquals    { value: "com.x".into() },
                Predicate::SecretNameEquals { value: "AWS_KEY".into() },
            ],
            "no AWS secrets in this skill");
        let event = PolicyEvent::SecretRead {
            agent_id: id(), skill_id: Some("com.x".into()),
            secret_name: "AWS_KEY".into(),
        };
        assert!(r.matches(&event));
    }

    #[test]
    fn rule_does_not_match_when_any_predicate_fails() {
        let r = rule("admin/x", "secret_read", vec![
            Predicate::SkillIdEquals { value: "com.x".into() },
            Predicate::SecretNameEquals { value: "AWS_KEY".into() },
        ], "x");
        // Right skill, wrong secret — AND semantics → no match.
        let event = PolicyEvent::SecretRead {
            agent_id: id(), skill_id: Some("com.x".into()),
            secret_name: "OTHER".into(),
        };
        assert!(!r.matches(&event));
    }

    #[test]
    fn disabled_rule_never_matches() {
        let mut r = rule("admin/x", "secret_read",
            vec![Predicate::SkillIdEquals { value: "com.x".into() }],
            "x");
        r.enabled = false;
        let event = PolicyEvent::SecretRead {
            agent_id: id(), skill_id: Some("com.x".into()),
            secret_name: "ANY".into(),
        };
        assert!(!r.matches(&event));
    }

    #[test]
    fn rule_does_not_match_event_of_different_kind() {
        let r = rule("admin/x", "secret_read", vec![], "x");
        let event = PolicyEvent::ToolInvocation {
            agent_id: id(), skill_id: None,
            tool: "x".into(), args_summary: "".into(),
            running_cost_usd: 0.0, session_cost_usd: 0.0,
        };
        assert!(!r.matches(&event));
    }

    #[test]
    fn rule_with_zero_predicates_matches_every_event_of_its_kind() {
        // Empty predicate list = "all events of this kind." Useful
        // for "block all secret_read entirely" without needing to
        // enumerate predicates.
        let r = rule("admin/no-secrets", "secret_read", vec![], "secrets disabled");
        let event = PolicyEvent::SecretRead {
            agent_id: id(), skill_id: Some("com.x".into()),
            secret_name: "ANYTHING".into(),
        };
        assert!(r.matches(&event));
    }

    // ── Store CRUD ─────────────────────────────────────────────────────

    #[test]
    fn upsert_then_get_round_trips_rule() {
        let store = AdminRulesStore::open_in_memory();
        let r = rule("admin/x", "spawn_worker",
            vec![Predicate::DepthExceeds { value: 2 }],
            "depth too high");
        store.upsert(&r).unwrap();
        let got = store.get("admin/x").unwrap().unwrap();
        assert_eq!(got.id,         r.id);
        assert_eq!(got.event_kind, r.event_kind);
        assert_eq!(got.predicates, r.predicates);
        assert_eq!(got.reason,     r.reason);
        assert!(got.enabled);
    }

    #[test]
    fn upsert_preserves_created_at_and_updates_updated_at() {
        let store = AdminRulesStore::open_in_memory();
        let r = AdminRule::new("admin/x", "Display", "spawn_worker", vec![], "x");
        let original_created = r.created_at_ms;
        store.upsert(&r).unwrap();
        // Tiny pause then upsert again — updated_at should bump.
        std::thread::sleep(std::time::Duration::from_millis(2));
        store.upsert(&r).unwrap();
        let got = store.get("admin/x").unwrap().unwrap();
        assert_eq!(got.created_at_ms, original_created,
            "created_at must be preserved across upserts");
        assert!(got.updated_at_ms >= original_created,
            "updated_at must move forward");
    }

    #[test]
    fn list_returns_rules_in_id_order() {
        let store = AdminRulesStore::open_in_memory();
        for id in ["admin/c", "admin/a", "admin/b"] {
            store.upsert(&rule(id, "spawn_worker", vec![], "")).unwrap();
        }
        let listed = store.list().unwrap();
        let ids: Vec<&str> = listed.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["admin/a", "admin/b", "admin/c"]);
    }

    #[test]
    fn list_for_kind_skips_disabled_and_other_kinds() {
        let store = AdminRulesStore::open_in_memory();
        // Two rules of the right kind — one disabled.
        let mut on  = rule("admin/on",  "spawn_worker", vec![], "x");
        let mut off = rule("admin/off", "spawn_worker", vec![], "x");
        off.enabled = false;
        store.upsert(&on ).unwrap();
        store.upsert(&off).unwrap();
        // One rule of a different kind.
        store.upsert(&rule("admin/other", "tool_invocation", vec![], "x")).unwrap();

        on.enabled = true; // sanity
        let only_spawn = store.list_for_kind("spawn_worker").unwrap();
        let ids: Vec<&str> = only_spawn.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["admin/on"], "only enabled spawn_worker rules");
    }

    #[test]
    fn delete_removes_row_and_returns_true_on_first_call_only() {
        let store = AdminRulesStore::open_in_memory();
        store.upsert(&rule("admin/x", "spawn_worker", vec![], "")).unwrap();
        assert!( store.delete("admin/x").unwrap());
        assert!(!store.delete("admin/x").unwrap(), "second delete is a no-op");
        assert!(store.get("admin/x").unwrap().is_none());
    }

    // ── Engine end-to-end ──────────────────────────────────────────────

    #[tokio::test]
    async fn admin_engine_denies_when_rule_matches() {
        let store = Arc::new(AdminRulesStore::open_in_memory());
        store.upsert(&rule("admin/no-evil-net", "network_egress",
            vec![Predicate::HostHasSuffix { value: "evil.com".into() }],
            "evil.com is denied")).unwrap();
        let eng = AdminRulesEngine::new(store);

        let event = PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: Some("com.x".into()),
            url: "https://api.evil.com/".into(),
            host: "api.evil.com".into(), scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        };
        match eng.evaluate(&event).await {
            PolicyDecision::Deny { rule, reason } => {
                assert_eq!(rule, "admin/no-evil-net");
                assert_eq!(reason, "evil.com is denied");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admin_engine_allows_when_no_rule_matches() {
        let store = Arc::new(AdminRulesStore::open_in_memory());
        store.upsert(&rule("admin/no-evil-net", "network_egress",
            vec![Predicate::HostHasSuffix { value: "evil.com".into() }],
            "x")).unwrap();
        let eng = AdminRulesEngine::new(store);

        let event = PolicyEvent::NetworkEgress {
            agent_id: id(), skill_id: Some("com.x".into()),
            url: "https://wikipedia.org/".into(),
            host: "wikipedia.org".into(), scheme: "https".into(),
            direction: NetworkEgressDirection::Outbound,
        };
        assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn admin_engine_with_no_rules_is_a_pure_passthrough() {
        let store = Arc::new(AdminRulesStore::open_in_memory());
        let eng = AdminRulesEngine::new(store);
        let event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 100, budget_usd: 1e9, session_spent_usd: 1e9,
        };
        assert_eq!(eng.evaluate(&event).await, PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn admin_engine_respects_first_deny_wins_within_one_kind() {
        let store = Arc::new(AdminRulesStore::open_in_memory());
        store.upsert(&rule("admin/a", "secret_read", vec![], "first")).unwrap();
        store.upsert(&rule("admin/b", "secret_read", vec![], "second")).unwrap();
        let eng = AdminRulesEngine::new(store);
        let event = PolicyEvent::SecretRead {
            agent_id: id(), skill_id: None,
            secret_name: "X".into(),
        };
        match eng.evaluate(&event).await {
            // list_for_kind orders by id ASC, so admin/a wins.
            PolicyDecision::Deny { rule, .. } => assert_eq!(rule, "admin/a"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ── ChainedEngine ──────────────────────────────────────────────────

    #[tokio::test]
    async fn chained_engine_returns_first_deny_across_engines() {
        // Allow → Deny → Deny — should return the first Deny it hits.
        let chained = ChainedEngine::new(vec![
            Arc::new(AllowAllEngine),
            Arc::new(DenyAllEngine::new("first deny wins")),
            Arc::new(DenyAllEngine::new("never reached")),
        ]);
        let event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 1, budget_usd: 1.0, session_spent_usd: 0.0,
        };
        match chained.evaluate(&event).await {
            PolicyDecision::Deny { reason, .. } => {
                assert_eq!(reason, "first deny wins");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chained_engine_returns_allow_when_all_engines_allow() {
        let chained = ChainedEngine::new(vec![
            Arc::new(AllowAllEngine),
            Arc::new(AllowAllEngine),
        ]);
        let event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 1, budget_usd: 1.0, session_spent_usd: 0.0,
        };
        assert_eq!(chained.evaluate(&event).await, PolicyDecision::Allow);
    }

    #[tokio::test]
    async fn chained_engine_with_no_engines_allows_everything() {
        let chained = ChainedEngine::new(vec![]);
        let event = PolicyEvent::SpawnWorker {
            parent_id: id(), skill_id: "x".into(),
            child_depth: 100, budget_usd: 1.0, session_spent_usd: 1e9,
        };
        assert_eq!(chained.evaluate(&event).await, PolicyDecision::Allow);
    }

    // ── Predicate serde round-trip ─────────────────────────────────────

    #[test]
    fn predicate_serde_round_trips_with_snake_case_tag() {
        let original = vec![
            Predicate::SkillIdEquals      { value: "com.x".into() },
            Predicate::HostHasSuffix      { value: "evil.com".into() },
            Predicate::PathUnder          { value: "/etc/ssh".into() },
            Predicate::DepthExceeds       { value: 3 },
            Predicate::SessionCostExceedsUsd { value: 1.5 },
        ];
        let json = serde_json::to_string(&original).unwrap();
        // Verify snake_case discriminator on the wire.
        assert!(json.contains(r#""type":"skill_id_equals""#));
        assert!(json.contains(r#""type":"host_has_suffix""#));
        let back: Vec<Predicate> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }
}
