// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/orchestrator.rs

//! **Workflow orchestrator** (Phase C) — runs a [`WorkflowDefinition`] DAG.
//!
//! Given a workflow + an input string, the orchestrator computes the
//! execution waves (via [`workflow::execution_waves`]), then runs them in
//! order: every step in a wave is spawned as a supervised worker
//! concurrently, the orchestrator awaits the whole wave, feeds each step's
//! `result_summary` forward (interpolated into dependents' briefs), and moves
//! to the next wave. Run state is persisted to the [`WorkflowStore`] as it
//! progresses so the API/UI can observe in flight; on completion an
//! `agent.workflow.completed` event powers "ping me when done" delivery.
//!
//! C1 semantics are fail-fast: the first failing step fails the run. Branch /
//! continue-on-error / human-in-the-loop land in C2.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, warn};
use uuid::Uuid;

use crate::agent::instance::{Agent, AgentId, AgentRegistry};
use crate::agent::supervisor::{Supervisor, WorkerOutcome};
use crate::agent::workflow::{RunStatus, StepRun, WorkflowDefinition, WorkflowRun, WorkflowStore};

/// Drives workflow runs against the multi-agent supervisor.
pub struct Orchestrator {
    supervisor:     Arc<Supervisor>,
    agent_registry: Arc<AgentRegistry>,
    store:          Arc<WorkflowStore>,
    event_bus:      Option<Arc<crate::events::EventBus>>,
    default_budget: f64,
    max_budget:     f64,
}

impl Orchestrator {
    pub fn new(
        supervisor:     Arc<Supervisor>,
        agent_registry: Arc<AgentRegistry>,
        store:          Arc<WorkflowStore>,
        default_budget: f64,
        max_budget:     f64,
    ) -> Self {
        Self {
            supervisor, agent_registry, store,
            event_bus: None,
            default_budget, max_budget,
        }
    }

    pub fn with_event_bus(mut self, bus: Arc<crate::events::EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn store(&self) -> &Arc<WorkflowStore> { &self.store }

    fn now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    /// Kick off a run in the background and return its `run_id` immediately.
    /// The run row is persisted before this returns so a follow-up status
    /// query always finds it.
    pub fn start(
        self: &Arc<Self>,
        def:     WorkflowDefinition,
        input:   String,
        user_id: Option<String>,
    ) -> String {
        let run = self.new_run(&def, input, user_id);
        let run_id = run.id.clone();
        if let Err(e) = self.store.save_run(&run) {
            warn!("workflow run {} initial save failed: {e}", run_id);
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { me.execute(def, run).await; });
        run_id
    }

    fn new_run(&self, def: &WorkflowDefinition, input: String, user_id: Option<String>) -> WorkflowRun {
        let now = Self::now();
        let steps = def.steps.iter().map(|s| StepRun {
            step_id: s.id.clone(),
            target:  s.target_skill_id().unwrap_or_default(),
            status:  RunStatus::Pending,
            task_id: None,
            output:  None,
            error:   None,
        }).collect();
        WorkflowRun {
            id: Uuid::now_v7().to_string(),
            workflow_id: def.id.clone(),
            workflow_name: def.name.clone(),
            status: RunStatus::Pending,
            input,
            steps,
            error: None,
            user_id,
            approved_steps: vec![],
            created_at: now,
            updated_at: now,
        }
    }

    /// Run the whole DAG to a terminal state, persisting as it goes. Public so
    /// callers can `await` a synchronous run (tests, future inline use); the
    /// background path goes through [`start`].
    pub async fn execute(&self, def: WorkflowDefinition, mut run: WorkflowRun) -> WorkflowRun {
        // Index helpers.
        let step_index: HashMap<String, usize> =
            run.steps.iter().enumerate().map(|(i, s)| (s.step_id.clone(), i)).collect();
        let by_id: HashMap<String, &crate::agent::workflow::WorkflowStep> =
            def.steps.iter().map(|s| (s.id.clone(), s)).collect();

        let waves = match crate::agent::workflow::execution_waves(&def.steps) {
            Ok(w) => w,
            Err(e) => return self.fail(&mut run, format!("invalid workflow graph: {e}")).await,
        };

        run.status = RunStatus::Running;
        let _ = self.store.save_run(&run);
        info!("workflow '{}' run {} started ({} steps, {} waves)",
              def.name, run.id, def.steps.len(), waves.len());

        // Reconstruct execution state from the (possibly resumed) run record:
        // completed steps' outputs feed forward; skipped/failed steps make
        // their dependents skip. A fresh run has all steps Pending, so both
        // collections start empty — this is also the resume path after a
        // human-in-the-loop pause.
        let mut outputs: HashMap<String, String> = HashMap::new();
        let mut unavailable: std::collections::HashSet<String> = std::collections::HashSet::new();
        for s in &run.steps {
            match s.status {
                RunStatus::Completed => {
                    if let Some(o) = &s.output { outputs.insert(s.step_id.clone(), o.clone()); }
                }
                RunStatus::Skipped | RunStatus::Failed => { unavailable.insert(s.step_id.clone()); }
                _ => {}
            }
        }
        let approved: std::collections::HashSet<String> =
            run.approved_steps.iter().cloned().collect();
        // Set when a checkpoint blocks — the run parks until an approve/reject.
        let mut paused = false;

        for wave in &waves {
            // Spawn every ready step in the wave concurrently — unless it's
            // gated out (already done, a skipped/failed dependency, a false
            // `when`, or an unapproved checkpoint).
            let mut pending = Vec::new();
            for step_id in wave {
                let step = by_id[step_id];
                let idx  = step_index[step_id];

                // Already terminal from an earlier pass (resume) — leave as-is.
                if run.steps[idx].status.is_terminal() {
                    continue;
                }

                // Dependency gate: if any prerequisite is unavailable, this
                // step can't run with a complete input — skip it (and cascade).
                if let Some(dead) = step.depends_on.iter().find(|d| unavailable.contains(*d)) {
                    self.mark_skipped(&mut run, idx, &mut unavailable,
                        &format!("dependency '{dead}' did not complete"));
                    continue;
                }
                // Conditional gate.
                if let Some(cond) = &step.when {
                    let upstream = outputs.get(&cond.step).map(String::as_str).unwrap_or("");
                    if !cond.eval(upstream) {
                        self.mark_skipped(&mut run, idx, &mut unavailable,
                            &format!("guard on '{}' was false", cond.step));
                        continue;
                    }
                }

                // Checkpoint gate (human-in-the-loop): pause before an
                // unapproved checkpoint. Other ready steps in this wave still
                // run; the run parks after the wave drains.
                if step.requires_approval && !approved.contains(step_id) {
                    run.steps[idx].status = RunStatus::Paused;
                    run.steps[idx].error = Some("awaiting approval".into());
                    paused = true;
                    continue;
                }

                let Some(skill_id) = step.target_skill_id() else {
                    if step.continue_on_error {
                        self.mark_failed(&mut run, idx, &mut unavailable, "step has no agent/skill target");
                        continue;
                    }
                    return self.fail_step(&mut run, idx, "step has no agent/skill target").await;
                };
                let Some(executor) = self.supervisor.executor_for(&skill_id) else {
                    let msg = format!(
                        "no executor for target '{skill_id}' — the named agent may be disabled, deleted, or the skill isn't installed"
                    );
                    if step.continue_on_error {
                        self.mark_failed(&mut run, idx, &mut unavailable, &msg);
                        continue;
                    }
                    return self.fail_step(&mut run, idx, &msg).await;
                };

                let brief = render_brief(&step.brief, &run.input, &outputs);
                let budget = step.budget_usd.unwrap_or(self.default_budget)
                    .clamp(0.05, self.max_budget);

                let root = self.agent_registry.register(Agent::new_root());
                let root_id = root.read().expect("root read").id;
                let task_id = AgentId::new();

                let handle = self.supervisor.spawn_worker_full_with_id(
                    root_id, 0, skill_id.clone(), brief, None, budget, None,
                    executor, None, run.user_id.clone(), Some(task_id),
                );

                run.steps[idx].status = RunStatus::Running;
                run.steps[idx].task_id = Some(task_id.to_string());
                pending.push((step_id.clone(), idx, handle.completion));
            }
            let _ = self.store.save_run(&run);

            // Await the whole wave.
            let results = futures::future::join_all(
                pending.into_iter().map(|(sid, idx, rx)| async move {
                    (sid, idx, rx.await)
                })
            ).await;

            let mut fatal: Option<String> = None;
            for (sid, idx, res) in results {
                let err = match res {
                    Ok(WorkerOutcome::Complete(c)) => {
                        outputs.insert(sid, c.result_summary.clone());
                        run.steps[idx].status = RunStatus::Completed;
                        run.steps[idx].output = Some(c.result_summary);
                        continue;
                    }
                    Ok(WorkerOutcome::Failed(f)) => f.error,
                    Err(_) => "worker dropped before completing".to_string(),
                };
                // A failure. Mark it + make dependents skip. Fatal unless the
                // step opted into continue-on-error.
                run.steps[idx].status = RunStatus::Failed;
                run.steps[idx].error = Some(err.clone());
                unavailable.insert(run.steps[idx].step_id.clone());
                if !by_id[&run.steps[idx].step_id].continue_on_error {
                    fatal.get_or_insert(format!("step '{}' failed: {err}", run.steps[idx].step_id));
                }
            }
            let _ = self.store.save_run(&run);

            if let Some(err) = fatal {
                return self.finish(run, RunStatus::Failed, Some(err)).await;
            }
            // A checkpoint in (or before) this wave is awaiting approval — park
            // the run. The task exits; an approve/reject call resumes by
            // re-entering execute, which reconstructs state from the persisted
            // step outputs and skips everything already terminal.
            if paused {
                run.status = RunStatus::Paused;
                run.updated_at = Self::now();
                let _ = self.store.save_run(&run);
                info!("workflow '{}' run {} paused for approval", def.name, run.id);
                return run;
            }
        }

        // Success — the run summary is the concatenation of terminal steps
        // (those nothing depends on), which is what a caller actually wants.
        let summary = self.terminal_summary(&def, &outputs);
        info!("workflow '{}' run {} completed", def.name, run.id);
        self.finish_with_summary(run, RunStatus::Completed, None, summary).await
    }

    /// Resume a parked run in the background (after an approve/reject mutates
    /// it). Re-enters [`execute`], which is idempotent over terminal steps.
    pub fn resume(self: &Arc<Self>, def: WorkflowDefinition, run: WorkflowRun) {
        let me = Arc::clone(self);
        tokio::spawn(async move { me.execute(def, run).await; });
    }

    /// Approve or reject a checkpoint step on a paused run, then resume.
    /// Returns the run as left immediately after the decision (the resume runs
    /// asynchronously).
    pub fn act_on_checkpoint(
        self: &Arc<Self>,
        run_id:  &str,
        step_id: &str,
        approve: bool,
    ) -> Result<WorkflowRun, crate::MiraError> {
        let mut run = self.store.get_run(run_id)?
            .ok_or_else(|| crate::MiraError::NotFound(format!("no run {run_id}")))?;
        if run.status != RunStatus::Paused {
            return Err(crate::MiraError::ConfigError("run is not awaiting approval".into()));
        }
        let idx = run.steps.iter().position(|s| s.step_id == step_id)
            .ok_or_else(|| crate::MiraError::NotFound(format!("no step '{step_id}' in run")))?;
        if run.steps[idx].status != RunStatus::Paused {
            return Err(crate::MiraError::ConfigError(format!("step '{step_id}' is not awaiting approval")));
        }

        if approve {
            if !run.approved_steps.iter().any(|s| s == step_id) {
                run.approved_steps.push(step_id.to_string());
            }
            // Back to Pending so the resume pass runs it.
            run.steps[idx].status = RunStatus::Pending;
            run.steps[idx].error = None;
        } else {
            run.steps[idx].status = RunStatus::Skipped;
            run.steps[idx].error = Some("declined by approver".into());
        }
        run.status = RunStatus::Running;
        run.updated_at = Self::now();
        self.store.save_run(&run)?;

        let def = self.store.get(&run.workflow_id)?
            .ok_or_else(|| crate::MiraError::NotFound("workflow definition no longer exists".into()))?;
        self.resume(def, run.clone());
        Ok(run)
    }

    /// Steps that no other step depends on — the workflow's "outputs".
    fn terminal_summary(&self, def: &WorkflowDefinition, outputs: &HashMap<String, String>) -> String {
        use std::collections::HashSet;
        let depended: HashSet<&str> = def.steps.iter()
            .flat_map(|s| s.depends_on.iter().map(String::as_str))
            .collect();
        let mut parts = Vec::new();
        for s in &def.steps {
            if !depended.contains(s.id.as_str()) {
                if let Some(out) = outputs.get(&s.id) {
                    if def.steps.len() > 1 {
                        parts.push(format!("### {}\n{}", s.id, out));
                    } else {
                        parts.push(out.clone());
                    }
                }
            }
        }
        parts.join("\n\n")
    }

    /// Mark a step skipped (guard false / dependency unavailable) and cascade
    /// — its own dependents will then skip too.
    fn mark_skipped(
        &self, run: &mut WorkflowRun, idx: usize,
        unavailable: &mut std::collections::HashSet<String>, reason: &str,
    ) {
        run.steps[idx].status = RunStatus::Skipped;
        run.steps[idx].error = Some(format!("skipped: {reason}"));
        unavailable.insert(run.steps[idx].step_id.clone());
    }

    /// Mark a step failed non-fatally (continue-on-error) and cascade.
    fn mark_failed(
        &self, run: &mut WorkflowRun, idx: usize,
        unavailable: &mut std::collections::HashSet<String>, msg: &str,
    ) {
        run.steps[idx].status = RunStatus::Failed;
        run.steps[idx].error = Some(msg.to_string());
        unavailable.insert(run.steps[idx].step_id.clone());
    }

    async fn fail(&self, run: &mut WorkflowRun, msg: String) -> WorkflowRun {
        warn!("workflow run {} failed: {msg}", run.id);
        self.finish(run.clone(), RunStatus::Failed, Some(msg)).await
    }

    async fn fail_step(&self, run: &mut WorkflowRun, idx: usize, msg: &str) -> WorkflowRun {
        run.steps[idx].status = RunStatus::Failed;
        run.steps[idx].error = Some(msg.to_string());
        let full = format!("step '{}': {msg}", run.steps[idx].step_id);
        self.finish(run.clone(), RunStatus::Failed, Some(full)).await
    }

    async fn finish(&self, run: WorkflowRun, status: RunStatus, error: Option<String>) -> WorkflowRun {
        let summary = error.clone().unwrap_or_default();
        self.finish_with_summary(run, status, error, summary).await
    }

    async fn finish_with_summary(
        &self, mut run: WorkflowRun, status: RunStatus, error: Option<String>, summary: String,
    ) -> WorkflowRun {
        run.status = status;
        run.error = error.clone();
        run.updated_at = Self::now();
        let _ = self.store.save_run(&run);

        if let Some(bus) = self.event_bus.as_ref() {
            let (emoji, label, summary_or_error) = match status {
                RunStatus::Completed => ("✅", "finished", summary.clone()),
                _ => ("⚠️", "failed", format!("Error: {}", error.clone().unwrap_or_default())),
            };
            bus.emit(crate::events::Event::new(
                crate::events::names::AGENT_WORKFLOW_COMPLETED,
                run.user_id.clone(),
                serde_json::json!({
                    "run_id":           run.id,
                    "workflow":         run.workflow_name,
                    "status":           status.as_str(),
                    "summary":          if status == RunStatus::Completed { Some(summary) } else { None },
                    "failure_reason":   error,
                    "status_emoji":     emoji,
                    "status_label":     label,
                    "summary_or_error": summary_or_error,
                }),
            ));
        }
        run
    }
}

/// Substitute `{{input}}` and `{{steps.<id>.output}}` (also `{{steps.<id>}}`)
/// in a brief template. An empty/whitespace template falls back to the run
/// input, so a one-step "just hand the input to this agent" workflow works.
fn render_brief(template: &str, input: &str, outputs: &HashMap<String, String>) -> String {
    if template.trim().is_empty() {
        return input.to_string();
    }
    let mut out = template.replace("{{input}}", input);
    for (id, val) in outputs {
        out = out.replace(&format!("{{{{steps.{id}.output}}}}"), val);
        out = out.replace(&format!("{{{{steps.{id}}}}}"), val);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::resolver::MiraSkillResolver;
    use crate::agent::supervisor::{
        WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure, WorkerTask,
    };
    use crate::agent::workflow::{
        ConditionOp, NewWorkflowDefinition, StepCondition, WorkflowStep,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn step(id: &str, skill: &str, brief: &str, deps: &[&str]) -> WorkflowStep {
        WorkflowStep {
            id: id.into(), agent: None, skill: Some(skill.into()),
            brief: brief.into(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            budget_usd: None,
            continue_on_error: false,
            when: None,
            requires_approval: false,
        }
    }

    /// Records the brief it was handed and echoes a canned answer back.
    struct RecordingExec {
        tag:  &'static str,
        seen: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl WorkerTask for RecordingExec {
        async fn run(&self, a: WorkerAssignment, _: WorkerContext)
            -> Result<WorkerComplete, WorkerFailure>
        {
            self.seen.lock().unwrap().push(a.task.clone());
            Ok(WorkerComplete { result_summary: format!("{}-out", self.tag), artifacts: vec![] })
        }
    }

    struct FailingExec;
    #[async_trait]
    impl WorkerTask for FailingExec {
        async fn run(&self, _: WorkerAssignment, _: WorkerContext)
            -> Result<WorkerComplete, WorkerFailure>
        {
            Err(WorkerFailure { error: "boom".into(), partial_artifacts: vec![], fault: None })
        }
    }

    fn orchestrator_with(
        resolver: MiraSkillResolver,
    ) -> (Arc<Orchestrator>, Arc<WorkflowStore>) {
        let reg = Arc::new(AgentRegistry::new());
        let sup = Arc::new(Supervisor::new(Arc::clone(&reg)).with_resolver(Arc::new(resolver)));
        let store = Arc::new(WorkflowStore::open_memory().unwrap());
        let orch = Arc::new(Orchestrator::new(sup, reg, Arc::clone(&store), 2.0, 10.0));
        (orch, store)
    }

    #[tokio::test]
    async fn linear_chain_passes_output_forward() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let resolver = MiraSkillResolver::new()
            .with_skill("com.a", Arc::new(RecordingExec { tag: "a", seen: Arc::clone(&seen) }) as Arc<dyn WorkerTask>)
            .with_skill("com.b", Arc::new(RecordingExec { tag: "b", seen: Arc::clone(&seen) }) as Arc<dyn WorkerTask>);
        let (orch, store) = orchestrator_with(resolver);

        let def = store.create(NewWorkflowDefinition {
            name: "chain".into(), description: String::new(), enabled: true,
            steps: vec![
                step("first", "com.a", "research {{input}}", &[]),
                step("second", "com.b", "summarise: {{steps.first.output}}", &["first"]),
            ],
        }).unwrap();

        let run = orch.new_run(&def, "AI news".into(), Some("alice".into()));
        let run = orch.execute(def, run).await;

        assert_eq!(run.status, RunStatus::Completed, "run failed: {:?}", run.error);
        // Step 1 saw the input; step 2 saw step 1's output interpolated.
        let briefs = seen.lock().unwrap().clone();
        assert!(briefs.iter().any(|b| b == "research AI news"), "briefs: {briefs:?}");
        assert!(briefs.iter().any(|b| b == "summarise: a-out"), "briefs: {briefs:?}");
        // Terminal step's output is the run summary; persisted run reflects it.
        let persisted = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(persisted.status, RunStatus::Completed);
        assert_eq!(persisted.steps[1].output.as_deref(), Some("b-out"));
    }

    #[tokio::test]
    async fn parallel_wave_then_join() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let resolver = MiraSkillResolver::new()
            .with_skill("com.x", Arc::new(RecordingExec { tag: "x", seen: Arc::clone(&seen) }) as Arc<dyn WorkerTask>);
        let (orch, _store) = orchestrator_with(resolver);
        // a → {b, c} → d ; all use com.x
        let def = WorkflowDefinition {
            id: "w".into(), name: "diamond".into(), description: String::new(),
            enabled: true, created_at: 0, updated_at: 0,
            steps: vec![
                step("a", "com.x", "", &[]),
                step("b", "com.x", "{{steps.a.output}}", &["a"]),
                step("c", "com.x", "{{steps.a.output}}", &["a"]),
                step("d", "com.x", "{{steps.b.output}}+{{steps.c.output}}", &["b", "c"]),
            ],
        };
        let run = orch.new_run(&def, "go".into(), None);
        let run = orch.execute(def, run).await;
        assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
        // d saw both b and c outputs.
        let briefs = seen.lock().unwrap().clone();
        assert!(briefs.iter().any(|b| b == "x-out+x-out"), "briefs: {briefs:?}");
    }

    #[tokio::test]
    async fn failing_step_fails_run_fast() {
        let resolver = MiraSkillResolver::new()
            .with_skill("com.bad", Arc::new(FailingExec) as Arc<dyn WorkerTask>)
            .with_skill("com.good", Arc::new(RecordingExec { tag: "g", seen: Arc::new(Mutex::new(vec![])) }) as Arc<dyn WorkerTask>);
        let (orch, _store) = orchestrator_with(resolver);
        let def = WorkflowDefinition {
            id: "w".into(), name: "fails".into(), description: String::new(),
            enabled: true, created_at: 0, updated_at: 0,
            steps: vec![
                step("a", "com.bad", "", &[]),
                step("b", "com.good", "{{steps.a.output}}", &["a"]),
            ],
        };
        let run = orch.new_run(&def, "go".into(), None);
        let run = orch.execute(def, run).await;
        assert_eq!(run.status, RunStatus::Failed);
        assert!(run.error.unwrap().contains("boom"));
        // The dependent never ran.
        assert_eq!(run.steps[1].status, RunStatus::Pending);
    }

    #[tokio::test]
    async fn false_guard_skips_step_and_cascades() {
        let resolver = MiraSkillResolver::new()
            .with_skill("com.x", Arc::new(RecordingExec { tag: "x", seen: Arc::new(Mutex::new(vec![])) }) as Arc<dyn WorkerTask>);
        let (orch, _store) = orchestrator_with(resolver);
        // a → b (guarded: only if a's output contains "GO" — it won't) → c
        let mut b = step("b", "com.x", "{{steps.a.output}}", &["a"]);
        b.when = Some(StepCondition { step: "a".into(), op: ConditionOp::Contains, value: "GO".into() });
        let def = WorkflowDefinition {
            id: "w".into(), name: "guarded".into(), description: String::new(),
            enabled: true, created_at: 0, updated_at: 0,
            steps: vec![step("a", "com.x", "", &[]), b, step("c", "com.x", "{{steps.b.output}}", &["b"])],
        };
        let run = orch.new_run(&def, "go".into(), None);
        let run = orch.execute(def, run).await;
        // Run completes (no fatal); b skipped (guard false), c skipped (dep gone).
        assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
        assert_eq!(run.steps[0].status, RunStatus::Completed);
        assert_eq!(run.steps[1].status, RunStatus::Skipped);
        assert_eq!(run.steps[2].status, RunStatus::Skipped);
    }

    #[tokio::test]
    async fn continue_on_error_isolates_failed_branch() {
        let resolver = MiraSkillResolver::new()
            .with_skill("com.bad", Arc::new(FailingExec) as Arc<dyn WorkerTask>)
            .with_skill("com.good", Arc::new(RecordingExec { tag: "g", seen: Arc::new(Mutex::new(vec![])) }) as Arc<dyn WorkerTask>);
        let (orch, _store) = orchestrator_with(resolver);
        // a (fails, continue) → b (skips); c independent → completes.
        let mut a = step("a", "com.bad", "", &[]);
        a.continue_on_error = true;
        let def = WorkflowDefinition {
            id: "w".into(), name: "isolate".into(), description: String::new(),
            enabled: true, created_at: 0, updated_at: 0,
            steps: vec![a, step("b", "com.good", "{{steps.a.output}}", &["a"]), step("c", "com.good", "", &[])],
        };
        let run = orch.new_run(&def, "go".into(), None);
        let run = orch.execute(def, run).await;
        assert_eq!(run.status, RunStatus::Completed, "err: {:?}", run.error);
        assert_eq!(run.steps[0].status, RunStatus::Failed);   // a failed (non-fatal)
        assert_eq!(run.steps[1].status, RunStatus::Skipped);  // b skipped (dep gone)
        assert_eq!(run.steps[2].status, RunStatus::Completed); // c independent ran
    }

    fn checkpoint_def(store: &WorkflowStore) -> WorkflowDefinition {
        // a → b(checkpoint) → c
        let mut b = step("b", "com.x", "{{steps.a.output}}", &["a"]);
        b.requires_approval = true;
        store.create(NewWorkflowDefinition {
            name: "approve-flow".into(), description: String::new(), enabled: true,
            steps: vec![step("a", "com.x", "", &[]), b, step("c", "com.x", "{{steps.b.output}}", &["b"])],
        }).unwrap()
    }

    async fn wait_terminal(store: &WorkflowStore, run_id: &str) -> WorkflowRun {
        for _ in 0..100 {
            let r = store.get_run(run_id).unwrap().unwrap();
            if matches!(r.status, RunStatus::Completed | RunStatus::Failed) { return r; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("run never reached a terminal state");
    }

    #[tokio::test]
    async fn checkpoint_pauses_then_approve_resumes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let resolver = MiraSkillResolver::new()
            .with_skill("com.x", Arc::new(RecordingExec { tag: "x", seen }) as Arc<dyn WorkerTask>);
        let (orch, store) = orchestrator_with(resolver);
        let def = checkpoint_def(&store);

        let run = orch.new_run(&def, "go".into(), None);
        let run_id = run.id.clone();
        let run = orch.execute(def.clone(), run).await;
        // Parked at the checkpoint: a ran, b awaits, c not reached.
        assert_eq!(run.status, RunStatus::Paused);
        assert_eq!(run.steps[0].status, RunStatus::Completed);
        assert_eq!(run.steps[1].status, RunStatus::Paused);
        assert_eq!(run.steps[2].status, RunStatus::Pending);

        // Approve → resume runs b then c.
        let resumed = orch.act_on_checkpoint(&run_id, "b", true).unwrap();
        assert_eq!(resumed.status, RunStatus::Running);
        let done = wait_terminal(&store, &run_id).await;
        assert_eq!(done.status, RunStatus::Completed);
        assert_eq!(done.steps[1].status, RunStatus::Completed);
        assert_eq!(done.steps[2].status, RunStatus::Completed);
    }

    #[tokio::test]
    async fn checkpoint_reject_skips_and_cascades() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let resolver = MiraSkillResolver::new()
            .with_skill("com.x", Arc::new(RecordingExec { tag: "x", seen }) as Arc<dyn WorkerTask>);
        let (orch, store) = orchestrator_with(resolver);
        let def = checkpoint_def(&store);

        let run = orch.new_run(&def, "go".into(), None);
        let run_id = run.id.clone();
        let run = orch.execute(def, run).await;
        assert_eq!(run.status, RunStatus::Paused);

        // Reject → b skipped (declined), c skipped (cascade), run completes.
        orch.act_on_checkpoint(&run_id, "b", false).unwrap();
        let done = wait_terminal(&store, &run_id).await;
        assert_eq!(done.status, RunStatus::Completed);
        assert_eq!(done.steps[1].status, RunStatus::Skipped);
        assert!(done.steps[1].error.as_deref().unwrap().contains("declined"));
        assert_eq!(done.steps[2].status, RunStatus::Skipped);
    }

    #[tokio::test]
    async fn act_on_checkpoint_rejects_non_paused_run() {
        let (orch, store) = orchestrator_with(MiraSkillResolver::new());
        let def = store.create(NewWorkflowDefinition {
            name: "w".into(), description: String::new(), enabled: true,
            steps: vec![step("a", "com.x", "", &[])],
        }).unwrap();
        let run = orch.new_run(&def, "go".into(), None);
        store.save_run(&run).unwrap();
        // Run is Pending, not Paused → approving any step errors.
        let err = orch.act_on_checkpoint(&run.id, "a", true).unwrap_err().to_string();
        assert!(err.contains("not awaiting approval"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_executor_fails_cleanly() {
        let (orch, _store) = orchestrator_with(MiraSkillResolver::new());
        let def = WorkflowDefinition {
            id: "w".into(), name: "noexec".into(), description: String::new(),
            enabled: true, created_at: 0, updated_at: 0,
            steps: vec![step("a", "com.missing", "", &[])],
        };
        let run = orch.new_run(&def, "go".into(), None);
        let run = orch.execute(def, run).await;
        assert_eq!(run.status, RunStatus::Failed);
        assert!(run.error.unwrap().contains("no executor"));
    }
}
