// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/workflow.rs

//! **Workflows** (Phase C) — declarative orchestration over named agents.
//!
//! A workflow is a DAG of steps. Each step targets a named agent
//! (`@handle`) or a built-in skill, carries a brief *template* that can
//! interpolate the run input and any upstream step's output, and declares
//! which steps it depends on. The orchestrator (see [`super::orchestrator`])
//! topologically schedules the DAG — independent steps run in parallel, a
//! step starts once all its dependencies have completed, and each step's
//! result is fed forward to its dependents.
//!
//! This module owns the *data*: the definition + run models, validation
//! (slug names, unique step ids, single target per step, acyclicity), and the
//! SQLite store (definition CRUD + run persistence for observability).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::definitions::validate_name;
use crate::agent::named_agent::skill_id_for_handle;
use crate::MiraError;

/// Upper bound on steps per workflow — a guard against pathological graphs,
/// not a design limit anyone should hit.
pub const MAX_STEPS: usize = 50;

// ── Definition model ─────────────────────────────────────────────────────────

/// A saved orchestration: a DAG of steps over named agents / skills.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub id: String,
    /// Unique invocation key — a slug (`weekly-brief`, `triage-and-fix`).
    pub name: String,
    pub description: String,
    pub steps: Vec<WorkflowStep>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One node in the DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Unique-within-workflow slug; referenced by `depends_on` and by
    /// `{{steps.<id>.output}}` interpolation.
    pub id: String,
    /// Target a named agent by handle. Exactly one of `agent`/`skill`.
    #[serde(default)]
    pub agent: Option<String>,
    /// …or a built-in skill id (e.g. `com.mira.research`).
    #[serde(default)]
    pub skill: Option<String>,
    /// The brief handed to the step's worker. May interpolate `{{input}}`
    /// and `{{steps.<dep-id>.output}}` (resolved at run time).
    #[serde(default)]
    pub brief: String,
    /// Step ids that must complete before this one starts.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Per-step USD budget. `None` → the orchestrator's default.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// When `true`, a failure here doesn't fail the whole run — the step is
    /// marked failed and steps depending on it are skipped, but independent
    /// branches continue. Default `false` (fail-fast).
    #[serde(default)]
    pub continue_on_error: bool,
    /// Optional guard: the step runs only if this condition holds against an
    /// upstream step's output. A false condition skips the step (and, by
    /// extension, its dependents). The referenced step must be a dependency.
    #[serde(default)]
    pub when: Option<StepCondition>,
    /// Human-in-the-loop checkpoint: when `true`, the run pauses before this
    /// step and a human must approve it. Approval runs the step; rejection
    /// skips it (and cascades). Default `false`.
    #[serde(default)]
    pub requires_approval: bool,
}

/// A guard on a step — tests an upstream step's output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepCondition {
    /// Upstream step id whose output is tested. Must be in `depends_on`.
    pub step: String,
    pub op: ConditionOp,
    /// Comparison value for `contains`/`not_contains`/`equals`. Ignored by
    /// `not_empty`/`empty`.
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOp {
    Contains,
    NotContains,
    Equals,
    NotEmpty,
    Empty,
}

impl StepCondition {
    /// Evaluate against the referenced step's output (case-insensitive for
    /// `contains`/`equals`). A missing/None output reads as empty.
    pub fn eval(&self, output: &str) -> bool {
        let hay = output.trim().to_lowercase();
        let needle = self.value.trim().to_lowercase();
        match self.op {
            ConditionOp::Contains => hay.contains(&needle),
            ConditionOp::NotContains => !hay.contains(&needle),
            ConditionOp::Equals => hay == needle,
            ConditionOp::NotEmpty => !hay.is_empty(),
            ConditionOp::Empty => hay.is_empty(),
        }
    }
}

impl WorkflowStep {
    /// The supervisor skill-id this step spawns under: `named:<handle>` for an
    /// agent target, or the literal skill id. `None` if neither is set.
    pub fn target_skill_id(&self) -> Option<String> {
        if let Some(h) = self.agent.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(skill_id_for_handle(h));
        }
        self.skill.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
    }
}

fn default_true() -> bool { true }

/// Create/update payload (store assigns id + timestamps).
#[derive(Debug, Clone, Deserialize)]
pub struct NewWorkflowDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

// ── Run model ────────────────────────────────────────────────────────────────

/// Terminal + in-flight states for a workflow run and its steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Completed,
    Failed,
    /// A step whose guard was false, or whose dependency was skipped/failed —
    /// never ran. Only meaningful per-step, never the overall run status.
    Skipped,
    /// Awaiting human approval at a checkpoint. As a per-step status: this
    /// step is the one blocking. As the run status: the run is parked until an
    /// approve/reject call resumes it.
    Paused,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Running => "running",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Skipped => "skipped",
            RunStatus::Paused => "paused",
        }
    }

    /// Terminal for a *step* — no further work happens to it within a run.
    pub fn is_terminal(&self) -> bool {
        matches!(self, RunStatus::Completed | RunStatus::Failed | RunStatus::Skipped)
    }
}

/// A single execution of a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: String,
    pub workflow_id: String,
    pub workflow_name: String,
    pub status: RunStatus,
    pub input: String,
    pub steps: Vec<StepRun>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    /// Checkpoint steps the user has approved — they run on resume. Persisted
    /// so an approval survives a process restart mid-pause.
    #[serde(default)]
    pub approved_steps: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Per-step state within a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRun {
    pub step_id: String,
    /// The resolved `named:<handle>` / skill id this step ran under.
    pub target: String,
    pub status: RunStatus,
    /// The spawned worker's task id (set once the step starts).
    #[serde(default)]
    pub task_id: Option<String>,
    /// The worker's `result_summary` on success.
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

// ── Validation ───────────────────────────────────────────────────────────────

/// Validate a workflow's name + step graph. Returns the execution **waves**
/// (each inner vec is a set of step ids whose dependencies are all satisfied
/// by earlier waves, so they may run in parallel) on success.
pub fn validate_workflow(
    name: &str,
    steps: &[WorkflowStep],
) -> Result<Vec<Vec<String>>, MiraError> {
    validate_name(name).map_err(|e| MiraError::ConfigError(format!("workflow name: {e}")))?;
    if steps.is_empty() {
        return Err(MiraError::ConfigError("a workflow needs at least one step".into()));
    }
    if steps.len() > MAX_STEPS {
        return Err(MiraError::ConfigError(format!("a workflow may have at most {MAX_STEPS} steps")));
    }

    let mut ids: HashSet<&str> = HashSet::new();
    for s in steps {
        validate_name(&s.id)
            .map_err(|e| MiraError::ConfigError(format!("step id {:?}: {e}", s.id)))?;
        if !ids.insert(s.id.as_str()) {
            return Err(MiraError::ConfigError(format!("duplicate step id {:?}", s.id)));
        }
        let has_agent = s.agent.as_deref().map(str::trim).is_some_and(|v| !v.is_empty());
        let has_skill = s.skill.as_deref().map(str::trim).is_some_and(|v| !v.is_empty());
        if has_agent == has_skill {
            return Err(MiraError::ConfigError(format!(
                "step {:?} must set exactly one of `agent` or `skill`", s.id
            )));
        }
    }
    for s in steps {
        for dep in &s.depends_on {
            if dep == &s.id {
                return Err(MiraError::ConfigError(format!("step {:?} depends on itself", s.id)));
            }
            if !ids.contains(dep.as_str()) {
                return Err(MiraError::ConfigError(format!(
                    "step {:?} depends on unknown step {:?}", s.id, dep
                )));
            }
        }
        // A guard can only test a dependency's output (so it exists + is
        // ordered before this step).
        if let Some(cond) = &s.when {
            if !s.depends_on.iter().any(|d| d == &cond.step) {
                return Err(MiraError::ConfigError(format!(
                    "step {:?} has a `when` on step {:?}, which must be one of its `depends_on`",
                    s.id, cond.step
                )));
            }
        }
    }
    execution_waves(steps)
}

/// Kahn's algorithm, grouped by level — each returned wave is a maximal set of
/// steps whose dependencies are all in prior waves. A leftover indicates a
/// cycle.
pub fn execution_waves(steps: &[WorkflowStep]) -> Result<Vec<Vec<String>>, MiraError> {
    let mut indeg: HashMap<&str, usize> = steps.iter().map(|s| (s.id.as_str(), 0)).collect();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for s in steps {
        for dep in &s.depends_on {
            *indeg.get_mut(s.id.as_str()).unwrap() += 1;
            dependents.entry(dep.as_str()).or_default().push(s.id.as_str());
        }
    }
    let mut waves: Vec<Vec<String>> = Vec::new();
    let mut ready: Vec<&str> = indeg.iter().filter(|(_, d)| **d == 0).map(|(k, _)| *k).collect();
    ready.sort_unstable();
    let mut done = 0usize;
    while !ready.is_empty() {
        let mut next: Vec<&str> = Vec::new();
        let wave: Vec<String> = ready.iter().map(|s| s.to_string()).collect();
        done += ready.len();
        for id in &ready {
            if let Some(deps) = dependents.get(id) {
                for d in deps {
                    let e = indeg.get_mut(*d).unwrap();
                    *e -= 1;
                    if *e == 0 {
                        next.push(*d);
                    }
                }
            }
        }
        next.sort_unstable();
        waves.push(wave);
        ready = next;
    }
    if done != steps.len() {
        return Err(MiraError::ConfigError(
            "workflow has a dependency cycle".into(),
        ));
    }
    Ok(waves)
}

// ── Store ────────────────────────────────────────────────────────────────────

const MIGRATIONS: &[crate::db::Migration] = &[crate::db::Migration {
    version: 1,
    name: "create workflows + runs",
    up: |tx| {
        tx.execute_batch(
            r#"CREATE TABLE IF NOT EXISTS workflow_definitions (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                steps_json  TEXT NOT NULL DEFAULT '[]',
                enabled     INTEGER NOT NULL DEFAULT 1,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS workflow_runs (
                id            TEXT PRIMARY KEY,
                workflow_id   TEXT NOT NULL,
                workflow_name TEXT NOT NULL,
                status        TEXT NOT NULL,
                input         TEXT NOT NULL DEFAULT '',
                steps_json    TEXT NOT NULL DEFAULT '[]',
                error         TEXT,
                user_id       TEXT,
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_workflow_runs_wf
                ON workflow_runs(workflow_id, created_at DESC);"#,
        )
    },
}, crate::db::Migration {
    version: 2,
    name: "runs: approved_steps (HITL checkpoints)",
    up: |tx| {
        crate::db::add_column_if_missing(
            tx, "workflow_runs",
            "approved_steps_json TEXT NOT NULL DEFAULT '[]'",
        )
    },
}];

pub struct WorkflowStore {
    conn: Arc<Mutex<Connection>>,
}

impl WorkflowStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MiraError::DatabaseError(format!("create workflows DB dir: {e}")))?;
        }
        let mut conn = Connection::open(path)
            .map_err(|e| MiraError::DatabaseError(format!("open workflows DB: {e}")))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        crate::db::run(&mut conn, "workflows", MIGRATIONS)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self, MiraError> {
        Self::open(Path::new(":memory:"))
    }

    fn now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    // ── definition CRUD ──────────────────────────────────────────────

    pub fn create(&self, new: NewWorkflowDefinition) -> Result<WorkflowDefinition, MiraError> {
        validate_workflow(&new.name, &new.steps)?;
        let id = Uuid::now_v7().to_string();
        let now = Self::now();
        let steps = serde_json::to_string(&new.steps).unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO workflow_definitions
               (id, name, description, steps_json, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, new.name.trim(), new.description, steps, new.enabled as i64, now],
        )
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                MiraError::ConfigError(format!("a workflow named {:?} already exists", new.name))
            } else {
                MiraError::DatabaseError(format!("create workflow: {e}"))
            }
        })?;
        drop(conn);
        self.get(&id)?.ok_or_else(|| MiraError::DatabaseError("workflow vanished after create".into()))
    }

    pub fn update(&self, id: &str, new: NewWorkflowDefinition) -> Result<WorkflowDefinition, MiraError> {
        validate_workflow(&new.name, &new.steps)?;
        let steps = serde_json::to_string(&new.steps).unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE workflow_definitions SET
               name=?2, description=?3, steps_json=?4, enabled=?5, updated_at=?6
             WHERE id=?1",
            params![id, new.name.trim(), new.description, steps, new.enabled as i64, Self::now()],
        )
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                MiraError::ConfigError(format!("a workflow named {:?} already exists", new.name))
            } else {
                MiraError::DatabaseError(format!("update workflow: {e}"))
            }
        })?;
        if n == 0 {
            return Err(MiraError::NotFound(format!("workflow {id} not found")));
        }
        drop(conn);
        self.get(id)?.ok_or_else(|| MiraError::DatabaseError("workflow vanished after update".into()))
    }

    pub fn list(&self) -> Result<Vec<WorkflowDefinition>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, description, steps_json, enabled, created_at, updated_at FROM workflow_definitions ORDER BY name ASC")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map([], row_to_def).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    pub fn get(&self, id: &str) -> Result<Option<WorkflowDefinition>, MiraError> {
        self.query_one("WHERE id = ?1", id)
    }

    pub fn get_by_name(&self, name: &str) -> Result<Option<WorkflowDefinition>, MiraError> {
        self.query_one("WHERE name = ?1", name)
    }

    fn query_one(&self, clause: &str, key: &str) -> Result<Option<WorkflowDefinition>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT id, name, description, steps_json, enabled, created_at, updated_at FROM workflow_definitions {clause}");
        conn.query_row(&sql, params![key], row_to_def)
            .optional()
            .map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    pub fn delete(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM workflow_definitions WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(format!("delete workflow: {e}")))?;
        Ok(())
    }

    // ── run persistence ──────────────────────────────────────────────

    /// Insert-or-replace a run row. The orchestrator calls this as the run
    /// progresses so the API/UI can observe in-flight state.
    pub fn save_run(&self, run: &WorkflowRun) -> Result<(), MiraError> {
        let steps = serde_json::to_string(&run.steps).unwrap_or_else(|_| "[]".into());
        let approved = serde_json::to_string(&run.approved_steps).unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO workflow_runs
               (id, workflow_id, workflow_name, status, input, steps_json, error, user_id, approved_steps_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run.id, run.workflow_id, run.workflow_name, run.status.as_str(),
                run.input, steps, run.error, run.user_id, approved, run.created_at, Self::now(),
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("save run: {e}")))?;
        Ok(())
    }

    pub fn get_run(&self, id: &str) -> Result<Option<WorkflowRun>, MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, workflow_id, workflow_name, status, input, steps_json, error, user_id, approved_steps_json, created_at, updated_at FROM workflow_runs WHERE id = ?1",
            params![id], row_to_run,
        )
        .optional()
        .map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    /// Most-recent runs first, capped at `limit`.
    pub fn list_runs(&self, limit: usize) -> Result<Vec<WorkflowRun>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, workflow_id, workflow_name, status, input, steps_json, error, user_id, approved_steps_json, created_at, updated_at FROM workflow_runs ORDER BY created_at DESC LIMIT ?1")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map(params![limit as i64], row_to_run)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| MiraError::DatabaseError(e.to_string()))
    }
}

fn row_to_def(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowDefinition> {
    let steps_json: String = row.get(3)?;
    Ok(WorkflowDefinition {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        steps: serde_json::from_str(&steps_json).unwrap_or_default(),
        enabled: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowRun> {
    let steps_json: String = row.get(5)?;
    let status: String = row.get(3)?;
    let approved_json: String = row.get(8)?;
    Ok(WorkflowRun {
        id: row.get(0)?,
        workflow_id: row.get(1)?,
        workflow_name: row.get(2)?,
        status: parse_status(&status),
        input: row.get(4)?,
        steps: serde_json::from_str(&steps_json).unwrap_or_default(),
        error: row.get(6)?,
        user_id: row.get(7)?,
        approved_steps: serde_json::from_str(&approved_json).unwrap_or_default(),
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn parse_status(s: &str) -> RunStatus {
    match s {
        "completed" => RunStatus::Completed,
        "failed" => RunStatus::Failed,
        "running" => RunStatus::Running,
        "skipped" => RunStatus::Skipped,
        "paused" => RunStatus::Paused,
        _ => RunStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, agent: &str, deps: &[&str]) -> WorkflowStep {
        WorkflowStep {
            id: id.into(),
            agent: Some(agent.into()),
            skill: None,
            brief: format!("do {id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            budget_usd: None,
            continue_on_error: false,
            when: None,
            requires_approval: false,
        }
    }

    #[test]
    fn waves_order_linear_chain() {
        let steps = vec![
            step("a", "x", &[]),
            step("b", "x", &["a"]),
            step("c", "x", &["b"]),
        ];
        let waves = validate_workflow("chain", &steps).unwrap();
        assert_eq!(waves, vec![vec!["a"], vec!["b"], vec!["c"]]);
    }

    #[test]
    fn waves_group_parallel_then_join() {
        // a → {b, c} → d
        let steps = vec![
            step("a", "x", &[]),
            step("b", "x", &["a"]),
            step("c", "x", &["a"]),
            step("d", "x", &["b", "c"]),
        ];
        let waves = validate_workflow("diamond", &steps).unwrap();
        assert_eq!(waves[0], vec!["a"]);
        assert_eq!(waves[1], vec!["b", "c"]); // parallel wave
        assert_eq!(waves[2], vec!["d"]);
    }

    #[test]
    fn rejects_cycle() {
        let steps = vec![
            step("a", "x", &["b"]),
            step("b", "x", &["a"]),
        ];
        let err = validate_workflow("cyclic", &steps).unwrap_err().to_string();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_dep_and_dual_target() {
        let s = vec![step("a", "x", &["ghost"])];
        assert!(validate_workflow("w", &s).unwrap_err().to_string().contains("unknown step"));

        let mut both = step("a", "x", &[]);
        both.skill = Some("com.mira.research".into());
        assert!(validate_workflow("w", &[both]).unwrap_err().to_string().contains("exactly one"));
    }

    #[test]
    fn target_skill_id_maps_agent_and_skill() {
        assert_eq!(step("a", "researcher", &[]).target_skill_id().as_deref(), Some("named:researcher"));
        let s = WorkflowStep {
            id: "a".into(), agent: None, skill: Some("com.mira.research".into()),
            brief: String::new(), depends_on: vec![], budget_usd: None,
            continue_on_error: false, when: None, requires_approval: false,
        };
        assert_eq!(s.target_skill_id().as_deref(), Some("com.mira.research"));
    }

    #[test]
    fn condition_eval_cases() {
        let c = |op, v: &str| StepCondition { step: "x".into(), op, value: v.into() };
        assert!(c(ConditionOp::Contains, "Yes").eval("the answer is YES"));
        assert!(c(ConditionOp::NotContains, "no").eval("yes"));
        assert!(c(ConditionOp::Equals, "Done").eval(" done "));
        assert!(c(ConditionOp::NotEmpty, "").eval("something"));
        assert!(c(ConditionOp::Empty, "").eval("   "));
        assert!(!c(ConditionOp::Contains, "z").eval("abc"));
    }

    #[test]
    fn when_must_reference_a_dependency() {
        let mut a = step("a", "x", &[]);
        let mut b = step("b", "x", &["a"]);
        // valid: guard on the dependency `a`
        b.when = Some(StepCondition { step: "a".into(), op: ConditionOp::NotEmpty, value: String::new() });
        assert!(validate_workflow("w", &[a.clone(), b.clone()]).is_ok());
        // invalid: guard on a step that isn't a dependency
        a.when = Some(StepCondition { step: "b".into(), op: ConditionOp::NotEmpty, value: String::new() });
        let err = validate_workflow("w", &[a, b]).unwrap_err().to_string();
        assert!(err.contains("must be one of its `depends_on`"), "got: {err}");
    }

    #[test]
    fn store_crud_and_runs_round_trip() {
        let s = WorkflowStore::open_memory().unwrap();
        let def = s.create(NewWorkflowDefinition {
            name: "brief".into(),
            description: "daily".into(),
            steps: vec![step("research", "researcher", &[]), step("write", "writer", &["research"])],
            enabled: true,
        }).unwrap();
        assert_eq!(def.steps.len(), 2);
        assert_eq!(s.get_by_name("brief").unwrap().unwrap().id, def.id);

        let run = WorkflowRun {
            id: "run-1".into(),
            workflow_id: def.id.clone(),
            workflow_name: def.name.clone(),
            status: RunStatus::Running,
            input: "AI news".into(),
            steps: vec![StepRun {
                step_id: "research".into(), target: "named:researcher".into(),
                status: RunStatus::Completed, task_id: Some("t1".into()),
                output: Some("found things".into()), error: None,
            }],
            error: None,
            user_id: Some("alice".into()),
            approved_steps: vec![],
            created_at: 100,
            updated_at: 100,
        };
        s.save_run(&run).unwrap();
        let got = s.get_run("run-1").unwrap().unwrap();
        assert_eq!(got.status, RunStatus::Running);
        assert_eq!(got.steps[0].output.as_deref(), Some("found things"));
        assert_eq!(s.list_runs(10).unwrap().len(), 1);
    }
}
