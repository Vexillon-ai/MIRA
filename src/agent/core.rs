// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/core.rs
//! [`AgentCore`] — the shared reasoning engine for MIRA.
//!
//! `AgentCore` is constructed once by the Gateway and shared (via `Arc`) across
//! every surface: the TUI, the HTTP server (Telegram, Signal, future browser
//! client), and any future channel.
//!
//! # Responsibilities
//!
//! * Manage conversation sessions via [`SessionStore`].
//! * Query memory for relevant context before each turn (pre-hook).
//! * Run the tool-call loop (OpenAI or ReAct, or both) on model responses.
//! * Persist new memories extracted from each completed turn (post-hook).
//! * Return a streaming channel of [`StreamEvent`]s so callers can forward
//! tokens to the user in real time.

use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::agent::memory_hook;
use crate::agent::stream::StreamEvent;
use crate::agent::tool_loop::{self, ToolMode};
use crate::agent::wiki_hook;
use crate::auth::LocalAuthService;
use crate::config::MiraConfig;
use crate::events::EventBus;
use crate::memory::MemorySystem;
use crate::providers::ModelProvider;
use crate::history::HistoryStore;
use crate::session::{SessionStore, SessionData};
use crate::tools::ToolRegistry;
use crate::types::{ChatMessage, GenerationOptions};
use crate::wiki::WikiRegistry;
use crate::MiraError;

// ─────────────────────────────────────────────────────────────────────────────

// Per-turn overrides for [`AgentCore::process_with_context`]. `None` fields
// fall back to the agent's default behaviour. Non-`None` fields let a caller
// (e.g. the chat handler running an onboarding conversation) steer the turn
// without permanently mutating `AgentCore`.
#[derive(Debug, Default, Clone)]
pub struct TurnContext {
    // Replaces the base system prompt for this turn. Memory context is still
    // appended on top, so callers can treat this as "persona + instructions"
    // and expect memory to layer in.
    pub system_prompt_override: Option<String>,
    // Restricts the tool-loop to this exact set of tool names. Unknown tools
    // the LLM tries to call are reported as errors back to the model via the
    // usual tool-result channel. `None` = no restriction (default registry).
    pub allowed_tool_names:     Option<Vec<String>>,
    // Key/value pairs merged into every tool call's arguments before
    // `execute()`. Used to inject `_user_id` / `_conversation_id` for the
    // onboarding tools without trusting the model to pass them.
    pub inject_tool_args:       serde_json::Map<String, serde_json::Value>,
    // Skip the memory pre/post hooks for this turn. Onboarding conversations
    // don't want retrieved memories leaking into the prompt (nothing to
    // retrieve yet, anyway) or new memories auto-extracted from small-talk.
    pub skip_memory_hooks:      bool,
    // Skip wiki context injection for this turn. The per-conversation
    // "use my wiki for this chat" toggle (Slice H) sets this; onboarding
    // also sets it since there's no wiki content to inject yet.
    pub skip_wiki_hooks:        bool,
    // Q1.3 — non-text inputs for the *current* user turn. Attached to
    // the `ChatMessage::user(input)` built just before the tool loop;
    // provider wire layers translate these into image_content_blocks.
    // Not part of the session-history replay — multi-turn vision needs
    // the user to re-attach.
    pub attachments:            Vec<crate::types::Attachment>,
    // Reasoning-effort hint for this turn, set by auto-routing (roadmap #13)
    // when the turn is routed to the reasoning provider. Flows into the
    // turn's `GenerationOptions.reasoning_effort`. `None` = provider default.
    pub reasoning_effort:       Option<String>,
    // Persisted-history conversation id for this turn. When set (and a
    // `HistoryStore` is installed on the agent), the in-memory session is
    // rehydrated from this conversation's stored messages on a cache miss —
    // so conversation context survives a process restart or the 1-hour idle
    // eviction that wipes the in-memory `SessionStore`. `None` = no
    // rehydration (the session is whatever the in-memory cache holds).
    //     // This is distinct from `session_id`: channels key their agent session on
    // a channel-specific id (e.g. `tg-<user>-<chat>`) while persisting to a
    // history conversation with a different DB id, so the caller must supply
    // the conversation id explicitly.
    pub conversation_id:        Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────

// Re-exported for callers that prefer `agent::DEFAULT_SYSTEM_PROMPT`.
// The canonical definition lives in [`crate::system_prompt`] so the wiki
// scaffolding can seed `wikis/system/persona.md` with it on first boot.
pub use crate::system_prompt::DEFAULT_SYSTEM_PROMPT;

// ─────────────────────────────────────────────────────────────────────────────

// Shared, fully-wired agent service.
// // All fields are `Arc<_>` so `AgentCore` itself can be wrapped in `Arc<AgentCore>`
// and cloned freely across async task boundaries.
pub struct AgentCore {
    pub config:        Arc<MiraConfig>,
    pub provider:      Arc<dyn ModelProvider>,
    pub memory:        Arc<MemorySystem>,
    pub tools:         Arc<ToolRegistry>,
    pub sessions:      Arc<SessionStore>,
    // System prompt template. Wrapped in an `RwLock` so admins can hot-
    // reload it from the system wiki at runtime (Slice F). All reads
    // take a brief shared lock and clone the string — the prompt is
    // small (~1–2 KB) so this is cheap.
    system_prompt:     std::sync::RwLock<String>,
    tool_mode:         ToolMode,
    max_tool_rounds:   usize,
    max_context_turns: usize,
    // Installed by the Gateway after auth is built so the memory context
    // hook can resolve a user's group memberships. Absent in tests and
    // channel-only builds — falls back to empty group list.
    auth:              OnceLock<Arc<LocalAuthService>>,
    // Installed by the Gateway after the event bus is created, so internal
    // emitters (chat handler, tool loop, periodic tasks) can publish without
    // a direct dependency on the Gateway. Absent in unit tests — emitters
    // must no-op when this is `None`.
    event_bus:         OnceLock<Arc<EventBus>>,
    // Installed by the Gateway when the wiki feature is enabled. The
    // registry resolves per-user `WikiSystem` instances lazily.
    // Absent in unit tests and channel-only builds — the wiki hook
    // no-ops when this is `None`.
    wiki:              OnceLock<Arc<WikiRegistry>>,
    // Installed by the Gateway when companion mode is wired. Used
    // by the pre-hook (chit-chat detection → casual-mode addendum)
    // and the post-hook (engagement assessor fire-and-forget).
    // Absent in unit tests — companion hooks no-op when `None`.
    companion:         OnceLock<Arc<crate::companion::CompanionSystem>>,
    // Q1.6 — the same dispatcher the scheduler uses, stashed here so
    // the HTTP "send briefing now" endpoint can reach it without
    // rebuilding the full snapshot/auth/channel wiring inline.
    // Absent when companion isn't installed.
    companion_dispatcher: OnceLock<Arc<crate::companion::dispatcher::CompanionDispatcher>>,
    // Reasoning auto-routing target (roadmap #13). Installed by the Gateway
    // when `agent.reasoning.enabled` and a provider is configured. When a
    // turn's router decides it's "hard", this provider answers instead of the
    // default. Absent → routing is inert and turns use the default provider.
    reasoning_provider: OnceLock<Arc<dyn ModelProvider>>,
    // Cheap classifier provider for the hybrid router's ambiguous-turn
    // fallback (Slice C). Absent → ambiguous turns are classified with the
    // default provider.
    classifier_provider: OnceLock<Arc<dyn ModelProvider>>,
    // Installed by the Gateway so a turn can rehydrate its in-memory session
    // from persisted conversation history on a cache miss (process restart /
    // idle eviction). Absent in unit tests and channel-only builds — turns
    // then run with whatever the in-memory `SessionStore` holds (no seeding).
    history:           OnceLock<Arc<HistoryStore>>,
}

impl AgentCore {
    // ── Construction ─────────────────────────────────────────────────────────

    // Build an `AgentCore` from fully-constructed components.
    //     // The system prompt is loaded from `config.agent.system_prompt_file` when
    // the file exists; otherwise the built-in `DEFAULT_SYSTEM_PROMPT` is used.
    pub fn new(
        config:   Arc<MiraConfig>,
        provider: Arc<dyn ModelProvider>,
        memory:   Arc<MemorySystem>,
        tools:    Arc<ToolRegistry>,
        sessions: Arc<SessionStore>,
    ) -> Self {
        let system_prompt = load_system_prompt(&config);
        let tool_mode     = ToolMode::from_str(&config.agent.tool_mode);
        let max_rounds    = config.agent.max_tool_rounds;
        let max_turns     = config.agent.max_context_turns;

        info!(
            "AgentCore ready — provider='{}' tool_mode='{:?}' max_tool_rounds={} max_context_turns={}",
            provider.name(), tool_mode, max_rounds, max_turns
        );

        Self {
            config,
            provider,
            memory,
            tools,
            sessions,
            system_prompt: std::sync::RwLock::new(system_prompt),
            tool_mode,
            max_tool_rounds: max_rounds,
            max_context_turns: max_turns,
            auth: OnceLock::new(),
            event_bus: OnceLock::new(),
            wiki: OnceLock::new(),
            companion: OnceLock::new(),
            companion_dispatcher: OnceLock::new(),
            reasoning_provider: OnceLock::new(),
            classifier_provider: OnceLock::new(),
            history: OnceLock::new(),
        }
    }

    // Current system prompt. Cloned each call; cheap (a few KB).
    pub fn system_prompt(&self) -> String {
        self.system_prompt
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    // Swap in a new system prompt. Used by the admin endpoint that
    // edits the system wiki's `persona.md` and by [`Self::reload_system_prompt_from_wiki`].
    pub fn set_system_prompt(&self, new: String) {
        if let Ok(mut guard) = self.system_prompt.write() {
            *guard = new;
        }
    }

    // Read `persona.md` from the system wiki and use its body as the
    // runtime system prompt. Returns `Ok(true)` if a non-empty body was
    // loaded; `Ok(false)` if the wiki is not installed, the page is
    // missing, or the body is empty (the existing prompt is kept).
    pub fn reload_system_prompt_from_wiki(&self) -> Result<bool, crate::wiki::WikiError> {
        let Some(registry) = self.wiki.get() else {
            return Ok(false);
        };
        let system_wiki = registry.system()?;
        let persona_path = crate::wiki::WikiPath::parse("persona.md")
            .expect("persona.md is a valid WikiPath");
        let Some(page) = system_wiki.store().try_read_page(&persona_path)? else {
            return Ok(false);
        };
        let body = page.body.trim();
        if body.is_empty() {
            return Ok(false);
        }
        self.set_system_prompt(body.to_string());
        Ok(true)
    }

    // Install the auth service (called once by the Gateway after auth is built).
    // Lets the memory hook resolve group memberships for scoped retrieval.
    pub fn set_auth(&self, auth: Arc<LocalAuthService>) {
        let _ = self.auth.set(auth);
    }

    // Install the persisted-history store so turns can rehydrate their
    // in-memory session from stored conversation messages on a cache miss.
    // Called once by the Gateway. Optional — when absent, no seeding occurs.
    pub fn set_history(&self, history: Arc<HistoryStore>) {
        let _ = self.history.set(history);
    }

    // Run a single **MIRA-Guardian** turn (built-in watchdog persona + Ring-0
    // read-only tools, on the local guardian model) and return its text.
    // Fail-closed (§5): errors if the Guardian is `off` or its model isn't
    // local. Used by the proactive watch loop (P3) and any internal Guardian
    // invocation. Memory/wiki hooks are skipped — Guardian turns aren't the
    // user's conversation.
    pub async fn run_guardian_turn(self: &Arc<Self>, user_id: &str, task: &str)
        -> Result<String, MiraError>
    {
        use crate::agent::guardian;
        let gmode = guardian::mode(&self.config);
        if gmode == guardian::GuardianMode::Off {
            return Err(MiraError::ConfigError("guardian disabled (guardian.mode=off)".into()));
        }
        let chk = guardian::model_check(&self.config);
        if !chk.allowed {
            return Err(MiraError::ConfigError(format!("guardian refused (fail-closed): {}", chk.reason)));
        }
        // Build the Guardian's *local* provider from its alias (or primary
        // fallback — already confirmed local by `model_check`).
        let (prov, model) = self.config.agent.llm_aliases.get(guardian::GUARDIAN_ALIAS)
            .map(|a| (a.provider.clone(), a.model.clone()))
            .unwrap_or_else(|| (self.config.primary_provider.clone(), None));
        let provider = crate::agent::named_agent::build_provider_for_alias(
            &self.config, &prov, model.as_deref(),
        )?;

        let def = guardian::definition();
        let ctx = TurnContext {
            system_prompt_override: Some(def.system_prompt.clone()),
            // Ring-0 always; the Ring-1 propose tool is added only in active mode.
            allowed_tool_names:     Some(guardian::tools_for_mode(gmode)),
            skip_memory_hooks:      true,
            skip_wiki_hooks:        true,
            ..TurnContext::default()
        };
        let mut rx = self
            .process_with_context("guardian-watch", user_id, "guardian", task, Some(provider), ctx)
            .await?;
        let mut out = String::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                crate::agent::stream::StreamEvent::Token(t) => out.push_str(&t),
                crate::agent::stream::StreamEvent::Done { .. } => break,
                crate::agent::stream::StreamEvent::Error(e) => return Err(MiraError::ConfigError(e)),
                _ => {}
            }
        }
        Ok(out)
    }

    // Resolve the session for this turn, rehydrating its replay buffer from
    // the persisted history DB on a cache miss when the caller supplied a
    // `conversation_id` and a history store is installed.
    //     // The in-memory `SessionStore` is a cache that a process restart or the
    // 1-hour idle eviction wipes; the history DB is the source of truth. Without
    // this, continuing a conversation after a restart would start the agent
    // with an empty context even though the stored history (and the UI) still
    // show every prior turn. A still-live session is returned untouched.
    async fn session_for_turn(
        &self,
        session_id: &str,
        user_id:    &str,
        channel:    &str,
        input:      &str,
        context:    &TurnContext,
    ) -> SessionData {
        let (Some(conv_id), Some(history)) =
            (context.conversation_id.as_deref(), self.history.get())
        else {
            return self.sessions
                .get_or_create(session_id.to_string(), user_id.to_string(), channel.to_string())
                .await;
        };

        // Seed with the window the agent replays (max_context_turns*2); +2
        // leaves room to drop the current message below without starving it.
        let seed_limit = (self.max_context_turns * 2 + 2) as i64;
        let mut seed: Vec<crate::session::ConversationTurn> = history
            .get_recent_messages(conv_id, seed_limit)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                // Only user/assistant turns are replayed into the LLM context;
                // system/tool rows are reconstructed per-turn, not seeded.
                let role = match m.role {
                    crate::history::MessageRole::User      => "user",
                    crate::history::MessageRole::Assistant => "assistant",
                    _ => return None,
                };
                Some(crate::session::ConversationTurn {
                    role:      role.to_owned(),
                    content:   m.content,
                    timestamp: m.created_at.max(0) as u64,
                })
            })
            .collect();
        // Some callers (web chat) persist the current user message to history
        // BEFORE the turn runs; others (channels) persist AFTER. Drop a trailing
        // user turn equal to the current input so the former case doesn't
        // double-count it — run_turn appends the current input itself.
        if matches!(seed.last(), Some(t) if t.role == "user" && t.content == input) {
            seed.pop();
        }
        self.sessions
            .get_or_create_seeded(
                session_id.to_string(), user_id.to_string(), channel.to_string(), seed,
            )
            .await
    }

    // Install the reasoning auto-routing provider (roadmap #13). Called once
    // by the Gateway when `agent.reasoning.enabled` and a provider resolves.
    pub fn set_reasoning_provider(&self, provider: Arc<dyn ModelProvider>) {
        let _ = self.reasoning_provider.set(provider);
    }

    // Install the cheap classifier provider for the hybrid router's
    // ambiguous-turn fallback (Slice C). Optional — falls back to the default
    // provider when absent.
    pub fn set_classifier_provider(&self, provider: Arc<dyn ModelProvider>) {
        let _ = self.classifier_provider.set(provider);
    }

    // Install the wiki registry (called once by the Gateway when the wiki
    // feature is enabled). After installation, the agent's system prompt
    // is reloaded from the system wiki's `persona.md` if it exists and
    // has a non-empty body — making the wiki the source of truth at
    // startup. Returns `Err(())` if a registry was already installed.
    pub fn set_wiki(&self, wiki: Arc<WikiRegistry>) -> Result<(), ()> {
        self.wiki.set(wiki).map_err(|_| ())?;
        match self.reload_system_prompt_from_wiki() {
            Ok(true)  => info!("System prompt loaded from system wiki persona.md"),
            Ok(false) => debug!("System wiki has no persona.md body; keeping default prompt"),
            Err(e)    => warn!("Failed to load system prompt from wiki: {e}"),
        }
        Ok(())
    }

    // Borrow the installed wiki registry, if any.
    pub fn wiki(&self) -> Option<&Arc<WikiRegistry>> { self.wiki.get() }

    // Install the companion system (of companion mode).
    // Returns `Err(())` if one was already installed.
    pub fn set_companion(&self, sys: Arc<crate::companion::CompanionSystem>) -> Result<(), ()> {
        self.companion.set(sys).map_err(|_| ())
    }

    // Borrow the installed companion system, if any.
    pub fn companion(&self) -> Option<&Arc<crate::companion::CompanionSystem>> {
        self.companion.get()
    }

    // Q1.6 — install the companion dispatcher so HTTP endpoints can
    // reach it. Returns `Err(())` if one was already installed.
    pub fn set_companion_dispatcher(
        &self,
        d: Arc<crate::companion::dispatcher::CompanionDispatcher>,
    ) -> Result<(), ()> {
        self.companion_dispatcher.set(d).map_err(|_| ())
    }

    // Borrow the installed companion dispatcher, if any.
    pub fn companion_dispatcher(&self)
        -> Option<&Arc<crate::companion::dispatcher::CompanionDispatcher>>
    {
        self.companion_dispatcher.get()
    }

    // Install the event bus (called once by the Gateway after the bus is
    // created). Returns `Err(())` if a bus was already installed.
    pub fn set_event_bus(&self, bus: Arc<EventBus>) -> Result<(), ()> {
        self.event_bus.set(bus).map_err(|_| ())
    }

    // Borrow the installed event bus, if any. Internal emitters call this
    // and silently no-op when the bus is absent (e.g. in unit tests).
    pub fn event_bus(&self) -> Option<&Arc<EventBus>> {
        self.event_bus.get()
    }

    // Resolve the caller's group ids. Returns empty when auth isn't wired up
    // or the lookup fails — always safe to call.
    fn resolve_groups(&self, user_id: &str) -> Vec<String> {
        self.auth.get()
            .and_then(|a| a.list_user_group_ids(user_id).ok())
            .unwrap_or_default()
    }

    // ── Public API ───────────────────────────────────────────────────────────

    // Process one user turn and return a live event stream.
    //     // The returned receiver begins receiving [`StreamEvent`]s immediately.
    // The reasoning loop runs in a background task; the caller does not need
    // to await the loop — it just awaits events on the receiver.
    //     // # Session handling
    // `session_id` identifies the conversation thread (e.g. `"tg-12345"` for
    // a Telegram chat). `user_id` and `channel` are stored in the session for
    // memory namespacing and auditing.
    pub async fn process(
        self: &Arc<Self>,
        session_id: &str,
        user_id:    &str,
        channel:    &str,
        input:      &str,
    ) -> Result<mpsc::Receiver<StreamEvent>, MiraError> {
        self.process_with_provider(session_id, user_id, channel, input, None).await
    }

    // Like `process`, but uses `provider_override` for this turn instead of
    // the agent's default provider. Used by the web chat handler when the
    // user selects a specific model from the dropdown.
    pub async fn process_with_provider(
        self: &Arc<Self>,
        session_id:        &str,
        user_id:           &str,
        channel:           &str,
        input:             &str,
        provider_override: Option<Arc<dyn ModelProvider>>,
    ) -> Result<mpsc::Receiver<StreamEvent>, MiraError> {
        self.process_with_context(
            session_id, user_id, channel, input, provider_override, TurnContext::default()
        ).await
    }

    // Like `process_with_provider`, but with a [`TurnContext`] that can
    // override the system prompt, restrict the tool set, and inject extra
    // args into every tool call. Used by the chat handler for
    // `mode=onboarding` conversations.
    pub async fn process_with_context(
        self: &Arc<Self>,
        session_id:        &str,
        user_id:           &str,
        channel:           &str,
        input:             &str,
        provider_override: Option<Arc<dyn ModelProvider>>,
        mut context:       TurnContext,
    ) -> Result<mpsc::Receiver<StreamEvent>, MiraError> {
        // Trusted channel injection. Tools that route notifications back
        // to the user (e.g. `spawn_background_task`) need to know which
        // channel the turn came in on so completion messages land in
        // the same place. Overwrites any value the model tries to pass.
        if !channel.is_empty() {
            context.inject_tool_args.insert(
                "_channel".to_string(),
                serde_json::Value::String(channel.to_string()),
            );
        }

        let (tx, rx) = mpsc::channel::<StreamEvent>(512);

        let session = self
            .session_for_turn(session_id, user_id, channel, input, &context)
            .await;

        let core         = Arc::clone(self);
        let input_s      = input.to_string();
        let user_id_s    = user_id.to_string();
        let channel_s    = channel.to_string();
        let session_id_s = session_id.to_string();
        // Reasoning auto-routing (roadmap #13): an explicit `provider_override`
        // always wins; otherwise, when enabled and a reasoning provider is
        // installed, route "hard" turns to it (heuristic stage).
        // Reasoning auto-routing (roadmap #13). Explicit `provider_override`
        // always wins. Otherwise, when enabled with a reasoning provider
        // installed, triage the turn: a hard signal routes up immediately; a
        // clearly-trivial turn keeps the default; an ambiguous turn consults
        // the cheap classifier (the hybrid fallback — the only model call).
        let mut routed_up = false;
        let effective_provider = if let Some(p) = provider_override {
            p
        } else if let (true, Some(rp)) =
            (self.config.agent.reasoning.enabled, self.reasoning_provider.get())
        {
            use crate::agent::routing::Triage;
            let up = match crate::agent::routing::triage(input, self.config.agent.reasoning.min_chars) {
                Triage::Up(reason) => {
                    info!("reasoning router: up ({reason}) → '{}'", rp.name());
                    true
                }
                Triage::Down(_) => false,
                Triage::Ambiguous(_) => {
                    let clf = self.classifier_provider.get().unwrap_or(&self.provider);
                    let hard = crate::agent::routing::classify_ambiguous(clf, input).await;
                    info!("reasoning router: ambiguous → classifier {}", if hard { "up" } else { "keep" });
                    hard
                }
            };
            if up { routed_up = true; Arc::clone(rp) } else { Arc::clone(&core.provider) }
        } else {
            Arc::clone(&core.provider)
        };
        // Slice B: a routed-up turn also gets the configured reasoning effort,
        // which flows into the turn's GenerationOptions (→ OpenAI
        // reasoning_effort / Anthropic thinking budget).
        if routed_up {
            context.reasoning_effort = Some(self.config.agent.reasoning.effort.clone());
        }

        tokio::spawn(async move {
            let result = core
                .run_turn(session, &input_s, &user_id_s, &channel_s, &session_id_s,
                          &effective_provider, &tx, &context)
                .await;

            if let Err(e) = result {
                let _ = tx.send(StreamEvent::Error(e.to_string())).await;
            }
        });

        Ok(rx)
    }

    // Run the full reasoning turn synchronously (called from within the
    // spawned task). Sends all events to `tx`.
    async fn run_turn(
        &self,
        session:    SessionData,
        input:      &str,
        user_id:    &str,
        channel:    &str,
        session_id: &str,
        provider:   &Arc<dyn ModelProvider>,
        tx:         &mpsc::Sender<StreamEvent>,
        context:    &TurnContext,
    ) -> Result<(), MiraError> {

        // ── 1. Memory + identity pre-hook ─────────────────────────────────────
        //
        // Profile injection only runs when the caller didn't supply a custom
        // system prompt — the web chat handler already builds its own
        // per-user preamble via [`ProfilePreambleCache`] and passes it as
        // `system_prompt_override`, so injecting again would duplicate
        // identity facts. Channels (Signal/Telegram) take the simpler
        // identity block since they don't go through the web preamble path.
        let profile_block = if context.system_prompt_override.is_some() {
            String::new()
        } else {
            memory_hook::profile_hook(self.auth.get(), user_id)
        };
        let memory_context = if context.skip_memory_hooks {
            String::new()
        } else {
            let groups = self.resolve_groups(user_id);
            memory_hook::pre_hook(&self.memory, user_id, &groups, input, self.config.memory.context_top_k).await
        };
        // Wiki context — runs after the memory hook so narrative pages anchor
        // the conversation *before* atomic facts are injected. Failures here
        // are always non-fatal; a missing or empty wiki yields an empty
        // string and the turn proceeds.
        let wiki_result = if context.skip_wiki_hooks || context.system_prompt_override.is_some() {
            wiki_hook::WikiContextResult::default()
        } else {
            match self.wiki.get() {
                Some(registry) => match registry.for_user(user_id) {
                    Ok(wiki) => wiki_hook::pre_hook(&wiki, input).await,
                    Err(e) => {
                        tracing::warn!("wiki: open failed for user '{}' (non-fatal): {}", user_id, e);
                        wiki_hook::WikiContextResult::default()
                    }
                },
                None => wiki_hook::WikiContextResult::default(),
            }
        };
        // Slice H — emit a one-shot event so the chat UI can render
        // pills for the pages that fed this turn, before any tokens
        // arrive.
        if !wiki_result.loaded_pages.is_empty() {
            let _ = tx.send(StreamEvent::WikiContext {
                pages: wiki_result.loaded_pages.clone(),
            }).await;
        }
        let wiki_context = wiki_result.block;

        // ── 2. Build message list ─────────────────────────────────────────────
        //
        // Assembly order is load-bearing: profile (identity), then wiki
        // (narrative knowledge), then memory (atomic facts). Wiki goes
        // before memory so curated pages anchor the agent before specific
        // facts arrive; if both want the same fact, the wiki version wins
        // by being seen first.
        // Snapshot the current prompt up front so we hold the lock for
        // exactly one clone, not for the duration of the turn.
        let owned_prompt: String;
        let base_prompt: &str = match context.system_prompt_override.as_deref() {
            Some(s) => s,
            None    => { owned_prompt = self.system_prompt(); &owned_prompt }
        };
        // Companion-mode addenda (Slices 3 + 4):
        // - `companion_addendum` is the casual-conversation nudge that
        // fires when the user's message looks like chit-chat
        //.
        // - `safety_addendum` is the non-overridable safety floor.
        // It always appends when companion is active so the model
        // gets the rules on every turn. Goes LAST so it has final
        // say in the system prompt.
        let companion_active = self.companion.get()
            .filter(|_| context.system_prompt_override.is_none())
            .map(|sys| sys.is_active(user_id))
            .unwrap_or(false);

        let companion_addendum: String = if companion_active
            && crate::companion::chitchat::classify(input)
                == crate::companion::chitchat::Intent::Chat
        {
            crate::companion::chitchat::casual_mode_addendum().to_string()
        } else {
            String::new()
        };
        let safety_addendum: &'static str = if companion_active {
            crate::companion::safety::SAFETY_ADDENDUM
        } else {
            ""
        };

        let effective_system = if profile_block.is_empty()
            && wiki_context.is_empty()
            && memory_context.is_empty()
            && companion_addendum.is_empty()
            && safety_addendum.is_empty()
        {
            base_prompt.to_string()
        } else {
            format!(
                "{}{}{}{}{}{}",
                base_prompt, profile_block, wiki_context, memory_context,
                companion_addendum, safety_addendum,
            )
        };

        // Channel-context hint. Without this the model has no idea whether
        // the user is currently on web, Signal, or Telegram, so when it sets
        // up an automation that posts back to them it always defaults to
        // `channel=web` — and a Signal user gets nothing on their phone.
        // Suppressed for the `user` channel (a synthetic channel used by
        // automations.dispatch::run_prompt itself, where the model should
        // act normally without re-targeting itself), and skipped entirely
        // when the caller passed `system_prompt_override` (those flows —
        // notably onboarding — supply their own complete context).
        let channel_hint = if context.system_prompt_override.is_none()
            && channel != "user"
            && !channel.is_empty()
        {
            format!(
                "\n\n## Current channel\n\
                 You are currently chatting with the user via the `{channel}` channel. \
                 When you set up an automation, webhook, or scheduled follow-up that \
                 posts a message back to the user, default `channel=\"{channel}\"` \
                 unless they explicitly ask for a different one. Only use \
                 `channel=\"web\"` when the user is on the web UI or asks for the \
                 result to land in the in-app inbox."
            )
        } else {
            String::new()
        };
        let effective_system = if channel_hint.is_empty() {
            effective_system
        } else {
            format!("{effective_system}{channel_hint}")
        };

        // Truthful self-identification. The model has no idea what it actually
        // is — local/open models routinely claim to be GPT-4 or Claude — so we
        // tell it the real configured model + provider. Skipped for overrides
        // (onboarding etc. supply their own context).
        let identity_hint = if context.system_prompt_override.is_none() {
            runtime_identity_hint(&self.config)
        } else {
            String::new()
        };
        let effective_system = if identity_hint.is_empty() {
            effective_system
        } else {
            format!("{effective_system}{identity_hint}")
        };

        let mut messages: Vec<ChatMessage> = Vec::new();
        messages.push(ChatMessage::system(effective_system));

        // Inject recent conversation history (bounded by max_context_turns).
        let history = session.to_messages();
        let skip = history.len().saturating_sub(self.max_context_turns * 2);
        messages.extend_from_slice(&history[skip..]);

        // Add the current user message.
        let mut user_msg = ChatMessage::user(input.to_string());
        if !context.attachments.is_empty() {
            user_msg.attachments = Some(context.attachments.clone());
        }
        messages.push(user_msg);

        // Drop any leading assistant/tool turns before the first user message
        // (keeping the system prompt). Onboarding seeds an assistant greeting
        // for the UI, which would otherwise make the transcript start
        // system → assistant → … — a shape some local-model chat templates
        // reject ("No user query found in messages"). The greeting's intent is
        // folded into the onboarding system prompt instead. Normal chat never
        // leads with an assistant turn, so this is a no-op there.
        if let Some(first_user) = messages.iter().position(|m| m.role == crate::types::MessageRole::User) {
            let mut idx = 0usize;
            messages.retain(|m| {
                let keep = m.role == crate::types::MessageRole::System || idx >= first_user;
                idx += 1;
                keep
            });
        }

        debug!(
            "AgentCore: session='{}' history_turns={} total_messages={}",
            session_id,
            history.len() / 2,
            messages.len()
        );

        // ── 3. Tool loop ──────────────────────────────────────────────────────
        let options = GenerationOptions {
            reasoning_effort: context.reasoning_effort.clone(),
            ..GenerationOptions::default()
        };

        let event_ctx = tool_loop::ToolEventCtx {
            bus:     self.event_bus.get(),
            user_id,
        };

        let (response_text, usage) = tool_loop::run_tool_loop_with_context(
            provider,
            &self.tools,
            &mut messages,
            &options,
            &self.tool_mode,
            self.max_tool_rounds,
            tx,
            context.allowed_tool_names.as_deref(),
            &context.inject_tool_args,
            event_ctx,
        ).await?;

        // ── 4. Emit Done ──────────────────────────────────────────────────────
        let _ = tx.send(StreamEvent::Done { usage }).await;

        // ── 5. Persist session ────────────────────────────────────────────────
        let mut updated = session;
        updated.add_turn("user",      input.to_string());
        updated.add_turn("assistant", response_text.clone());
        updated.truncate_history(self.max_context_turns * 2);
        self.sessions.update(updated).await;

        // ── 6. Memory post-hook (fire-and-forget) ─────────────────────────────
        //
        // Per-channel extraction dispatch (see `AutoExtractConfig`). `mode =
        // "off"` disables all extraction. A channel runs the richer LLM
        // extractor when it's listed in `auto_extract.llm_channels` (or globally
        // when `mode = "llm"`); every other channel uses the cheap heuristic.
        // Onboarding turns (`skip_memory_hooks`) opt out entirely.
        //
        // The web chat handler runs its own LLM extractor post-stream (it uses
        // the per-turn model the user picked), so for "web" we don't double-
        // extract here — core drives only web's heuristic and the full dispatch
        // for every other channel.
        if !context.skip_memory_hooks {
            use crate::config::ExtractorKind;
            let ax     = &self.config.memory.auto_extract;
            let is_web = channel.eq_ignore_ascii_case("web");
            match ax.effective_extractor(channel) {
                ExtractorKind::Off => {}
                ExtractorKind::Llm if is_web => {} // web handler owns LLM extraction
                ExtractorKind::Llm => {
                    let memory   = Arc::clone(&self.memory);
                    let provider = Arc::clone(provider);
                    let cats     = ax.allowed_categories.clone();
                    let min_conf = crate::memory::auto_extract::ConfidenceTier::parse(
                        &ax.min_confidence,
                    );
                    let uid  = user_id.to_string();
                    let chan = channel.to_string();
                    let conv = context.conversation_id.clone();
                    let umsg = input.to_string();
                    let amsg = response_text.clone();
                    tokio::spawn(async move {
                        memory.auto_extract_llm_and_store(
                            &provider, &umsg, &amsg, &uid, &chan,
                            conv.as_deref(), None, &cats, min_conf,
                        ).await;
                    });
                }
                ExtractorKind::Heuristic => {
                    memory_hook::post_hook(
                        Arc::clone(&self.memory),
                        user_id.to_string(),
                        channel.to_string(),
                        input.to_string(),
                        response_text.clone(),
                    );
                }
            }
        }

        // ── 7. Companion engagement post-hook (, fire-and-forget) ─────
        //
        // When companion mode is active for the user, classify the
        // turn (engaged/brief/declined/distressed) and log the
        // label. The scheduler reads this for cadence adjustment.
        // Skipped on system_prompt_override flows (onboarding) and
        // when companion isn't active for the user.
        if !context.system_prompt_override.is_some() {
            if let Some(companion) = self.companion.get() {
                if companion.is_active(user_id) {
                    // Resolve tz once for the post-hook so the
                    // engagement entry's hour_of_day / day_of_week
                    // line up with the user's local clock.
                    let tz = self.auth.get()
                        .and_then(|a| a.get_profile(user_id).ok().flatten())
                        .and_then(|p| p.timezone);
                    // Build the safety floor handle so a Distressed
                    // label triggers escalation. History +
                    // notifications need wiring at the gateway level
                    // (AgentCore doesn't hold them directly); we
                    // build a minimal SafetyFloor that the engagement
                    // post-hook can call. When the gateway didn't
                    // wire history into the companion, the safety
                    // floor's `deliver` returns DeliveryFailed —
                    // recorded in the audit log either way.
                    let safety = crate::companion::safety::SafetyFloor {
                        log: companion.safety_log_arc(),
                        store: companion.store_arc(),
                        history: companion.history_arc(),
                        auth: self.auth.get().map(Arc::clone),
                        notifications: companion.notifications_arc(),
                        groups: Some(companion.groups_arc()),
                    };
                    let assessor = crate::companion::engagement::EngagementAssessor {
                        provider: Arc::clone(provider),
                        log: companion.engagement_arc(),
                        safety: Some(safety),
                    };
                    crate::companion::engagement::spawn_post_hook(
                        assessor,
                        user_id.to_string(),
                        Some(session_id.to_string()),
                        Some(uuid::Uuid::now_v7().to_string()),
                        input.to_string(),
                        response_text.clone(),
                        tz,
                    );
                }
            }
        }

        // ── 8. Wiki post-hook (fire-and-forget) ───────────────────────────────
        //
        // LLM extractor that derives narrative observations and files them
        // on wiki pages. Mode-gated: `"review"` (default) lands ops in the
        // pending review queue; `"auto"` applies immediately; `"off"`
        // skips entirely. Skipped when the caller set skip_wiki_hooks
        // (onboarding, ephemeral chats).
        if !context.skip_wiki_hooks
            && self.config.wiki.enabled
            && !self.config.wiki.auto_extract.mode.eq_ignore_ascii_case("off")
        {
            if let Some(registry) = self.wiki.get() {
                let turn_id = uuid::Uuid::now_v7().to_string();
                wiki_hook::post_hook(
                    Arc::clone(registry),
                    Arc::clone(provider),
                    user_id.to_string(),
                    session_id.to_string(),
                    turn_id,
                    input.to_string(),
                    response_text,
                    self.config.wiki.auto_extract.clone(),
                );
            }
        }

        Ok(())
    }

    // ── Utilities ────────────────────────────────────────────────────────────

    // Return `true` if the provider is healthy.
    pub async fn health_check(&self) -> bool {
        self.provider.health_check().await
    }

    // Convenience: collect all tokens from a process() receiver into a String.
    // Useful in tests and for non-streaming callers (Telegram, Signal handlers).
    pub async fn collect_response(
        mut rx: mpsc::Receiver<StreamEvent>,
    ) -> (String, Vec<StreamEvent>) {
        let mut tokens  = String::new();
        let mut all     = Vec::new();
        let mut had_error = false;
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Token(ref t) = event { tokens.push_str(t); }
            if matches!(event, StreamEvent::Error(_)) { had_error = true; }
            let is_done = matches!(event, StreamEvent::Done { .. } | StreamEvent::Error(_));
            all.push(event);
            if is_done { break; }
        }
        // Empty-final-turn guard. Some (esp. local) models return no text after
        // a successful tool call — which on a messaging channel surfaces as a
        // blank "MIRA" bubble (the user thinks it froze). When the model said
        // nothing but tools ran cleanly, synthesize a short confirmation from
        // the tool results so the user always gets a reply. Skipped on an error
        // turn (that path reports the failure itself).
        if tokens.trim().is_empty() && !had_error {
            if let Some(confirmation) = synthesize_tool_confirmation(&all) {
                tokens = confirmation;
            }
        }
        (tokens, all)
    }
}

// Build a brief confirmation from a turn's *successful* tool calls — used by
// `collect_response` when the model produced no text of its own, so a messaging
// channel never sends a blank message. Returns `None` when no tool ran (nothing
// to confirm; the caller keeps the empty string).
fn synthesize_tool_confirmation(events: &[StreamEvent]) -> Option<String> {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for e in events {
        if let StreamEvent::ToolResult { name, success: true, .. } = e {
            *counts.entry(name.as_str()).or_default() += 1;
        }
    }
    if counts.is_empty() { return None; }

    // Friendly phrasing for the common write tools; other tools fall through to
    // a plain "Done." so we never dump raw tool output at the user.
    let phrase = |name: &str, n: u32| -> Option<String> {
        Some(match name {
            "calendar_create_event" =>
                if n == 1 { "added an event to your calendar".into() }
                else { format!("added {n} events to your calendar") },
            "calendar_update_event" =>
                if n == 1 { "updated a calendar event".into() }
                else { format!("updated {n} calendar events") },
            "calendar_delete_event" =>
                if n == 1 { "removed a calendar event".into() }
                else { format!("removed {n} calendar events") },
            "automations" =>
                if n == 1 { "set up a reminder".into() }
                else { format!("set up {n} reminders") },
            "wiki_write_page" | "wiki_append_section" | "wiki_log_entry" =>
                "saved a note".into(),
            "memory_supersede" => "updated my memory".into(),
            _ => return None,
        })
    };
    let parts: Vec<String> = counts.iter().filter_map(|(n, c)| phrase(n, *c)).collect();
    if parts.is_empty() {
        Some("✅ Done.".to_string())
    } else {
        Some(format!("✅ Done — I {}.", human_join(&parts)))
    }
}

// Join phrases as "a", "a and b", or "a, b, and c".
fn human_join(parts: &[String]) -> String {
    match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        2 => format!("{} and {}", parts[0], parts[1]),
        _ => format!("{}, and {}", parts[..parts.len() - 1].join(", "), parts[parts.len() - 1]),
    }
}

// ─────────────────────────────────────────────────────────────────────────────

// Build the "Your runtime" system-prompt block stating the real model +
// provider, so MIRA self-identifies truthfully instead of hallucinating
// (open/local models love to claim they're GPT-4 or Claude). Derived from the
// configured primary provider + its model. Returns "" for an unknown primary or
// an unset model. Reflects the default/primary path — a per-conversation model
// override or a live failover to a fallback isn't echoed here.
fn runtime_identity_hint(config: &MiraConfig) -> String {
    let p = &config.providers;
    let (label, model): (&str, &str) = match config.primary_provider.as_str() {
        "lmstudio"   => ("LM Studio (a local model server)", p.lmstudio.default_model.as_str()),
        "ollama"     => ("Ollama (a local model server)",    p.ollama.default_model.as_str()),
        "openrouter" => ("OpenRouter",                       p.openrouter.default_model.as_str()),
        "anthropic"  => ("Anthropic's API",                  p.anthropic.default_model.as_str()),
        "gemini"     => ("Google's Gemini API",              p.gemini.default_model.as_str()),
        "openai"     => ("the OpenAI API",                   p.openai.default_model.as_str()),
        "deepseek"   => ("the DeepSeek API",                 p.deepseek.default_model.as_str()),
        "moonshot"   => ("the Moonshot API",                 p.moonshot.default_model.as_str()),
        "groq"       => ("Groq",                             p.groq.default_model.as_str()),
        "xai"        => ("xAI",                              p.xai.default_model.as_str()),
        _ => return String::new(),
    };
    let model = model.trim();
    if model.is_empty() { return String::new(); }
    // Gemini stores model ids with a `models/` resource prefix — drop it for display.
    let model = model.strip_prefix("models/").unwrap_or(model);
    format!(
        "\n\n## Your runtime (factual)\n\
         You are currently served by the model `{model}` running via {label}. \
         If the user asks what model or which provider you are, answer truthfully \
         with this — do not guess or claim to be a different model (such as GPT-4 \
         or Claude) unless that is literally the model named here.",
    )
}

fn load_system_prompt(config: &MiraConfig) -> String {
    if config.agent.system_prompt_file.is_empty() {
        return DEFAULT_SYSTEM_PROMPT.to_string();
    }

    let path = crate::config::expand_path(&config.agent.system_prompt_file);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            info!("Loaded system prompt from {:?}", path);
            content
        }
        Err(e) => {
            warn!(
                "Cannot read agent system prompt '{}': {} — using built-in default",
                path.display(), e
            );
            DEFAULT_SYSTEM_PROMPT.to_string()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use crate::types::{GenerationResponse, TokenUsage, ProviderId};

    // ── Mock provider ─────────────────────────────────────────────────────────

    struct EchoProvider(String);

    #[async_trait]
    impl ModelProvider for EchoProvider {
        fn name(&self) -> &str { "echo" }
        async fn generate(&self, _m: &[ChatMessage], _o: &GenerationOptions)
            -> Result<GenerationResponse, MiraError>
        {
            Ok(GenerationResponse {
                content:     self.0.clone(),
                tool_calls:  None,
                reasoning:   None,
                usage:       TokenUsage::default(),
                provider_id: ProviderId::Local("echo".to_string()),
                model_name:  "echo".to_string(),
                fallback: None,
            })
        }
        async fn health_check(&self) -> bool { true }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    async fn make_core(reply: &str) -> Arc<AgentCore> {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("memory.db");

        let mut cfg = MiraConfig::default();
        cfg.agent.tool_mode     = "disabled".to_string();
        cfg.agent.max_tool_rounds = 4;
        cfg.agent.max_context_turns = 10;
        // Disable semantic embedding for unit tests
        cfg.memory.embedding.provider = "lmstudio".to_string();
        cfg.data_dir = dir.path().to_string_lossy().to_string();

        let config   = Arc::new(cfg);
        let provider = Arc::new(EchoProvider(reply.to_string())) as Arc<dyn ModelProvider>;
        let memory   = Arc::new(
            MemorySystem::new_keyword_only(db_path).expect("memory init")
        );
        let tools    = Arc::new(ToolRegistry::new());
        let sessions = Arc::new(SessionStore::new());

        Arc::new(AgentCore::new(config, provider, memory, tools, sessions))
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    fn tool_ok(name: &str) -> StreamEvent {
        StreamEvent::ToolResult {
            name: name.into(), output: "{}".into(), success: true, call_id: "c".into(),
        }
    }

    #[test]
    fn runtime_identity_hint_names_real_model_and_provider() {
        let mut cfg = MiraConfig::default();
        cfg.primary_provider = "lmstudio".to_string();
        cfg.providers.lmstudio.default_model = "openai/gpt-oss-20b".to_string();
        let hint = runtime_identity_hint(&cfg);
        assert!(hint.contains("openai/gpt-oss-20b"), "got: {hint}");
        assert!(hint.contains("LM Studio"), "got: {hint}");
        assert!(hint.to_lowercase().contains("truthful"), "got: {hint}");

        // Gemini's `models/` prefix is stripped for display.
        cfg.primary_provider = "gemini".to_string();
        cfg.providers.gemini.default_model = "models/gemini-flash-lite-latest".to_string();
        let g = runtime_identity_hint(&cfg);
        assert!(g.contains("gemini-flash-lite-latest") && !g.contains("models/gemini"), "got: {g}");

        // Unknown primary / empty model → no hint (don't fabricate).
        cfg.primary_provider = "something-else".to_string();
        assert!(runtime_identity_hint(&cfg).is_empty());
        cfg.primary_provider = "lmstudio".to_string();
        cfg.providers.lmstudio.default_model = "".to_string();
        assert!(runtime_identity_hint(&cfg).is_empty());
    }

    #[test]
    fn synthesize_confirmation_counts_calendar_events() {
        // Three successful calendar creates with no text → one pluralised line.
        let events = vec![
            tool_ok("calendar_create_event"),
            tool_ok("calendar_create_event"),
            tool_ok("calendar_create_event"),
        ];
        let s = synthesize_tool_confirmation(&events).unwrap();
        assert!(s.contains("added 3 events to your calendar"), "got: {s}");
    }

    #[test]
    fn synthesize_confirmation_handles_mixed_and_unknown_tools() {
        // A known + an unknown tool → the known phrase, never raw output.
        let s = synthesize_tool_confirmation(&[tool_ok("calendar_create_event"), tool_ok("web_search")]).unwrap();
        assert!(s.contains("added an event to your calendar"), "got: {s}");
        // Only an unknown tool → a generic "Done." (never empty, never raw JSON).
        let s2 = synthesize_tool_confirmation(&[tool_ok("web_search")]).unwrap();
        assert_eq!(s2, "✅ Done.");
        // No tools at all → None (caller keeps the empty string).
        assert!(synthesize_tool_confirmation(&[StreamEvent::Token("".into())]).is_none());
        // A failed tool doesn't count as a confirmation.
        let failed = StreamEvent::ToolResult {
            name: "calendar_create_event".into(), output: "boom".into(), success: false, call_id: "c".into(),
        };
        assert!(synthesize_tool_confirmation(&[failed]).is_none());
    }

    #[tokio::test]
    async fn process_returns_echo_as_tokens() {
        let core  = make_core("Hello from MIRA!").await;
        let rx    = core.process("sess-1", "user-1", "cli", "hi").await.unwrap();
        let (txt, _) = AgentCore::collect_response(rx).await;
        assert_eq!(txt, "Hello from MIRA!");
    }

    #[tokio::test]
    async fn process_emits_done_event() {
        let core = make_core("done-test").await;
        let rx   = core.process("sess-2", "user-2", "cli", "test").await.unwrap();
        let (_, events) = AgentCore::collect_response(rx).await;
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done { .. })));
    }

    #[tokio::test]
    async fn session_history_accumulates_across_turns() {
        let core = make_core("reply").await;

        let rx = core.process("sess-3", "u3", "cli", "first").await.unwrap();
        let _ = AgentCore::collect_response(rx).await;

        let rx2 = core.process("sess-3", "u3", "cli", "second").await.unwrap();
        let _ = AgentCore::collect_response(rx2).await;

        // get_or_create returns the existing session if it already exists
        let s = core.sessions.get_or_create("sess-3".to_string(), "u3".to_string(), "cli".to_string()).await;
        // Two turns → 4 history entries (user+assistant × 2)
        assert_eq!(s.conversation_history.len(), 4);
    }

    #[tokio::test]
    async fn health_check_delegates_to_provider() {
        let core = make_core("x").await;
        assert!(core.health_check().await);
    }

    #[test]
    fn default_system_prompt_is_non_empty() {
        assert!(!DEFAULT_SYSTEM_PROMPT.is_empty());
    }

    #[test]
    fn load_system_prompt_uses_default_when_file_not_set() {
        let cfg = MiraConfig::default();
        let prompt = load_system_prompt(&cfg);
        assert_eq!(prompt, DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn load_system_prompt_uses_default_on_missing_file() {
        let mut cfg = MiraConfig::default();
        cfg.agent.system_prompt_file = "/nonexistent/path/agent.md".to_string();
        let prompt = load_system_prompt(&cfg);
        assert_eq!(prompt, DEFAULT_SYSTEM_PROMPT);
    }
}
