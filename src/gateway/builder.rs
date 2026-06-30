// SPDX-License-Identifier: AGPL-3.0-or-later

// src/gateway/builder.rs
//! `GatewayBuilder` — constructs a fully-wired [`Gateway`] via an ordered
//! startup sequence.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::agent::AgentCore;
use crate::auth::LocalAuthService;
use crate::calendar::{CalendarStore, SyncEngine};
use crate::channel_accounts::ChannelAccountStore;
use crate::config::MiraConfig;
use crate::gateway::channel_manager::ChannelManager;
use crate::history::HistoryStore;
use crate::memory::MemorySystem;
use crate::notifications::NotificationBus;
use crate::proxy::NginxProxy;
use crate::security::SecurityConfig;
use crate::server::MiraServer;
use crate::server::handlers::status::init_start_time;
use crate::session::SessionStore;
use crate::stt::SttService;
use crate::tts::TtsService;
use crate::onboarding::OnboardingSchema;
use crate::providers::ModelProvider;
use crate::tools::{
    ToolRegistry,
    audit::ToolAuditStore,
    shell::ShellExecuteTool,
    filesystem::{FileReadTool, FileWriteTool},
    onboarding::{
        CompleteOnboardingTool, MarkGroupCompleteTool, OnboardingServices,
        RecordProfileTool, ResolveTimezoneTool, SkipTopicTool,
    },
    recall::RecallHistoryTool,
    datetime::{NowTool, DateMathTool},
    math_eval::MathEvalTool,
    pdf::PdfExtractTool,
    summarize::SummarizeConversationTool,
    memory_supersede::MemorySupersedeTool,
    http_policy::{HttpPolicy, HttpPolicyConfig},
    web_fetch::{WebFetchTool, WebFetchSettings},
    url_preview::UrlPreviewTool,
    search::{
        WebSearchTool, SearchBackend,
        DdgHtmlBackend, BraveApiBackend, SearxngBackend,
        searxng::extract_host_port,
    },
    code_run::CodeRunTool,
    calendar::{
        CalendarCreateEventTool, CalendarDeleteEventTool,
        CalendarListEventsTool,  CalendarUpdateEventTool,
    },
    automations::{
        CancelScheduleTool, ListSelfSchedulesTool, RegisterWebhookTool,
        ScheduleFollowupTool, SubscribeEventTool,
    },
};
use crate::automations::AutomationsStore;
use crate::web::LiveConfig;
use crate::MiraError;

// ─────────────────────────────────────────────────────────────────────────────

// Builder for [`super::Gateway`].
pub struct GatewayBuilder {
    config_path: Option<PathBuf>,
    config:      Option<Arc<MiraConfig>>,
}

impl GatewayBuilder {
    pub fn new() -> Self {
        Self { config_path: None, config: None }
    }

    pub fn config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    pub fn config_path_opt(mut self, path: Option<PathBuf>) -> Self {
        self.config_path = path;
        self
    }

    // Provide an already-loaded config, skipping phase 1 of the startup sequence.
    //     // Useful when the caller needs the config before `build()` (e.g. to set up logging).
    pub fn with_config(mut self, config: Arc<MiraConfig>) -> Self {
        self.config = Some(config);
        self
    }

    // Run the full startup sequence and return a ready-to-serve [`super::Gateway`].
    pub async fn build(self) -> Result<super::Gateway, MiraError> {
        // Subsystem-fallback tracker. Built first so the early startup
        // fallbacks (embeddings, reasoning router) can record into it before
        // the notification bus exists; the bus is attached once it's built so
        // later (per-request) fallbacks also toast.
        let degradation_tracker =
            Arc::new(crate::health::degradation::DegradationTracker::new());

        // ── Configuration ────────────────────────────────────────────
        let config = match self.config {
            Some(c) => c,
            None    => {
                let c = Arc::new(MiraConfig::load(self.config_path)?);
                info!("Config loaded from {:?}", c.config_path);
                c
            }
        };

        // ── Memory system ────────────────────────────────────────────
        let data_dir = config.data_dir_path();
        std::fs::create_dir_all(&data_dir).map_err(|e| {
            MiraError::ConfigError(format!("Cannot create data dir {:?}: {}", data_dir, e))
        })?;

        // 0.106.0 — record boot for the restart-count detector. Best-effort;
        // a write failure here is logged but never blocks startup.
        if let Err(e) = crate::health::boot::record_boot(&data_dir) {
            tracing::warn!("boot history not recorded: {e}");
        }

        // 0.110.0 — HealthStore opens early so the Supervisor can also
        // hold a handle (it writes to llm_charges on every cost delta).
        // Same Arc is reused later by the heartbeat + the dashboard
        // router. None means open failed → ledger writes are no-ops
        // and the dashboard endpoints 503; non-fatal.
        let health_store_arc: Option<Arc<crate::health::store::HealthStore>> =
            match crate::health::store::HealthStore::open(&data_dir.join("health.db")) {
                Ok(hs) => Some(Arc::new(hs)),
                Err(e) => { tracing::warn!("HealthStore open failed: {e}"); None }
            };

        // 0.111.0 — task artifacts root. Best-effort mkdir; failures
        // log and keep the store None (spawn_background_task falls
        // back to no-cwd and free-form file writes).
        let task_artifacts_arc: Option<Arc<crate::task_artifacts::TaskArtifactsStore>> = {
            let root = config.artifacts_root_path();
            match std::fs::create_dir_all(&root) {
                Ok(()) => {
                    tracing::info!("task artifacts root: {}", root.display());
                    Some(Arc::new(crate::task_artifacts::TaskArtifactsStore::new(root)))
                }
                Err(e) => {
                    tracing::warn!("task artifacts root mkdir failed: {e}");
                    None
                }
            }
        };

        let db_path = data_dir.join("memory.db");

        // Resolve effective memory config — derive the embedding URL from the
        // active provider config when the user hasn't set one explicitly.
        let memory_config = {
            let mut mc = config.memory.clone();
            if mc.embedding.provider_url.is_none() {
                mc.embedding.provider_url = Some(match mc.embedding.provider.as_str() {
                    "lmstudio"   => config.providers.lmstudio.url.clone(),
                    "ollama"     => config.providers.ollama.url.clone(),
                    "openai"     => "https://api.openai.com/v1".to_string(),
                    "openrouter" => "https://openrouter.ai/api/v1".to_string(),
                    _            => config.providers.lmstudio.url.clone(),
                });
                info!(
                    "Memory: embedding URL derived from {} provider config: {}",
                    mc.embedding.provider,
                    mc.embedding.provider_url.as_deref().unwrap_or("?")
                );
            }
            mc
        };

        // For HTTP-backed providers, probe the endpoint before committing.
        // If the server is unreachable, fall back to the built-in fastembed
        // provider so semantic search always works without external dependencies.
        let memory_config = match memory_config.embedding.provider.as_str() {
            "lmstudio" | "ollama" | "openai" | "openrouter" => {
                let url = memory_config.embedding.provider_url.as_deref().unwrap_or("");
                if probe_embedding_endpoint(url).await {
                    info!("Memory: embedding server reachable at {}", url);
                    memory_config
                } else {
                    warn!(
                        "Memory: embedding server unreachable ({}) — \
                         falling back to internal fastembed provider (BGE-small-en-v1.5, ~24 MB download on first run)",
                        url
                    );
                    degradation_tracker.record(
                        "embeddings", "Embeddings (memory)",
                        memory_config.embedding.provider.as_str(), "internal fastembed",
                        &format!("server unreachable at {url}"), true,
                    );
                    let mut mc = memory_config;
                    mc.embedding.provider = "internal".to_string();
                    mc.embedding.model    = "BGE-small-en-v1.5".to_string();
                    mc
                }
            }
            _ => memory_config,
        };

        // `internal` embeddings (fastembed) need the ONNX Runtime native lib,
        // loaded dynamically. It isn't bundled and `mira setup` doesn't fetch
        // it, so on a fresh install the provider silently degrades to noop
        // embeddings (semantic memory DISABLED — only a WARN nobody sees).
        // Auto-provision it via the managed deps installer (same as
        // `mira deps install`) so memory works out of the box. The model
        // download on first embed already assumes first-run network, so this is
        // consistent. If it fails (e.g. offline), record a degradation the UI
        // can surface and let the noop fallback in new_from_embedding_config
        // stand.
        if memory_config.embedding.provider == "internal"
            && !crate::install::deps::is_onnxruntime_available()
        {
            info!("Memory: ONNX Runtime not found — provisioning it for internal embeddings (first run only)…");
            match tokio::task::spawn_blocking(|| {
                // Map to String inside the closure — Box<dyn Error> isn't Send,
                // so it can't be returned across the spawn_blocking boundary.
                crate::install::deps::install_named("onnxruntime", false)
                    .map_err(|e| e.to_string())
            }).await {
                Ok(Ok(_)) => {
                    crate::install::deps::maybe_apply_runtime_env();
                    if crate::install::deps::is_onnxruntime_available() {
                        info!("Memory: ONNX Runtime provisioned — semantic embeddings enabled");
                    }
                }
                Ok(Err(e)) => {
                    warn!("Memory: ONNX Runtime auto-install failed ({e}) — semantic search \
                           disabled until `mira deps install` + restart");
                    degradation_tracker.record(
                        "embeddings", "Embeddings (memory)",
                        "internal fastembed", "noop (disabled)",
                        &format!("ONNX Runtime unavailable; auto-install failed: {e}"), true,
                    );
                }
                Err(e) => {
                    warn!("Memory: ONNX Runtime auto-install task failed to join ({e}) — \
                           semantic search disabled");
                }
            }
        }

        let memory = Arc::new(
            match memory_config.embedding.provider.as_str() {
                "lmstudio" | "ollama" | "openai" | "openrouter" | "internal" => {
                    MemorySystem::new_from_embedding_config(db_path, &memory_config).await?
                }
                _ => {
                    warn!("Memory: unknown embedding provider '{}' — using keyword search only",
                          memory_config.embedding.provider);
                    MemorySystem::new_keyword_only(db_path)?
                }
            }
        );
        info!("Memory system initialised");

        // ── Provider chain ───────────────────────────────────────────
        let provider = build_provider_chain(&config)?;
        if provider.health_check().await {
            info!("Provider health check: ok");
        } else {
            warn!("Provider chain unhealthy at startup — model server may not be running yet");
        }

        // ── Auth DB (moved ahead of Tools so onboarding tools
        // can be registered with a live auth service) ───────────────────────
        let auth_db_path  = data_dir.join("auth.db");
        let jwt_secret    = ensure_jwt_secret(&config);
        let session_days  = config.security.session_days;

        let auth_service: Option<Arc<LocalAuthService>> =
            match LocalAuthService::new(&auth_db_path, jwt_secret, session_days) {
                Ok(svc) => {
                    info!("Auth service initialised ({})", auth_db_path.display());
                    match svc.ensure_admin_exists() {
                        Ok(Some(pw)) => {
                            tracing::warn!("┌─────────────────────────────────────────────────┐");
                            tracing::warn!("│  MIRA first run — default admin credentials:    │");
                            tracing::warn!("│  username : admin                               │");
                            tracing::warn!("│  password : {:<37}│", pw);
                            tracing::warn!("│  Change this password immediately!              │");
                            tracing::warn!("└─────────────────────────────────────────────────┘");
                        }
                        Ok(None) => {}
                        Err(e)   => warn!("ensure_admin_exists (non-fatal): {}", e),
                    }
                    Some(Arc::new(svc))
                }
                Err(e) => {
                    warn!("Auth service failed (non-fatal): {}", e);
                    None
                }
            };

        // ── History DB (also moved ahead of Tools) ───────────────
        let history_db_path = data_dir.join("history.db");
        let history: Option<Arc<HistoryStore>> =
            match HistoryStore::open(&history_db_path) {
                Ok(store) => {
                    info!("History store initialised ({})", history_db_path.display());
                    Some(Arc::new(store))
                }
                Err(e) => {
                    warn!("History store failed (non-fatal): {}", e);
                    None
                }
            };

        // ── Transcript indexer ────────────────────────────────────
        // The indexer populates `message_vectors` so the upcoming
        // `recall_history` tool can semantic-search past messages. It's a
        // fire-and-forget background task — we drop the handle so the
        // Gateway doesn't have to plumb it into its shutdown path. Aborting
        // the Tokio runtime at exit stops it cleanly.
        if let Some(hist) = history.as_ref() {
            if config.memory.indexer.enabled {
                let cfg = crate::history::IndexerConfig {
                    interval:   Duration::from_secs(config.memory.indexer.interval_secs.max(1)),
                    batch_size: config.memory.indexer.batch_size as i64,
                    skip_roles: config.memory.indexer.skip_roles.clone(),
                };
                let _handle = crate::history::MessageIndexer::start(
                    Arc::clone(hist), Arc::clone(&memory), cfg,
                );
            } else {
                info!("Transcript indexer disabled by config (memory.indexer.enabled=false)");
            }
        }

        // ── Daily memory rollup ───────────────────────────────────
        // Opt-in (off by default). When enabled, a background poller
        // consolidates each active user's previous UTC day into one summary
        // memory. Needs the provider chain to generate summaries, so it
        // lives after  Like the indexer, we drop the handle — the
        // task stops when the Tokio runtime does.
        if let Some(hist) = history.as_ref() {
            if config.memory.rollup.enabled {
                let cfg = crate::memory::rollup::RollupConfig {
                    interval:              Duration::from_secs(config.memory.rollup.interval_secs.max(60)),
                    day_lag_days:          config.memory.rollup.day_lag_days,
                    max_messages:          config.memory.rollup.max_messages as usize,
                    max_chars_per_message: config.memory.rollup.max_chars_per_message as usize,
                    // Phase C — single-valued contradiction resolution (off by
                    // default). When on, runs per active user inside the same
                    // nightly tick after their daily summary.
                    consolidate_contradictions: config.memory.consolidation.contradictions_enabled,
                    // Phase A — entity dedup (off by default). Runs after C.
                    consolidate_entities: config.memory.consolidation.entity_dedup_enabled,
                    entity_dedup_ratio:   config.memory.consolidation.entity_dedup_ratio,
                    // Phase D — importance scoring (off by default). Runs LAST.
                    consolidate_importance:    config.memory.consolidation.importance_enabled,
                    importance_half_life_days: config.memory.consolidation.importance_half_life_days,
                };
                let _handle = crate::memory::rollup::MemoryRollup::start(
                    Arc::clone(hist),
                    Arc::clone(&memory),
                    Arc::clone(&provider),
                    cfg,
                );
                info!("Memory rollup started (every {}s, lag={}d)",
                    config.memory.rollup.interval_secs,
                    config.memory.rollup.day_lag_days,
                );
            } else {
                info!("Memory rollup disabled by config (memory.rollup.enabled=false)");
            }
        }

        // ── Tools ────────────────────────────────────────────────────
        // Audit store lives in its own DB so retention and pruning can be
        // managed independently of memory/history/auth. Non-fatal on failure —
        // tools still run, just without audit rows written.
        let tool_audit: Option<Arc<ToolAuditStore>> =
            match ToolAuditStore::open(&data_dir.join("tools.db")) {
                Ok(s)  => { info!("Tool audit store initialised"); Some(Arc::new(s)) }
                Err(e) => { warn!("Tool audit store failed (non-fatal): {}", e); None }
            };

        // Calendar store — always open when the subsystem is enabled. The
        // native store is always available; external sync only fires when
        // `sync_provider != "none"` (see  below).
        let calendar_store: Option<Arc<CalendarStore>> = if config.calendar.enabled {
            match CalendarStore::open(&data_dir.join("calendar.db")) {
                Ok(s)  => { info!("Calendar store initialised"); Some(Arc::new(s)) }
                Err(e) => { warn!("Calendar store failed (non-fatal): {}", e); None }
            }
        } else {
            info!("Calendar disabled by config (calendar.enabled=false)");
            None
        };

        // open the automations store early so the
        // agent-callable `automations_*` tools have a handle to
        // create rows during a chat turn. Worker + dispatcher are spawned
        // later (they need AgentCore); failure here is non-fatal and just
        // means the automations tools aren't registered.
        let automations_store: Option<Arc<AutomationsStore>> =
            match crate::automations::open_and_seed(&data_dir) {
                Ok(s)  => { info!("Automations store initialised"); Some(s) }
                Err(e) => { warn!("Automations store failed (non-fatal): {}", e); None }
            };

        // ── Policy engine (Phase D + 1.1 plumbing) ───────────────────────────
        // Constructed BEFORE the tool registry so HttpPolicy can be attached
        // to it (network egress consults the engine). Same engine
        // instance is then handed to the Supervisor below for spawn-child
        // policy gating (D1) — keeps admin rules consistent across both
        // surfaces.
        let admin_policy_rules: Option<Arc<crate::policy::AdminRulesStore>> =
            match crate::policy::AdminRulesStore::open(&data_dir.join("admin_policy_rules.db")) {
                Ok(s)  => Some(Arc::new(s)),
                Err(e) => {
                    tracing::warn!(
                        "admin policy rules disabled — cannot open \
                         {:?}/admin_policy_rules.db: {e}",
                        data_dir,
                    );
                    None
                }
            };
        let policy_engine: Arc<dyn crate::policy::PolicyEngine> = {
            let mut engines: Vec<Arc<dyn crate::policy::PolicyEngine>> = Vec::new();
            engines.push(Arc::new(crate::policy::BuiltinRulesEngine::standard(
                crate::agent::supervisor::MAX_RECURSION_DEPTH,
                config.agent.session_budget_usd,
                // 1.5 — no fleet-wide default per-agent cap; each agent
                // carries its own budget and the rule reads it from the
                // event payload when the supervisor populates it.
                None,
                None, // skill registry wires in once Phase A is fully integrated
            )));
            if let Some(store) = admin_policy_rules.as_ref() {
                engines.push(Arc::new(crate::policy::AdminRulesEngine::new(
                    Arc::clone(store),
                )));
            }
            Arc::new(crate::policy::ChainedEngine::new(engines))
        };

        // Build the shared HttpPolicy + search backends ONCE. They're used by
        // network-tier tools (web_fetch / web_search / url_preview) AND by the
        // skill resolver below (ResearchAdapter takes the same fetcher and
        // search backend). Sharing avoids a fragmented rate-limit state.
        let http_policy: Arc<HttpPolicy> = Arc::new({
            let mut p = HttpPolicy::new(build_http_policy_config(&config));
            p = p.with_policy_engine(Arc::clone(&policy_engine));
            p
        });
        let search_backends: Vec<Arc<dyn SearchBackend>> =
            build_search_backends(&config, &http_policy);

        // ── Event bus ───────────────────────────────────────────────────────
        // Built before the supervisor so terminal worker outcomes can be
        // emitted as `agent.worker.completed` for `spawn_background_task`'s
        // auto-delivery wiring. Same Arc gets handed to AgentCore + the
        // automations subscriber later.
        let event_bus = Arc::new(crate::events::EventBus::new());

        // ── Skill secrets vault ───────────────────────────────────────────
        // Encrypted store of per-skill env vars (e.g. ANTHROPIC_API_KEY
        // for the coding skill). Best-effort: if the master key file is
        // unreadable or DB open fails we boot without secrets — adapters
        // fall back to whatever's in the process env.
        let secrets_store: Option<Arc<crate::skills::SecretsStore>> = {
            let (db_path, key_path) = crate::skills::secrets::default_paths(&data_dir);
            match crate::skills::SecretsStore::open(&db_path, &key_path) {
                Ok(s)  => {
                    info!("Skill secrets vault opened at {:?}", db_path);
                    // 0.93.0 rename: com.mira.coding → com.mira.claudecode.
                    // The vault's row AAD includes `skill_id`, so a plain
                    // UPDATE would orphan every ciphertext. `rename_skill`
                    // decrypts each row, re-encrypts under the new identity
                    // (with a fresh nonce), and deletes the old row.
                    // Idempotent — a no-op once migration has run.
                    if let Err(e) = s.rename_skill("com.mira.coding", "com.mira.claudecode") {
                        tracing::warn!(
                            "skill secret rename com.mira.coding → com.mira.claudecode \
                             failed: {e}. Existing secrets remain under the old skill id; \
                             you may need to re-set them under the new id."
                        );
                    }
                    Some(Arc::new(s))
                }
                Err(e) => {
                    tracing::warn!(
                        "skill secrets vault disabled — open failed: {e}. \
                         Skills that depend on env-var secrets will use \
                         only what's in the process environment."
                    );
                    None
                }
            }
        };

        // ── Multi-agent runtime (Phase B) ──────────────────────────────────
        // Built BEFORE the tool registry so the agent-task tools
        // (`spawn_background_task`, `get_task_result`) can take the
        // supervisor + agent registry as deps. Empty registry at startup;
        // populated when the first worker spawns.
        let agent_registry = Arc::new(crate::agent::AgentRegistry::new());
        let agent_audit: Option<Arc<crate::agent::AuditStore>> =
            match crate::agent::AuditStore::open(&data_dir.join("agent_audit.db")) {
                Ok(s)  => Some(Arc::new(s)),
                Err(e) => {
                    tracing::warn!(
                        "agent audit log disabled — cannot open {:?}/agent_audit.db: {e}",
                        data_dir,
                    );
                    None
                }
            };
        let mut sup = crate::agent::Supervisor::new(agent_registry.clone())
            .with_session_budget(config.agent.session_budget_usd)
            .with_policy_engine(Arc::clone(&policy_engine))
            .with_event_bus(Arc::clone(&event_bus));
        if let Some(store) = agent_audit.as_ref() {
            sup = sup.with_audit_store(Arc::clone(store));
        }
        // 0.110.0 — wire the health store so cost deltas land in the
        // llm_charges ledger.
        if let Some(hs) = health_store_arc.as_ref() {
            sup = sup.with_health_store(Arc::clone(hs));
        }
        // 0.111.0 — wire the task-artifacts store so terminal-outcome
        // finalisation runs the slug rename.
        if let Some(arts) = task_artifacts_arc.as_ref() {
            sup = sup.with_task_artifacts(Arc::clone(arts));
        }
        let resolver = build_skill_resolver(
            &config,
            Arc::clone(&provider),
            Arc::clone(&http_policy),
            search_backends.clone(),
            secrets_store.clone(),
        );
        if !resolver.is_empty() {
            info!("Skill resolver wired: {:?}", resolver.known_skills());
        } else {
            info!("Skill resolver wired with 0 skills (no executors registered)");
        }
        // Phase B slice 2 — named-agent resolver. Looks up `named:<handle>`
        // spawns in the agent-definitions store and runs them through the
        // standard tool-use loop. Chained AFTER the built-in skill resolver.
        // The tool registry doesn't exist yet (it depends on this supervisor),
        // so the resolver gets a late-bound cell we fill once tools are built.
        let mut named_agents_cell:
            Option<Arc<std::sync::OnceLock<Arc<crate::tools::ToolRegistry>>>> = None;
        let mut named_agents_store:
            Option<Arc<crate::agent::AgentDefinitionStore>> = None;
        let resolver_arc: Arc<dyn crate::agent::SkillExecutorResolver> =
            match crate::agent::AgentDefinitionStore::open(
                &data_dir.join("agent_definitions.db"),
            ) {
                Ok(store) => {
                    let store = Arc::new(store);
                    named_agents_store = Some(Arc::clone(&store));
                    let (named, cell) = crate::agent::NamedAgentResolver::new(
                        Arc::clone(&store),
                        Arc::clone(&config),
                        Arc::clone(&provider),
                    );
                    named_agents_cell = Some(cell);
                    info!("Named-agent resolver wired (named:<handle> spawns enabled)");
                    Arc::new(crate::agent::ChainedResolver::new(vec![
                        Arc::new(resolver),
                        named,
                    ]))
                }
                Err(e) => {
                    warn!("named-agent resolver NOT wired — store unavailable: {e}");
                    Arc::new(resolver)
                }
            };
        sup = sup.with_resolver(resolver_arc);
        let supervisor = Arc::new(sup);
        info!("Multi-agent supervisor ready (Phase B+D)");

        // Phase C — workflow store + orchestrator. The store is opened here
        // (the orchestrator + run_workflow tool share this handle; the HTTP
        // API opens its own handle to the same file). Optional: a failed open
        // just disables `run_workflow`/`list_workflows`.
        let (workflow_store, orchestrator): (
            Option<Arc<crate::agent::WorkflowStore>>,
            Option<Arc<crate::agent::Orchestrator>>,
        ) = match crate::agent::WorkflowStore::open(&data_dir.join("workflows.db")) {
            Ok(store) => {
                let store = Arc::new(store);
                let orch = crate::agent::Orchestrator::new(
                    Arc::clone(&supervisor),
                    Arc::clone(&agent_registry),
                    Arc::clone(&store),
                    config.agent.default_task_budget_usd,
                    config.agent.max_task_budget_usd,
                ).with_event_bus(Arc::clone(&event_bus));
                info!("Workflow orchestrator wired (run_workflow enabled)");
                (Some(store), Some(Arc::new(orch)))
            }
            Err(e) => {
                warn!("workflow orchestrator NOT wired — store unavailable: {e}");
                (None, None)
            }
        };

        // Wiki registry — lazily resolves per-user wikis under
        // {data_dir}/wikis/users/<id>/. Created before the tool registry
        // so the model-callable wiki tools (Slice D) can register against
        // it, then handed to AgentCore for the pre/post wiki hooks.
        let wiki_registry = {
            let mut reg = crate::wiki::WikiRegistry::new(data_dir.clone());
            if config.wiki.enabled && config.wiki.git.enabled {
                reg = reg.with_git(crate::wiki::GitPolicy {
                    auto_commit: config.wiki.git.auto_commit,
                });
            }
            Arc::new(reg)
        };
        info!("Wiki registry initialised at {}", data_dir.join("wikis").display());

        // Companion mode — open the settings store and wire
        // in the deps that the facade needs to validate safety
        // contacts (auth) and seed persona pages (wiki). Failure to
        // open the DB drops the whole feature; the tools will surface
        // a clean error instead of crashing the gateway.
        let companion_system: Option<Arc<crate::companion::CompanionSystem>> =
            match crate::companion::CompanionSystem::open(&data_dir) {
                Ok(mut sys) => {
                    if let Some(auth) = auth_service.as_ref() {
                        sys = sys.with_auth(Arc::clone(auth));
                    }
                    sys = sys.with_wiki(Arc::clone(&wiki_registry));
                    // wire history + notifications so the
                    // safety floor can deliver alerts into the
                    // contact's "Safety alerts" web thread.
                    if let Some(hist) = history.as_ref() {
                        sys = sys.with_history(Arc::clone(hist));
                    }
                    // notification_bus is constructed later in the
                    // builder; we attach it after `agent_core` is up
                    // (see set_companion site).
                    info!("Companion system initialised at {}", data_dir.join("companion.db").display());
                    Some(Arc::new(sys))
                }
                Err(e) => {
                    warn!("Companion system disabled (open failed): {e}");
                    None
                }
            };

        // Deferred handle to the LiveConfig, filled once it's built (below).
        // `settings_set` needs it to apply global writes live, but the tool
        // registry is constructed before LiveConfig exists.
        let settings_live_config: Arc<std::sync::OnceLock<Arc<crate::web::LiveConfig>>> =
            Arc::new(std::sync::OnceLock::new());

        // Shared restart notifier — created here (well before MiraServer)
        // because the backup_restore agent tool needs to hold the same
        // Arc so a tool-triggered restore actually restarts the service.
        // MiraServer::new is given the same Arc below.
        let restart_notify = Arc::new(tokio::sync::Notify::new());

        // MIRA-Guardian action proposals (P4). Durable store of pending/decided
        // remediation proposals. The propose tool writes here (active mode only);
        // the approval endpoints (P4a-2) read + execute. Non-fatal if it fails to
        // open — the Guardian just can't propose.
        let guardian_action_store: Option<Arc<crate::agent::guardian_actions::GuardianActionStore>> =
            match crate::agent::guardian_actions::GuardianActionStore::open(
                &data_dir.join("guardian_actions.db"),
            ) {
                Ok(s)  => Some(Arc::new(s)),
                Err(e) => { warn!("guardian_actions store open failed (non-fatal): {e}"); None }
            };
        // Deferred ChannelManager handle for guardian_decide (P4b) — the manager
        // is built further below; we fill this once it exists.
        let guardian_channel_manager: Arc<std::sync::OnceLock<Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>>> =
            Arc::new(std::sync::OnceLock::new());

        let tool_registry = build_tool_registry(
            &config,
            auth_service.as_ref(),
            history.as_ref(),
            &memory,
            &provider,
            &data_dir,
            tool_audit.clone(),
            calendar_store.clone(),
            automations_store.clone(),
            Some(Arc::clone(&policy_engine)),
            Arc::clone(&http_policy),
            search_backends.clone(),
            Arc::clone(&supervisor),
            Arc::clone(&agent_registry),
            task_artifacts_arc.clone(),
            named_agents_store.clone(),
            orchestrator.clone(),
            workflow_store.clone(),
            Arc::clone(&wiki_registry),
            companion_system.clone(),
            Arc::clone(&settings_live_config),
            Arc::clone(&restart_notify),
            health_store_arc.clone(),
            Arc::clone(&degradation_tracker),
            guardian_action_store.clone(),
            agent_audit.clone(),
            Arc::clone(&guardian_channel_manager),
        );

        // ── MCP host (Q2 #7, Slices 1-4) ───────────────────────────
        // per-user storage. Open the mcp_servers table next
        // to channel_accounts (same auth.db), run the legacy migrate
        // once (config.mcp.servers → admin's rows), then connect
        // every enabled row across every user and splat the
        // discovered tools onto the shared registry. The agent treats
        // them like any builtin; the per-user filter applies later
        // at turn time via `TurnContext.allowed_tool_names`.
        let mcp_store: Option<Arc<crate::mcp::McpServerStore>> =
            match crate::mcp::McpServerStore::open(&auth_db_path) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    warn!("mcp_servers store open failed (non-fatal): {e}");
                    None
                }
            };
        // Admin-managed catalog of recommended MCP servers — seeds the
        // default set on first open. Non-fatal if it fails (the /mcp page
        // just won't offer the catalog picker).
        let mcp_catalog: Option<Arc<crate::mcp::McpCatalogStore>> =
            match crate::mcp::McpCatalogStore::open(&auth_db_path) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    warn!("mcp_catalog store open failed (non-fatal): {e}");
                    None
                }
            };
        if let (Some(store), Some(auth)) = (mcp_store.as_ref(), auth_service.as_ref()) {
            match auth.current_admin_user_id() {
                Ok(Some(admin_id)) => {
                    if let Err(e) = crate::mcp::legacy_migrate::migrate_if_empty(
                        store, &config.mcp, &admin_id,
                    ) {
                        warn!("mcp legacy migrate failed (non-fatal): {e}");
                    }
                }
                Ok(None) => warn!("No admin user yet — skipping mcp legacy migrate"),
                Err(e)   => warn!("Could not resolve admin user for mcp migrate: {e}"),
            }
        }
        // ── Email channel store (Q2 #8, E1+E3 chunk 1) ────────────
        // Account rows live in auth.db beside channel_accounts + mcp_servers.
        // Chunk 1 only opens the store + serves CRUD; the IMAP poller in
        // chunk 2 starts consuming the rows. Until then, accounts created
        // here are inert.
        let email_store: Option<Arc<crate::email::EmailAccountStore>> =
            match crate::email::EmailAccountStore::open(&auth_db_path) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    warn!("email_accounts store open failed (non-fatal): {e}");
                    None
                }
            };
        let email_quarantine: Option<Arc<crate::email::EmailQuarantineStore>> =
            match crate::email::EmailQuarantineStore::open(&auth_db_path) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    warn!("email_quarantine store open failed (non-fatal): {e}");
                    None
                }
            };
        let email_audit: Option<Arc<crate::email::EmailAuditStore>> =
            match crate::email::EmailAuditStore::open(&auth_db_path) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    warn!("email_audit store open failed (non-fatal): {e}");
                    None
                }
            };

        // NOTE: poller spawn moved below — needs `agent_core` which
        // is constructed further down. See "" comment.

        // pass the primary provider so sampling-opted-in rows can
        // fulfil server-initiated `sampling/createMessage`. The registry is
        // reload-capable: it holds the store + provider + tool-registry
        // handle so adding/editing/removing a server hot-reloads its tools
        // without a restart (the CRUD handlers call `reload`).
        let mcp_servers = Arc::new(match mcp_store.as_ref() {
            Some(s) => crate::mcp::McpServerRegistry::new(
                Arc::clone(s),
                Arc::clone(&provider),
                // Lets tool adapters save image results (e.g. browser
                // screenshots) so the UI renders them instead of the model
                // getting a base64 blob. Same artifacts/ dir /api/artifacts
                // serves from.
                crate::artifacts::ArtifactStore::new(&data_dir).ok(),
            ),
            None    => crate::mcp::McpServerRegistry::empty(),
        });

        // Built-in tools are registered above; MCP tools land via the
        // registry's reload (which calls `set_mcp_tools`) once the Arc
        // exists so startup and hot-reload share one path.
        let tools = Arc::new(tool_registry);
        mcp_servers.attach_tool_registry(Arc::clone(&tools));
        // Weak self-handle so background browser (Chrome) provisioning can
        // reconnect the Puppeteer server once the download finishes.
        mcp_servers.attach_self(Arc::downgrade(&mcp_servers));
        // Connect MCP servers in the BACKGROUND so the HTTP server comes up
        // immediately instead of blocking boot on N stdio handshakes (each a
        // process spawn + JSON-RPC init — ~1s apiece, slower on Windows). The
        // registry hot-swaps the tool surface via `set_mcp_tools` as servers
        // connect, so MCP tools simply attach a beat after the UI is live;
        // built-in tools are available from the first request. (`connect_state`
        // also connects the servers concurrently + with a per-server timeout.)
        {
            let reg = Arc::clone(&mcp_servers);
            tokio::spawn(async move { reg.reload().await; });
        }

        // Phase B slice 2 — hand the tool registry to the named-agent
        // resolver now that it exists, so `named:<handle>` workers can run
        // the tool-use loop. Set-once; ignore the (impossible) double-set.
        if let Some(cell) = named_agents_cell.as_ref() {
            let _ = cell.set(Arc::clone(&tools));
        }

        // ── Session store ────────────────────────────────────────────
        let sessions = Arc::new(SessionStore::new_with_config(
            config.session.max_turns,
            config.session.timeout_secs,
        ));
        let _session_cleanup = SessionStore::start_cleanup_task(
            Arc::clone(&sessions),
            config.session.cleanup_interval_secs,
        );
        info!("Session store initialised (cleanup every {}s)", config.session.cleanup_interval_secs);

        // ── AgentCore ────────────────────────────────────────────────
        let agent_core = Arc::new(AgentCore::new(
            Arc::clone(&config),
            provider,
            Arc::clone(&memory),
            Arc::clone(&tools),
            Arc::clone(&sessions),
        ));
        info!("AgentCore ready");

        // MIRA-Guardian: the built-in system watchdog is code-defined (immutable,
        // non-deletable). Log its definition fingerprint at boot so any drift
        // from the shipped spec is visible/auditable, and its operating mode.
        {
            use crate::agent::guardian;
            let gmode = guardian::mode(&config);
            info!(
                "MIRA-Guardian: mode={:?} definition=sha256:{}",
                gmode, guardian::fingerprint(),
            );
            // Fail-closed local-only verdict (§5) — only meaningful when enabled.
            if gmode != guardian::GuardianMode::Off {
                let chk = guardian::model_check(&config);
                if chk.allowed {
                    info!("MIRA-Guardian model: provider='{}' {:?} — {}",
                          chk.provider, chk.locality, chk.reason);
                } else {
                    warn!("MIRA-Guardian will NOT run (fail-closed): {}", chk.reason);
                }
            }
        }

        // Hand auth to AgentCore so the memory pre-hook can resolve the
        // caller's group memberships for visibility-scoped retrieval.
        if let Some(ref a) = auth_service {
            agent_core.set_auth(Arc::clone(a));
        }

        // Install the history store so a turn can rehydrate its in-memory
        // session from persisted conversation messages on a cache miss (after a
        // restart or 1-hour idle eviction). Callers opt in per turn by setting
        // `TurnContext.conversation_id`.
        if let Some(ref h) = history {
            agent_core.set_history(Arc::clone(h));
        }

        // Reasoning auto-routing (roadmap #13): build the routed-to provider
        // chain (headed by `agent.reasoning.provider`) and install it. Skipped
        // when disabled or no provider id is set — routing stays inert.
        if config.agent.reasoning.enabled && !config.agent.reasoning.provider.trim().is_empty() {
            let mut rc = (*config).clone();
            rc.primary_provider = config.agent.reasoning.provider.clone();
            match build_provider_chain(&rc) {
                Ok(rp) => {
                    info!("Reasoning auto-routing enabled → provider '{}'", config.agent.reasoning.provider);
                    agent_core.set_reasoning_provider(rp);
                }
                Err(e) => {
                    warn!(
                        "reasoning auto-routing: provider '{}' failed to build ({e}) — routing disabled",
                        config.agent.reasoning.provider
                    );
                    degradation_tracker.record(
                        "reasoning", "Reasoning auto-routing",
                        config.agent.reasoning.provider.as_str(), "default provider (routing off)",
                        &crate::health::degradation::DegradationTracker::short(&e.to_string()), true,
                    );
                }
            }
            // Optional cheap classifier for ambiguous turns (Slice C). Empty →
            // ambiguous turns are classified with the default provider.
            let clf_id = config.agent.reasoning.classifier_provider.trim();
            if !clf_id.is_empty() {
                let mut cc = (*config).clone();
                cc.primary_provider = clf_id.to_string();
                match build_provider_chain(&cc) {
                    Ok(cp) => {
                        info!("Reasoning classifier → provider '{}'", clf_id);
                        agent_core.set_classifier_provider(cp);
                    }
                    Err(e) => warn!(
                        "reasoning classifier: provider '{clf_id}' failed to build ({e}) — using default provider"
                    ),
                }
            }
        }

        // Hand the wiki registry (built earlier, before the tool registry)
        // to AgentCore so the pre/post wiki hooks can resolve per-user
        // wikis. The hook no-ops gracefully if this fails.
        if let Err(()) = agent_core.set_wiki(Arc::clone(&wiki_registry)) {
            warn!("wiki: registry already installed on AgentCore (unexpected)");
        }

        // Companion install the companion system onto
        // AgentCore so the chit-chat pre-hook + engagement post-hook
        // can resolve per-user state. No-ops when companion_system
        // failed to open earlier (the OnceLock stays empty).
        if let Some(sys) = &companion_system {
            if let Err(()) = agent_core.set_companion(Arc::clone(sys)) {
                warn!("companion: system already installed on AgentCore (unexpected)");
            }
        }

        // ── Email IMAP pollers (Q2 #8 E1+E3 chunk 4) ─────────────
        // Spawn one poller task per enabled email account. Sits here
        // (not at  where the store opens) because chunk 4's
        // dispatch path needs `agent_core` + `history` to actually
        // ── LiveConfig ────────────────────────────────────────────
        // Built before the email pollers so the OAuth XOAUTH2 path
        // (E4-2) can read `email_oauth.*_client_id` on every refresh.
        let live_config: Option<Arc<LiveConfig>> =
            Some(Arc::new(LiveConfig::new((*config).clone())));
        info!("LiveConfig initialised");

        // Hand the live handle to `settings_set` so admin global writes apply
        // live (validate → persist → broadcast) instead of needing a restart.
        if let Some(lc) = &live_config {
            let _ = settings_live_config.set(Arc::clone(lc));
            // Also hand it to AgentCore so hot-reloadable per-turn settings
            // (agent.tool_selection) pick up admin changes without a restart.
            agent_core.set_live_config(Arc::clone(lc));
        }

        // hand an accepted inbound to the agent loop. Per-account
        // spawn failures are logged + visible in the status snapshot.
        let email_pollers = Arc::new(match (
            email_store.as_ref(), history.as_ref(),
            email_quarantine.as_ref(), email_audit.as_ref(),
            live_config.as_ref(),
        ) {
            (Some(s), Some(h), Some(q), Some(a), Some(lc)) =>
                crate::email::EmailPollerRegistry::start_all(
                    Arc::clone(s),
                    Arc::clone(h),
                    Arc::clone(&agent_core),
                    Arc::clone(q),
                    Arc::clone(a),
                    Arc::clone(lc),
                ),
            _ => crate::email::EmailPollerRegistry::empty(),
        });

        // ── System email mailer (Q2 #8 E5) ───────────────────────
        // Builds AFTER the poller registry so it can share the
        // reply-loop cache. Always constructed — the mailer itself
        // refuses sends when `system_email.enabled = false`, so an
        // unconfigured install costs nothing.
        let system_mailer: Option<Arc<crate::email::SystemMailer>> =
            live_config.as_ref().map(|lc| {
                Arc::new(crate::email::SystemMailer::new(
                    Arc::clone(lc),
                    Arc::clone(&email_pollers.loop_cache),
                ))
            });

        // ── Q2 #10 K3: Chatterbox server supervisor ───────────────────────────
        // Only when the integration is enabled, supervision is requested, and
        // a binary path is configured. Spawns the spawn/health/restart loop as
        // a detached background task; the Arc is also injected into the router
        // so GET /api/system/chatterbox/status can report live state. When
        // supervision is off (e.g. WSL2 talking to a Windows-side Chatterbox)
        // this stays None and MIRA only talks to the URL.
        let chatterbox_supervisor: Option<Arc<crate::tts::chatterbox::ChatterboxSupervisor>> = {
            let cb = &config.tts.chatterbox;
            if cb.enabled && cb.supervise && !cb.binary_path.is_empty() {
                let sup = Arc::new(crate::tts::chatterbox::ChatterboxSupervisor::new(
                    crate::config::expand_path(&cb.binary_path),
                    cb.port,
                    cb.extra_args.clone(),
                ));
                let task = Arc::clone(&sup);
                tokio::spawn(async move { task.run().await; });
                info!("chatterbox: supervisor started (binary={}, port={})",
                    cb.binary_path, cb.port);
                Some(sup)
            } else {
                None
            }
        };

        // ── Channel account store ─────────────────────────────────
        // Lives in auth.db alongside users so the FK cascades on delete.
        let channel_accounts: Option<Arc<ChannelAccountStore>> =
            match ChannelAccountStore::open(&auth_db_path) {
                Ok(s)  => { info!("Channel account store initialised"); Some(Arc::new(s)) }
                Err(e) => { warn!("Channel account store failed (non-fatal): {}", e); None }
            };

        // ── Legacy config migration (one-shot) ────────────────────
        // Seeds `channel_accounts` from the old `[channels.signal]` /
        // `[channels.telegram]` TOML blocks if the store is empty, and
        // re-stamps any conversations that were stored under the pre-refactor
        // `"local-user"` fallback onto the real admin id. No-op on fresh
        // installs or deployments that already use the per-user API.
        if let (Some(store), Some(auth)) = (channel_accounts.as_ref(), auth_service.as_ref()) {
            match auth.current_admin_user_id() {
                Ok(Some(admin_id)) => {
                    if let Err(e) = crate::channel_accounts::migrate_if_empty(
                        store, history.as_ref(), &config, &admin_id,
                    ) {
                        warn!("Legacy channel migration failed (non-fatal): {}", e);
                    }
                }
                Ok(None) => warn!("No admin user yet — skipping legacy channel migration"),
                Err(e)   => warn!("Could not resolve admin user for migration: {}", e),
            }
        }

        // ── Security ─────────────────────────────────────────────────
        let security = SecurityConfig::from_mira_config(&config);
        if config.server.enabled {
            match (security.auth_token.is_some(), auth_service.is_some()) {
                (false, false) => warn!(
                    "Security: no auth_token configured and no JWT auth service — \
                     server API is open (dev mode)"
                ),
                (true,  false) => info!("Security: static auth_token mode (JWT auth service not wired)"),
                (false, true)  => info!("Security: JWT-only auth (no static auth_token)"),
                (true,  true)  => info!("Security: dual-mode auth — static token OR JWT accepted"),
            }
        }

        // ── Notification bus ───────────────────────────────────────
        let notification_bus = Arc::new(NotificationBus::new());
        // Now that the bus exists, attach it to the degradation tracker so
        // per-request fallbacks (TTS/STT) also fire a notification toast. Any
        // startup fallbacks already recorded above are picked up by the
        // `subsystem.degraded` health detector.
        degradation_tracker.attach_bus(Arc::clone(&notification_bus));
        // Wire the bus into the companion system so the safety floor
        // can wake the contact's web tab when a notice is delivered.
        if let Some(sys) = &companion_system {
            let _ = sys.set_notifications(Arc::clone(&notification_bus));
        }
        init_start_time();

        // ── nginx proxy (non-fatal) ─────────────────────────────────
        let proxy = if config.proxy.enabled {
            let log_dir = data_dir.join("logs");
            let p = NginxProxy::new(config.proxy.clone(), config.server.port, log_dir);
            match p.start_or_reload().await {
                Ok(()) => { info!("nginx proxy started"); Some(p) }
                Err(e) => { warn!("nginx proxy failed (non-fatal): {}", e); None }
            }
        } else {
            None
        };

        // ── Channel startup — per-user fan-out ──────────────────────
        // Each enabled `ChannelAccount` row gets its own signal-cli daemon +
        // listener (for Signal) or an entry in the telegram lookup table
        // (for Telegram). One misbehaving account doesn't block the others.
        // Build STT service once and share with the channel listeners that
        // need to transcribe inbound voice notes (Signal today, Telegram
        // next). The HTTP router constructs its own clone for the
        // `/api/stt/*` endpoints — both share the same on-disk config so
        // backend selection stays consistent.
        let stt_service = SttService::from_config(&config)
            .with_degradations(Arc::clone(&degradation_tracker));
        let tts_service = TtsService::from_config(&config)
            .with_degradations(Arc::clone(&degradation_tracker));

        let mut channel_manager = ChannelManager::new();
        // R1+R2 — open the identity + link-code stores against the same
        // auth.db every other store uses. Failures here drop the
        // self-serve link surface to 500 but don't block startup.
        let identity_store: Option<Arc<crate::channel_identity::IdentityStore>> =
            crate::channel_identity::IdentityStore::open(&auth_db_path)
                .map_err(|e| warn!("identity store open: {}", e))
                .ok().map(Arc::new);
        let link_code_store: Option<Arc<crate::channel_identity::LinkCodeStore>> =
            crate::channel_identity::LinkCodeStore::open(&auth_db_path)
                .map_err(|e| warn!("link-code store open: {}", e))
                .ok().map(Arc::new);
        if let Some(ref store) = channel_accounts {
            channel_manager.start_all(
                Arc::clone(store),
                Arc::clone(&agent_core),
                history.clone(),
                auth_service.clone(),
                Some(stt_service.clone()),
                Some(tts_service.clone()),
                live_config.as_ref().map(Arc::clone),
                Some(Arc::clone(&mcp_servers)),
                identity_store.clone(),
                link_code_store.clone(),
            ).await;
        } else {
            warn!("No channel_accounts store — skipping channel fan-out");
        }

        // Snapshot the telegram lookup table for the router. Cheap to clone
        // (Arc<HashMap>). If new accounts are added at runtime we reload via
        // the /api/admin/restart endpoint.
        let telegram_accounts = Arc::new(channel_manager.telegram.clone());
        let whatsapp_accounts = Arc::new(channel_manager.whatsapp.clone());
        let slack_accounts    = Arc::new(channel_manager.slack.clone());
        let external_accounts = Arc::new(channel_manager.external.clone());

        // Wrap the manager in Arc<RwLock<>> so per-account lifecycle
        // endpoints (start / stop / restart Signal daemons) can take a
        // brief write lock from inside the HTTP handlers. The gateway
        // shutdown closure also takes a write lock to stop everything
        // on SIGTERM.
        let channel_manager = Arc::new(tokio::sync::RwLock::new(channel_manager));
        // Fill the deferred handle so guardian_decide (P4b) can restart bridges.
        let _ = guardian_channel_manager.set(Arc::clone(&channel_manager));

        // ──  (Slices 1–3): Automations subsystem ──────────────────────
        // Built before MiraServer so the HTTP handlers can pull the store +
        // worker via Extension layers. Failure is non-fatal — the gateway
        // still serves chat/etc; only the schedule routes return 500.
        // (event_bus was constructed earlier alongside the supervisor.)
        if let Err(e) = agent_core.set_event_bus(Arc::clone(&event_bus)) {
            warn!("AgentCore event bus already installed: {e:?}");
        }

        // MIRA-Guardian (P3) — proactive watch loop. Self-contained 15-min (cfg)
        // background task: on a *new* non-green health snapshot it runs a Guardian
        // turn and alerts via the NotificationBus (web/push) + the watchdog.alert
        // event rail. No-op while guardian.mode=off. Recipient reuses the
        // watchdog's notify_user_id.
        if crate::agent::guardian::mode(&config) != crate::agent::guardian::GuardianMode::Off {
            if let Some(ref hs) = health_store_arc {
                let _ = crate::agent::guardian::spawn_watch_loop(
                    Arc::clone(&agent_core),
                    Arc::clone(hs),
                    Arc::clone(&notification_bus),
                    Some(Arc::clone(&event_bus)),
                    Arc::clone(&config),
                    config.automations.watchdog.notify_user_id.clone(),
                    guardian_action_store.clone(),
                    agent_audit.clone(),
                    automations_store.clone(),
                    Some(Arc::clone(&channel_manager)),
                );
            } else {
                warn!("MIRA-Guardian enabled but no health store available — watch loop not started");
            }
        }

        // ── WSL host-URL misrouting check (startup, one-shot) ───────────────
        // On WSL2 NAT, service URLs pointed at the Windows host's LAN IP are
        // unreachable from the guest; if any are (and `windows-host` would fix
        // them), notify the operator so they can one-click fix from Settings.
        // Spawned + probes on a blocking thread — never blocks startup.
        if crate::wsl_net::is_wsl() {
            let cfg = Arc::clone(&config);
            let bus = Arc::clone(&notification_bus);
            tokio::spawn(async move {
                let findings = tokio::task::spawn_blocking(move || crate::wsl_net::scan_misrouted(&cfg))
                    .await.unwrap_or_default();
                if !findings.is_empty() {
                    let list = findings.iter().map(|f| f.path.as_str()).collect::<Vec<_>>().join(", ");
                    warn!("WSL: {} service URL(s) point at an unreachable Windows-host address \
                           (windows-host would work): {list}. Fix in Settings.", findings.len());
                    bus.send(crate::notifications::Notification {
                        kind:            crate::notifications::NotificationKind::SystemDegraded,
                        conversation_id: None,
                        channel:         Some("web".to_string()),
                        user_id:         None,
                        message:         Some(format!(
                            "{} service URL(s) can't reach the Windows host from WSL ({list}). \
                             Open Settings to switch them to windows-host (one click).",
                            findings.len())),
                        category:        None,
                    });
                }
            });
        }

        // ── Companion proactive check-in scheduler ────────────────
        // Spawned after AgentCore + history are ready (the scheduler
        // needs both to dispatch a check-in). Only wired when the
        // companion system AND history store both exist.
        // Held on the returned `Gateway` (see field doc) — must NOT be a
        // bare local here, or its `Drop` aborts the scheduler task on
        // `build()` return and silently breaks morning briefings/check-ins.
        let companion_scheduler: Option<crate::companion::scheduler::CompanionScheduler> =
            match (companion_system.as_ref(), history.as_ref()) {
                (Some(sys), Some(hist)) => {
                    let dispatcher_inner = crate::companion::dispatcher::CompanionDispatcher::new(
                        Arc::clone(&agent_core),
                        Arc::clone(hist),
                        sys.store_arc(),
                    )
                    .with_notifications(Arc::clone(&notification_bus))
                    // Outbound Signal bridge — without this a user
                    // whose preferred_channels=["signal"] would only
                    // see check-ins in their web history. Mirrors the
                    // automations dispatcher's signal wiring.
                    .with_signal(
                        auth_service.as_ref().map(Arc::clone),
                        Some(config.channels.signal.rest_port),
                        config.channels.signal.phone_number.clone(),
                    )
                    // Outbound Telegram bridge — the dispatcher reads
                    // the recipient's bot_token from channel_accounts
                    // and derives their chat_id from the most-recent
                    // inbound conversation. `None` keeps the
                    // history-only fallback for users without a
                    // channel-accounts row.
                    .with_telegram(channel_accounts.as_ref().map(Arc::clone))
                    // E2 — outbound email bridge. The dispatcher
                    // sends FROM the user's first enabled email
                    // account TO their `users.email`. Sharing the
                    // poller's reply-loop cache so companion sends
                    // can't bypass the same-body guard.
                    .with_email(
                        email_store.as_ref().map(Arc::clone),
                        Some(Arc::clone(&email_pollers.loop_cache)),
                    )
                    // Q1.6 — Daily Briefing snapshot sources. Each
                    // optional; a user with only a wiki still gets a
                    // meaningful (if narrower) briefing.
                    .with_briefing_sources(
                        calendar_store.clone(),
                        automations_store.clone(),
                        Some(Arc::clone(&wiki_registry)),
                    )
                    .with_live_config(live_config.as_ref().map(Arc::clone))
                    // Agent activity log — lets "status update" check-ins
                    // narrate MIRA's recent autonomous work for the user.
                    // Same store the supervisor records into (with_audit_store).
                    .with_agent_audit(agent_audit.clone())
                    // TTS so proactive check-ins/briefings honour the
                    // owner's per-channel "voice: always" preference and
                    // go out as voice notes, matching normal replies.
                    .with_tts(Some(tts_service.clone()));
                    // Wrap in Arc so the HTTP "send briefing now"
                    // endpoint can hold its own ref (CompanionDispatcher
                    // is Clone, but every clone re-allocates the
                    // Option<Arc> internals; cleaner to share one Arc).
                    let dispatcher = Arc::new(dispatcher_inner);
                    // build the safety floor so the
                    // scheduler can escalate missed check-ins. The
                    // engagement post-hook builds its own copy
                    // inside AgentCore.
                    let safety = crate::companion::safety::SafetyFloor {
                        log:           sys.safety_log_arc(),
                        store:         sys.store_arc(),
                        history:       Some(Arc::clone(hist)),
                        auth:          auth_service.as_ref().map(Arc::clone),
                        notifications: Some(Arc::clone(&notification_bus)),
                        groups:        Some(sys.groups_arc()),
                    };
                    let scheduler = crate::companion::scheduler::CompanionScheduler::spawn(
                        sys.store_arc(),
                        (*dispatcher).clone(),
                        auth_service.as_ref().map(Arc::clone),
                        Arc::clone(hist),
                        Some(sys.engagement_arc()),
                        Some(safety),
                        config.companion.max_unanswered_checkins,
                        config.companion.max_per_day,
                        config.companion.min_gap_minutes,
                    );
                    info!("Companion scheduler running (tick = {}s)",
                          crate::companion::scheduler::TICK_INTERVAL_SECS);
                    // Stash the Arc<Dispatcher> on the AgentCore so the
                    // HTTP send-briefing-now endpoint can reach it.
                    if let Err(()) = agent_core.set_companion_dispatcher(dispatcher) {
                        warn!("companion: dispatcher already installed on AgentCore (unexpected)");
                    }
                    Some(scheduler)
                }
                (Some(_), None) => {
                    warn!("Companion scheduler skipped — history store unavailable");
                    None
                }
                _ => None,
            };

        // 0.107.0 — HealthStore handle, populated when the automations
        // store is wired (it depends on it). Threaded into the router
        // so the dashboard endpoints can read snapshots + per-signal
        // config rows.
        // 0.110.0 — always thread the HealthStore handle into the
        // router; the dashboard endpoints depend on it. Independent
        // of whether the automations heartbeat registers (slice 3+).
        let health_store_for_router: Option<Arc<crate::health::store::HealthStore>> =
            health_store_arc.clone();

        let (automations_worker_arc, automations_worker_handle, event_subscriber_handle) =
            match automations_store.as_ref() {
                Some(store) => {
                    // Slice W1 — Watchdog. Registered conditionally so a
                    // disabled (default) config doesn't add an idle handler.
                    // The seeded schedule + auto-route subscription are
                    // also gated on `enabled` (see seed_watchdog_*).
                    let mut heartbeats =
                        crate::automations::heartbeats::HeartbeatRegistry::with_watchdog(
                            config.automations.watchdog.clone(),
                            data_dir.clone(),
                            config.log_file_path(),
                            Some(Arc::clone(store)),
                        );
                    if let Err(e) = crate::automations::heartbeats::seed_watchdog_schedule(
                        store, &config.automations.watchdog,
                    ) {
                        warn!("watchdog schedule seed failed: {e}");
                    }
                    if let Err(e) = crate::automations::heartbeats::seed_watchdog_subscription(
                        store, &config.automations.watchdog,
                    ) {
                        warn!("watchdog subscription seed failed: {e}");
                    }

                    // 0.105.0 + 0.106.0 + 0.107.0 — self-monitoring.
                    // Always registered alongside the watchdog so the
                    // seeded `heartbeat.system_audit` schedule resolves
                    // to a real handler. Files watchdog incidents through
                    // the same pipeline; routing depends on
                    // `automations.watchdog.notify_user_id` (no separate knob).
                    // The Arc is held across the function so the dashboard
                    // HTTP handlers can also access it via Extension.
                    // 0.110.0 — HealthStore was opened upfront so the
                    // Supervisor could also hold it. Reuse the Arc.
                    if let Some(hs_arc) = health_store_arc.as_ref() {
                        let notify = config.automations.watchdog.notify_user_id.clone()
                            .filter(|s| !s.is_empty());
                        heartbeats.register_system_audit(
                            Arc::clone(hs_arc),
                            Arc::clone(store),
                            agent_audit.clone(),
                            Some(Arc::clone(&agent_registry)),
                            auth_service.as_ref().map(|s| s.db_arc()),
                            Some(config.log_file_path()),
                            Some(Arc::clone(&channel_manager)),
                            secrets_store.clone(),
                            channel_accounts.as_ref().map(Arc::clone),
                            Some(Arc::clone(&degradation_tracker)),
                            config.memory.embedding.provider.clone(),
                            notify.clone(),
                        );
                        info!("system_audit heartbeat registered");
                        heartbeats.register_weekly_digest(
                            Arc::clone(hs_arc),
                            Arc::clone(store),
                            notify,
                        );
                        info!("health_weekly_digest heartbeat registered");
                    } else {
                        warn!("system_audit disabled — HealthStore not available");
                    }
                    // (health_store_for_router is set unconditionally above.)

                    // Recover any system schedule orphaned by a crash/restart
                    // mid-run: claim_due nulls next_run_at while a job runs, and
                    // a death before completion leaves it stuck NULL → the job
                    // goes dormant (this is why the hourly system_audit had been
                    // silent for days). Recompute next_run_at for those rows.
                    match store.requeue_orphaned_system_schedules(chrono::Utc::now().timestamp()) {
                        Ok(0) => {}
                        Ok(n) => info!("recovered {n} orphaned system schedule(s) (NULL next_run_at)"),
                        Err(e) => warn!("orphaned-schedule recovery failed: {e}"),
                    }

                    let heartbeats = Arc::new(heartbeats);
                    let ctx = Arc::new(crate::automations::heartbeats::HeartbeatContext {
                        data_dir:  data_dir.clone(),
                        event_bus: Some(Arc::clone(&event_bus)),
                    });
                    let rate_limiter = Arc::new(
                        crate::automations::ChannelRateLimiter::new(
                            config.automations.channel_rate_limits.clone(),
                        ),
                    );
                    let dispatcher = Arc::new(crate::automations::Dispatcher {
                        heartbeats,
                        ctx,
                        store: Arc::clone(store),
                        agent:         Some(Arc::clone(&agent_core)),
                        history:       history.clone(),
                        notifications: Some(Arc::clone(&notification_bus)),
                        max_chain_depth: config.automations.max_chain_depth,
                        rate_limiter:    Some(rate_limiter),
                        // Outbound bridge — Signal delivery for `Action::Prompt`
                        // and `Action::ChannelMessage` with channel=signal. The
                        // dispatcher resolves the recipient via `users.phone` at
                        // fire time, so unconfigured users just get a warning.
                        // `tts` is shared with the SSE listener so voice prefs
                        // and routing stay consistent across inbound replies
                        // and outbound automations.
                        auth:               auth_service.clone(),
                        signal_port:        Some(config.channels.signal.rest_port),
                        signal_bot_number:  config.channels.signal.phone_number.clone()
                            .filter(|s| !s.is_empty()),
                        channel_accounts:   channel_accounts.as_ref().map(Arc::clone),
                        email_accounts:     email_store.as_ref().map(Arc::clone),
                        email_loop_cache:   Some(Arc::clone(&email_pollers.loop_cache)),
                        tts:                Some(tts_service.clone()),
                        live_config:        live_config.as_ref().map(Arc::clone),
                    });
                    let worker = Arc::new(crate::automations::Worker::new(
                        Arc::clone(store),
                        dispatcher,
                    ));
                    let worker_handle = Arc::clone(&worker).spawn();
                    info!("Automations worker started");

                    // Spawn the event subscriber loop. It listens on the bus
                    // and dispatches matching event_subscriptions through the
                    // same worker → dispatcher pipeline.
                    let sub_handle = crate::events::subscriber::spawn(
                        Arc::clone(&event_bus),
                        Arc::clone(store),
                        Arc::clone(&worker),
                    );
                    info!("Event subscriber started");

                    // Orphan completion sweep. `spawn_background_task`
                    // registers an `agent.worker.completed` subscription
                    // per task; if the supervisor was killed mid-flight
                    // (service restart, crash) the worker never emits
                    // its terminal event and the subscription sits
                    // `active` with `last_fired_at = NULL` forever. Walk
                    // those rows once at startup, deliver a one-shot
                    // "abandoned" notification through the dispatcher,
                    // then mark each row `failed` so it's a no-op next
                    // time. Best-effort: we log and continue on any error
                    // because the rest of the gateway must come up either
                    // way. Spawned async so the dispatcher's own work
                    // (including the channel_message side effects) runs
                    // off the build path.
                    {
                        let store    = Arc::clone(store);
                        let worker_c = Arc::clone(&worker);
                        tokio::spawn(async move {
                            sweep_orphan_completion_subs(&store, &worker_c).await;
                        });
                    }

                    (Some(worker), Some(worker_handle), Some(sub_handle))
                }
                None => (None, None, None),
            };

        // ── Q1.7: waitlist store ─────────────────────────────────────────────
        // Public POST endpoint persists landing-page signups; admin-only
        // read + export. None on open failure — handler returns 503.
        let waitlist_store: Option<Arc<crate::waitlist::WaitlistStore>> =
            match crate::waitlist::WaitlistStore::open(&data_dir.join("waitlist.db")) {
                Ok(s) => {
                    info!("Waitlist store initialised at {}", data_dir.join("waitlist.db").display());
                    Some(Arc::new(s))
                }
                Err(e) => {
                    warn!("Waitlist store failed to open (non-fatal): {e}");
                    None
                }
            };

        // ── Q1.2: Web Push (VAPID) service ────────────────────────────────────
        // Opens the per-data-dir VAPID keypair + subscriptions store and
        // wires the bus forwarder so companion check-ins and inbound
        // messages reach registered browsers/phones. `None` on failure —
        // the HTTP endpoints will 503 but the rest of the server is
        // unaffected.
        // FCM transport (opt-in, mobile app). Misconfiguration while enabled
        // is non-fatal: log and fall back to web-push-only rather than block
        // boot. `None` when disabled.
        let fcm = match crate::notifications::fcm::FcmService::open(&config.notifications.fcm) {
            Ok(Some(svc)) => { info!("FCM transport initialised (project {})", config.notifications.fcm.project_id.as_deref().unwrap_or("from-service-account")); Some(svc) }
            Ok(None)      => None,
            Err(e)        => { warn!("FCM transport failed to open (non-fatal — web push still active): {e}"); None }
        };
        let web_push: Option<Arc<crate::notifications::web_push::WebPushService>> =
            match crate::notifications::web_push::WebPushService::open(
                &data_dir,
                &crate::notifications::web_push::service_path(&data_dir),
                fcm,
            ) {
                Ok(svc) => {
                    info!("Web Push service initialised (VAPID at {})", data_dir.display());
                    crate::notifications::web_push::spawn_bus_forwarder(
                        Arc::clone(&notification_bus), svc.clone(),
                    );
                    Some(Arc::new(svc))
                }
                Err(e) => {
                    warn!("Web Push service failed to open (non-fatal): {e}");
                    None
                }
            };

        // ── Central Server ───────────────────────────────────────────
        // Built after  so the telegram account lookup can be injected,
        // and after the automations subsystem so the schedule routes have
        // their store + worker. `restart_notify` was created up by the tool
        // registry so the `backup_restore` agent tool shares the same Arc
        // the admin /restart handler triggers.
        let server = MiraServer::new(
            Arc::clone(&agent_core),
            security,
            &config,
            auth_service.clone(),
            history.clone(),
            live_config.clone(),
            Arc::clone(&notification_bus),
            Arc::clone(&telegram_accounts),
            Arc::clone(&whatsapp_accounts),
            Arc::clone(&slack_accounts),
            Arc::clone(&external_accounts),
            channel_accounts.clone(),
            // R1+R2 — same Arcs the ChannelManager.start_all received above.
            identity_store.clone(),
            link_code_store.clone(),
            Some(Arc::clone(&channel_manager)),
            tool_audit.clone(),
            calendar_store.clone(),
            automations_store.clone(),
            automations_worker_arc.clone(),
            Arc::clone(&event_bus),
            Arc::clone(&agent_registry),
            Arc::clone(&supervisor),
            admin_policy_rules.clone(),
            secrets_store.clone(),
            health_store_for_router.clone(),
            task_artifacts_arc.clone(),
            web_push.clone(),
            waitlist_store.clone(),
            Arc::clone(&mcp_servers),
            mcp_store.clone(),
            mcp_catalog.clone(),
            email_store.clone(),
            Arc::clone(&email_pollers),
            email_quarantine.clone(),
            email_audit.clone(),
            system_mailer.clone(),
            chatterbox_supervisor.clone(),
            Some(Arc::clone(&degradation_tracker)),
            guardian_action_store.clone(),
            agent_audit.clone(),
            Arc::clone(&restart_notify),
        );
        info!("Central Server configured on {}:{}", config.server.host, config.server.port);

        // ── Calendar sync engine ───────────────────────────────────
        // Fires a periodic pull when `calendar.sync_provider != "none"`. The
        // returned handle is kept as part of the Gateway so it drops on
        // shutdown; the engine aborts its own task on drop.
        let calendar_sync = match (calendar_store.as_ref(), auth_service.as_ref()) {
            (Some(store), Some(auth)) => Some(SyncEngine::start(
                Arc::clone(&config),
                Arc::clone(store),
                Arc::clone(auth),
            )),
            _ => None,
        };

        // Scheduled backups (off by default). Held on Gateway — same
        // lifetime-on-Gateway rule as the companion scheduler (whose
        // bare-local lifetime bug was the 0.189.1 fix). Background loop
        // skipped at the gateway level when the flag is off, so an
        // operator who never enables it pays nothing.
        let backup_scheduler = if config.backup.scheduled_enabled {
            let data_dir = crate::config::expand_path(&config.data_dir);
            let s = crate::install::backup_scheduler::BackupScheduler::spawn(
                std::path::PathBuf::from(data_dir),
                config.config_path.clone(),
                crate::install::backup_scheduler::ScheduledBackupConfig {
                    interval_secs:   config.backup.scheduled_interval_secs,
                    retention_count: config.backup.scheduled_retention_count,
                },
            );
            info!("Scheduled backups running (every {}s, retention={})",
                config.backup.scheduled_interval_secs,
                config.backup.scheduled_retention_count);
            Some(s)
        } else {
            None
        };

        info!("Gateway startup complete");

        Ok(super::Gateway {
            config,
            agent_core,
            agent_registry,
            supervisor,
            auth_service,
            history,
            live_config,
            notification_bus,
            channel_accounts,
            server,
            proxy,
            channel_manager,
            _session_cleanup,
            _calendar_sync: calendar_sync,
            _automations_worker: automations_worker_handle,
            _event_subscriber: event_subscriber_handle,
            _companion_scheduler: companion_scheduler,
            _backup_scheduler: backup_scheduler,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────

// Return the JWT secret from config, generating and saving one if absent.
fn ensure_jwt_secret(config: &MiraConfig) -> String {
    if let Some(ref s) = config.security.jwt_secret {
        if !s.is_empty() {
            return s.clone();
        }
    }
    // Generate a random 32-byte hex secret.
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let secret = hex::encode(bytes);

    // Attempt to persist it so it survives restarts.
    let mut updated = (*config).clone();
    updated.security.jwt_secret = Some(secret.clone());
    if let Err(e) = updated.save() {
        warn!("Could not persist jwt_secret to config (non-fatal): {}", e);
    } else {
        info!("Generated and saved jwt_secret to config");
    }
    secret
}

// ─────────────────────────────────────────────────────────────────────────────

// Probe an OpenAI-compatible `/embeddings` endpoint with a short timeout.
// Returns `true` if the server responds with any non-5xx status (including
// 400/422 for a malformed payload — the server is up and talking).
async fn probe_embedding_endpoint(base_url: &str) -> bool {
    if base_url.is_empty() {
        return false;
    }
    let url = format!("{}/embeddings", base_url.trim_end_matches('/'));
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    else {
        return false;
    };
    match client
        .post(&url)
        .json(&serde_json::json!({"model": "probe", "input": "ping"}))
        .send()
        .await
    {
        Ok(r)  => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn build_provider_chain(
    config: &MiraConfig,
) -> Result<Arc<dyn crate::providers::ModelProvider>, MiraError> {
    use crate::providers::{
        failover::FailoverProvider,
        lmstudio::LmStudioProvider,
        local::OllamaProvider,
        openrouter::OpenRouterProvider,
        openai_compat::{AuthHeader, OpenAiCompatClient, OpenAiCompatConfig},
        anthropic::AnthropicProvider,
        gemini::GeminiProvider,
    };

    // (slug, boxed provider) — `config.primary_provider` is matched
    // against the slug to decide which one heads the chain; everything
    // else falls in after as failover in registration order.
    let mut providers: Vec<(&'static str, Box<dyn crate::providers::ModelProvider>)> = Vec::new();

    // LM Studio is registered when `enabled` (default true). Has no
    // api_key gate — the URL is the connection contract.
    if config.providers.lmstudio.enabled {
        let url   = config.providers.lmstudio.url.clone();
        let model = config.providers.lmstudio.default_model.clone();
        providers.push(("lmstudio", Box::new(
            LmStudioProvider::new(url, model)
                .with_token_caps(
                    config.agent.max_tool_round_tokens,
                    config.agent.max_response_tokens,
                )
        )));
        info!("Provider: LM Studio registered");
    } else {
        info!("Provider: LM Studio skipped (enabled=false)");
    }

    // Ollama — same shape as LM Studio: keyless, URL is the contract.
    // The OllamaProvider uses its own /api/chat endpoint (not OpenAI's
    // /v1/chat/completions), so it has its own client.
    if config.providers.ollama.enabled {
        let url   = config.providers.ollama.url.clone();
        let model = config.providers.ollama.default_model.clone();
        providers.push(("ollama", Box::new(OllamaProvider::new(url, model))));
        info!("Provider: Ollama registered (model={})", config.providers.ollama.default_model);
    } else {
        info!("Provider: Ollama skipped (enabled=false)");
    }

    if config.providers.openrouter.enabled {
        if let Some(ref key) = config.providers.openrouter.api_key {
            if !key.is_empty() {
                let model = config.providers.openrouter.default_model.clone();
                info!("Provider: OpenRouter registered (model={model})");
                providers.push(("openrouter", Box::new(OpenRouterProvider::new(key.clone(), model))));
            }
        }
    }

    // ── OpenAI-compatible cloud providers ────────────────────────────
    // Each is registered iff `enabled` AND its api_key is set, so
    // adding empty config blocks for them in the JSON has no runtime
    // cost. The shared OpenAiCompatClient handles
    // `/v1/chat/completions` for all of them.
    macro_rules! register_openai_compat {
        ($slug:expr, $cfg:expr) => {
            if !$cfg.enabled {
                info!("Provider: {} skipped (enabled=false)", $slug);
            } else if let Some(ref key) = $cfg.api_key {
                if !key.is_empty() {
                    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                        provider_name: $slug.into(),
                        base_url:      $cfg.base_url.clone(),
                        api_key:       key.clone(),
                        model:         $cfg.default_model.clone(),
                        timeout_secs:  $cfg.timeout_secs,
                        auth_header:   AuthHeader::Bearer,
                        extra_headers: vec![],
                    });
                    providers.push(($slug, Box::new(client)));
                    info!("Provider: {} registered (model={})", $slug, $cfg.default_model);
                }
            }
        }
    }
    register_openai_compat!("openai",   config.providers.openai);
    register_openai_compat!("deepseek", config.providers.deepseek);
    register_openai_compat!("moonshot", config.providers.moonshot);
    register_openai_compat!("groq",     config.providers.groq);
    register_openai_compat!("xai",      config.providers.xai);

    // Anthropic (native /v1/messages — separate client because the
    // wire format isn't OpenAI-shaped).
    {
        let ac = &config.providers.anthropic;
        if !ac.enabled {
            info!("Provider: anthropic skipped (enabled=false)");
        } else if let Some(ref key) = ac.api_key {
            if !key.is_empty() {
                let client = AnthropicProvider::new(
                    key.clone(),
                    ac.default_model.clone(),
                    ac.base_url.clone(),
                    ac.timeout_secs,
                );
                providers.push(("anthropic", Box::new(client)));
                info!("Provider: anthropic registered (model={})", ac.default_model);
            }
        }
    }

    // Google Gemini (native :generateContent — also not OpenAI-shaped).
    {
        let gc = &config.providers.gemini;
        if !gc.enabled {
            info!("Provider: gemini skipped (enabled=false)");
        } else if let Some(ref key) = gc.api_key {
            if !key.is_empty() {
                let client = GeminiProvider::new(
                    key.clone(),
                    gc.default_model.clone(),
                    gc.base_url.clone(),
                    gc.timeout_secs,
                );
                providers.push(("gemini", Box::new(client)));
                info!("Provider: gemini registered (model={})", gc.default_model);
            }
        }
    }

    // Catch-all OpenAI-compat block — registered when `enabled` AND
    // the user picks a non-empty `name`. `auth_style = "none"` lets
    // unsecured local gateways (vLLM open install) register without
    // an api_key.
    {
        let cc = &config.providers.openai_compat;
        if !cc.enabled {
            if !cc.name.is_empty() {
                info!("Provider: openai_compat '{}' skipped (enabled=false)", cc.name);
            }
        } else if !cc.name.is_empty() {
            let auth = match cc.auth_style.to_ascii_lowercase().as_str() {
                "azure" | "azure_openai" | "api-key" | "api_key" => AuthHeader::AzureApiKey,
                "none" | "anonymous"                              => AuthHeader::None,
                _                                                 => AuthHeader::Bearer,
            };
            let key  = cc.api_key.clone().unwrap_or_default();
            let ok   = match auth {
                AuthHeader::None => true,
                _ => !key.is_empty(),
            };
            if ok && !cc.base_url.is_empty() && !cc.default_model.is_empty() {
                // SAFETY: ProviderConfig.name lives as long as the gateway
                // (until restart), but provider_name in OpenAiCompatConfig
                // is owned, so we clone it in. The macro path used a
                // &'static str slug; here we accept a runtime string.
                let client = OpenAiCompatClient::new(OpenAiCompatConfig {
                    provider_name: cc.name.clone(),
                    base_url:      cc.base_url.clone(),
                    api_key:       key,
                    model:         cc.default_model.clone(),
                    timeout_secs:  cc.timeout_secs,
                    auth_header:   auth,
                    extra_headers: vec![],
                });
                providers.push(("openai_compat", Box::new(client)));
                info!("Provider: openai_compat '{}' registered (url={}, model={})",
                      cc.name, cc.base_url, cc.default_model);
            } else {
                warn!(
                    "Provider openai_compat '{}' skipped — incomplete config \
                     (base_url={:?}, model={:?}, key_set={}, auth_style={:?})",
                    cc.name, cc.base_url, cc.default_model, !key.is_empty(), cc.auth_style,
                );
            }
        }
    }

    if providers.is_empty() {
        return Err(MiraError::ProviderError(
            "No providers configured — set at least one provider's api_key or URL".into()
        ));
    }

    // Honour `primary_provider` from config: pull that slug to the front
    // of the chain if it's registered. Otherwise keep LM Studio first
    // (the historical behaviour).
    let primary_slug = config.primary_provider.as_str();
    if let Some(idx) = providers.iter().position(|(slug, _)| *slug == primary_slug) {
        let entry = providers.remove(idx);
        providers.insert(0, entry);
    }

    if providers.len() == 1 {
        let (_, p) = providers.remove(0);
        return Ok(Arc::from(p));
    }

    let mut iter = providers.into_iter().map(|(_, p)| p);
    let primary = iter.next().unwrap();
    let mut chain = FailoverProvider::new(primary, vec![]);
    for fallback in iter {
        chain = chain.with_fallback(fallback);
    }
    Ok(Arc::new(chain))
}

fn build_tool_registry(
    config:      &MiraConfig,
    auth:        Option<&Arc<LocalAuthService>>,
    history:     Option<&Arc<HistoryStore>>,
    memory:      &Arc<crate::memory::MemorySystem>,
    provider:    &Arc<dyn ModelProvider>,
    data_dir:    &std::path::Path,
    audit:       Option<Arc<ToolAuditStore>>,
    calendar:    Option<Arc<CalendarStore>>,
    automations: Option<Arc<AutomationsStore>>,
    // when present, attached to the shared HttpPolicy so
    // network-tier tools (web_fetch, url_preview, web_search) consult
    // the engine via NetworkEgress events. None = no engine wiring
    // (legacy behaviour, useful for dev / minimal builds).
    policy_engine: Option<Arc<dyn crate::policy::PolicyEngine>>,
    // Shared HTTP policy + search-backend chain (built by the gateway up
    // top so the skill resolver can reuse the same instances).
    http_policy:     Arc<HttpPolicy>,
    search_backends: Vec<Arc<dyn SearchBackend>>,
    // Multi-agent runtime, used by `spawn_background_task` /
    // `get_task_result`.
    supervisor:      Arc<crate::agent::Supervisor>,
    agent_registry:  Arc<crate::agent::AgentRegistry>,
    // 0.111.0 — task artifact dir manager, threaded into spawn so
    // each task gets a tidy ~/mira-artifacts/<skill>/<slug>_<task>/.
    task_artifacts:  Option<Arc<crate::task_artifacts::TaskArtifactsStore>>,
    // Phase B slice 2 — named-agent definition store, used by
    // `spawn_background_task` (agent handle resolution) and
    // `list_named_agents`. None when the store failed to open.
    agent_defs:      Option<Arc<crate::agent::AgentDefinitionStore>>,
    // Phase C — workflow orchestrator + store, used by `run_workflow` /
    // `list_workflows`. None when the store failed to open.
    orchestrator:    Option<Arc<crate::agent::Orchestrator>>,
    workflow_store:  Option<Arc<crate::agent::WorkflowStore>>,
    // 0.116.0 — wiki registry, used to register the model-callable
    // `wiki` skill (Slice D). When `config.wiki.enabled` is false the
    // tools are not registered.
    wiki_registry:   Arc<crate::wiki::WikiRegistry>,
    // 0.121.0 — companion system. When `None` (DB open
    // failed), the companion tools are not registered. Otherwise all
    // six tools are wired in.
    companion_system: Option<Arc<crate::companion::CompanionSystem>>,
    // 0.179.0 — deferred LiveConfig handle for `settings_set` global writes.
    // Empty when this runs (LiveConfig is built afterwards); the gateway
    // fills it once LiveConfig exists so writes apply live.
    settings_live_config: Arc<std::sync::OnceLock<Arc<crate::web::LiveConfig>>>,
    // Shared restart notifier — held by the destructive `backup_restore`
    // agent tool so a successful restore can stage `.restore_pending` and
    // then trigger the same graceful shutdown the /api/admin/restart
    // handler does. Same Arc is also threaded into MiraServer.
    restart_notify:       Arc<tokio::sync::Notify>,
    // MIRA-Guardian read-only inspection (P1): the health snapshot store +
    // subsystem degradation tracker, surfaced through `guardian_inspect`.
    health_store:         Option<Arc<crate::health::store::HealthStore>>,
    degradation_tracker:  Arc<crate::health::degradation::DegradationTracker>,
    // MIRA-Guardian action proposals (P4): the propose tool writes pending
    // proposals here, in `active` mode only.
    guardian_actions:     Option<Arc<crate::agent::guardian_actions::GuardianActionStore>>,
    // HMAC-chained agent audit log — the propose tool records the "proposed"
    // event so the proposal→decision→execution chain is tamper-evident (P4).
    guardian_audit:       Option<Arc<crate::agent::AuditStore>>,
    // Deferred ChannelManager handle for guardian_decide's restart_bridge (the
    // manager is built after this registry; the gateway fills it later) (P4b).
    guardian_channel_manager: Arc<std::sync::OnceLock<Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>>>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    if let Some(store) = audit {
        registry = registry.with_audit(store);
    }
    if let Some(engine) = &policy_engine {
        registry = registry.with_policy_engine(Arc::clone(engine));
        info!("ToolRegistry attached to policy engine (1.2)");
    }

    if config.agent.tools.shell.enabled {
        registry.register(ShellExecuteTool::new(30));
        info!("Tool registered: shell");
    }
    if config.agent.tools.filesystem.enabled {
        let mut read_tool  = FileReadTool::new(None);
        let mut write_tool = FileWriteTool::new(None);
        if let Some(engine) = &policy_engine {
            read_tool  = read_tool.with_policy_engine(Arc::clone(engine));
            write_tool = write_tool.with_policy_engine(Arc::clone(engine));
        }
        registry.register(read_tool);
        registry.register(write_tool);
        info!("Tool registered: filesystem");
    }

    // ── Tier 1 — pure, always-on tools ────────────────────────────────────────
    // These have no external dependencies (network, code exec, etc.) so they
    // ship unconditionally. See design-docs/phase7-tools-and-sandbox.md §1.
    registry.register(NowTool::new(auth.cloned()));
    registry.register(DateMathTool::new());
    registry.register(MathEvalTool::new());
    // Built-in weather (keyless Open-Meteo by default; configurable provider).
    registry.register(crate::tools::weather::WeatherTool::new(config.weather.clone()));
    registry.register(PdfExtractTool::new(data_dir.to_path_buf()));
    // MIRA self-knowledge: answer questions about MIRA's own features,
    // settings, limitations, and how-tos from the bundled mira-docs/.
    registry.register(crate::tools::mira_help::MiraHelpTool);
    // MIRA-Guardian's read-only window into health/degradations/logs. System-
    // visibility (hidden from the user palette); the Guardian reaches it via its
    // explicit allowlist. See `agent::guardian`.
    registry.register(crate::tools::guardian_inspect::GuardianInspectTool::new(
        health_store,
        Some(degradation_tracker),
        Some(config.log_file_path()),
    ));
    // MIRA-Guardian propose tool (P4) — records pending remediation proposals.
    // System-visibility; only reachable when the Guardian's allowlist includes
    // it (active mode). Registered when the store opened.
    if let Some(store) = guardian_actions {
        registry.register(crate::tools::guardian_propose::GuardianProposeTool::new(
            Arc::clone(&store), guardian_audit.clone()));
        // P4b — conversational approval, authorized to the Guardian's operator.
        registry.register(crate::tools::guardian_decide::GuardianDecideTool::new(
            store,
            automations.clone(),
            guardian_channel_manager,
            guardian_audit,
            config.automations.watchdog.notify_user_id.clone(),
        ));
    }
    // Settings introspection: describe any setting (open) +
    // read live values access-gated (own per-user settings for anyone;
    // global/operator settings admin-only, secrets redacted, read fresh
    // from the on-disk config).
    registry.register(crate::tools::settings::SettingsDescribeTool);
    registry.register(crate::tools::settings::SettingsGetTool::new(
        config.config_path.clone(),
        auth.cloned(),
        companion_system.as_ref().map(|s| s.store_arc()),
    ));
    // writes. Per-user voice writes apply immediately; global
    // (admin-only) writes are denylisted (security/providers/proxy + secrets),
    // require confirm, are schema-validated before persist, and apply live
    // via the deferred LiveConfig handle.
    registry.register(crate::tools::settings::SettingsSetTool::new(
        config.config_path.clone(),
        auth.cloned(),
        companion_system.as_ref().map(|s| s.store_arc()),
        settings_live_config,
    ));
    info!("Tool registered: now, date_math, math_eval, pdf_extract");

    // Backup / restore agent tools (Q1.5 follow-on). Three tools so the
    // user can say "back up my data", "what backups do I have", and
    // "restore from yesterday's backup" in chat. The destructive
    // `backup_restore` is admin-gated (looks up role via LocalAuthService)
    // and requires `confirm: true`. Encryption deliberately UI-only —
    // passphrases must not flow through chat. Holds the SAME restart
    // notifier the HTTP /api/admin/restart handler does so a tool-
    // triggered restore actually restarts cleanly.
    {
        let deps = crate::tools::backup::BackupToolDeps {
            data_dir:        data_dir.to_path_buf(),
            config_path:     config.config_path.clone(),
            retention_count: config.backup.scheduled_retention_count,
            auth:            auth.cloned(),
            shutdown:        Arc::clone(&restart_notify),
        };
        registry.register(crate::tools::backup::BackupCreateTool::new(deps.clone()));
        registry.register(crate::tools::backup::BackupListTool::new(deps.clone()));
        registry.register(crate::tools::backup::BackupRestoreTool::new(deps));
        info!("Tool registered: backup_{{create,list,restore}}");
    }

    // summarize_conversation needs both history and provider — skip if either
    // is unavailable (history failed at startup, etc.).
    if let Some(hist) = history {
        registry.register(SummarizeConversationTool::new(
            Arc::clone(hist),
            Arc::clone(provider),
        ));
        info!("Tool registered: summarize_conversation");
    } else {
        warn!("summarize_conversation skipped (history store unavailable)");
    }

    // memory_supersede needs auth for group resolution.
    if let Some(auth_svc) = auth {
        registry.register(MemorySupersedeTool::new(
            Arc::clone(memory),
            Arc::clone(auth_svc),
        ));
        info!("Tool registered: memory_supersede");
    } else {
        warn!("memory_supersede skipped (auth service unavailable)");
    }

    // Onboarding (system-tier, flow="onboarding"). Requires auth+history to
    // be present — if auth failed at startup, skip registration so chat and
    // channels still work; the onboarding flow just can't be entered.
    // `resolve_timezone` is the only onboarding tool with no backend deps;
    // keep it paired with the others so user-facing code only has one
    // registration site to reason about.
    match (auth, history) {
        (Some(auth), Some(history)) => {
            let schema = match OnboardingSchema::bundled() {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    warn!("Onboarding schema failed to load — onboarding tools disabled: {}", e);
                    return registry;
                }
            };
            let services = Arc::new(OnboardingServices {
                auth:     Arc::clone(auth),
                history:  Arc::clone(history),
                memory:   Arc::clone(memory),
                schema,
                data_dir: data_dir.to_path_buf(),
                // Mirror onboarding-captured values into the user's
                // wiki profile.md. Only wired when wiki is enabled —
                // when disabled the bridge is a no-op anyway.
                wiki:     if config.wiki.enabled { Some(Arc::clone(&wiki_registry)) } else { None },
                // Wire the companion system so onboarding answers configure
                // (and, for admins, enable) Presence check-ins. None when the
                // companion DB failed to open — the bridge is a no-op then.
                companion: companion_system.clone(),
            });
            registry.register(RecordProfileTool::new(Arc::clone(&services)));
            registry.register(SkipTopicTool::new(Arc::clone(&services)));
            registry.register(MarkGroupCompleteTool::new(Arc::clone(&services)));
            registry.register(CompleteOnboardingTool::new(Arc::clone(&services)));
            registry.register(ResolveTimezoneTool::new());
            info!("Tool registered: onboarding (system-tier, 5 tools)");
        }
        _ => {
            warn!("Onboarding tools skipped (auth or history unavailable)");
        }
    }

    // Calendar tools — four thin wrappers over CalendarStore. Registered as
    // a group when the subsystem is up; absent when `calendar.enabled=false`
    // or the store failed to open at startup.
    if let Some(cal) = calendar.as_ref() {
        registry.register(CalendarListEventsTool::new(Arc::clone(cal)));
        registry.register(CalendarCreateEventTool::new(Arc::clone(cal)));
        registry.register(CalendarUpdateEventTool::new(Arc::clone(cal)));
        registry.register(CalendarDeleteEventTool::new(Arc::clone(cal)));
        info!("Tool registered: calendar (4 tools)");
    }

    // recall_history — user-tier, model-callable semantic search over the
    // user's own past messages. Needs history (for the vector index) and
    // memory (for on-the-fly query embedding). If either is missing we just
    // don't register it; chat still works without long-term recall.
    if let Some(history) = history {
        registry.register(RecallHistoryTool::new(
            Arc::clone(history),
            Arc::clone(memory),
        ));
        info!("Tool registered: recall_history");
    } else {
        warn!("recall_history skipped (history store unavailable)");
    }

    // ── Slice D — wiki tools (per-user markdown knowledge base) ──────────────
    // Reads (search, read) are always wired when the wiki is enabled.
    // Writes (append_section, write_page, log_entry) are gated by
    // `wiki.agent_tools.write_mode`: "review" (default — pending until
    // user approves), "auto" (apply immediately), "off" (writes not
    // registered at all). Matches the auto-extractor's safety posture.
    if config.wiki.enabled && config.wiki.agent_tools.enabled {
        registry.register(crate::tools::wiki::WikiSearchTool::new(Arc::clone(&wiki_registry)));
        registry.register(crate::tools::wiki::WikiReadTool::new(Arc::clone(&wiki_registry)));
        info!("Tool registered: wiki_search, wiki_read");

        let write_mode = config.wiki.agent_tools.write_mode.as_str();
        if write_mode != "off" {
            registry.register(crate::tools::wiki::WikiAppendSectionTool::new(
                Arc::clone(&wiki_registry), write_mode,
            ));
            registry.register(crate::tools::wiki::WikiWritePageTool::new(
                Arc::clone(&wiki_registry), write_mode,
            ));
            registry.register(crate::tools::wiki::WikiLogEntryTool::new(
                Arc::clone(&wiki_registry), write_mode,
            ));
            info!("Tool registered: wiki_append_section, wiki_write_page, wiki_log_entry (mode={})", write_mode);
        } else {
            info!("Wiki write tools skipped (wiki.agent_tools.write_mode = \"off\")");
        }
    } else {
        info!("Wiki tools skipped (wiki.enabled or agent_tools.enabled = false)");
    }

    // ── Companion model-callable companion-mode tools ─────────────
    // Six tools (status / enable / disable / pause / resume / configure).
    // Only registered when the companion system opened successfully at
    // gateway start.
    if let Some(sys) = &companion_system {
        registry.register(crate::tools::companion::CompanionStatusTool::new(Arc::clone(sys)));
        registry.register(crate::tools::companion::CompanionEnableTool::new(Arc::clone(sys)));
        registry.register(crate::tools::companion::CompanionDisableTool::new(Arc::clone(sys)));
        registry.register(crate::tools::companion::CompanionPauseTool::new(Arc::clone(sys)));
        registry.register(crate::tools::companion::CompanionResumeTool::new(Arc::clone(sys)));
        registry.register(crate::tools::companion::CompanionConfigureTool::new(Arc::clone(sys)));
        registry.register(crate::tools::companion::CompanionBriefingSetTool::new(Arc::clone(sys)));
        info!("Tool registered: companion_{{status,enable,disable,pause,resume,configure,briefing_set}}");
    } else {
        warn!("Companion tools skipped (companion system not available)");
    }

    // ── Tier 2 — network tools (web_fetch, url_preview, web_search) ──────────
    // The shared HttpPolicy + search backends were built by the gateway and
    // passed in so the skill resolver (research adapter) can reuse them and
    // share rate-limit state.
    let web_fetch_cfg   = &config.agent.tools.web_fetch;
    let url_preview_cfg = &config.agent.tools.url_preview;
    let web_search_cfg  = &config.agent.tools.web_search;
    if web_fetch_cfg.enabled {
        registry.register(WebFetchTool::new(
            Arc::clone(&http_policy),
            WebFetchSettings {
                max_text_chars: web_fetch_cfg.max_text_chars,
            },
        ));
        info!("Tool registered: web_fetch");
    }
    if url_preview_cfg.enabled {
        registry.register(UrlPreviewTool::new(Arc::clone(&http_policy)));
        info!("Tool registered: url_preview");
    }
    if web_search_cfg.enabled {
        let any_configured = search_backends.iter().any(|b| b.is_configured());
        if any_configured {
            let tool = WebSearchTool::new(search_backends.clone(), web_search_cfg.top_k);
            info!("Tool registered: web_search (backends: {:?})", tool.backend_status());
            registry.register(tool);
        } else {
            warn!("web_search not registered — no backend is configured. \
                   Enable DDG implicitly, or set brave.api_key / searxng.url.");
        }
    }

    // ── image_generate (Network tier) ────────────────────────────────────────
    // Text→image via the OpenAI Images API; bytes land in the artifact store and
    // render inline in chat. Always registered so the UI can badge it; it
    // self-reports `enabled() == false` (and refuses) without an OpenAI key.
    match crate::artifacts::ArtifactStore::new(data_dir) {
        Ok(store) => {
            let tool = crate::tools::image_generate::ImageGenerateTool::new(config, Arc::new(store));
            let on = crate::tools::Tool::enabled(&tool);
            registry.register(tool);
            info!("Tool registered: image_generate (enabled={on})");
        }
        Err(e) => warn!("image_generate not registered — artifact store init failed: {e}"),
    }

    // ── video_generate (Network tier) ────────────────────────────────────────
    // Text→video via the OpenAI Videos (Sora) API; the rendered MP4 lands in
    // the artifact store and renders inline as a <video> player in chat. Always
    // registered so the UI can badge it; it self-reports `enabled() == false`
    // (and refuses) without an OpenAI key.
    match crate::artifacts::ArtifactStore::new(data_dir) {
        Ok(store) => {
            let tool = crate::tools::video_generate::VideoGenerateTool::new(config, Arc::new(store));
            let on = crate::tools::Tool::enabled(&tool);
            registry.register(tool);
            info!("Tool registered: video_generate (enabled={on})");
        }
        Err(e) => warn!("video_generate not registered — artifact store init failed: {e}"),
    }

    // ──  / agent autonomy tools ────────────────────────────
    // Five tools that let the agent self-schedule follow-ups, register
    // webhooks, and subscribe to internal events. All routed through the
    // create-time gate so quota + approval rules apply uniformly.
    let cfg_arc = Arc::new(config.clone());
    if let Some(store) = automations.as_ref() {
        registry.register(ScheduleFollowupTool::new(Arc::clone(store), Arc::clone(&cfg_arc)));
        registry.register(ListSelfSchedulesTool::new(Arc::clone(store)));
        registry.register(CancelScheduleTool::new(Arc::clone(store)));
        registry.register(RegisterWebhookTool::new(Arc::clone(store), Arc::clone(&cfg_arc)));
        registry.register(SubscribeEventTool::new(Arc::clone(store), Arc::clone(&cfg_arc)));
        info!("Tool registered: automations (5 agent tools)");
    } else {
        warn!("automations agent tools skipped (store unavailable)");
    }

    // Pending-approval tools — let the user summarise + approve agent-created
    // schedules and wiki review-queue edits right in chat (check-ins/briefings
    // nudge them; these action it). Registered whenever either backing store
    // exists; they degrade gracefully when one is absent.
    {
        let wiki_for_approvals = if config.wiki.enabled {
            Some(Arc::clone(&wiki_registry))
        } else {
            None
        };
        if automations.is_some() || wiki_for_approvals.is_some() {
            registry.register(crate::tools::approvals::PendingApprovalsTool::new(
                automations.clone(), wiki_for_approvals.clone(),
            ));
            registry.register(crate::tools::approvals::ApprovePendingTool::new(
                automations.clone(), wiki_for_approvals,
            ));
            info!("Tool registered: pending_approvals, approve_pending");
        }
    }

    // ──  / agent task lifecycle ───────────────────────────────────
    // `spawn_background_task` + `get_task_result` — the primitives that
    // turn the multi-agent runtime into something a chat-tier LLM can
    // actually drive ("research this in the background and ping me when
    // done"). spawn auto-registers a completion subscription so the user
    // gets pinged on the originating channel without further wiring.
    let mut spawn_tool = crate::tools::agent_tasks::SpawnBackgroundTaskTool::new(
        Arc::clone(&supervisor),
        Arc::clone(&agent_registry),
        automations.clone(),
        Arc::clone(&cfg_arc),
    );
    if let Some(arts) = task_artifacts.as_ref() {
        spawn_tool = spawn_tool.with_task_artifacts(Arc::clone(arts));
    }
    if let Some(defs) = agent_defs.as_ref() {
        spawn_tool = spawn_tool.with_agent_defs(Arc::clone(defs));
    }
    if let Some(auth) = auth {
        // Capability RBAC — clamp autonomous spawn budgets to the user's cap.
        spawn_tool = spawn_tool.with_auth_db(auth.db_arc());
    }
    registry.register(spawn_tool);
    registry.register(crate::tools::agent_tasks::GetTaskResultTool::new(
        Arc::clone(&agent_registry),
    ));
    registry.register(crate::tools::agent_tasks::ListNamedAgentsTool::new(
        agent_defs.clone(),
    ));
    registry.register(crate::tools::agent_tasks::CreateNamedAgentTool::new(
        agent_defs.clone(),
    ));
    info!("Tool registered: agent_tasks (spawn_background_task, get_task_result, list_named_agents, create_named_agent)");

    // Phase C — workflow orchestration tools. Registered only when the
    // orchestrator + store are wired; `list_workflows` is always registered
    // (returns empty without a store) so the model can discover the surface.
    if let Some(orch) = orchestrator.as_ref() {
        if let Some(store) = workflow_store.as_ref() {
            registry.register(crate::tools::workflow_tasks::RunWorkflowTool::new(
                Arc::clone(orch), Arc::clone(store), automations.clone(), Arc::clone(&cfg_arc),
            ));
        }
    }
    registry.register(crate::tools::workflow_tasks::ListWorkflowsTool::new(
        workflow_store.clone(),
    ));
    registry.register(crate::tools::workflow_tasks::CreateWorkflowTool::new(
        workflow_store.clone(),
    ));
    info!("Tool registered: workflow_tasks (run_workflow, list_workflows, create_workflow)");

    // ── Tier 4 — sandboxed code execution ────────────────────────────────────
    // Disabled unless the operator opts in via [sandbox] in the config.
    // Even then we only register if at least one rootfs is installed; an
    // un-installed rootfs would just produce per-call errors and clutter.
    if config.sandbox.enabled && config.sandbox.code_run.enabled {
        use crate::sandbox::{default_backend, rootfs::RootfsManager, SeccompMode};
        use crate::config::SeccompModeConfig;

        let seccomp_mode = match config.sandbox.seccomp_mode {
            SeccompModeConfig::Denylist  => SeccompMode::Denylist,
            SeccompModeConfig::Allowlist => SeccompMode::Allowlist,
        };
        let artifacts = match crate::artifacts::ArtifactStore::new(data_dir) {
            Ok(s)  => Arc::new(s),
            Err(e) => {
                warn!("code_run not registered — cannot init artifact store at {}/artifacts: {e}", data_dir.display());
                return registry;
            }
        };

        // Backend selection: "namespace" (Linux seccomp/namespaces), "wasm"
        // (cross-platform WASM/WASI), or "pyodide" (scientific Python on Node).
        // "auto"/"" prefers namespace when its rootfs is installed (best
        // fidelity on Linux), else WASM. Independently, the Pyodide backend is
        // attached as a *secondary* "scientific" route on top of the primary
        // namespace/WASM backend when enabled — so `import numpy` works while
        // plain scripts stay on the lighter primary backend.
        let want = config.sandbox.backend.trim().to_ascii_lowercase();

        // Resolve the Linux namespace rootfs (if any).
        let manager = RootfsManager::new(data_dir);
        let configured = config.sandbox.python.rootfs_path.trim();
        let pivot = if configured.is_empty() {
            manager.python_pivot_root()
        } else {
            std::path::PathBuf::from(crate::config::expand_path(configured))
        };
        let namespace_ok = pivot.is_dir() && default_backend().supported();

        // Optional Pyodide scientific backend (numpy/pandas/matplotlib). Enabled
        // via [sandbox.pyodide] enabled=true, or implied when backend="pyodide".
        // Feature-gated; None on builds without `sandbox-wasm`.
        #[cfg(feature = "sandbox-wasm")]
        let pyodide_backend: Option<Arc<dyn crate::sandbox::CodeSandbox>> = {
            let pyo_enabled = config.sandbox.pyodide.enabled || want == "pyodide";
            if pyo_enabled {
                if crate::sandbox::pyodide::is_provisioned(data_dir) {
                    info!("code_run: Pyodide scientific backend available (numpy/pandas/matplotlib)");
                    Some(Arc::new(crate::sandbox::pyodide::PyodideSandbox::new(data_dir)))
                } else {
                    // Auto-provision (download dist + pre-warm) in the background;
                    // available after the next restart.
                    let dd = data_dir.to_path_buf();
                    let prewarm = config.sandbox.pyodide.prewarm.clone();
                    tokio::spawn(async move {
                        match crate::sandbox::pyodide::ensure_pyodide(&dd, &prewarm).await {
                            Ok(()) => info!("pyodide ready — scientific code_run available after restart"),
                            Err(e) => warn!("pyodide provisioning failed: {e}"),
                        }
                    });
                    warn!("code_run: Pyodide downloading in the background; scientific Python available after restart");
                    None
                }
            } else {
                None
            }
        };
        #[cfg(not(feature = "sandbox-wasm"))]
        let pyodide_backend: Option<Arc<dyn crate::sandbox::CodeSandbox>> = None;

        let use_namespace = match want.as_str() {
            "namespace"        => true,
            "wasm" | "pyodide" => false,
            _                  => namespace_ok, // auto
        };

        #[allow(unused_mut, unused_assignments)]
        let mut registered = false;

        // Explicit "pyodide" primary backend: every call runs in Pyodide.
        #[cfg(feature = "sandbox-wasm")]
        if !registered && want == "pyodide" {
            if let Some(sb) = pyodide_backend.clone() {
                registry.register(CodeRunTool::new(
                    sb, pivot.clone(), config.sandbox.code_run.clone(), seccomp_mode, artifacts.clone(),
                ));
                info!("Tool registered: code_run (pyodide primary — scientific Python)");
                registered = true;
            } else {
                warn!("code_run (pyodide) requested but not yet provisioned — downloading; available after restart");
            }
        }

        if !registered && use_namespace {
            if namespace_ok {
                let backend: Arc<dyn crate::sandbox::CodeSandbox> = Arc::from(default_backend());
                registry.register(CodeRunTool::new(
                    backend, pivot.clone(), config.sandbox.code_run.clone(), seccomp_mode, artifacts.clone(),
                ).with_scientific(pyodide_backend.clone()));
                info!("Tool registered: code_run (namespace, rootfs={}, seccomp={:?}{})",
                      pivot.display(), config.sandbox.seccomp_mode,
                      if pyodide_backend.is_some() { ", +pyodide scientific" } else { "" });
                registered = true;
            } else {
                warn!("code_run (namespace) requested but rootfs missing at {} — run `mira sandbox install python`", pivot.display());
            }
        }

        // Cross-platform WASM backend (Windows/macOS, or when selected/forced).
        #[cfg(feature = "sandbox-wasm")]
        if !registered {
            use crate::sandbox::wasm::{managed_python_wasm_path, WasmSandbox};
            let wcfg = config.sandbox.wasm.python_path.trim();
            let wpath = if wcfg.is_empty() {
                managed_python_wasm_path(data_dir)
            } else {
                std::path::PathBuf::from(crate::config::expand_path(wcfg))
            };
            if wpath.is_file() {
                let backend: Arc<dyn crate::sandbox::CodeSandbox> = Arc::new(WasmSandbox::new(Some(wpath.clone())));
                if backend.supported() {
                    // `rootfs_path` is passed through to limits.rootfs, which the
                    // WASM backend ignores (it uses its own scratch preopen).
                    registry.register(CodeRunTool::new(
                        backend, wpath.clone(), config.sandbox.code_run.clone(), seccomp_mode, artifacts.clone(),
                    ).with_scientific(pyodide_backend.clone()));
                    info!("Tool registered: code_run (wasm, module={}{})", wpath.display(),
                          if pyodide_backend.is_some() { ", +pyodide scientific" } else { "" });
                    registered = true;
                } else {
                    warn!("code_run (wasm) — module at {} failed to compile", wpath.display());
                }
            } else {
                // Auto-provision the WASI Python in the background so code_run
                // is available after the next restart.
                let dd = data_dir.to_path_buf();
                tokio::spawn(async move {
                    match crate::sandbox::wasm::ensure_python_wasm(&dd).await {
                        Ok(p)  => info!("wasm python ready at {} — code_run available after restart", p.display()),
                        Err(e) => warn!("wasm python provisioning failed: {e}"),
                    }
                });
                warn!("code_run (wasm) — Python runtime downloading in the background; available after restart");
            }
        }

        #[cfg(not(feature = "sandbox-wasm"))]
        if !registered && !use_namespace {
            warn!("code_run not registered — 'wasm'/'pyodide' backend requested but this build lacks the sandbox-wasm feature");
        }

        let _ = registered;
        let _ = &pyodide_backend;
    }

    // ── Skills (slice A3.5 + A5 from design-docs/skills-and-agents.md) ──────────
    // Load installed Skills from <data_dir>/skills/ and register each as a
    // SkillTool. Snapshot the registry's *builtins* into a dispatcher
    // BEFORE adding SkillTools — that way each SkillTool's dispatcher only
    // points at builtins, never at other SkillTools, so the
    // `SkillTool ↔ ToolRegistry` ownership graph stays acyclic.
    //
    // The SkillPrefsStore (A5) lets each user disable individual Skills;
    // a disabled call returns an explicit "skill disabled" error before
    // any tool work happens.
    {
        let skills_dir = crate::skills::default_skills_dir(data_dir);
        let mira_version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("CARGO_PKG_VERSION is always valid semver");

        // 0.93.0 rename: park the old `com.mira.coding/` skill dir so
        // the auto-refresher below sees a clean slate and writes the
        // renamed bundle (`com.mira.claudecode/`) freshly. The original
        // contents move into `.bundled-uninstalled/com.mira.coding/`
        // for forensic recovery instead of being deleted outright.
        // Idempotent — no-op when the dir is already absent.
        match crate::skills::bundled::park_stale_skill(&skills_dir, "com.mira.coding") {
            Ok(true)  => info!("0.93.0 migration: parked stale skill dir com.mira.coding/"),
            Ok(false) => {} // not present; nothing to do
            Err(e)    => tracing::warn!("0.93.0 migration: park_stale_skill failed: {e}"),
        }

        // Bundled starter Skills (slice A9). Extract any that don't yet
        // exist on disk AND auto-refresh ones whose bundled-manifest
        // version is newer than what's currently installed (so a
        // MIRA upgrade that ships a new declared-secrets schema, etc.
        // takes effect without operator intervention). User uninstall
        // markers and user-edited dev installs are left alone.
        match crate::skills::bundled::extract_or_refresh(&skills_dir, false) {
            Ok(report) => {
                use crate::skills::bundled::RefreshOutcome;
                let mut extracted = Vec::new();
                let mut refreshed = Vec::new();
                for (id, outcome) in report {
                    match outcome {
                        RefreshOutcome::Extracted => extracted.push(id),
                        RefreshOutcome::Refreshed { from, to } =>
                            refreshed.push(format!("{id} ({from}→{to})")),
                        _ => {}
                    }
                }
                if !extracted.is_empty() {
                    info!("Bundled Skills extracted: {}", extracted.join(", "));
                }
                if !refreshed.is_empty() {
                    info!("Bundled Skills refreshed: {}", refreshed.join(", "));
                }
            }
            Err(e) => warn!(
                "Could not extract/refresh bundled Skills at {}: {e}",
                skills_dir.display(),
            ),
        }

        // Trust store (slice A7) — empty store means "no Skills can be
        // verified", which is OK for fresh installs. Admins add publisher
        // keys via /api/skills/trust-store.
        let trust = match crate::skills::TrustStore::load(
            &crate::skills::TrustStore::default_path(&skills_dir),
        ) {
            Ok(s)  => Some(s),
            Err(e) => { warn!("Skill trust store unavailable: {e}"); None }
        };

        let skills = crate::skills::load_dir_with_trust(
            &skills_dir, &mira_version, trust.as_ref(),
        );

        for err in &skills.errors {
            warn!("Skill load error at {}: {}", err.path.display(), err.error);
        }

        let prefs = match crate::skills::SkillPrefsStore::open(
            &data_dir.join("skill_prefs.db"),
        ) {
            Ok(s)  => Some(Arc::new(s)),
            Err(e) => {
                warn!("Skill prefs store unavailable, per-user enable/disable disabled: {e}");
                None
            }
        };

        if !skills.loaded.is_empty() {
            let dispatcher: Arc<dyn crate::skills::BuiltinDispatcher> = Arc::new(
                crate::skills::runtime::BuiltinSnapshotDispatcher::from_registry(&registry),
            );
            let count = crate::skills::runtime::register_skills(
                &mut registry, &skills, dispatcher, prefs,
            );
            info!(
                "Skills registered: {count} (skills_dir={})",
                skills_dir.display(),
            );
        } else if skills_dir.exists() {
            info!("No Skills found at {}", skills_dir.display());
        }
    }

    registry
}

// Build the backend list in the configured order: `default` first, then
// `failover` entries, deduplicated. Each backend is always constructed;
// unconfigured ones are filtered at call-time by `is_configured()`.
fn build_search_backends(
    config: &MiraConfig,
    policy: &Arc<HttpPolicy>,
) -> Vec<Arc<dyn SearchBackend>> {
    let ws = &config.agent.tools.web_search;
    let mut order: Vec<String> = vec![ws.default.clone()];
    for f in &ws.failover {
        if !order.contains(f) { order.push(f.clone()); }
    }

    let build_one = |id: &str| -> Option<Arc<dyn SearchBackend>> {
        match id {
            "ddg"     => Some(Arc::new(DdgHtmlBackend::new(Arc::clone(policy)))),
            "brave"   => Some(Arc::new(BraveApiBackend::new(
                Arc::clone(policy),
                ws.brave.api_key.clone(),
            ))),
            "searxng" => Some(Arc::new(SearxngBackend::new(
                Arc::clone(policy),
                ws.searxng.url.clone(),
            ))),
            other     => {
                warn!("web_search: unknown backend id '{}' in config; skipping", other);
                None
            }
        }
    };

    order.iter()
        .filter_map(|id| build_one(id))
        .collect()
}

// Translate the config-side HTTP policy into the in-process `HttpPolicyConfig`.
// Lives in the builder so tool construction stays ignorant of config shape.
fn build_http_policy_config(config: &MiraConfig) -> HttpPolicyConfig {
    let wf   = &config.agent.tools.web_fetch;
    let http = &config.security.http;

    // Use the most generous per-tool size + timeout across Tier 2 tools so
    // smaller-bound tools (url_preview) still fit. Per-tool tightening stays
    // the tool's own concern — the policy is a floor, not an exact match.
    let max_body = wf.max_body_bytes.max(config.agent.tools.url_preview.max_body_bytes);

    // Explicit `security.http.searxng_exception` wins; otherwise derive
    // from the user's SearXNG URL so the common "home LAN" setup works
    // without the admin having to set both knobs.
    let searxng_exception = http.searxng_exception.as_deref()
        .and_then(parse_host_port)
        .or_else(|| config.agent.tools.web_search.searxng.url.as_deref()
            .and_then(extract_host_port));

    HttpPolicyConfig {
        user_agent:         format!("MIRA/{}", env!("CARGO_PKG_VERSION")),
        max_body_bytes:     max_body,
        request_timeout:    Duration::from_secs(wf.timeout_secs.max(1)),
        max_redirects:      wf.max_redirects,
        denylist:           http.denylist.clone(),
        allowlist:          http.allowlist.clone(),
        allowlist_only:     http.allowlist_only,
        searxng_exception,
        rate_user_per_min:  http.rate.user_per_min,
        rate_user_per_hour: http.rate.user_per_hour,
        rate_user_per_domain_per_min: http.rate.user_per_domain_per_min,
        rate_search_per_min: http.rate.search_per_min,
    }
}

// Parse `"host:port"` strict form. Returns `None` on any malformed input so
// misconfigurations degrade to "no exception" rather than a panic.
fn parse_host_port(s: &str) -> Option<(String, u16)> {
    let (h, p) = s.rsplit_once(':')?;
    let port = p.parse::<u16>().ok()?;
    let host = h.trim();
    if host.is_empty() { return None; }
    Some((host.to_owned(), port))
}

// Build the production skill resolver. Maps known skill IDs to the
// adapters that run them. Adapters are constructed only when their
// dependencies are present:
// - `com.mira.research` requires at least one configured search
// backend.
// - `com.mira.claudecode` requires the `claude` CLI on `PATH`.
// - `com.mira.opencode` requires the `opencode` CLI on `PATH`.
// // Skills without their backing adapter are simply not registered;
// requests for them resolve to `None` and the supervisor refuses the
// spawn with the existing "no executor configured" error.
fn build_skill_resolver(
    config:          &MiraConfig,
    provider:        Arc<dyn ModelProvider>,
    http_policy:     Arc<HttpPolicy>,
    search_backends: Vec<Arc<dyn SearchBackend>>,
    secrets:         Option<Arc<crate::skills::SecretsStore>>,
) -> crate::agent::MiraSkillResolver {
    use crate::agent::{
        ClaudeCodeAdapter, ClaudeCodeConfig,
        HttpPolicyFetcher, MiraSkillResolver, OpenCodeAdapter, OpenCodeConfig,
        ResearchAdapter, ResearchConfig,
    };

    let mut resolver = MiraSkillResolver::new();

    // ── com.mira.research ────────────────────────────────────────────────
    // Pick the first configured search backend. The adapter only ever
    // hits one, and `WebSearchTool` already retains the rest for
    // user-driven failover.
    if let Some(backend) = search_backends.iter().find(|b| b.is_configured()).cloned() {
        let max_body = config.agent.tools.web_fetch.max_text_chars.max(2000);
        let fetcher = Arc::new(HttpPolicyFetcher::new(
            Arc::clone(&http_policy),
            "research-adapter",
            max_body,
        ));
        let cfg = ResearchConfig::new(backend, fetcher, Arc::clone(&provider));
        resolver = resolver.with_skill(
            "com.mira.research",
            ResearchAdapter::new(cfg) as Arc<dyn crate::agent::WorkerTask>,
        );
    } else {
        warn!("com.mira.research adapter NOT registered — no configured search backend");
    }

    // ── com.mira.claudecode ─────────────────────────────────────────────
    // Registered UNCONDITIONALLY so the skill is enableable + offers one-click
    // install even when `claude` isn't on the box yet; the adapter resolves the
    // CLI lazily at spawn (PATH + common install dirs + MIRA-managed npm
    // install under ~/.mira/deps), so a freshly-installed CLI works without a
    // restart. We still seed `config.binary` with whatever resolves now.
    {
        let mut cc = ClaudeCodeConfig::new();
        if let Some(claude_bin) = crate::install::deps::resolve_external_cli("claude") {
            cc = cc.with_binary(claude_bin);
        }
        cc = cc.with_skip_permissions(true); // headless subagent
        cc = cc.with_bare(true);             // no operator's CLAUDE.md
        // Standard coding toolset. Without --allowedTools the headless
        // claude CLI registers a minimal Bash/Edit/Read subset and
        // burns rounds calling Write before falling back to
        // `cat > file <<EOF`. Spell out the full set so file creation,
        // bulk edits, and discovery work natively.
        cc = cc.with_allowed_tools(vec![
            "Bash".into(), "Edit".into(), "Read".into(), "Write".into(),
            "Glob".into(), "Grep".into(), "MultiEdit".into(),
        ]);
        let mut adapter = ClaudeCodeAdapter::new(cc);
        if let Some(store) = secrets.as_ref() {
            adapter = adapter.with_secrets(Arc::clone(store));
        }
        resolver = resolver.with_skill(
            "com.mira.claudecode",
            adapter as Arc<dyn crate::agent::WorkerTask>,
        );
    }

    // ── com.mira.opencode ───────────────────────────────────────────────
    // Same shape as claudecode but pointed at the `opencode` CLI from
    // sst/opencode. The OpenCode adapter (slice C3) handles its distinct
    // NDJSON output format and per-step cost accounting. Registered
    // unconditionally (lazy CLI resolve at spawn) so one-click install works
    // without a restart. `with_skip_permissions(true)` is required for
    // unattended runs — there's no human to approve permission prompts when
    // MIRA spawns the subagent in the background.
    {
        let mut oc = OpenCodeConfig::new();
        if let Some(opencode_bin) = crate::install::deps::resolve_external_cli("opencode") {
            oc = oc.with_binary(opencode_bin);
        }
        oc = oc.with_skip_permissions(true);
        let mut adapter = OpenCodeAdapter::new(oc);
        if let Some(store) = secrets.as_ref() {
            adapter = adapter.with_secrets(Arc::clone(store));
        }
        resolver = resolver.with_skill(
            "com.mira.opencode",
            adapter as Arc<dyn crate::agent::WorkerTask>,
        );
    }

    resolver
}

// One-shot startup sweep of `agent.worker.completed` subscriptions whose
// worker never reached a terminal state (typically because the service
// was restarted while the task was still running). For each, dispatches
// a synthetic "abandoned" notification through the existing
// `Action::ChannelMessage` action so the user finds out the task was
// lost, then marks the subscription `failed` so it becomes a no-op on
// future restarts. Errors are logged at warn but never propagated —
// gateway boot must not be blocked on this housekeeping.
async fn sweep_orphan_completion_subs(
    store:  &Arc<crate::automations::AutomationsStore>,
    worker: &Arc<crate::automations::Worker>,
) {
    let orphans = match store.list_orphan_completion_subscriptions() {
        Ok(rows) => rows,
        Err(e) => {
            warn!("orphan sweep: list failed: {e}");
            return;
        }
    };
    if orphans.is_empty() { return; }
    info!("orphan sweep: found {} stranded completion subscription(s)", orphans.len());

    let now = chrono::Utc::now().timestamp();
    let dispatcher = worker.dispatcher();

    // Synthetic payload — same shape the supervisor emits on a real
    // failure so the existing template ("{{payload.status_emoji}} Task
    // … {{payload.status_label}}\n\n{{payload.summary_or_error}}…")
    // renders cleanly without changes.
    let payload = serde_json::json!({
        "status":           "failed",
        "status_emoji":     "⚠️",
        "status_label":     "abandoned",
        "summary":          serde_json::Value::Null,
        "failure_reason":   "Worker abandoned by service restart",
        "summary_or_error": "Error: Worker was abandoned by a service restart and never reported a result. The task may have completed, partially completed, or never started — there's no way to tell from here.",
        "spent_usd":        0.0,
    });

    for sub in orphans {
        let activation = crate::automations::dispatch::Activation {
            source_kind: "event",
            source_id:   &sub.id,
            user_id:     &sub.user_id,
            action:      &sub.action,
            payload:     Some(&payload),
            chain_ids:   &[],
        };
        let outcome = dispatcher.dispatch(activation).await;
        if let Some(err) = outcome.error.as_deref() {
            warn!("orphan sweep: dispatch for sub {} failed: {err}", sub.id);
        }
        if let Err(e) = store.fail_event_subscription(
            &sub.id, now, "abandoned by service restart",
        ) {
            warn!("orphan sweep: failed to mark sub {} as failed: {e}", sub.id);
        } else {
            info!("orphan sweep: subscription {} marked failed (was waiting on agent.worker.completed)", sub.id);
        }
    }
}
