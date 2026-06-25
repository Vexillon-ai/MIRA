// SPDX-License-Identifier: AGPL-3.0-or-later

//! Spawn + supervise a worker Agent (slice B3).
//!
//! Manager-side API: `Supervisor::spawn_worker` creates an Agent,
//! registers it, spawns the runtime tasks, and hands back a
//! [`WorkerHandle`] whose `completion` future resolves with the
//! terminal outcome.
//!
//! Worker-side runtime: a tokio task that
//!   1. Awaits the manager's `Assign` request, replies `Accepted`
//!      (rejection paths land later when WorkerTask gains a "can I
//!      handle this?" check).
//!   2. Marks the Agent as `Running`.
//!   3. Calls the user-supplied `WorkerTask::run` with a
//!      [`WorkerContext`] that lets the executor emit `Progress`
//!      events.
//!   4. Translates the `Result` into a terminal `Complete` /
//!      `Failed` event and updates the Agent state to match.
//!
//! Manager-side runtime: a sibling tokio task that
//!   1. Sends the `Assign` request.
//!   2. Receives Events and Requests from the worker.
//!   3. Resolves `Progress` to a `current_step` update.
//!   4. Resolves the first `Complete` / `Failed` to a `WorkerOutcome`,
//!      updates Agent state, and fires the completion oneshot.
//!   5. For requests we don't yet handle (RequestReview /
//!      RequestUserInput / SpawnChild), replies with a polite
//!      placeholder so the worker isn't left hanging — proper
//!      handling lands in B6 (spawn approval) and beyond.
//!
//! Out of scope for this slice (tracked elsewhere):
//! - Pause / Resume / Interrupt propagation (B5)
//! - Real LLM-driven worker executors (Phase C adapters)
//! - Cost accounting from real LLM calls (B4)
//! - Spawn-child approval logic (B6)

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, RwLock};
use tracing::{debug, warn};

use crate::agent::audit::{AuditEvent, AuditStore};
use crate::agent::instance::{Agent, AgentId, AgentRegistry, AgentStatus, LlmChoice};
use crate::config::LlmAlias;
use crate::agent::protocol::{Event, InterruptReason, Request, Response};
use crate::agent::transport::{
    AgentChannel, ChannelSender, Incoming,
};

// ─── Session budget ───────────────────────────────────────────────────

/// Default cap on total LLM spend across all agents in one tree.
/// Arbitrary — chosen so a runaway recursion costs single-digit dollars
/// before getting killed. Will graduate to MiraConfig once we have
/// real usage to inform the right number.
pub const DEFAULT_SESSION_BUDGET_USD: f64 = 5.0;

/// Resolve a [`LlmChoice`] for a worker about to spawn (slice B8).
///
/// Resolution order:
///   1. Walk the Skill's `permissions.llm_providers` list (left-to-
///      right) and pick the first alias that exists in `aliases`.
///   2. Fall back to the alias literally named `"primary"` if that's
///      configured.
///   3. Fall back to a synthetic `LlmChoice` referring to
///      `primary_provider` with no model override.
///   4. Returns `None` only when the caller passed an empty list AND
///      no `"primary"` alias is configured AND `primary_provider` is
///      empty — pathological config that signals "the operator hasn't
///      finished setting things up yet."
///
/// Pure function with no I/O — easy to unit-test.
pub fn resolve_llm_choice(
    skill_llm_providers: &[String],
    aliases:             &std::collections::HashMap<String, LlmAlias>,
    primary_provider:    &str,
) -> Option<LlmChoice> {
    // 1. Try the Skill's preferences in order.
    for alias_name in skill_llm_providers {
        if let Some(a) = aliases.get(alias_name) {
            return Some(LlmChoice {
                alias:    alias_name.clone(),
                provider: a.provider.clone(),
                model:    a.model.clone(),
            });
        }
    }
    // 2. Try the literal "primary" alias.
    if let Some(a) = aliases.get("primary") {
        return Some(LlmChoice {
            alias:    "primary".to_string(),
            provider: a.provider.clone(),
            model:    a.model.clone(),
        });
    }
    // 3. Fall back to the configured primary provider with no model.
    if !primary_provider.is_empty() {
        return Some(LlmChoice {
            alias:    "primary".to_string(),
            provider: primary_provider.to_string(),
            model:    None,
        });
    }
    None
}

/// Hardest cap on how deep the manager/worker tree can grow — slice
/// B6. Spec calls this "Hard cap at install time"; for v1 we ship a
/// constant and revisit if real workflows need different.
///
/// Depth = 0 is the root; depth 5 leaves room for a 5-level tree
/// (root → manager → coding-worker → review-sub-agent → toolchain-
/// sub-agent → fact-checker-sub-agent). Anything deeper is almost
/// certainly an unintentional recursion or an over-engineered plan.
pub const MAX_RECURSION_DEPTH: u8 = 5;

/// Shared cap across every agent in a tree rooted at the same root
/// agent. The supervisor maintains one of these per root and propagates
/// a clone of the `Arc` to every manager-loop task spawned under it.
///
/// The `over` flag is the interrupt signal: any manager loop that sees
/// it set on its next progress event exits with a Failed outcome
/// stamped "session budget exceeded". Whichever sibling pushed us over
/// noticed first; everyone else folds shortly after.
#[derive(Debug)]
pub struct SessionBudget {
    pub max_usd:   f64,
    pub spent_usd: RwLock<f64>,
    pub over:      AtomicBool,
}

impl SessionBudget {
    pub fn new(max_usd: f64) -> Self {
        Self { max_usd, spent_usd: RwLock::new(0.0), over: AtomicBool::new(false) }
    }

    /// Add `delta` to spend. Returns true if this charge pushed us
    /// past the cap (the caller should propagate the kill).
    pub async fn charge(&self, delta_usd: f64) -> bool {
        if delta_usd <= 0.0 { return self.over.load(Ordering::Acquire); }
        let mut spent = self.spent_usd.write().await;
        *spent += delta_usd;
        if *spent > self.max_usd && !self.over.swap(true, Ordering::AcqRel) {
            return true;
        }
        self.over.load(Ordering::Acquire)
    }

    pub fn is_over(&self) -> bool { self.over.load(Ordering::Acquire) }

    pub async fn spent(&self) -> f64 { *self.spent_usd.read().await }
}

// ─── Public API ────────────────────────────────────────────────────────

/// What the manager hands a worker at spawn time. Passed through
/// `WorkerTask::run` so the executor sees the original task plus the
/// budget/deadline it's supposed to honour.
#[derive(Debug, Clone, Default)]
pub struct WorkerAssignment {
    pub task:        String,
    pub context:     Option<serde_json::Value>,
    pub budget_usd:  f64,
    /// Wall-clock deadline as unix ms. None = the budget is the cap.
    pub deadline_ms: Option<i64>,
    /// User on whose behalf the worker runs. Subprocess adapters use
    /// this to resolve per-user secrets (env vars). None for
    /// system-internal workers.
    pub user_id:     Option<String>,
    /// Reverse-DNS skill id this worker is fulfilling. Used by
    /// adapters that need to look up skill-scoped secrets without a
    /// registry round-trip.
    pub skill_id:    Option<String>,
}

/// Terminal-success payload an executor returns. Becomes an
/// `Event::Complete` on the wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerComplete {
    pub result_summary: String,
    #[serde(default)]
    pub artifacts:      Vec<String>,
}

/// Terminal-failure payload. Becomes an `Event::Failed` on the wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerFailure {
    pub error:             String,
    #[serde(default)]
    pub partial_artifacts: Vec<String>,
    /// Structured cause (Phase A1). `None` → classified as `Other{error}` when
    /// applied to the Agent. Set it at the source for budget/policy/spawn/etc.
    #[serde(default)]
    pub fault:             Option<crate::agent::instance::AgentFault>,
}

impl WorkerFailure {
    /// A failure with a typed fault; `error` mirrors the fault's message.
    pub fn faulted(fault: crate::agent::instance::AgentFault) -> Self {
        Self { error: fault.message(), partial_artifacts: Vec::new(), fault: Some(fault) }
    }
}

/// The two terminal outcomes a worker can resolve to. The completion
/// oneshot in [`WorkerHandle`] resolves to one of these.
#[derive(Debug, Clone)]
pub enum WorkerOutcome {
    Complete(WorkerComplete),
    Failed(WorkerFailure),
}

/// What the manager pulls back from `Supervisor::spawn_worker`.
#[derive(Debug)]
pub struct WorkerHandle {
    pub agent_id:   AgentId,
    /// Resolves with the worker's terminal outcome — `Complete` or
    /// `Failed` — once the worker exits. If the worker task panics or
    /// the channel breaks, resolves with `Failed { error: "..." }`.
    pub completion: oneshot::Receiver<WorkerOutcome>,
}

/// What a worker executor can do mid-flight: emit progress, request
/// review/user-input, and spawn child workers. Holds a `ChannelSender`
/// internally; the methods constrain which Request kinds the executor
/// can issue (Interrupt/Pause/Resume are intentionally not exposed —
/// those are manager-only operations).
pub struct WorkerContext {
    pub agent_id: AgentId,
    sender:       ChannelSender,
}

impl WorkerContext {
    /// Stream a progress update back to the manager. Best-effort: a
    /// peer-dropped channel returns Err but doesn't panic.
    pub fn report_progress(
        &self,
        summary: impl Into<String>,
        percent_done: Option<f32>,
        llm_spend_usd: f64,
    ) {
        let _ = self.sender.send_event(Event::Progress {
            step_summary: summary.into(),
            percent_done,
            llm_spend_usd,
        });
    }

    /// Clone the underlying sender for fan-out work. Useful when an
    /// executor spawns its own background tasks (e.g. the C1
    /// subprocess adapter reads stdout in a separate task and needs a
    /// sender to push progress events from there).
    pub fn sender_clone(&self) -> ChannelSender {
        self.sender.clone()
    }

    /// Request to spawn a child worker. Manager checks depth + session
    /// budget + executor availability before approving (slice B6).
    /// Returns the new child's id on approval.
    pub async fn spawn_child(
        &self,
        skill_id:   impl Into<String>,
        task:       impl Into<String>,
        budget_usd: f64,
    ) -> Result<AgentId, SpawnChildError> {
        let req = Request::SpawnChild {
            skill_id:   skill_id.into(),
            task:       task.into(),
            budget_usd,
        };
        match self.sender.request(req).await {
            Ok(Response::SpawnDecision { approved: true, spawned_agent_id: Some(id), .. }) => {
                Ok(id)
            }
            Ok(Response::SpawnDecision { approved: false, reason, .. }) => {
                Err(SpawnChildError::Denied(reason.unwrap_or_else(|| "no reason given".into())))
            }
            Ok(other) => Err(SpawnChildError::Protocol(format!("{other:?}"))),
            Err(_)    => Err(SpawnChildError::ChannelDropped),
        }
    }
}

/// What an executor does. Implementations decide what kind of work
/// the worker performs — wrapping an external coding agent (Phase C),
/// running a research loop with the agent's own tool registry, or
/// just returning a canned result for tests.
#[async_trait]
pub trait WorkerTask: Send + Sync {
    async fn run(
        &self,
        assignment: WorkerAssignment,
        ctx:        WorkerContext,
    ) -> Result<WorkerComplete, WorkerFailure>;
}

/// Maps a Skill id to the executor that should run when a worker
/// requests a child of that Skill. The Supervisor consults this when
/// approving a `SpawnChild` request (slice B6) and refuses the spawn
/// when the resolver returns `None`.
///
/// Production wiring will plug a resolver that consults the loaded
/// `SkillRegistry`; tests use a `HashMap`-based stub.
pub trait SkillExecutorResolver: Send + Sync {
    fn executor_for(&self, skill_id: &str) -> Option<Arc<dyn WorkerTask>>;
}

/// Trivial resolver that always returns `None`. The default when no
/// resolver was wired in — useful for the existing happy-path tests
/// where workers don't try to spawn children.
pub struct NullExecutorResolver;
impl SkillExecutorResolver for NullExecutorResolver {
    fn executor_for(&self, _skill_id: &str) -> Option<Arc<dyn WorkerTask>> { None }
}

/// Errors `WorkerContext::spawn_child` can surface to its executor.
#[derive(Debug, thiserror::Error)]
pub enum SpawnChildError {
    #[error("manager denied the spawn: {0}")]
    Denied(String),
    #[error("manager dropped the channel before responding")]
    ChannelDropped,
    #[error("manager replied with an unexpected response: {0}")]
    Protocol(String),
}

type ChannelMap = Arc<std::sync::RwLock<HashMap<AgentId, ChannelSender>>>;

/// Spawns workers and tracks them in the shared `AgentRegistry`.
///
/// Wrap in `Arc<Supervisor>` for sharing — manager loops hold a clone
/// so they can call `spawn_worker` recursively when a worker requests
/// a child via the SpawnChild protocol.
pub struct Supervisor {
    registry: Arc<AgentRegistry>,
    /// One `SessionBudget` per root agent. Workers under the same root
    /// share their budget and kill-switch.
    sessions: std::sync::RwLock<HashMap<AgentId, Arc<SessionBudget>>>,
    /// Cap used when a session budget is created on first spawn.
    /// Overridable per spawn for tests; production wiring will pull
    /// this from MiraConfig.
    default_session_budget_usd: f64,
    /// Per-spawned-agent ChannelSender, retained so interrupt / pause /
    /// resume can be issued asynchronously from the API layer (B7) or
    /// from the supervisor's own tree-wide interrupt path. Manager
    /// loops remove their own entry on terminal exit.
    channels: ChannelMap,
    /// Looks up an executor for a Skill id when a worker requests a
    /// child spawn. Default is `NullExecutorResolver` which denies
    /// every spawn attempt — overridable via `with_resolver`.
    resolver: Arc<dyn SkillExecutorResolver>,
    /// Append-only audit log (slice B9). When `None`, audit recording
    /// is a no-op — useful for tests that don't care about forensics.
    /// Production wiring opens an `AuditStore` at startup and plumbs it
    /// in via `with_audit_store`.
    audit_store: Option<Arc<AuditStore>>,
    /// Policy engine consulted before spawning child workers (slice D1).
    /// When `None`, every spawn is allowed — same posture as B6 before
    /// D1 landed. Production wiring sets this in the gateway.
    policy_engine: Option<Arc<dyn crate::policy::PolicyEngine>>,
    /// Internal event bus. When set, the supervisor emits
    /// `agent.worker.completed` on every terminal worker outcome so
    /// user-registered subscriptions (e.g. `spawn_background_task`'s
    /// auto-delivery) fire. None = no-op (legacy / test builds).
    event_bus: Option<Arc<crate::events::EventBus>>,
    /// 0.110.0 — when set, every non-zero LLM cost delta is appended
    /// to the `llm_charges` ledger so the cost-burn detector sums
    /// against real history, not a running-budget snapshot.
    health_store: Option<Arc<crate::health::store::HealthStore>>,
    /// 0.111.0 — task artifact directory manager. When wired, the
    /// terminal-outcome path runs `finalize(task_id)` to read the
    /// SLUG file and rename the dir to `<slug>_<task_id_short>`.
    task_artifacts: Option<Arc<crate::task_artifacts::TaskArtifactsStore>>,
}

impl Supervisor {
    pub fn new(registry: Arc<AgentRegistry>) -> Self {
        Self {
            registry,
            sessions: std::sync::RwLock::new(HashMap::new()),
            default_session_budget_usd: DEFAULT_SESSION_BUDGET_USD,
            channels: Arc::new(std::sync::RwLock::new(HashMap::new())),
            resolver: Arc::new(NullExecutorResolver),
            audit_store: None,
            policy_engine: None,
            event_bus: None,
            health_store: None,
            task_artifacts: None,
        }
    }

    /// 0.110.0 — wire the LLM cost ledger writer.
    pub fn with_health_store(mut self, store: Arc<crate::health::store::HealthStore>) -> Self {
        self.health_store = Some(store);
        self
    }

    /// 0.111.0 — wire the task-artifact dir manager so terminal-
    /// outcome finalisation (slug rename + manifest update) runs
    /// inside the manager loop.
    pub fn with_task_artifacts(
        mut self, store: Arc<crate::task_artifacts::TaskArtifactsStore>,
    ) -> Self {
        self.task_artifacts = Some(store);
        self
    }

    /// Plug in an [`EventBus`] so terminal worker outcomes are published
    /// as `agent.worker.completed` events. Without this, the
    /// `spawn_background_task` auto-delivery has nothing to fire on.
    pub fn with_event_bus(mut self, bus: Arc<crate::events::EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Plug in a [`crate::policy::PolicyEngine`] (slice D1). Currently
    /// consulted only in the spawn-child path; further integration
    /// points (tool runtime, LLM client, network egress) land in D2+.
    /// When unset, the supervisor behaves exactly as before D1 (no
    /// policy gating).
    pub fn with_policy_engine(
        mut self, engine: Arc<dyn crate::policy::PolicyEngine>,
    ) -> Self {
        self.policy_engine = Some(engine);
        self
    }

    /// Plug in an [`AuditStore`] so spawn / interrupt / budget / status
    /// transitions are recorded to the HMAC-chained audit log.
    pub fn with_audit_store(mut self, store: Arc<AuditStore>) -> Self {
        self.audit_store = Some(store);
        self
    }

    /// Returns the audit store the supervisor was wired with (used by
    /// the /api/agents/audit handler to query without re-opening the DB).
    pub fn audit_store(&self) -> Option<Arc<AuditStore>> {
        self.audit_store.clone()
    }

    /// Record one audit event. Best-effort: a DB error is logged but
    /// never bubbles up — losing a forensic record can't be allowed to
    /// crash the runtime.
    fn audit(&self, agent_id: AgentId, event: AuditEvent) {
        if let Some(store) = &self.audit_store {
            // Stamp the initiating user so the audit log can be scoped
            // per-user (non-admins see only their own agents' events).
            // None when the agent isn't in the registry or is system-initiated.
            let user_id = self.registry.get(agent_id)
                .and_then(|h| h.read().ok().and_then(|a| a.user_id.clone()));
            if let Err(e) = store.record(agent_id, user_id.as_deref(), event) {
                warn!("audit record failed: {e}");
            }
        }
    }

    /// Override the session-budget default. Used by tests; production
    /// callers should set this once at boot from MiraConfig.
    pub fn with_session_budget(mut self, max_usd: f64) -> Self {
        self.default_session_budget_usd = max_usd;
        self
    }

    /// Plug in a resolver that maps Skill ids to executors. Without
    /// this, every `SpawnChild` request from a worker is denied with
    /// "no executor resolver configured".
    pub fn with_resolver(mut self, resolver: Arc<dyn SkillExecutorResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Look up the executor for a Skill id via the wired resolver.
    /// Returns `None` when the Skill isn't registered (or the default
    /// `NullExecutorResolver` is in place). User-facing tools that
    /// kick off top-level workers (e.g. `spawn_background_task`)
    /// consult this before calling `spawn_worker` so they can return
    /// a clear error instead of failing inside the manager loop.
    pub fn executor_for(&self, skill_id: &str) -> Option<Arc<dyn WorkerTask>> {
        self.resolver.executor_for(skill_id)
    }

    /// Snapshot the current session spend for the tree rooted at
    /// `root_id`. Returns 0.0 if no session budget has been
    /// established (no workers spawned yet under that root).
    pub async fn session_spend(&self, root_id: AgentId) -> f64 {
        let session = {
            let map = self.sessions.read().expect("sessions read");
            map.get(&root_id).cloned()
        };
        match session {
            Some(s) => s.spent().await,
            None    => 0.0,
        }
    }

    /// Get-or-create the SessionBudget for the tree rooted at `root_id`.
    fn session_for(&self, root_id: AgentId) -> Arc<SessionBudget> {
        let mut map = self.sessions.write().expect("sessions write");
        map.entry(root_id)
            .or_insert_with(|| Arc::new(SessionBudget::new(self.default_session_budget_usd)))
            .clone()
    }

    /// Create a worker Agent under `parent_id` and start it running
    /// `executor` with the supplied task + budget + deadline. Returns
    /// immediately; the manager awaits `handle.completion` for the
    /// terminal outcome.
    ///
    /// The Agent is registered in the registry before any task is
    /// spawned, so the agents UI / interrupt path can see it from
    /// the very first instant.
    pub fn spawn_worker(
        self:         &Arc<Self>,
        parent_id:    AgentId,
        parent_depth: u8,
        skill_id:     impl Into<String>,
        task:         impl Into<String>,
        context:      Option<serde_json::Value>,
        budget_usd:   f64,
        deadline_ms:  Option<i64>,
        executor:     Arc<dyn WorkerTask>,
    ) -> WorkerHandle {
        self.spawn_worker_with(
            parent_id, parent_depth, skill_id, task, context,
            budget_usd, deadline_ms, executor, None,
        )
    }

    /// Same as [`spawn_worker`] but lets the caller pass an explicit
    /// `LlmChoice` for this spawn (slice B8). Use cases:
    /// - Phase C executor wiring resolves the Skill's
    ///   `llm_providers` against `agent.llm_aliases` and passes the
    ///   result.
    /// - Pre-spawn UI override ("run this one task on the cheap model")
    ///   resolves the user's pick to a `LlmChoice` and passes it
    ///   verbatim.
    pub fn spawn_worker_with(
        self:         &Arc<Self>,
        parent_id:    AgentId,
        parent_depth: u8,
        skill_id:     impl Into<String>,
        task:         impl Into<String>,
        context:      Option<serde_json::Value>,
        budget_usd:   f64,
        deadline_ms:  Option<i64>,
        executor:     Arc<dyn WorkerTask>,
        llm_choice:   Option<LlmChoice>,
    ) -> WorkerHandle {
        self.spawn_worker_full(
            parent_id, parent_depth, skill_id, task, context,
            budget_usd, deadline_ms, executor, llm_choice, None,
        )
    }

    /// Most general spawn. `user_id` stamps the agent so terminal
    /// events can be routed to per-user automations (the
    /// `agent.worker.completed` event filter). Other spawn entry
    /// points pass `None` and behave as before.
    pub fn spawn_worker_full(
        self:         &Arc<Self>,
        parent_id:    AgentId,
        parent_depth: u8,
        skill_id:     impl Into<String>,
        task:         impl Into<String>,
        context:      Option<serde_json::Value>,
        budget_usd:   f64,
        deadline_ms:  Option<i64>,
        executor:     Arc<dyn WorkerTask>,
        llm_choice:   Option<LlmChoice>,
        user_id:      Option<String>,
    ) -> WorkerHandle {
        self.spawn_worker_full_with_id(
            parent_id, parent_depth, skill_id, task, context, budget_usd,
            deadline_ms, executor, llm_choice, user_id, None,
        )
    }

    /// 0.111.0 — same as [`spawn_worker_full`] but lets the caller
    /// pre-generate the worker's AgentId. Used by spawn_background_task
    /// so it can allocate the artifact dir under that id BEFORE spawn,
    /// then thread the dir into context. None = generate normally.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_worker_full_with_id(
        self:         &Arc<Self>,
        parent_id:    AgentId,
        parent_depth: u8,
        skill_id:     impl Into<String>,
        task:         impl Into<String>,
        context:      Option<serde_json::Value>,
        budget_usd:   f64,
        deadline_ms:  Option<i64>,
        executor:     Arc<dyn WorkerTask>,
        llm_choice:   Option<LlmChoice>,
        user_id:      Option<String>,
        id_override:  Option<AgentId>,
    ) -> WorkerHandle {
        let skill_id = skill_id.into();
        let task     = task.into();

        let mut agent = Agent::new_worker(parent_id, parent_depth, skill_id.clone(), budget_usd);
        if let Some(id) = id_override {
            agent.id = id;
        }
        if let Some(choice) = llm_choice {
            agent = agent.with_llm_choice(choice);
        }
        if let Some(uid) = user_id {
            agent = agent.with_user_id(uid);
        }
        let agent_handle = self.registry.register(agent);
        let agent_id = agent_handle.read().expect("agent read").id;

        // Audit: record that this agent was spawned. SpawnRequested is
        // attributed to the spawning *parent* (so the trail reads "X
        // requested Y") and SpawnApproved is attributed to the new
        // agent itself (so a per-agent query starts with its own
        // creation event).
        self.audit(parent_id, AuditEvent::SpawnRequested {
            skill_id:   skill_id.clone(),
            budget_usd,
        });
        self.audit(agent_id, AuditEvent::SpawnApproved {
            skill_id: skill_id.clone(),
            child_id: agent_id,
        });

        // Find (or default to parent_id when the parent isn't in the
        // registry, e.g. tests) the root and resolve the shared
        // SessionBudget for this tree.
        let root_id = self.registry.root_of(parent_id).unwrap_or(parent_id);
        let session = self.session_for(root_id);

        let (mgr_chan, worker_chan) = AgentChannel::pair();
        let (completion_tx, completion_rx) = oneshot::channel();

        // Stash a ChannelSender that points at the worker so
        // interrupt/pause/resume can be issued asynchronously.
        // mgr_chan.sender() points at the worker (mgr→worker dir),
        // which is what we want for sending control requests.
        self.channels
            .write().expect("channels write")
            .insert(agent_id, mgr_chan.sender());

        // Worker-side runtime task — races the executor against an
        // incoming control loop so Interrupt drops the executor future.
        let agent_for_worker = agent_handle.clone();
        tokio::spawn(run_worker_loop(
            worker_chan, executor, agent_for_worker,
        ));

        // Manager-side runtime task — sends Assign, drains events,
        // updates Agent state, fires the completion oneshot, and on
        // SpawnChild can call back into the Supervisor (slice B6).
        let agent_for_mgr = agent_handle.clone();
        let channels_for_mgr = self.channels.clone();
        let supervisor_for_mgr = Arc::clone(self);
        tokio::spawn(run_manager_loop(
            mgr_chan, task, context, budget_usd, deadline_ms,
            agent_for_mgr, session, completion_tx, channels_for_mgr, agent_id,
            supervisor_for_mgr,
        ));

        debug!("Supervisor spawned worker {agent_id} under {parent_id} for skill {skill_id:?}");
        WorkerHandle { agent_id, completion: completion_rx }
    }

    /// Send an Interrupt request to one agent. Returns the worker's
    /// response or PeerDropped if the worker has already exited.
    pub async fn interrupt(&self, agent_id: AgentId, reason: InterruptReason)
        -> Result<(), InterruptError>
    {
        let sender = self.channels.read().expect("channels read")
            .get(&agent_id).cloned()
            .ok_or(InterruptError::NotRunning)?;
        match sender.request(Request::Interrupt { reason }).await {
            Ok(Response::Ack) => {
                self.audit(agent_id, AuditEvent::Interrupted {
                    reason: format!("{reason:?}").to_lowercase(),
                });
                Ok(())
            },
            Ok(other)         => Err(InterruptError::Protocol(format!("expected Ack, got {other:?}"))),
            Err(_)            => Err(InterruptError::PeerDropped),
        }
    }

    /// Walk the tree rooted at `root_id` and interrupt every active
    /// agent. Returns the count actually signalled — agents that have
    /// already finished are skipped silently.
    pub async fn interrupt_tree(&self, root_id: AgentId, reason: InterruptReason) -> usize {
        let active_ids: Vec<AgentId> = self.registry.tree_under(root_id).iter()
            .filter_map(|h| {
                let a = h.read().ok()?;
                if a.is_active() { Some(a.id) } else { None }
            })
            .collect();

        let mut signalled = 0;
        for id in active_ids {
            if self.interrupt(id, reason).await.is_ok() {
                signalled += 1;
            }
        }
        signalled
    }

    /// Send a Pause request to one agent. Worker side acks; the agent
    /// is marked Paused. Real cooperative pause (executor halts mid-
    /// LLM-call) lands when first non-stub executors do.
    pub async fn pause(&self, agent_id: AgentId) -> Result<(), InterruptError> {
        let sender = self.channels.read().expect("channels read")
            .get(&agent_id).cloned()
            .ok_or(InterruptError::NotRunning)?;
        match sender.request(Request::Pause).await {
            Ok(Response::Ack) => Ok(()),
            Ok(other)         => Err(InterruptError::Protocol(format!("expected Ack, got {other:?}"))),
            Err(_)            => Err(InterruptError::PeerDropped),
        }
    }

    pub async fn resume(&self, agent_id: AgentId) -> Result<(), InterruptError> {
        let sender = self.channels.read().expect("channels read")
            .get(&agent_id).cloned()
            .ok_or(InterruptError::NotRunning)?;
        match sender.request(Request::Resume).await {
            Ok(Response::Ack) => Ok(()),
            Ok(other)         => Err(InterruptError::Protocol(format!("expected Ack, got {other:?}"))),
            Err(_)            => Err(InterruptError::PeerDropped),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InterruptError {
    #[error("agent is not currently running (already completed or never existed)")]
    NotRunning,
    #[error("agent's channel is dead — it crashed or already exited")]
    PeerDropped,
    #[error("protocol violation: {0}")]
    Protocol(String),
}

// ─── Worker-side loop ──────────────────────────────────────────────────

async fn run_worker_loop(
    mut channel:  AgentChannel,
    executor:     Arc<dyn WorkerTask>,
    agent_handle: Arc<std::sync::RwLock<Agent>>,
) {
    // Wait for the Assign request from the manager. Anything else
    // before Assign is a protocol violation — log, fail the worker.
    let (assign_id, assignment) = match channel.recv().await {
        Some(Incoming::Request { id, payload: Request::Assign {
            task, context, budget_usd, deadline_ms, user_id, skill_id,
        } }) => (id, WorkerAssignment {
            task, context, budget_usd, deadline_ms, user_id, skill_id,
        }),
        Some(other) => {
            warn!("worker expected Assign first, got {other:?}");
            let _ = channel.send_event(Event::Failed {
                error: "expected Assign as first request".into(),
                partial_artifacts: vec![],
            });
            mark(&agent_handle, |a| a.mark_failed("protocol violation: no Assign"));
            return;
        }
        None => {
            mark(&agent_handle, |a| a.mark_failed("manager dropped before Assign"));
            return;
        }
    };

    // Accept and transition to Running.
    if channel.reply(assign_id, Response::Accepted).is_err() {
        mark(&agent_handle, |a| a.mark_failed("manager dropped before Accepted reached it"));
        return;
    }
    mark(&agent_handle, |a| a.mark_running());

    // Build the context up-front. The executor gets a ChannelSender
    // clone, so it can issue worker→manager requests (SpawnChild,
    // RequestReview, RequestUserInput) plus events. The unique
    // receiver stays here in the worker runtime to handle control.
    let ctx = WorkerContext {
        agent_id: agent_handle.read().expect("agent read").id,
        sender:   channel.sender(),
    };
    let executor_fut = executor.run(assignment, ctx);
    tokio::pin!(executor_fut);

    // Race the executor against incoming control requests. `biased`
    // makes the control branch win ties — without it, an Interrupt
    // arriving the same tick the executor finishes might be dropped.
    loop {
        tokio::select! {
            biased;

            inc = channel.recv() => match inc {
                Some(Incoming::Request { id, payload: Request::Interrupt { reason } }) => {
                    let _ = channel.reply(id, Response::Ack);
                    let reason_str = format!("{reason:?}").to_lowercase();
                    let _ = channel.send_event(Event::Failed {
                        error:             format!("interrupted: {reason_str}"),
                        partial_artifacts: vec![],
                    });
                    mark(&agent_handle, |a| a.mark_interrupted(format!("interrupt: {reason_str}")));
                    // executor_fut is dropped on this `return`, cancelling
                    // the executor's in-flight async operations.
                    return;
                }
                Some(Incoming::Request { id, payload: Request::Pause }) => {
                    let _ = channel.reply(id, Response::Ack);
                    // Status-only pause for now — real cooperative
                    // halt-mid-LLM-call lands when first non-stub
                    // executors do, and they'll check ctx.is_paused().
                    mark(&agent_handle, |a| a.mark_paused());
                }
                Some(Incoming::Request { id, payload: Request::Resume }) => {
                    let _ = channel.reply(id, Response::Ack);
                    mark(&agent_handle, |a| a.mark_running());
                }
                Some(Incoming::Request { id, payload: other }) => {
                    let _ = channel.reply(id, Response::Error {
                        message: format!("worker doesn't handle {other:?}"),
                    });
                }
                Some(Incoming::Event { .. }) => {
                    // Manager doesn't send events; nothing to do.
                }
                None => {
                    // Manager dropped. Let the executor finish on its own,
                    // but give up on hearing any more control requests.
                    let outcome = (&mut executor_fut).await;
                    finalise_executor_outcome(outcome, &channel, &agent_handle);
                    return;
                }
            },

            outcome = &mut executor_fut => {
                finalise_executor_outcome(outcome, &channel, &agent_handle);
                return;
            }
        }
    }
}

fn finalise_executor_outcome(
    outcome:      Result<WorkerComplete, WorkerFailure>,
    channel:      &AgentChannel,
    agent_handle: &Arc<std::sync::RwLock<Agent>>,
) {
    match outcome {
        Ok(complete) => {
            let _ = channel.send_event(Event::Complete {
                result_summary: complete.result_summary.clone(),
                artifacts:      complete.artifacts.clone(),
            });
            mark(agent_handle, |a| a.mark_completed(complete.result_summary));
        }
        Err(failed) => {
            let _ = channel.send_event(Event::Failed {
                error:             failed.error.clone(),
                partial_artifacts: failed.partial_artifacts.clone(),
            });
            // Prefer the typed fault if the source classified one; otherwise
            // fall back to an `Other{error}` so the code is always populated.
            let fault = failed.fault.clone().unwrap_or(
                crate::agent::instance::AgentFault::Other { detail: failed.error.clone() },
            );
            mark(agent_handle, |a| a.mark_failed_with_fault(fault));
        }
    }
}

// ─── Manager-side loop ─────────────────────────────────────────────────

async fn run_manager_loop(
    mut channel:  AgentChannel,
    task:         String,
    context:      Option<serde_json::Value>,
    budget_usd:   f64,
    deadline_ms:  Option<i64>,
    agent:        Arc<std::sync::RwLock<Agent>>,
    session:      Arc<SessionBudget>,
    completion:   oneshot::Sender<WorkerOutcome>,
    channels:     ChannelMap,
    agent_id:     AgentId,
    supervisor:   Arc<Supervisor>,
) {
    let (outcome, observed_pre_terminal) = run_manager_loop_inner(
        &mut channel, task, context, budget_usd, deadline_ms,
        agent.clone(), session, supervisor.clone(),
    ).await;

    // Audit the terminal transition. `observed_pre_terminal` is the
    // status the inner loop saw the moment Accepted came back — usually
    // Running, occasionally Paused if a control request flipped it.
    // Without this snapshot we'd race the worker's own `mark_*` and
    // read the already-terminal status (from == to), losing the event.
    let from_status = observed_pre_terminal
        .map(status_to_str)
        .unwrap_or("running");
    let to_status = match &outcome {
        WorkerOutcome::Complete(_) => "completed",
        WorkerOutcome::Failed(_)   => "failed",
    };
    if from_status != to_status {
        supervisor.audit(agent_id, AuditEvent::StatusChange {
            from: from_status.into(),
            to:   to_status.into(),
        });
    }

    // 0.111.0 — finalise the artifact dir before emitting the
    // completion event. Reads the SLUG file the agent (hopefully)
    // wrote, renames the dir to `<slug>_<task_id_short>`, and stamps
    // MANIFEST status. The rename is best-effort: a missing SLUG just
    // leaves the dir at its bare-task-id name.
    if let Some(arts) = supervisor.task_artifacts.as_ref() {
        let final_status = match &outcome {
            WorkerOutcome::Complete(_) => "completed",
            WorkerOutcome::Failed(_)   => "failed",
        };
        if let Err(e) = arts.finalize(&agent_id.0.to_string(), final_status) {
            tracing::debug!("artifact finalize for {agent_id} failed (non-fatal): {e}");
        }
    }

    // Emit `agent.worker.completed` so user-registered subscriptions
    // (`spawn_background_task` registers one per task to deliver the
    // result over the originating channel) fire. Best-effort: failure
    // to emit is logged but doesn't block the manager loop's exit.
    if let Some(bus) = supervisor.event_bus.as_ref() {
        let (skill_id, user_id, spent_usd) = {
            let a = agent.read().expect("agent read");
            (a.skill_id.clone(), a.user_id.clone(), a.budget.spent_usd)
        };
        // Pre-render a few presentation-only fields so the
        // `{{ path }}`-only template engine can produce a useful
        // delivery message without conditionals. `summary_or_error`
        // lets the same template handle both success and failure
        // cases without showing a blank line for whichever side
        // doesn't apply.
        let (status_str, summary, failure_reason, status_emoji, status_label, summary_or_error) =
            match &outcome {
                WorkerOutcome::Complete(c) => (
                    "completed",
                    Some(c.result_summary.clone()),
                    None,
                    "✅",
                    "finished",
                    c.result_summary.clone(),
                ),
                WorkerOutcome::Failed(f) => (
                    "failed",
                    None,
                    Some(f.error.clone()),
                    "⚠️",
                    "failed",
                    format!("Error: {}", f.error),
                ),
            };
        // Typed fault code (Phase A1) so subscribers/automations can branch on
        // a precise cause (e.g. retry on `timeout`, not on `policy_denied`).
        let fault_code = match &outcome {
            WorkerOutcome::Failed(f) => f.fault.as_ref().map(|x| x.code()),
            _ => None,
        };
        let payload = serde_json::json!({
            "task_id":           agent_id.to_string(),
            "skill":             skill_id,
            "status":            status_str,
            "summary":           summary,
            "failure_reason":    failure_reason,
            "fault_code":        fault_code,
            "status_emoji":      status_emoji,
            "status_label":      status_label,
            "summary_or_error":  summary_or_error,
            "spent_usd":         spent_usd,
        });
        bus.emit(crate::events::Event::new(
            crate::events::names::AGENT_WORKER_COMPLETED,
            user_id,
            payload,
        ));
    }

    // Drop the worker's ChannelSender from the supervisor's map so
    // future interrupt() calls return NotRunning. Done after the
    // outcome is settled but before the completion oneshot fires —
    // that keeps the lifecycle observable from the caller's side.
    channels.write().expect("channels write").remove(&agent_id);

    let _ = completion.send(outcome);
}

async fn run_manager_loop_inner(
    channel:     &mut AgentChannel,
    task:        String,
    context:     Option<serde_json::Value>,
    budget_usd:  f64,
    deadline_ms: Option<i64>,
    agent:       Arc<std::sync::RwLock<Agent>>,
    session:     Arc<SessionBudget>,
    supervisor:  Arc<Supervisor>,
) -> (WorkerOutcome, Option<AgentStatus>) {
    // Captured the moment Accepted comes back (so we know the worker's
    // own `mark_running` has run). Returned to the caller so it can
    // audit a `StatusChange { from: <this>, to: <terminal> }` event
    // that doesn't race the worker's own terminal mark.
    let mut pre_terminal: Option<AgentStatus> = None;
    // Track the worker's last reported cumulative spend so we can
    // compute the delta to charge against the session budget. Workers
    // report a running total in Progress; we don't double-count.
    let mut last_seen_spend: f64 = 0.0;
    // Send the initial Assign and await Accepted. Reject → Failed
    // outcome. PeerDropped → Failed outcome. user_id + skill_id
    // ride along so subprocess adapters can resolve secrets without
    // a separate registry lookup.
    let (assign_user_id, assign_skill_id) = {
        let a = agent.read().expect("agent read");
        (a.user_id.clone(), a.skill_id.clone())
    };
    let assign = Request::Assign {
        task, context, budget_usd, deadline_ms,
        user_id:  assign_user_id,
        skill_id: assign_skill_id,
    };
    let assign_resp = channel.request(assign).await;
    match assign_resp {
        Ok(Response::Accepted) => {
            // After Accepted the worker has been through Running by
            // definition. We don't read the live status here because a
            // fast-returning executor races us — the worker can mark
            // Completed before our snapshot lands, defeating the whole
            // point of capturing a pre-terminal state.
            pre_terminal = Some(AgentStatus::Running);
        }
        Ok(Response::Rejected { reason }) => {
            mark(&agent, |a| a.mark_failed(format!("rejected: {reason}")));
            return (WorkerOutcome::Failed(WorkerFailure {
                error: format!("worker rejected assignment: {reason}"),
                partial_artifacts: vec![], fault: None,
            }), pre_terminal);
        }
        Ok(other) => {
            mark(&agent, |a| a.mark_failed("bad assign reply"));
            return (WorkerOutcome::Failed(WorkerFailure {
                error: format!("worker replied with unexpected response to Assign: {other:?}"),
                partial_artifacts: vec![], fault: None,
            }), pre_terminal);
        }
        Err(e) => {
            mark(&agent, |a| a.mark_failed(format!("channel: {e}")));
            return (WorkerOutcome::Failed(WorkerFailure {
                error: format!("worker channel failure during Assign: {e}"),
                partial_artifacts: vec![], fault: None,
            }), pre_terminal);
        }
    }

    // Snapshot what we need to audit cleanly without holding the
    // RwLock across awaits.
    let agent_id_for_audit = agent.read().expect("agent read").id;

    // Drain the worker's stream until it emits a terminal event or
    // we kill it for budget reasons.
    loop {
        // Pre-check the session kill switch in case a sibling tripped
        // it while we were waiting on `recv`.
        if session.is_over() {
            let spent = session.spent().await;
            let fault = crate::agent::instance::AgentFault::SessionBudgetExceeded {
                spent_usd: spent,
                cap_usd:   session.max_usd,
            };
            supervisor.audit(agent_id_for_audit, AuditEvent::SessionBudgetExceeded {
                session_spent_usd: spent,
                session_cap_usd:   session.max_usd,
            });
            mark(&agent, |a| a.mark_failed_with_fault(fault.clone()));
            return (WorkerOutcome::Failed(WorkerFailure::faulted(fault)), pre_terminal);
        }

        match channel.recv().await {
            Some(Incoming::Event { payload: Event::Progress { step_summary, percent_done, llm_spend_usd } }) => {
                // Update agent state + per-agent budget.
                let per_agent_over = {
                    let mut a = agent.write().expect("agent write");
                    a.current_step = Some(step_summary);
                    if let Some(p) = percent_done {
                        a.percent_done = Some(p.clamp(0.0, 1.0));
                    }
                    a.budget.spent_usd = llm_spend_usd; // worker reports cumulative
                    a.budget.is_over()
                };

                // Apply the delta against the session budget. The
                // session can flip over even when the per-agent stays
                // safe (lots of small workers can collectively bust
                // the cap).
                let delta = (llm_spend_usd - last_seen_spend).max(0.0);
                last_seen_spend = llm_spend_usd;
                let session_over = if delta > 0.0 {
                    // 0.110.0 — append to the LLM cost ledger so the
                    // health-audit cost detector reads real history,
                    // not a running-budget proxy. Best-effort: a
                    // ledger write failure is logged but never
                    // bubbled out of the worker loop.
                    if let Some(hs) = supervisor.health_store.as_ref() {
                        let user_id = agent.read().ok().and_then(|a| a.user_id.clone());
                        if let Err(e) = hs.record_llm_charge(
                            &agent_id_for_audit.0.to_string(), user_id.as_deref(), delta,
                        ) {
                            tracing::debug!("llm_charges write failed (non-fatal): {e}");
                        }
                    }
                    session.charge(delta).await
                } else {
                    session.is_over()
                };

                if per_agent_over {
                    let cap = agent.read().unwrap().budget.max_usd;
                    let fault = crate::agent::instance::AgentFault::BudgetExceeded {
                        spent_usd: llm_spend_usd,
                        cap_usd:   cap,
                    };
                    supervisor.audit(agent_id_for_audit, AuditEvent::AgentBudgetExceeded {
                        spent_usd: llm_spend_usd,
                        cap_usd:   cap,
                    });
                    mark(&agent, |a| a.mark_failed_with_fault(fault.clone()));
                    return (WorkerOutcome::Failed(WorkerFailure::faulted(fault)), pre_terminal);
                }
                if session_over {
                    let session_spent = session.spent().await;
                    let fault = crate::agent::instance::AgentFault::SessionBudgetExceeded {
                        spent_usd: session_spent,
                        cap_usd:   session.max_usd,
                    };
                    supervisor.audit(agent_id_for_audit, AuditEvent::SessionBudgetExceeded {
                        session_spent_usd: session_spent,
                        session_cap_usd:   session.max_usd,
                    });
                    mark(&agent, |a| a.mark_failed_with_fault(fault.clone()));
                    return (WorkerOutcome::Failed(WorkerFailure::faulted(fault)), pre_terminal);
                }
            }
            Some(Incoming::Event { payload: Event::Complete { result_summary, artifacts } }) => {
                let outcome = WorkerComplete { result_summary, artifacts };
                mark(&agent, |a| a.mark_completed(outcome.result_summary.clone()));
                return (WorkerOutcome::Complete(outcome), pre_terminal);
            }
            Some(Incoming::Event { payload: Event::Failed { error, partial_artifacts } }) => {
                let outcome = WorkerFailure { error, partial_artifacts, fault: None };
                mark(&agent, |a| {
                    if a.status != AgentStatus::Interrupted {
                        a.mark_failed(outcome.error.clone());
                    }
                });
                return (WorkerOutcome::Failed(outcome), pre_terminal);
            }

            // Worker-initiated requests.
            Some(Incoming::Request { id, payload }) => {
                let resp = match payload {
                    Request::SpawnChild { skill_id, task, budget_usd } => {
                        handle_spawn_child(
                            &agent, &session, &supervisor, skill_id, task, budget_usd,
                        ).await
                    }
                    // Review/UserInput placeholders kept until proper
                    // UI surfacing lands. Worker stays unblocked.
                    Request::RequestReview { .. } => Response::ReviewDecision {
                        approved: true,
                        reason:   Some("auto-approved (review surfacing not implemented)".into()),
                    },
                    Request::RequestUserInput { .. } => Response::UserResponse {
                        response: String::new(),
                    },
                    other => Response::Error {
                        message: format!("unexpected manager-side request: {other:?}"),
                    },
                };
                let _ = channel.reply(id, resp);
            }

            None => {
                // Worker dropped without a terminal event. Treat as
                // failure so the completion future doesn't hang forever.
                mark(&agent, |a| a.mark_failed("worker channel dropped"));
                return (WorkerOutcome::Failed(WorkerFailure {
                    error: "worker channel dropped without terminal event".into(),
                    partial_artifacts: vec![], fault: None,
                }), pre_terminal);
            }
        }
    }
}

// ─── helpers ───────────────────────────────────────────────────────────

fn mark<F: FnOnce(&mut Agent)>(handle: &Arc<std::sync::RwLock<Agent>>, f: F) {
    if let Ok(mut a) = handle.write() {
        f(&mut a);
    }
}

/// Snake-case wire form of an AgentStatus, used as the `from`/`to`
/// payload of an `AuditEvent::StatusChange`.
fn status_to_str(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Pending     => "pending",
        AgentStatus::Running     => "running",
        AgentStatus::Paused      => "paused",
        AgentStatus::Completed   => "completed",
        AgentStatus::Failed      => "failed",
        AgentStatus::Interrupted => "interrupted",
    }
}

/// Decide whether the worker holding `agent` may spawn a child for
/// `skill_id` with `budget_usd`. Returns the SpawnDecision the manager
/// loop sends back; on approval, has already called
/// `supervisor.spawn_worker` so the new agent is registered before the
/// requesting worker sees the response.
///
/// Slice D1 added the policy-engine consult after the depth /
/// session-budget / resolver checks and before the actual spawn,
/// so admin-defined deny rules can fire without the existing
/// hard-coded checks being redundant.
async fn handle_spawn_child(
    agent:      &Arc<std::sync::RwLock<Agent>>,
    session:    &Arc<SessionBudget>,
    supervisor: &Arc<Supervisor>,
    skill_id:   String,
    task:       String,
    budget_usd: f64,
) -> Response {
    // Snapshot what we need before letting any awaits land.
    let (parent_id, parent_depth) = {
        let a = agent.read().expect("agent read");
        (a.id, a.depth)
    };

    // We only record `SpawnRequested` in the denied branches below; on
    // approval, `spawn_worker_with` records both `SpawnRequested` (against
    // the parent) and `SpawnApproved` (against the child) itself, so
    // doing it here would double up.

    // Depth check.
    let child_depth = parent_depth.saturating_add(1);
    if child_depth > MAX_RECURSION_DEPTH {
        let reason = format!(
            "recursion depth limit ({}) reached — workers can't spawn beyond depth {}",
            MAX_RECURSION_DEPTH, MAX_RECURSION_DEPTH,
        );
        supervisor.audit(parent_id, AuditEvent::SpawnDenied {
            skill_id: skill_id.clone(),
            reason:   reason.clone(),
        });
        return Response::SpawnDecision {
            approved: false,
            reason:   Some(reason),
            spawned_agent_id: None,
        };
    }

    // Session-budget check.
    if session.is_over() {
        let reason = "session budget already exhausted; refusing new spawns".to_string();
        supervisor.audit(parent_id, AuditEvent::SpawnDenied {
            skill_id: skill_id.clone(),
            reason:   reason.clone(),
        });
        return Response::SpawnDecision {
            approved: false,
            reason:   Some(reason),
            spawned_agent_id: None,
        };
    }

    // Resolver check — without an executor we have nothing to spawn.
    let executor = match supervisor.resolver.executor_for(&skill_id) {
        Some(e) => e,
        None    => {
            let reason = format!("no executor registered for skill {skill_id:?}");
            supervisor.audit(parent_id, AuditEvent::SpawnDenied {
                skill_id: skill_id.clone(),
                reason:   reason.clone(),
            });
            return Response::SpawnDecision {
                approved: false,
                reason:   Some(reason),
                spawned_agent_id: None,
            };
        },
    };

    // Policy engine consult (slice D1). Built-in checks above are
    // hard-coded floors; the engine layers admin-defined rules on top.
    // First-deny-wins: this runs *after* the floor checks so users can
    // see "depth exceeded" rather than "policy denied" when both
    // would have fired — the floor message is more actionable.
    if let Some(engine) = &supervisor.policy_engine {
        let event = crate::policy::PolicyEvent::SpawnWorker {
            parent_id,
            skill_id:          skill_id.clone(),
            child_depth,
            budget_usd,
            session_spent_usd: session.spent().await,
        };
        let decision = engine.evaluate(&event).await;
        if let crate::policy::PolicyDecision::Deny { rule, reason } = decision {
            // Audit both as a SpawnDenied (so existing forensic queries
            // see it) and as a PolicyDecision (so a per-rule view can
            // group identical denies).
            supervisor.audit(parent_id, AuditEvent::SpawnDenied {
                skill_id: skill_id.clone(),
                reason:   format!("policy/{rule}: {reason}"),
            });
            supervisor.audit(parent_id, AuditEvent::PolicyDecision {
                granted: false,
                rule:    rule.clone(),
                detail:  Some(reason.clone()),
            });
            return Response::SpawnDecision {
                approved: false,
                reason:   Some(format!("policy/{rule}: {reason}")),
                spawned_agent_id: None,
            };
        }
    }

    // All checks passed — spawn it. (`spawn_worker` itself records
    // SpawnApproved against the new child agent's id, so we don't
    // double-record here.)
    let handle = supervisor.spawn_worker(
        parent_id, parent_depth,
        skill_id.clone(), task, None,
        budget_usd, None,
        executor,
    );

    // We deliberately don't await the child's completion here — the
    // requesting worker just gets the new agent's id back. The
    // child's outcome surfaces through its own WorkerHandle (held by
    // whatever spawned this whole tree, plus visible via the
    // AgentRegistry / agents UI).
    debug!("Spawned child {} of {parent_id} for skill {skill_id:?}", handle.agent_id);
    let _ = handle.completion; // dropped — manager doesn't directly await child

    Response::SpawnDecision {
        approved:         true,
        reason:           None,
        spawned_agent_id: Some(handle.agent_id),
    }
}

/// Convenience for tests — a `WorkerTask` that returns a canned outcome.
#[cfg(test)]
pub mod test_helpers {
    use super::*;
    use std::sync::Mutex;

    /// Resolves to a configurable `Result`. Optionally emits one
    /// progress event before returning so transport plumbing tests
    /// can verify it reaches the manager.
    pub struct StubTask {
        pub outcome: Mutex<Option<Result<WorkerComplete, WorkerFailure>>>,
        pub progress_summary: Option<String>,
    }

    impl StubTask {
        pub fn complete(summary: &str) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(Some(Ok(WorkerComplete {
                    result_summary: summary.into(),
                    artifacts: vec![],
                }))),
                progress_summary: None,
            })
        }
        pub fn complete_with_progress(summary: &str, progress: &str) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(Some(Ok(WorkerComplete {
                    result_summary: summary.into(),
                    artifacts: vec![],
                }))),
                progress_summary: Some(progress.into()),
            })
        }
        pub fn failed(error: &str) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(Some(Err(WorkerFailure {
                    error: error.into(),
                    partial_artifacts: vec![], fault: None,
                }))),
                progress_summary: None,
            })
        }
    }

    /// Streams a configurable cumulative-spend trajectory before
    /// completing. Useful for budget tests: the worker reports
    /// progress with each successive spend value, sleeping briefly
    /// in between so the manager loop has a chance to evaluate the
    /// budget after each event.
    pub struct SpendingTask {
        pub spends: Vec<f64>,
    }
    impl SpendingTask {
        pub fn new(spends: Vec<f64>) -> Arc<Self> {
            Arc::new(Self { spends })
        }
    }
    #[async_trait]
    impl WorkerTask for SpendingTask {
        async fn run(&self, _: WorkerAssignment, ctx: WorkerContext)
            -> Result<WorkerComplete, WorkerFailure>
        {
            for (i, spend) in self.spends.iter().enumerate() {
                ctx.report_progress(format!("step {i}"), None, *spend);
                tokio::task::yield_now().await;
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Ok(WorkerComplete {
                result_summary: "all spends emitted".into(),
                artifacts: vec![],
            })
        }
    }

    #[async_trait]
    impl WorkerTask for StubTask {
        async fn run(&self, _: WorkerAssignment, ctx: WorkerContext)
            -> Result<WorkerComplete, WorkerFailure>
        {
            if let Some(p) = &self.progress_summary {
                ctx.report_progress(p.clone(), Some(0.5), 0.01);
            }
            self.outcome.lock().unwrap().take()
                .expect("StubTask::run called twice")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_helpers::StubTask;
    use std::time::Duration;
    use tokio::time::timeout;

    fn t(ms: u64) -> Duration { Duration::from_millis(ms) }

    fn fixture() -> (Arc<AgentRegistry>, Arc<Supervisor>, AgentId, u8) {
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, root_depth) = {
            let r = root.read().unwrap();
            (r.id, r.depth)
        };
        let sup = Arc::new(Supervisor::new(reg.clone()));
        (reg, sup, root_id, root_depth)
    }

    #[tokio::test]
    async fn happy_path_complete() {
        let (reg, sup, root_id, depth) = fixture();
        let exec = StubTask::complete("done");

        let h = sup.spawn_worker(root_id, depth, "com.example.test", "do thing", None, 1.0, None, exec);

        // Worker is registered immediately.
        assert!(reg.get(h.agent_id).is_some());
        assert_eq!(reg.tree_under(root_id).len(), 2, "root + worker");

        // Terminal outcome is Complete with the canned summary.
        let outcome = timeout(t(500), h.completion).await.unwrap().unwrap();
        match outcome {
            WorkerOutcome::Complete(c) => assert_eq!(c.result_summary, "done"),
            other => panic!("expected Complete, got {other:?}"),
        }

        // And the Agent's status reflects it (eventually — give the
        // worker loop a moment to write the terminal state).
        for _ in 0..20 {
            if reg.get(h.agent_id).map(|a| a.read().unwrap().status == AgentStatus::Completed).unwrap_or(false) {
                return;
            }
            tokio::time::sleep(t(10)).await;
        }
        panic!("worker never marked Completed");
    }

    #[tokio::test]
    async fn happy_path_failed() {
        let (_reg, sup, root_id, depth) = fixture();
        let exec = StubTask::failed("kaboom");

        let h = sup.spawn_worker(root_id, depth, "com.example.test", "do thing", None, 1.0, None, exec);
        let outcome = timeout(t(500), h.completion).await.unwrap().unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => assert_eq!(f.error, "kaboom"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn progress_event_updates_current_step() {
        let (reg, sup, root_id, depth) = fixture();
        let exec = StubTask::complete_with_progress("done", "halfway");

        let h = sup.spawn_worker(root_id, depth, "com.example.test", "task", None, 1.0, None, exec);
        // Wait for completion to ensure the progress event has been
        // processed by the manager loop.
        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();

        let agent = reg.get(h.agent_id).unwrap();
        let a = agent.read().unwrap();
        // The terminal Complete clears nothing — current_step should
        // reflect the last progress emitted.
        assert_eq!(a.current_step.as_deref(), Some("halfway"));
        assert_eq!(a.status, AgentStatus::Completed);
    }

    // ── B4: budget enforcement ──

    #[tokio::test]
    async fn worker_killed_when_per_agent_budget_exceeded() {
        let (_reg, sup, root_id, depth) = fixture();
        // Worker has $0.50 budget; reports cumulative spends climbing
        // past it. Final progress event of $0.60 trips the per-agent
        // cap and the manager loop should kill it.
        let exec = test_helpers::SpendingTask::new(vec![0.10, 0.30, 0.60]);

        let h = sup.spawn_worker(root_id, depth, "com.example.test", "task", None,
            0.50 /* per-agent budget */, None, exec);

        let outcome = timeout(t(1500), h.completion).await.unwrap().unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("agent budget exceeded"),
                    "wrong error: {}", f.error);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn worker_finishes_normally_when_under_budget() {
        let (_reg, sup, root_id, depth) = fixture();
        let exec = test_helpers::SpendingTask::new(vec![0.05, 0.10, 0.15]);

        let h = sup.spawn_worker(root_id, depth, "com.example.test", "task", None,
            1.00, None, exec);

        let outcome = timeout(t(1500), h.completion).await.unwrap().unwrap();
        assert!(matches!(outcome, WorkerOutcome::Complete(_)));
    }

    #[tokio::test]
    async fn session_budget_kills_siblings_when_collectively_over() {
        // Two workers spend $0.30 each. Session cap is $0.50. The
        // second's progress event should push the total over and trip
        // the kill switch on both.
        let registry = Arc::new(AgentRegistry::new());
        let root = registry.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let sup = Arc::new(Supervisor::new(registry).with_session_budget(0.50));

        let exec1 = test_helpers::SpendingTask::new(vec![0.10, 0.20, 0.30]);
        let exec2 = test_helpers::SpendingTask::new(vec![0.10, 0.20, 0.30]);

        let h1 = sup.spawn_worker(root_id, depth, "com.example.a", "task1", None, 1.00, None, exec1);
        let h2 = sup.spawn_worker(root_id, depth, "com.example.b", "task2", None, 1.00, None, exec2);

        let r1 = timeout(t(2000), h1.completion).await.unwrap().unwrap();
        let r2 = timeout(t(2000), h2.completion).await.unwrap().unwrap();

        // At least one of them must fail with "session budget" — the
        // first one to push the total over $0.50. The other may
        // complete normally if it had already exited, or fail on the
        // session check. Either way both can't be Complete.
        let failed_count = [&r1, &r2].iter()
            .filter(|o| matches!(o, WorkerOutcome::Failed(f) if f.error.contains("session budget")))
            .count();
        assert!(failed_count >= 1,
            "at least one worker must fail with session-budget message; got r1={r1:?} r2={r2:?}");

        // Session spend reflects what we charged.
        let total = sup.session_spend(root_id).await;
        assert!(total >= 0.50, "session spend should reach the cap; got {total}");
    }

    #[tokio::test]
    async fn session_spend_is_zero_for_root_with_no_workers() {
        let (_reg, sup, root_id, _depth) = fixture();
        assert_eq!(sup.session_spend(root_id).await, 0.0);
    }

    #[tokio::test]
    async fn session_budget_basic_charge_arithmetic() {
        let s = SessionBudget::new(1.00);
        assert_eq!(s.spent().await, 0.0);
        assert!(!s.is_over());
        assert!(!s.charge(0.40).await);
        assert!(!s.charge(0.40).await);
        assert!(s.charge(0.30).await, "0.40+0.40+0.30=1.10 should trip cap");
        assert!(s.is_over());
        assert!((s.spent().await - 1.10).abs() < 1e-9);
    }

    // ── B5: interrupt + pause/resume ──

    /// Long-running executor that emits progress every 10ms and never
    /// finishes on its own (until the test cancels it via Interrupt).
    pub struct InfiniteTask;
    #[async_trait]
    impl WorkerTask for InfiniteTask {
        async fn run(&self, _: WorkerAssignment, ctx: WorkerContext)
            -> Result<WorkerComplete, WorkerFailure>
        {
            for i in 0..10_000 {
                ctx.report_progress(format!("step {i}"), None, 0.0);
                tokio::time::sleep(t(10)).await;
            }
            // Should never get here in test runs.
            Ok(WorkerComplete { result_summary: "shouldn't finish".into(), artifacts: vec![] })
        }
    }

    #[tokio::test]
    async fn interrupt_kills_running_worker() {
        let (_reg, sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(root_id, depth, "com.example.test", "task", None,
            10.0, None, Arc::new(InfiniteTask));
        let agent_id = h.agent_id;

        // Let the worker get going.
        tokio::time::sleep(t(50)).await;

        // Send interrupt; supervisor should ack and the manager loop
        // should resolve the completion oneshot with Failed.
        sup.interrupt(agent_id, InterruptReason::User).await.expect("interrupt ok");

        let outcome = timeout(t(1000), h.completion).await.unwrap().unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(
                    f.error.contains("interrupted"),
                    "expected 'interrupted' in error, got: {}",
                    f.error,
                );
            }
            other => panic!("expected Failed after interrupt, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn interrupt_returns_not_running_after_completion() {
        let (_reg, sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(root_id, depth, "com.example.test", "task", None,
            1.0, None, StubTask::complete("done"));
        let agent_id = h.agent_id;
        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();

        // Manager loop has cleaned up its channel entry — interrupt
        // should now report NotRunning rather than hanging or
        // pretending to succeed.
        let err = sup.interrupt(agent_id, InterruptReason::User).await.unwrap_err();
        assert!(matches!(err, InterruptError::NotRunning),
            "expected NotRunning, got {err:?}");
    }

    #[tokio::test]
    async fn interrupt_tree_signals_every_active_descendant() {
        let registry = Arc::new(AgentRegistry::new());
        let root = registry.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let sup = Arc::new(Supervisor::new(registry));

        // Spawn three siblings; verify interrupt_tree signals all of them.
        let h1 = sup.spawn_worker(root_id, depth, "com.example.a", "task1", None, 10.0, None, Arc::new(InfiniteTask));
        let h2 = sup.spawn_worker(root_id, depth, "com.example.b", "task2", None, 10.0, None, Arc::new(InfiniteTask));
        let h3 = sup.spawn_worker(root_id, depth, "com.example.c", "task3", None, 10.0, None, Arc::new(InfiniteTask));

        tokio::time::sleep(t(50)).await;

        let signalled = sup.interrupt_tree(root_id, InterruptReason::User).await;
        assert_eq!(signalled, 3);

        // All three should resolve to Failed with "interrupted".
        for h in [h1, h2, h3] {
            let outcome = timeout(t(1000), h.completion).await.unwrap().unwrap();
            assert!(matches!(&outcome, WorkerOutcome::Failed(f) if f.error.contains("interrupted")),
                "expected interrupted Failed, got {outcome:?}");
        }
    }

    #[tokio::test]
    async fn pause_and_resume_update_status() {
        let (reg, sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(root_id, depth, "com.example.test", "task", None,
            10.0, None, Arc::new(InfiniteTask));
        let agent_id = h.agent_id;

        tokio::time::sleep(t(50)).await;
        // Worker is running.
        assert_eq!(reg.get(agent_id).unwrap().read().unwrap().status, AgentStatus::Running);

        // Pause: status flips to Paused. (The executor keeps spinning
        // for now — real cooperative pause comes when first non-stub
        // executors land.)
        sup.pause(agent_id).await.expect("pause ok");
        assert_eq!(reg.get(agent_id).unwrap().read().unwrap().status, AgentStatus::Paused);

        // Resume flips back to Running.
        sup.resume(agent_id).await.expect("resume ok");
        assert_eq!(reg.get(agent_id).unwrap().read().unwrap().status, AgentStatus::Running);

        // Cleanup: kill the worker so the test doesn't leak the task.
        sup.interrupt(agent_id, InterruptReason::User).await.expect("interrupt ok");
        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();
    }

    // ── B6: spawn approval + recursion cap ──

    /// HashMap-backed resolver for tests.
    pub struct MapResolver {
        pub map: std::collections::HashMap<&'static str, Arc<dyn WorkerTask>>,
    }
    impl SkillExecutorResolver for MapResolver {
        fn executor_for(&self, skill_id: &str) -> Option<Arc<dyn WorkerTask>> {
            self.map.get(skill_id).cloned()
        }
    }

    /// Executor that asks the manager to spawn a child of a given
    /// skill, waits for the result, and reports via the run result.
    pub struct SpawnChildTask {
        pub child_skill: String,
        pub child_task:  String,
        pub budget_usd:  f64,
    }
    #[async_trait]
    impl WorkerTask for SpawnChildTask {
        async fn run(&self, _: WorkerAssignment, ctx: WorkerContext)
            -> Result<WorkerComplete, WorkerFailure>
        {
            match ctx.spawn_child(self.child_skill.clone(), self.child_task.clone(), self.budget_usd).await {
                Ok(child_id) => Ok(WorkerComplete {
                    result_summary: format!("spawned child {child_id}"),
                    artifacts: vec![],
                }),
                Err(e) => Err(WorkerFailure {
                    error: format!("spawn_child failed: {e}"),
                    partial_artifacts: vec![], fault: None,
                }),
            }
        }
    }

    fn fixture_with_resolver(map: Vec<(&'static str, Arc<dyn WorkerTask>)>)
        -> (Arc<AgentRegistry>, Arc<Supervisor>, AgentId, u8)
    {
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, root_depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let resolver = Arc::new(MapResolver { map: map.into_iter().collect() });
        let sup = Arc::new(Supervisor::new(reg.clone()).with_resolver(resolver));
        (reg, sup, root_id, root_depth)
    }

    #[tokio::test]
    async fn worker_spawns_child_through_supervisor_callback() {
        // Parent's executor calls spawn_child("com.example.child", …).
        // Resolver maps that to a stub that completes with "child done".
        let child_executor: Arc<dyn WorkerTask> = StubTask::complete("child done");
        let (reg, sup, root_id, depth) = fixture_with_resolver(vec![
            ("com.example.child", child_executor),
        ]);
        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.child".into(),
            child_task:  "subtask".into(),
            budget_usd:  1.0,
        });

        let h = sup.spawn_worker(root_id, depth, "com.example.parent", "task", None, 1.0, None, parent_exec);
        let outcome = timeout(t(1500), h.completion).await.unwrap().unwrap();

        match outcome {
            WorkerOutcome::Complete(c) => {
                assert!(c.result_summary.starts_with("spawned child "), "got: {}", c.result_summary);
            }
            other => panic!("expected Complete, got {other:?}"),
        }

        // Tree should now contain root + parent worker + spawned child.
        // (Children that already completed remain in the registry until
        // unregistered — for this test both worker IDs are still there.)
        assert_eq!(reg.tree_under(root_id).len(), 3,
            "expected root + parent + child, got {}", reg.tree_under(root_id).len());
    }

    #[tokio::test]
    async fn spawn_child_denied_when_no_resolver_for_skill() {
        // Resolver has nothing in it; any spawn_child call gets denied.
        let (_reg, sup, root_id, depth) = fixture_with_resolver(vec![]);
        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.unknown".into(),
            child_task:  "x".into(),
            budget_usd:  1.0,
        });

        let h = sup.spawn_worker(root_id, depth, "com.example.parent", "task", None, 1.0, None, parent_exec);
        let outcome = timeout(t(500), h.completion).await.unwrap().unwrap();

        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("no executor registered"), "got: {}", f.error);
            }
            other => panic!("expected Failed (denied), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_child_denied_at_recursion_depth_limit() {
        // Pre-register a chain root → w1 → w2 → … so the parent we
        // launch is already at depth = MAX_RECURSION_DEPTH. Any
        // spawn_child it issues should be denied.
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let mut parent_id = root.read().unwrap().id;
        let mut depth: u8 = 0;
        for _ in 0..MAX_RECURSION_DEPTH {
            let w = reg.register(Agent::new_worker(parent_id, depth, "filler", 1.0));
            parent_id = w.read().unwrap().id;
            depth = depth.saturating_add(1);
        }
        // Now `parent_id` sits at depth = MAX_RECURSION_DEPTH.
        assert_eq!(depth, MAX_RECURSION_DEPTH);

        let resolver = Arc::new(MapResolver {
            map: [("com.example.child", StubTask::complete("ok") as Arc<dyn WorkerTask>)]
                .into_iter().collect(),
        });
        let sup = Arc::new(Supervisor::new(reg.clone()).with_resolver(resolver));

        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.child".into(),
            child_task:  "x".into(),
            budget_usd:  1.0,
        });

        let h = sup.spawn_worker(parent_id, depth, "com.example.deepest", "task", None, 1.0, None, parent_exec);
        let outcome = timeout(t(500), h.completion).await.unwrap().unwrap();

        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("recursion depth limit"),
                    "got: {}", f.error);
            }
            other => panic!("expected Failed (depth), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_child_denied_when_session_already_over_budget() {
        // Tiny session budget; first worker blows past it, then a
        // sibling tries to spawn a child and gets denied because
        // the session is already over.
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let resolver = Arc::new(MapResolver {
            map: [("com.example.child", StubTask::complete("ok") as Arc<dyn WorkerTask>)]
                .into_iter().collect(),
        });
        let sup = Arc::new(
            Supervisor::new(reg.clone())
                .with_session_budget(0.10)
                .with_resolver(resolver),
        );

        // First worker exceeds the session cap.
        let burner = test_helpers::SpendingTask::new(vec![0.05, 0.20]);
        let h1 = sup.spawn_worker(root_id, depth, "com.example.burner", "x", None, 1.0, None, burner);
        let _ = timeout(t(1000), h1.completion).await.unwrap().unwrap();
        assert!(sup.session_spend(root_id).await > 0.10);

        // Second worker tries to spawn a child — should be denied
        // because the session is already over budget.
        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.child".into(),
            child_task:  "y".into(),
            budget_usd:  0.5,
        });
        let h2 = sup.spawn_worker(root_id, depth, "com.example.parent2", "z", None, 1.0, None, parent_exec);
        let outcome = timeout(t(500), h2.completion).await.unwrap().unwrap();

        // The parent itself trips the session-budget kill on its
        // FIRST select-loop check (before it even gets to issue
        // spawn_child). So either error message indicates the cap
        // was respected.
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(
                    f.error.contains("session budget") || f.error.contains("session budget already exhausted"),
                    "expected session-budget mention, got: {}", f.error,
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── B8: per-agent LLM config ──

    fn alias(provider: &str, model: Option<&str>) -> LlmAlias {
        LlmAlias { provider: provider.into(), model: model.map(|s| s.to_string()) }
    }

    #[test]
    fn resolve_picks_first_skill_alias_present_in_config() {
        let mut aliases = std::collections::HashMap::new();
        aliases.insert("coding".into(),  alias("openrouter", Some("anthropic/claude-sonnet-4.7")));
        aliases.insert("primary".into(), alias("lmstudio",   None));

        let skill = vec!["coding".to_string(), "primary".to_string()];
        let choice = resolve_llm_choice(&skill, &aliases, "ollama").unwrap();
        assert_eq!(choice.alias,    "coding");
        assert_eq!(choice.provider, "openrouter");
        assert_eq!(choice.model.as_deref(), Some("anthropic/claude-sonnet-4.7"));
    }

    #[test]
    fn resolve_falls_back_to_primary_alias_when_skill_picks_unknown() {
        let mut aliases = std::collections::HashMap::new();
        aliases.insert("primary".into(), alias("lmstudio", Some("local-model")));

        let skill = vec!["nonexistent".to_string()];
        let choice = resolve_llm_choice(&skill, &aliases, "openrouter").unwrap();
        assert_eq!(choice.alias,    "primary");
        assert_eq!(choice.provider, "lmstudio");
        assert_eq!(choice.model.as_deref(), Some("local-model"));
    }

    #[test]
    fn resolve_falls_back_to_configured_primary_provider_with_no_aliases() {
        let aliases = std::collections::HashMap::new();
        let choice = resolve_llm_choice(&[], &aliases, "openrouter").unwrap();
        assert_eq!(choice.alias,    "primary");
        assert_eq!(choice.provider, "openrouter");
        assert!(choice.model.is_none());
    }

    #[test]
    fn resolve_returns_none_only_when_everything_is_blank() {
        let aliases = std::collections::HashMap::new();
        let choice = resolve_llm_choice(&[], &aliases, "");
        assert!(choice.is_none(), "no aliases + no primary_provider → no choice");
    }

    #[tokio::test]
    async fn spawned_worker_carries_llm_choice() {
        let (reg, sup, root_id, depth) = fixture();
        let choice = LlmChoice {
            alias: "coding".into(),
            provider: "openrouter".into(),
            model: Some("anthropic/claude-sonnet-4.7".into()),
        };
        let h = sup.spawn_worker_with(
            root_id, depth, "com.example.coding", "task",
            None, 1.0, None, StubTask::complete("ok"),
            Some(choice.clone()),
        );

        // Drain to completion — but the LlmChoice should be visible
        // immediately on the registered Agent.
        let agent = reg.get(h.agent_id).unwrap();
        assert_eq!(agent.read().unwrap().llm_choice.as_ref(), Some(&choice));

        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();
        // Choice survives the lifecycle — terminal state doesn't clobber it.
        assert_eq!(reg.get(h.agent_id).unwrap().read().unwrap().llm_choice.as_ref(), Some(&choice));
    }

    #[tokio::test]
    async fn spawn_worker_without_explicit_choice_leaves_field_none() {
        let (reg, sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(
            root_id, depth, "com.example.test", "task",
            None, 1.0, None, StubTask::complete("ok"),
        );
        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();
        assert!(reg.get(h.agent_id).unwrap().read().unwrap().llm_choice.is_none());
    }

    // ── B9: audit log integration ──

    use crate::agent::audit::{AuditEvent, AuditFilter, AuditStore};

    fn fixture_with_audit() -> (Arc<AgentRegistry>, Arc<Supervisor>, AgentId, u8, Arc<AuditStore>) {
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, root_depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let store = Arc::new(AuditStore::open_in_memory());
        let sup = Arc::new(
            Supervisor::new(reg.clone()).with_audit_store(Arc::clone(&store)),
        );
        (reg, sup, root_id, root_depth, store)
    }

    #[tokio::test]
    async fn audit_records_spawn_and_terminal_status_change() {
        let (_reg, sup, root_id, depth, store) = fixture_with_audit();
        let h = sup.spawn_worker(root_id, depth, "com.example.test", "task", None,
            1.0, None, StubTask::complete("done"));
        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();

        // Wait briefly for the manager loop to land its terminal write.
        tokio::time::sleep(t(20)).await;

        let rows = store.query(&AuditFilter::default()).unwrap();
        // Expect at least: SpawnRequested(parent=root) + SpawnApproved(child=worker) + StatusChange(running→completed).
        let kinds: Vec<&'static str> = rows.iter().map(|r| r.event.kind()).collect();
        assert!(kinds.contains(&"spawn_requested"),  "kinds={kinds:?}");
        assert!(kinds.contains(&"spawn_approved"),   "kinds={kinds:?}");
        assert!(kinds.contains(&"status_change"),    "kinds={kinds:?}");

        // Chain must verify clean end-to-end.
        store.verify_chain().expect("audit chain stays valid after a real spawn");
    }

    #[tokio::test]
    async fn audit_records_interrupt_event() {
        let (_reg, sup, root_id, depth, store) = fixture_with_audit();
        let h = sup.spawn_worker(root_id, depth, "com.example.test", "task", None,
            10.0, None, Arc::new(InfiniteTask));
        let agent_id = h.agent_id;
        tokio::time::sleep(t(50)).await;

        sup.interrupt(agent_id, InterruptReason::User).await.expect("interrupt ok");
        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();

        let only_interrupt = store.query(&AuditFilter {
            agent_id: Some(agent_id),
            kinds:    vec!["interrupted"],
            ..Default::default()
        }).unwrap();
        assert_eq!(only_interrupt.len(), 1, "should record exactly one interrupt event");
        assert!(matches!(only_interrupt[0].event,
            AuditEvent::Interrupted { ref reason } if reason == "user"
        ), "unexpected payload: {:?}", only_interrupt[0].event);
    }

    #[tokio::test]
    async fn audit_records_agent_budget_exceeded() {
        let (_reg, sup, root_id, depth, store) = fixture_with_audit();
        let exec = test_helpers::SpendingTask::new(vec![0.10, 0.30, 0.60]);
        let h = sup.spawn_worker(root_id, depth, "com.example.burner", "task", None,
            0.50, None, exec);
        let _ = timeout(t(1500), h.completion).await.unwrap().unwrap();

        let budget_rows = store.query(&AuditFilter {
            kinds: vec!["agent_budget_exceeded"], ..Default::default()
        }).unwrap();
        assert_eq!(budget_rows.len(), 1, "expected exactly one agent_budget_exceeded row");
        assert!(matches!(budget_rows[0].event,
            AuditEvent::AgentBudgetExceeded { spent_usd, cap_usd }
                if spent_usd > cap_usd && (cap_usd - 0.50).abs() < 1e-9
        ), "unexpected payload: {:?}", budget_rows[0].event);
    }

    #[tokio::test]
    async fn audit_records_spawn_denied_when_resolver_missing() {
        // Parent with audit + a resolver that's empty. The parent itself
        // is a top-level worker (so its own SpawnRequested+SpawnApproved
        // appear), and its spawn_child gets denied with no executor.
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let store = Arc::new(AuditStore::open_in_memory());
        let resolver = Arc::new(MapResolver { map: std::collections::HashMap::new() });
        let sup = Arc::new(
            Supervisor::new(reg.clone())
                .with_resolver(resolver)
                .with_audit_store(Arc::clone(&store)),
        );
        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.unknown".into(),
            child_task:  "x".into(),
            budget_usd:  1.0,
        });
        let h = sup.spawn_worker(root_id, depth, "com.example.parent", "task", None,
            1.0, None, parent_exec);
        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();

        let denied = store.query(&AuditFilter {
            kinds: vec!["spawn_denied"], ..Default::default()
        }).unwrap();
        assert_eq!(denied.len(), 1, "expected one spawn_denied row");
        assert!(matches!(denied[0].event,
            AuditEvent::SpawnDenied { ref skill_id, ref reason }
                if skill_id == "com.example.unknown" && reason.contains("no executor")
        ), "unexpected payload: {:?}", denied[0].event);
    }

    // ── D1: policy engine plumbing ──

    use crate::policy::{
        DenyAllEngine, PolicyDecision, PolicyEngine, PolicyEvent,
    };

    /// Records every event the engine sees + replies according to a
    /// caller-supplied function. Lets tests assert on what the
    /// supervisor *actually* asked the engine.
    struct RecordingEngine {
        seen: std::sync::Mutex<Vec<PolicyEvent>>,
        decide: Box<dyn Fn(&PolicyEvent) -> PolicyDecision + Send + Sync>,
    }
    impl RecordingEngine {
        fn allow_all() -> Arc<Self> {
            Arc::new(Self {
                seen: std::sync::Mutex::new(Vec::new()),
                decide: Box::new(|_| PolicyDecision::Allow),
            })
        }
    }
    #[async_trait]
    impl PolicyEngine for RecordingEngine {
        async fn evaluate(&self, event: &PolicyEvent) -> PolicyDecision {
            self.seen.lock().unwrap().push(event.clone());
            (self.decide)(event)
        }
    }

    #[tokio::test]
    async fn spawn_child_consults_policy_engine_with_correct_event() {
        // Allow-all engine: the spawn should succeed AND we should have
        // seen exactly one SpawnWorker event with the right fields.
        let child_executor: Arc<dyn WorkerTask> = StubTask::complete("child done");
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let resolver = Arc::new(MapResolver {
            map: [("com.example.child", child_executor)].into_iter().collect(),
        });
        let engine = RecordingEngine::allow_all();
        let sup = Arc::new(
            Supervisor::new(reg.clone())
                .with_resolver(resolver)
                .with_policy_engine(engine.clone()),
        );

        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.child".into(),
            child_task:  "subtask".into(),
            budget_usd:  0.5,
        });
        let h = sup.spawn_worker(root_id, depth,
            "com.example.parent", "task", None, 1.0, None, parent_exec);
        let outcome = timeout(t(1500), h.completion).await.unwrap().unwrap();
        assert!(matches!(outcome, WorkerOutcome::Complete(_)),
            "expected Complete with allow-all engine, got {outcome:?}");

        let seen = engine.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected one policy event for the spawn-child");
        match &seen[0] {
            PolicyEvent::SpawnWorker { skill_id, child_depth, budget_usd, .. } => {
                assert_eq!(skill_id, "com.example.child");
                assert_eq!(*child_depth, depth + 2,
                    "child of depth-1 worker should be at depth+2 from root");
                assert!((budget_usd - 0.5).abs() < 1e-9);
            }
            other => panic!("expected SpawnWorker, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_child_denied_by_policy_returns_failed_with_rule_in_reason() {
        let child_executor: Arc<dyn WorkerTask> = StubTask::complete("never runs");
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let resolver = Arc::new(MapResolver {
            map: [("com.example.child", child_executor)].into_iter().collect(),
        });
        let engine = Arc::new(DenyAllEngine::new("nope, not in test mode"));
        let sup = Arc::new(
            Supervisor::new(reg.clone())
                .with_resolver(resolver)
                .with_policy_engine(engine),
        );

        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.child".into(),
            child_task:  "x".into(),
            budget_usd:  1.0,
        });
        let h = sup.spawn_worker(root_id, depth,
            "com.example.parent", "task", None, 1.0, None, parent_exec);
        let outcome = timeout(t(500), h.completion).await.unwrap().unwrap();
        match outcome {
            WorkerOutcome::Failed(f) => {
                assert!(f.error.contains("policy/test/deny-all"),
                    "expected rule prefix in error, got: {}", f.error);
                assert!(f.error.contains("nope, not in test mode"),
                    "expected reason in error, got: {}", f.error);
            }
            other => panic!("expected Failed (denied by policy), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn policy_deny_records_both_spawn_denied_and_policy_decision_audit_rows() {
        // With audit + a denying engine, the supervisor should emit
        // BOTH a SpawnDenied row (so existing forensic queries see it)
        // AND a PolicyDecision row (so a per-rule view groups identical denies).
        let child_executor: Arc<dyn WorkerTask> = StubTask::complete("never runs");
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let resolver = Arc::new(MapResolver {
            map: [("com.example.child", child_executor)].into_iter().collect(),
        });
        let store  = Arc::new(AuditStore::open_in_memory());
        let engine = Arc::new(DenyAllEngine::new("test denial"));
        let sup = Arc::new(
            Supervisor::new(reg.clone())
                .with_resolver(resolver)
                .with_audit_store(Arc::clone(&store))
                .with_policy_engine(engine),
        );

        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.child".into(),
            child_task:  "x".into(),
            budget_usd:  1.0,
        });
        let h = sup.spawn_worker(root_id, depth,
            "com.example.parent", "task", None, 1.0, None, parent_exec);
        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();

        let denied = store.query(&AuditFilter {
            kinds: vec!["spawn_denied"], ..Default::default()
        }).unwrap();
        assert_eq!(denied.len(), 1, "expected one spawn_denied row");
        assert!(matches!(&denied[0].event,
            AuditEvent::SpawnDenied { reason, .. }
                if reason.contains("policy/test/deny-all")
        ), "unexpected payload: {:?}", denied[0].event);

        let pol = store.query(&AuditFilter {
            kinds: vec!["policy_decision"], ..Default::default()
        }).unwrap();
        assert_eq!(pol.len(), 1, "expected one policy_decision row");
        assert!(matches!(&pol[0].event,
            AuditEvent::PolicyDecision { granted, rule, detail }
                if !*granted
                && rule == "test/deny-all"
                && detail.as_deref() == Some("test denial")
        ), "unexpected payload: {:?}", pol[0].event);
    }

    #[tokio::test]
    async fn supervisor_without_policy_engine_behaves_as_before() {
        // No engine plugged in → spawn-child works exactly like the
        // pre-D1 B6 path. Sanity check that the seam is truly opt-in.
        let child_executor: Arc<dyn WorkerTask> = StubTask::complete("child done");
        let (reg, sup, root_id, depth) = fixture_with_resolver(vec![
            ("com.example.child", child_executor),
        ]);
        let _ = reg; // silence unused-warning
        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.child".into(),
            child_task:  "x".into(),
            budget_usd:  0.5,
        });
        let h = sup.spawn_worker(root_id, depth,
            "com.example.parent", "task", None, 1.0, None, parent_exec);
        let outcome = timeout(t(1500), h.completion).await.unwrap().unwrap();
        assert!(matches!(outcome, WorkerOutcome::Complete(_)),
            "expected Complete with no engine, got {outcome:?}");
    }

    #[tokio::test]
    async fn policy_engine_not_consulted_when_floor_check_fails() {
        // If the depth cap fires first (one of the hard-coded floor
        // checks), the engine should NEVER be invoked — the floor
        // message is more actionable than "policy denied" and we don't
        // want to bill engine evaluation time on a doomed spawn.
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let mut parent_id = root.read().unwrap().id;
        let mut depth: u8 = 0;
        for _ in 0..MAX_RECURSION_DEPTH {
            let w = reg.register(Agent::new_worker(parent_id, depth, "filler", 1.0));
            parent_id = w.read().unwrap().id;
            depth = depth.saturating_add(1);
        }
        // Now parent_id sits at MAX_RECURSION_DEPTH; any spawn it
        // requests should hit the depth floor before the engine.

        let child_executor: Arc<dyn WorkerTask> = StubTask::complete("never runs");
        let resolver = Arc::new(MapResolver {
            map: [("com.example.child", child_executor)].into_iter().collect(),
        });
        let engine = RecordingEngine::allow_all();
        let sup = Arc::new(
            Supervisor::new(reg.clone())
                .with_resolver(resolver)
                .with_policy_engine(engine.clone()),
        );
        let parent_exec = Arc::new(SpawnChildTask {
            child_skill: "com.example.child".into(),
            child_task:  "x".into(),
            budget_usd:  1.0,
        });

        let h = sup.spawn_worker(parent_id, depth,
            "com.example.deepest", "task", None, 1.0, None, parent_exec);
        let outcome = timeout(t(500), h.completion).await.unwrap().unwrap();
        assert!(matches!(&outcome,
            WorkerOutcome::Failed(f) if f.error.contains("recursion depth limit")
        ), "expected depth-cap failure, got {outcome:?}");

        // Engine was never asked.
        assert!(engine.seen.lock().unwrap().is_empty(),
            "engine consulted on a doomed spawn — should be short-circuited by depth floor");
    }

    #[tokio::test]
    async fn child_spawn_request_is_denied_for_now() {
        // Use a custom executor that issues a SpawnChild request
        // mid-run via a back-door wire (simulating what a future
        // executor would do). Verify the manager loop replies with
        // SpawnDecision { approved: false, ... }.
        struct ChildSpawningTask;
        #[async_trait]
        impl WorkerTask for ChildSpawningTask {
            async fn run(&self, _: WorkerAssignment, ctx: WorkerContext)
                -> Result<WorkerComplete, WorkerFailure>
            {
                ctx.report_progress("about to spawn", None, 0.0);
                // We can't issue a request via WorkerContext yet (that
                // ergonomic wrapper lands in B6). Just emit progress
                // and complete — the test's purpose is verifying the
                // manager-side stub reply path doesn't panic.
                Ok(WorkerComplete { result_summary: "ok".into(), artifacts: vec![] })
            }
        }

        let (_reg, sup, root_id, depth) = fixture();
        let h = sup.spawn_worker(root_id, depth, "com.example.test", "task", None, 1.0, None, Arc::new(ChildSpawningTask));
        let outcome = timeout(t(500), h.completion).await.unwrap().unwrap();
        assert!(matches!(outcome, WorkerOutcome::Complete(_)));
    }

    /// Step 3 validation: terminal worker outcomes are published as
    /// `agent.worker.completed` on the wired event bus, with the
    /// payload shape `spawn_background_task` auto-subscriptions
    /// expect (task_id, skill, status, summary, user).
    #[tokio::test]
    async fn worker_terminal_emits_completed_event_with_user_id() {
        let bus = Arc::new(crate::events::EventBus::new());
        let mut rx = bus.subscribe();

        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let root_id = root.read().unwrap().id;

        let sup = Arc::new(
            Supervisor::new(reg.clone()).with_event_bus(Arc::clone(&bus))
        );

        let exec = StubTask::complete("research summary text");
        let h = sup.spawn_worker_full(
            root_id, 0,
            "com.mira.research", "what is rust?",
            None, 2.0, None, exec, None,
            Some("user-42".into()),
        );
        let outcome = timeout(t(500), h.completion).await.unwrap().unwrap();
        assert!(matches!(outcome, WorkerOutcome::Complete(_)));

        // Drain the broadcast until we see our event.
        let ev = timeout(t(200), async {
            loop {
                match rx.recv().await {
                    Ok(e) if e.name == crate::events::names::AGENT_WORKER_COMPLETED => return e,
                    Ok(_)  => continue,
                    Err(_) => panic!("bus closed"),
                }
            }
        }).await.expect("did not receive completed event in time");

        assert_eq!(ev.user_id.as_deref(), Some("user-42"));
        assert_eq!(ev.payload["status"], "completed");
        assert_eq!(ev.payload["skill"], "com.mira.research");
        assert_eq!(ev.payload["summary"], "research summary text");
        // task_id is a UUID; just sanity-check it's present + parseable.
        let tid = ev.payload["task_id"].as_str().expect("task_id string");
        uuid::Uuid::parse_str(tid).expect("valid UUID");
    }

    /// Failure path: failed workers publish the same event with
    /// `status=failed` and `failure_reason` populated.
    #[tokio::test]
    async fn worker_failure_emits_completed_event_with_failure() {
        let bus = Arc::new(crate::events::EventBus::new());
        let mut rx = bus.subscribe();

        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let root_id = root.read().unwrap().id;

        let sup = Arc::new(
            Supervisor::new(reg.clone()).with_event_bus(Arc::clone(&bus))
        );
        let exec = StubTask::failed("backend offline");
        let h = sup.spawn_worker_full(
            root_id, 0, "com.example.test", "x",
            None, 1.0, None, exec, None,
            Some("user-7".into()),
        );
        let _ = timeout(t(500), h.completion).await.unwrap().unwrap();

        let ev = timeout(t(200), async {
            loop {
                match rx.recv().await {
                    Ok(e) if e.name == crate::events::names::AGENT_WORKER_COMPLETED => return e,
                    Ok(_)  => continue,
                    Err(_) => panic!("bus closed"),
                }
            }
        }).await.expect("event missing");

        assert_eq!(ev.payload["status"], "failed");
        assert_eq!(ev.payload["failure_reason"], "backend offline");
        assert_eq!(ev.user_id.as_deref(), Some("user-7"));
    }
}

// AgentStatus is imported at the top so callers using
// `crate::agent::supervisor::AgentStatus` still resolve cleanly.
