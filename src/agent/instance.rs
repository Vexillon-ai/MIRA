// SPDX-License-Identifier: AGPL-3.0-or-later

//! `Agent` — a spawnable instance of MIRA's reasoning loop (slice B1).
//!
//! Phase B in `design-docs/skills-and-agents.md`. The Agent type is the unit
//! Phase B's manager/worker hierarchy is built on:
//!
//! - The root MIRA agent (today's only agent) becomes one `Agent` with
//!   `parent = None`, `skill_id = None`, `depth = 0`, `budget = ∞`.
//! - Future workers are `Agent`s with `parent = Some(parent_id)`,
//!   `skill_id = Some("com.mira.research" / etc.)`, `depth = parent.depth + 1`,
//!   and a finite USD budget.
//!
//! What this slice ships: the type + lifecycle + budget tracking + the
//! `AgentRegistry` (parent/child lookup, tree traversal). Spawning,
//! manager↔worker protocol, and the actual reasoning-loop wiring all live
//! in subsequent slices (B2, B3, …).
//!
//! What this slice deliberately does NOT do:
//! - Persist agent state to SQLite (B9 / audit).
//! - Wire `Agent::run_turn` through the existing chat handler — that's a
//!   refactor with cross-cutting impact, scoped per slice as needed.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::ChatMessage;

/// UUID v7 — time-sortable so the registry's natural ordering matches
/// when agents were spawned, which is what the UI tree views want.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub Uuid);

impl AgentId {
    pub fn new() -> Self { Self(Uuid::now_v7()) }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Created but not yet running. Workers sit here briefly between
    /// `assign` and the first model call.
    Pending,
    Running,
    /// Mid-flight LLM call finished; in-memory state persisted; no new
    /// work begins until `resume`.
    Paused,
    /// Reported `complete` to its manager (or the root finished its
    /// session). `result_summary` carries the final output.
    Completed,
    /// Reported `failed` or hit a panic / unrecoverable error.
    /// `failure_reason` carries a human-readable description.
    Failed,
    /// Stopped via `interrupt`. Distinct from Failed so the UI can show
    /// "stopped by user" vs "crashed" without guessing.
    Interrupted,
}

impl AgentStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
}

/// A typed, machine-readable reason an agent ended unsuccessfully — so a
/// failure reads as a precise cause ("budget exceeded", "policy denied: …")
/// instead of an opaque string, both in the UI and to anything programmatic.
/// `failure_reason` keeps the human one-liner; `fault` adds the structured
/// code. (Phase A1 — fixes "a long run failed with a mysterious 'timeout'".)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum AgentFault {
    /// This agent's own USD budget was exhausted.
    BudgetExceeded { spent_usd: f64, cap_usd: f64 },
    /// The shared per-session (root-tree) USD budget was exhausted — the run
    /// was cut off because the whole tree spent over the cap.
    SessionBudgetExceeded { spent_usd: f64, cap_usd: f64 },
    /// A wall-clock / provider / fetch timeout fired.
    Timeout { detail: String },
    /// The policy engine denied an action.
    PolicyDenied { rule: String, reason: String },
    /// The model provider / LLM call errored.
    ProviderError { detail: String },
    /// The executor's output stream ended without a result (e.g. truncated
    /// past the capture cap, or the subprocess died mid-stream).
    StreamTruncated { detail: String },
    /// Stopped via interrupt (user / timeout / budget / policy).
    Cancelled { reason: String },
    /// Spawn refused — recursion-depth cap, rejected assignment, or a
    /// worker-channel failure before the task started.
    Spawn { detail: String },
    /// Not yet classified — carries the raw message.
    Other { detail: String },
}

impl AgentFault {
    /// Stable machine code, e.g. `"session_budget_exceeded"`.
    pub fn code(&self) -> &'static str {
        match self {
            AgentFault::BudgetExceeded { .. } => "budget_exceeded",
            AgentFault::SessionBudgetExceeded { .. } => "session_budget_exceeded",
            AgentFault::Timeout { .. } => "timeout",
            AgentFault::PolicyDenied { .. } => "policy_denied",
            AgentFault::ProviderError { .. } => "provider_error",
            AgentFault::StreamTruncated { .. } => "stream_truncated",
            AgentFault::Cancelled { .. } => "cancelled",
            AgentFault::Spawn { .. } => "spawn",
            AgentFault::Other { .. } => "other",
        }
    }

    /// Human-readable one-liner for `failure_reason` / display.
    pub fn message(&self) -> String {
        match self {
            AgentFault::BudgetExceeded { spent_usd, cap_usd } =>
                format!("agent budget exceeded — spent ${spent_usd:.2} of ${cap_usd:.2}"),
            AgentFault::SessionBudgetExceeded { spent_usd, cap_usd } =>
                format!("session budget exceeded — the run spent ${spent_usd:.2} of ${cap_usd:.2} across all agents (raise agent.session_budget_usd)"),
            AgentFault::Timeout { detail } => format!("timed out: {detail}"),
            AgentFault::PolicyDenied { rule, reason } => format!("policy '{rule}' denied: {reason}"),
            AgentFault::ProviderError { detail } => format!("model provider error: {detail}"),
            AgentFault::StreamTruncated { detail } => format!("output stream ended without a result: {detail}"),
            AgentFault::Cancelled { reason } => format!("cancelled: {reason}"),
            AgentFault::Spawn { detail } => format!("could not start: {detail}"),
            AgentFault::Other { detail } => detail.clone(),
        }
    }
}

/// USD budget tracked at per-agent granularity. Hard cap; enforcement
/// (auto-kill on exceedance) is the policy engine's job in Phase D, but
/// the `over_budget()` predicate is here so other layers can defer to a
/// single source of truth.
///
/// A `max_usd` of `f64::INFINITY` means "no cap" — used for the root
/// agent whose budget is the user's wallet. Workers always get finite
/// budgets passed at spawn time (B3).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AgentBudget {
    pub max_usd:   f64,
    pub spent_usd: f64,
}

impl AgentBudget {
    pub fn unlimited() -> Self {
        Self { max_usd: f64::INFINITY, spent_usd: 0.0 }
    }

    pub fn capped(max_usd: f64) -> Self {
        Self { max_usd, spent_usd: 0.0 }
    }

    pub fn remaining(&self) -> f64 {
        (self.max_usd - self.spent_usd).max(0.0)
    }

    pub fn is_over(&self) -> bool {
        self.spent_usd > self.max_usd
    }

    /// Charge `delta` against the budget. Saturates at `max_usd` so a
    /// single overshooting call doesn't make `spent_usd` wildly bigger
    /// than the cap (only the policy engine's "kill" decision needs to
    /// know we exceeded, not by how much).
    pub fn charge(&mut self, delta_usd: f64) {
        if delta_usd <= 0.0 { return; }
        self.spent_usd += delta_usd;
    }
}

/// Resolved `(provider, model)` choice for a single agent (slice B8).
/// Stored on each Agent so the UI can show "this worker is talking to
/// `openrouter` / `anthropic/claude-sonnet-4.7`" — and, more importantly,
/// so the executor wiring knows which provider to call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmChoice {
    /// Logical alias name (e.g. `"primary"`, `"coding"`) when the choice
    /// came from `agent.llm_aliases`; `"override"` when the spawning
    /// caller supplied an explicit `(provider, model)` for this spawn.
    pub alias:    String,
    /// Concrete provider name from `[providers]`. Resolution against
    /// `MiraConfig.providers` happens at execution time — this struct
    /// only records the choice.
    pub provider: String,
    /// Model id within that provider. None means "use the provider's
    /// configured default".
    pub model:    Option<String>,
}

/// One Agent instance. Cheap to clone via Arc — the registry hands out
/// `Arc<RwLock<Agent>>` so callers can hold a handle while another task
/// updates state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id:         AgentId,
    pub parent:     Option<AgentId>,
    /// Reverse-DNS Skill id this agent was spawned to fulfil. None for
    /// the root agent (which serves the user directly, not a Skill).
    pub skill_id:   Option<String>,
    pub status:     AgentStatus,
    pub budget:     AgentBudget,
    /// Recursion depth from the root. Root = 0, its workers = 1, etc.
    /// Hard-capped (B6) to prevent fork-bomb-style runaway spawning.
    pub depth:      u8,
    pub created_at: i64,
    /// Conversation between this agent and its manager (for workers) or
    /// with the user (for root). Append-only; trimmed only by callers
    /// who know the LLM context window.
    pub history:    Vec<ChatMessage>,
    /// One-line description of the current step, surfaced in the agents
    /// UI without needing to drill into the transcript. Workers update
    /// this via `progress` events; the root agent updates it from chat
    /// turns.
    pub current_step: Option<String>,
    /// Last self-reported progress, 0.0–1.0 (Phase A3). Drives a progress bar
    /// in the dashboard. `None` until the worker reports a `percent_done`.
    #[serde(default)]
    pub percent_done: Option<f32>,
    /// Populated when status == Completed.
    pub result_summary: Option<String>,
    /// Populated when status == Failed / Interrupted. Human one-liner.
    pub failure_reason: Option<String>,
    /// Structured fault code (Phase A1), set alongside `failure_reason` for a
    /// failed/interrupted agent so callers + the UI get a precise cause.
    #[serde(default)]
    pub fault: Option<AgentFault>,
    /// Which LLM (provider + model) this agent uses (slice B8). None
    /// when the agent is the root and routing falls back to whatever
    /// path AgentCore takes through MiraConfig today, OR for workers
    /// where the spawn omitted a choice (executor uses its own default).
    pub llm_choice: Option<LlmChoice>,
    /// Which user this agent runs on behalf of. Populated when the
    /// spawn comes from a user-facing tool (`spawn_background_task`)
    /// so the supervisor can stamp `user_id` onto terminal events for
    /// per-user automation routing. None for legacy paths (tests,
    /// internal heartbeats, root agents that aren't user-scoped).
    #[serde(default)]
    pub user_id: Option<String>,
}

impl Agent {
    /// Construct the root agent — the one bound to the user's session.
    /// Always uses an unlimited budget; the user's "budget" is their
    /// wallet, not a per-session cap.
    pub fn new_root() -> Self {
        Self {
            id:             AgentId::new(),
            parent:         None,
            skill_id:       None,
            status:         AgentStatus::Pending,
            budget:         AgentBudget::unlimited(),
            depth:          0,
            created_at:     Utc::now().timestamp_millis(),
            history:        Vec::new(),
            current_step:   None,
            percent_done:   None,
            result_summary: None,
            failure_reason: None,
            fault: None,
            llm_choice:     None,
            user_id:        None,
        }
    }

    /// Construct a worker agent under `parent_id`, bound to a Skill.
    /// Subsequent slices (B3) wire the spawn protocol so callers don't
    /// build these by hand.
    pub fn new_worker(
        parent_id: AgentId,
        parent_depth: u8,
        skill_id: impl Into<String>,
        budget_usd: f64,
    ) -> Self {
        Self {
            id:             AgentId::new(),
            parent:         Some(parent_id),
            skill_id:       Some(skill_id.into()),
            status:         AgentStatus::Pending,
            budget:         AgentBudget::capped(budget_usd),
            depth:          parent_depth.saturating_add(1),
            created_at:     Utc::now().timestamp_millis(),
            history:        Vec::new(),
            current_step:   None,
            percent_done:   None,
            result_summary: None,
            failure_reason: None,
            fault: None,
            llm_choice:     None,
            user_id:        None,
        }
    }

    /// Stamp the user this agent runs on behalf of. Builder-style so it
    /// composes with `with_llm_choice`. Only relevant for top-level
    /// user-spawned tasks; child workers inherit context via the spawn
    /// protocol, not this field.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Record the resolved LLM choice for this agent. Called by
    /// `Supervisor::spawn_worker` after the alias resolver picks a
    /// provider/model pair (slice B8).
    pub fn with_llm_choice(mut self, choice: LlmChoice) -> Self {
        self.llm_choice = Some(choice);
        self
    }

    pub fn mark_running(&mut self)              { self.status = AgentStatus::Running; }
    pub fn mark_paused(&mut self)               { self.status = AgentStatus::Paused; }
    pub fn mark_completed(&mut self, summary: impl Into<String>) {
        self.status = AgentStatus::Completed;
        self.result_summary = Some(summary.into());
    }
    pub fn mark_failed(&mut self, reason: impl Into<String>) {
        self.status = AgentStatus::Failed;
        self.failure_reason = Some(reason.into());
    }
    /// Mark failed with a typed [`AgentFault`] — sets both the structured code
    /// and the human `failure_reason` from its `message()`.
    pub fn mark_failed_with_fault(&mut self, fault: AgentFault) {
        self.status = AgentStatus::Failed;
        self.failure_reason = Some(fault.message());
        self.fault = Some(fault);
    }
    pub fn mark_interrupted(&mut self, reason: impl Into<String>) {
        self.status = AgentStatus::Interrupted;
        let reason = reason.into();
        self.failure_reason = Some(reason.clone());
        self.fault = Some(AgentFault::Cancelled { reason });
    }

    pub fn append_message(&mut self, msg: ChatMessage) {
        self.history.push(msg);
    }

    /// Convenience for "is this agent in a state where it makes sense
    /// to send it more work?" Used by the registry / supervisor.
    pub fn is_active(&self) -> bool {
        matches!(self.status, AgentStatus::Pending | AgentStatus::Running | AgentStatus::Paused)
    }
}

// ── Registry ─────────────────────────────────────────────────────────────

/// Process-wide list of live Agents. Single source of truth for "what's
/// running right now" — the agents UI (B7) reads from this, the
/// interrupt button (B5) walks it, the supervisor (B3) inserts into it.
///
/// Keyed by AgentId. Children are not stored in a separate index — we
/// scan when asked because trees are small (few hundred agents max in
/// any realistic single-host scenario).
pub struct AgentRegistry {
    agents: RwLock<HashMap<AgentId, Arc<RwLock<Agent>>>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self { agents: RwLock::new(HashMap::new()) }
    }

    /// Insert an agent. Returns the shared handle for callers who want
    /// to update it.
    pub fn register(&self, agent: Agent) -> Arc<RwLock<Agent>> {
        let id = agent.id;
        let handle = Arc::new(RwLock::new(agent));
        self.agents.write().expect("registry write").insert(id, handle.clone());
        handle
    }

    /// Drop an agent from the registry. Typically called once an agent
    /// reaches a terminal state and the UI has had a chance to record
    /// the final state. Returns `true` if the agent was present.
    pub fn unregister(&self, id: AgentId) -> bool {
        self.agents.write().expect("registry write").remove(&id).is_some()
    }

    pub fn get(&self, id: AgentId) -> Option<Arc<RwLock<Agent>>> {
        self.agents.read().expect("registry read").get(&id).cloned()
    }

    /// All currently-registered agents, in arbitrary order. Consumers
    /// who care about ordering should sort by `created_at` or `id`.
    pub fn list(&self) -> Vec<Arc<RwLock<Agent>>> {
        self.agents.read().expect("registry read").values().cloned().collect()
    }

    /// Direct children of `parent_id`. Returns handles in spawn order
    /// (UUIDv7 sorts naturally by creation time).
    pub fn children_of(&self, parent_id: AgentId) -> Vec<Arc<RwLock<Agent>>> {
        let mut out: Vec<Arc<RwLock<Agent>>> = self.agents.read().expect("registry read")
            .values()
            .filter(|h| h.read().map(|a| a.parent == Some(parent_id)).unwrap_or(false))
            .cloned()
            .collect();
        out.sort_by_key(|h| h.read().map(|a| a.id.0).unwrap_or_default());
        out
    }

    /// Every agent in the subtree rooted at `root_id`, including the
    /// root itself. Used by the interrupt-propagation path (B5) and the
    /// agents UI's tree view (B7).
    pub fn tree_under(&self, root_id: AgentId) -> Vec<Arc<RwLock<Agent>>> {
        let mut out: Vec<Arc<RwLock<Agent>>> = Vec::new();
        let mut stack = vec![root_id];
        let snapshot = self.agents.read().expect("registry read").clone();
        while let Some(id) = stack.pop() {
            if let Some(handle) = snapshot.get(&id) {
                out.push(handle.clone());
                for (other_id, other_handle) in &snapshot {
                    if other_handle.read().map(|a| a.parent == Some(id)).unwrap_or(false) {
                        stack.push(*other_id);
                    }
                }
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.agents.read().expect("registry read").len()
    }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    /// Walk up `agent_id`'s parent chain to the root. Returns the
    /// agent itself when it has no parent. Returns `None` only when
    /// `agent_id` isn't registered, or when the parent chain is broken
    /// (a parent id that's no longer in the registry — shouldn't
    /// happen in practice but we handle it defensively rather than
    /// looping forever).
    pub fn root_of(&self, agent_id: AgentId) -> Option<AgentId> {
        let snapshot = self.agents.read().expect("registry read").clone();
        let mut current = agent_id;
        // Bounded by the registry size — the depth-cap (B6) keeps
        // trees shallow but a generic guard is cheap.
        for _ in 0..snapshot.len().saturating_add(1) {
            let handle = snapshot.get(&current)?;
            let parent = handle.read().ok()?.parent;
            match parent {
                Some(p) => current = p,
                None    => return Some(current),
            }
        }
        None
    }
}

impl Default for AgentRegistry {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> Agent { Agent::new_root() }

    #[test]
    fn fault_taxonomy_sets_code_message_and_marks_failed() {
        let mut a = Agent::new_root();
        a.mark_failed_with_fault(AgentFault::SessionBudgetExceeded { spent_usd: 6.0, cap_usd: 5.0 });
        assert_eq!(a.status, AgentStatus::Failed);
        assert_eq!(a.fault.as_ref().unwrap().code(), "session_budget_exceeded");
        // failure_reason mirrors the fault's human message.
        assert!(a.failure_reason.as_deref().unwrap().contains("session budget exceeded"));
        assert!(a.failure_reason.as_deref().unwrap().contains("$5.00"));

        // Interrupt sets a Cancelled fault.
        let mut b = Agent::new_root();
        b.mark_interrupted("user");
        assert_eq!(b.status, AgentStatus::Interrupted);
        assert_eq!(b.fault.as_ref().unwrap().code(), "cancelled");

        // The fault serializes internally-tagged on `code`.
        let j = serde_json::to_value(AgentFault::PolicyDenied {
            rule: "net".into(), reason: "blocked".into(),
        }).unwrap();
        assert_eq!(j["code"], "policy_denied");
        assert_eq!(j["rule"], "net");
    }

    #[test]
    fn ids_are_unique_and_v7() {
        let a = AgentId::new();
        let b = AgentId::new();
        assert_ne!(a, b);
        assert_eq!(a.0.get_version_num(), 7);
    }

    #[test]
    fn root_agent_has_unlimited_budget_and_depth_zero() {
        let r = root();
        assert!(r.parent.is_none());
        assert!(r.skill_id.is_none());
        assert_eq!(r.depth, 0);
        assert!(r.budget.max_usd.is_infinite());
        assert_eq!(r.status, AgentStatus::Pending);
    }

    #[test]
    fn worker_inherits_depth_plus_one() {
        let r = root();
        let w = Agent::new_worker(r.id, r.depth, "com.mira.research", 1.50);
        assert_eq!(w.depth, 1);
        assert_eq!(w.parent, Some(r.id));
        assert_eq!(w.skill_id.as_deref(), Some("com.mira.research"));
        assert_eq!(w.budget.max_usd, 1.50);
    }

    #[test]
    fn budget_charge_and_over_predicate() {
        let mut b = AgentBudget::capped(1.00);
        assert!(!b.is_over());
        assert_eq!(b.remaining(), 1.00);
        b.charge(0.40);
        b.charge(0.40);
        assert!(!b.is_over());
        assert!((b.remaining() - 0.20).abs() < 1e-9);
        b.charge(0.50);
        assert!(b.is_over(), "exceeded cap should report over");
        assert_eq!(b.remaining(), 0.0, "remaining clamps to 0 when over");
    }

    #[test]
    fn negative_charges_are_ignored() {
        // We never want to "refund" a budget — a tool that fails after
        // the LLM call still cost the LLM call. Make this explicit.
        let mut b = AgentBudget::capped(1.00);
        b.charge(0.50);
        b.charge(-0.30);
        assert_eq!(b.spent_usd, 0.50);
    }

    #[test]
    fn lifecycle_transitions() {
        let mut a = root();
        assert_eq!(a.status, AgentStatus::Pending);
        assert!(a.is_active());

        a.mark_running();
        assert_eq!(a.status, AgentStatus::Running);
        assert!(a.is_active());

        a.mark_paused();
        assert_eq!(a.status, AgentStatus::Paused);
        assert!(a.is_active());

        a.mark_completed("done");
        assert_eq!(a.status, AgentStatus::Completed);
        assert!(!a.is_active());
        assert_eq!(a.result_summary.as_deref(), Some("done"));
        assert!(a.status.is_terminal());
    }

    #[test]
    fn registry_register_get_unregister() {
        let reg = AgentRegistry::new();
        let id = {
            let h = reg.register(root());
            let a = h.read().unwrap();
            a.id
        };

        let h = reg.get(id).expect("registered");
        assert_eq!(h.read().unwrap().id, id);

        assert!(reg.unregister(id));
        assert!(reg.get(id).is_none());
        assert!(!reg.unregister(id));
    }

    #[test]
    fn registry_tree_under_walks_subtree() {
        let reg = AgentRegistry::new();
        let r = reg.register(Agent::new_root());
        let r_id = r.read().unwrap().id;

        let w1 = reg.register(Agent::new_worker(r_id, 0, "com.example.a", 1.0));
        let w1_id = w1.read().unwrap().id;
        let _w2 = reg.register(Agent::new_worker(r_id, 0, "com.example.b", 1.0));

        // grandchild under w1
        let _gc = reg.register(Agent::new_worker(w1_id, 1, "com.example.c", 0.5));

        let subtree = reg.tree_under(r_id);
        assert_eq!(subtree.len(), 4, "root + 2 children + 1 grandchild");

        let only_w1 = reg.tree_under(w1_id);
        assert_eq!(only_w1.len(), 2, "w1 + its grandchild");

        let children = reg.children_of(r_id);
        assert_eq!(children.len(), 2, "two direct children");
    }

    #[test]
    fn registry_root_of_walks_up_to_top() {
        let reg = AgentRegistry::new();
        let r = reg.register(Agent::new_root());     let r_id = r.read().unwrap().id;
        let w = reg.register(Agent::new_worker(r_id, 0, "com.x", 1.0));
                                                    let w_id = w.read().unwrap().id;
        let g = reg.register(Agent::new_worker(w_id, 1, "com.y", 0.5));
                                                    let g_id = g.read().unwrap().id;

        assert_eq!(reg.root_of(r_id), Some(r_id), "root's root is itself");
        assert_eq!(reg.root_of(w_id), Some(r_id));
        assert_eq!(reg.root_of(g_id), Some(r_id));
        assert_eq!(reg.root_of(AgentId::new()), None, "unknown id");
    }

    #[test]
    fn registry_isolates_unrelated_trees() {
        let reg = AgentRegistry::new();
        let a = reg.register(Agent::new_root()); let a_id = a.read().unwrap().id;
        let b = reg.register(Agent::new_root()); let b_id = b.read().unwrap().id;

        let _wa = reg.register(Agent::new_worker(a_id, 0, "com.x", 1.0));
        let _wb = reg.register(Agent::new_worker(b_id, 0, "com.y", 1.0));

        assert_eq!(reg.tree_under(a_id).len(), 2);
        assert_eq!(reg.tree_under(b_id).len(), 2);
        assert_eq!(reg.len(), 4);
    }
}
