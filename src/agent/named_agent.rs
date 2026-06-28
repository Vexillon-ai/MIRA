// SPDX-License-Identifier: AGPL-3.0-or-later

//! Named-agent invocation (Phase B slice 2).
//!
//! A named agent is a saved [`AgentDefinition`] — a persona (system prompt),
//! a tool allowlist, an optional model alias, and a budget — addressed by a
//! lowercase `@handle`.  stored and managed them; this slice makes one
//! *run*.
//!
//! [`NamedAgentExecutor`] is a [`WorkerTask`] that drives MIRA's reusable
//! tool-use loop ([`run_tool_loop_with_context`]) with the definition's
//! persona, tool subset, and model. [`NamedAgentResolver`] is a
//! [`SkillExecutorResolver`] that maps the `named:<handle>` skill-id
//! convention to a freshly-built executor, looking the definition up in the
//! [`AgentDefinitionStore`] at spawn time (so edits/enables take effect
//! immediately, no restart). It composes with the built-in
//! [`MiraSkillResolver`](super::resolver::MiraSkillResolver) via
//! [`ChainedResolver`](super::resolver::ChainedResolver).
//!
//! Both invocation entry points — `spawn_background_task` (MIRA delegating in
//! its own activity) and a user naming an agent in chat — funnel through the
//! same `named:<handle>` skill id, so the supervisor, budgets, audit log,
//! artifact dirs, and completion notifications all work unchanged.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::warn;

use crate::agent::definitions::{AgentDefinition, AgentDefinitionStore};
use crate::agent::stream::StreamEvent;
use crate::agent::supervisor::{
    SkillExecutorResolver, WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure,
    WorkerTask,
};
use crate::agent::tool_loop::{run_tool_loop_with_context, ToolEventCtx, ToolMode};
use crate::config::MiraConfig;
use crate::providers::ModelProvider;
use crate::tools::ToolRegistry;
use crate::types::{ChatMessage, GenerationOptions};

// Skill-id prefix that routes a spawn to a named agent rather than a
// packaged skill. e.g. `named:researcher`.
pub const NAMED_AGENT_PREFIX: &str = "named:";

// Build the `named:<handle>` skill id for a definition handle.
pub fn skill_id_for_handle(handle: &str) -> String {
    format!("{NAMED_AGENT_PREFIX}{handle}")
}

// Strip the `named:` prefix, returning the handle if this skill id targets a
// named agent. Returns `None` for ordinary skill ids and for an empty handle.
pub fn handle_from_skill_id(skill_id: &str) -> Option<&str> {
    skill_id
        .strip_prefix(NAMED_AGENT_PREFIX)
        .filter(|h| !h.is_empty())
}

// ─────────────────────────────────────────────────────────────────────────────

// A [`WorkerTask`] bound to a single [`AgentDefinition`]. Runs the standard
// tool-use loop with the definition's persona, tool subset, and model.
pub struct NamedAgentExecutor {
    def:        AgentDefinition,
    provider:   Arc<dyn ModelProvider>,
    tools:      Arc<ToolRegistry>,
    tool_mode:  ToolMode,
    max_rounds: usize,
}

impl NamedAgentExecutor {
    pub fn new(
        def:        AgentDefinition,
        provider:   Arc<dyn ModelProvider>,
        tools:      Arc<ToolRegistry>,
        tool_mode:  ToolMode,
        max_rounds: usize,
    ) -> Self {
        Self { def, provider, tools, tool_mode, max_rounds }
    }
}

#[async_trait]
impl WorkerTask for NamedAgentExecutor {
    async fn run(
        &self,
        assignment: WorkerAssignment,
        ctx:        WorkerContext,
    ) -> Result<WorkerComplete, WorkerFailure> {
        let task = assignment.task.trim();
        if task.is_empty() {
            return Err(WorkerFailure {
                error: format!("named agent @{}: empty task", self.def.name),
                partial_artifacts: vec![],
                fault: None,
            });
        }

        // Persona + task. The system prompt is the saved persona; the user
        // message is the brief the caller handed us (already carrying any
        // output-dir addendum the spawn tool appended).
        let mut messages = vec![
            ChatMessage::system(self.def.system_prompt.clone()),
            ChatMessage::user(task.to_string()),
        ];

        // Tool allowlist: an empty list means "the default visible set"
        // (`None` → the loop offers every non-System tool). A non-empty list
        // restricts the loop to exactly those names.
        let allowed: Option<Vec<String>> = if self.def.allowed_tools.is_empty() {
            None
        } else {
            Some(self.def.allowed_tools.clone())
        };

        // Bridge the loop's `StreamEvent` channel onto the worker's progress
        // feed so tool activity shows up in the live fleet view. We forward
        // only the structural events (tool calls/results, warnings) — token
        // deltas would flood the progress log.
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
        let sender = ctx.sender_clone();
        let pump = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let line = match ev {
                    StreamEvent::ToolCall { name, .. } => Some(format!("→ {name}")),
                    StreamEvent::ToolResult { name, success, .. } => {
                        Some(format!("{} {name}", if success { "✓" } else { "✗" }))
                    }
                    StreamEvent::Warning(w) => Some(format!("⚠ {w}")),
                    _ => None,
                };
                if let Some(step_summary) = line {
                    let _ = sender.send_event(crate::agent::protocol::Event::Progress {
                        step_summary,
                        percent_done: None,
                        llm_spend_usd: 0.0,
                    });
                }
            }
        });

        ctx.report_progress(format!("[@{}] starting", self.def.name), None, 0.0);

        let options = GenerationOptions::default();
        let result = run_tool_loop_with_context(
            &self.provider,
            &self.tools,
            &mut messages,
            &options,
            &self.tool_mode,
            self.max_rounds,
            &tx,
            allowed.as_deref(),
            &serde_json::Map::new(),
            ToolEventCtx::NONE,
            None, // named agents use a fixed toolset; no progressive disclosure
            None, // …and therefore no find_tools pool
        )
        .await;

        // Closing `tx` lets the pump drain and exit.
        drop(tx);
        let _ = pump.await;

        match result {
            Ok((text, usage)) => {
                ctx.report_progress(
                    format!(
                        "[@{}] done ({} prompt + {} completion tokens)",
                        self.def.name, usage.prompt_tokens, usage.completion_tokens,
                    ),
                    Some(1.0),
                    0.0,
                );
                Ok(WorkerComplete { result_summary: text, artifacts: vec![] })
            }
            Err(e) => Err(WorkerFailure {
                error: format!("named agent @{} failed: {e}", self.def.name),
                partial_artifacts: vec![],
                fault: None,
            }),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

// Store-backed resolver for `named:<handle>` skill ids. Builds a fresh
// [`NamedAgentExecutor`] per spawn from the current definition row, so an
// admin's edits and enable/disable toggles apply on the next invocation
// without a restart.
// // The tool registry is late-bound: the resolver is constructed before the
// registry exists (the registry's `spawn_background_task` tool needs the
// supervisor, which needs the resolver), so the gateway fills [`tools`] via
// the cell returned by [`NamedAgentResolver::new`] once the registry is
// built. A spawn that somehow races startup before the cell is filled
// resolves to `None` (deny), the same as an unknown skill.
// // [`tools`]: NamedAgentResolver
pub struct NamedAgentResolver {
    store:    Arc<AgentDefinitionStore>,
    config:   Arc<MiraConfig>,
    provider: Arc<dyn ModelProvider>,
    tools:    Arc<OnceLock<Arc<ToolRegistry>>>,
}

impl NamedAgentResolver {
    // Build the resolver and return it alongside the tool-registry cell the
    // gateway must fill once the registry is constructed.
    pub fn new(
        store:    Arc<AgentDefinitionStore>,
        config:   Arc<MiraConfig>,
        provider: Arc<dyn ModelProvider>,
    ) -> (Arc<Self>, Arc<OnceLock<Arc<ToolRegistry>>>) {
        let cell = Arc::new(OnceLock::new());
        let me = Arc::new(Self {
            store,
            config,
            provider,
            tools: Arc::clone(&cell),
        });
        (me, cell)
    }

    // Resolve the definition's `model_alias` to a provider. Falls back to the
    // shared primary provider when the alias is unset, unknown, or fails to
    // build (logged, never fatal — a working primary beats a dead spawn).
    fn resolve_provider(&self, def: &AgentDefinition) -> Arc<dyn ModelProvider> {
        let Some(alias) = def.model_alias.as_deref() else {
            return Arc::clone(&self.provider);
        };
        match self.config.agent.llm_aliases.get(alias) {
            Some(a) => match build_provider_for_alias(&self.config, &a.provider, a.model.as_deref()) {
                Ok(p) => p,
                Err(e) => {
                    warn!(
                        "named agent @{}: model alias '{alias}' → provider '{}' failed to build ({e}); using primary",
                        def.name, a.provider,
                    );
                    Arc::clone(&self.provider)
                }
            },
            None => {
                warn!(
                    "named agent @{}: model alias '{alias}' not found in llm_aliases; using primary",
                    def.name,
                );
                Arc::clone(&self.provider)
            }
        }
    }
}

impl SkillExecutorResolver for NamedAgentResolver {
    fn executor_for(&self, skill_id: &str) -> Option<Arc<dyn WorkerTask>> {
        let handle = handle_from_skill_id(skill_id)?;
        // Built-in MIRA-Guardian: code-defined, never from the DB. Resolves only
        // when `guardian.mode` is not `off`. Identity/tools are immutable.
        let def = if handle == crate::agent::guardian::RESERVED_NAME {
            use crate::agent::guardian;
            if guardian::mode(&self.config) == guardian::GuardianMode::Off {
                return None;
            }
            // Fail-closed local-only check (§5): never run the Guardian on a
            // cloud/remote model — it must not egress conversation/log data.
            let chk = guardian::model_check(&self.config);
            if !chk.allowed {
                warn!("MIRA-Guardian refused to run: {}", chk.reason);
                return None;
            }
            if chk.locality == guardian::ModelLocality::LanLocal {
                warn!("MIRA-Guardian: {}", chk.reason);
            }
            guardian::definition()
        } else {
            match self.store.get_by_name(handle) {
                Ok(Some(def)) => def,
                Ok(None) => return None,
                Err(e) => {
                    warn!("named agent resolve '{handle}' failed: {e}");
                    return None;
                }
            }
        };
        if !def.enabled {
            return None;
        }
        // No registry yet → deny (only possible in a startup race window).
        let tools = Arc::clone(self.tools.get()?);
        let provider = self.resolve_provider(&def);
        let tool_mode = ToolMode::from_str(&self.config.agent.tool_mode);
        let max_rounds = self.config.agent.max_tool_rounds;
        Some(Arc::new(NamedAgentExecutor::new(
            def, provider, tools, tool_mode, max_rounds,
        )))
    }
}

// Build a single provider for a named alias: clone the config, head the
// failover chain with the alias's provider + model, and reuse the gateway's
// provider-chain builder. Mirrors `bench::run::build_provider_for`.
pub(crate) fn build_provider_for_alias(
    config:   &MiraConfig,
    provider: &str,
    model:    Option<&str>,
) -> Result<Arc<dyn ModelProvider>, crate::MiraError> {
    let mut c = config.clone();
    c.primary_provider = provider.to_string();
    if let Some(m) = model {
        let m = m.to_string();
        let p = &mut c.providers;
        match provider {
            "ollama"     => p.ollama.default_model     = m,
            "lmstudio"   => p.lmstudio.default_model   = m,
            "openrouter" => p.openrouter.default_model = m,
            "openai"     => p.openai.default_model     = m,
            "deepseek"   => p.deepseek.default_model   = m,
            "moonshot"   => p.moonshot.default_model   = m,
            "groq"       => p.groq.default_model       = m,
            "xai"        => p.xai.default_model        = m,
            "anthropic"  => p.anthropic.default_model  = m,
            "gemini"     => p.gemini.default_model     = m,
            other => {
                return Err(crate::MiraError::ConfigError(format!(
                    "model alias targets provider '{other}', which has no model override path"
                )))
            }
        }
    }
    crate::gateway::builder::build_provider_chain(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_id_round_trips() {
        let id = skill_id_for_handle("researcher");
        assert_eq!(id, "named:researcher");
        assert_eq!(handle_from_skill_id(&id), Some("researcher"));
    }

    #[test]
    fn non_named_skill_ids_are_ignored() {
        assert_eq!(handle_from_skill_id("com.mira.research"), None);
        assert_eq!(handle_from_skill_id("named:"), None);
    }

    // ── resolver + executor end-to-end ────────────────────────────────

    use crate::agent::definitions::NewAgentDefinition;
    use crate::agent::instance::{Agent, AgentRegistry};
    use crate::agent::supervisor::{Supervisor, WorkerOutcome};
    use crate::types::{GenerationResponse, MessageRole, ProviderId, TokenUsage};
    use std::sync::Mutex;
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    // Minimal provider that returns a canned answer and records what it saw.
    struct StubProvider {
        canned: String,
        seen:   Mutex<Vec<ChatMessage>>,
    }
    #[async_trait]
    impl ModelProvider for StubProvider {
        fn name(&self) -> &str { "stub" }
        async fn generate(
            &self, messages: &[ChatMessage], _opts: &GenerationOptions,
        ) -> Result<GenerationResponse, crate::MiraError> {
            self.seen.lock().unwrap().extend(messages.iter().cloned());
            Ok(GenerationResponse {
                content:     self.canned.clone(),
                tool_calls:  None,
                reasoning:   None,
                usage:       TokenUsage::default(),
                provider_id: ProviderId::Local("stub".into()),
                model_name:  "stub".into(),
                fallback: None,
            })
        }
        async fn health_check(&self) -> bool { true }
    }

    fn make_resolver(enabled: bool) -> (
        Arc<NamedAgentResolver>,
        Arc<OnceLock<Arc<ToolRegistry>>>,
        Arc<StubProvider>,
    ) {
        let dir = Box::leak(Box::new(tempdir().unwrap()));
        let store = Arc::new(
            AgentDefinitionStore::open(&dir.path().join("defs.db")).unwrap(),
        );
        store.create(NewAgentDefinition {
            name:          "echo".into(),
            description:   "echoes".into(),
            system_prompt: "You are echo.".into(),
            allowed_tools: vec![],
            model_alias:   None,
            budget_usd:    Some(1.0),
            enabled,
        }).unwrap();
        let provider = Arc::new(StubProvider {
            canned: "hello from echo".into(),
            seen:   Mutex::new(Vec::new()),
        });
        let config = Arc::new(crate::config::MiraConfig::default());
        let (resolver, cell) = NamedAgentResolver::new(
            store, config, Arc::clone(&provider) as Arc<dyn ModelProvider>,
        );
        (resolver, cell, provider)
    }

    #[test]
    fn resolver_denies_until_registry_bound() {
        let (resolver, _cell, _p) = make_resolver(true);
        // Cell not yet filled → deny even though the def exists + is enabled.
        assert!(resolver.executor_for("named:echo").is_none());
    }

    #[test]
    fn resolver_skips_disabled_and_unknown() {
        let (resolver, cell, _p) = make_resolver(false);
        cell.set(Arc::new(ToolRegistry::new())).ok();
        assert!(resolver.executor_for("named:echo").is_none(), "disabled must not resolve");
        assert!(resolver.executor_for("named:ghost").is_none(), "unknown handle must not resolve");
        assert!(resolver.executor_for("com.mira.research").is_none(), "non-named id ignored");
    }

    #[tokio::test]
    async fn named_agent_runs_and_returns_model_output() {
        let (resolver, cell, provider) = make_resolver(true);
        cell.set(Arc::new(ToolRegistry::new())).ok();

        let exec = resolver.executor_for("named:echo").expect("enabled + bound resolves");

        // Spawn it through a real supervisor so the full worker lifecycle runs.
        let reg = Arc::new(AgentRegistry::new());
        let root = reg.register(Agent::new_root());
        let (root_id, depth) = { let r = root.read().unwrap(); (r.id, r.depth) };
        let sup = Arc::new(Supervisor::new(reg));
        let h = sup.spawn_worker(
            root_id, depth, "named:echo", "say hi", None, 1.0, None, exec,
        );
        let outcome = timeout(Duration::from_secs(5), h.completion).await
            .expect("worker hung").unwrap();

        match outcome {
            WorkerOutcome::Complete(c) => {
                assert_eq!(c.result_summary, "hello from echo");
            }
            other => panic!("expected Complete, got {other:?}"),
        }

        // The persona landed as the system message; the brief as the user turn.
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[0].role, MessageRole::System);
        assert!(seen[0].content.contains("You are echo."));
        assert_eq!(seen[1].role, MessageRole::User);
        assert!(seen[1].content.contains("say hi"));
    }
}
