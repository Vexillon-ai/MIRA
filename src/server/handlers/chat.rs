// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/chat.rs
//! POST /api/chat — streaming chat over SSE with history persistence and auto-title.

use std::sync::Arc;

use axum::{
    extract::{Json, Path},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Sse},
    Extension,
};
use axum::response::sse::{Event, KeepAlive};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

use crate::agent::{
    AgentCore, CancelOutcome, CancelRequest, CommitOutcome, StreamEvent, TurnContext, TurnRegistry,
};
use crate::auth::{ChatAuth, ChatCancelAuth, LocalAuthService};
use crate::history::{HistoryStore, NewConversation, NewMessage, MessageRole};
use crate::notifications::{NotificationBus, Notification, NotificationKind};
use crate::onboarding::{
    apply_ops, build_onboarding_prompt, extract_updates_from_transcript_with_context,
    OnboardingSchema, ProfilePreambleCache,
};
use crate::server::handlers::onboarding::DataDir;
use crate::providers::{ModelProvider, ProviderRequestContext};
use crate::types::{ChatMessage as ProviderChatMessage, MessageRole as ProviderMessageRole};
use crate::web::LiveConfig;

// ── Request / Response ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ChatRequest {
    pub conversation_id: Option<String>,
    pub message:         String,
    pub model_override:  Option<String>,
    // "lmstudio" | "openrouter" — which provider to use for this turn.
    pub provider_override: Option<String>,
    // Q1.3 — non-text inputs for this turn. Today only inline base64
    // images. The client posts `{ kind, mime_type, data_b64 }` per
    // entry (matches the on-wire shape of `types::Attachment`); the
    // server persists them onto the user message's metadata blob and
    // passes them through to the agent for provider-side translation.
    pub attachments:     Option<Vec<crate::types::Attachment>>,
    // Per-conversation override for reasoning suppression (`/no_think`).
    // `Some(true/false)` overrides the global `agent.disable_reasoning` for this
    // turn; `None` uses the global default. Set by the chat window's toggle.
    pub disable_reasoning: Option<bool>,

    // ── Voice (all optional; absent == today's behaviour) ──────────────
    // The channel this turn arrives on. When set it wins over the
    // `X-Mira-Client` header; `"voice"` opts the turn into the voice path
    // (spoken style, timings, prompt-cache stability). Absent → header logic.
    pub channel: Option<String>,
    // Language the spoken reply should be in (item 2). Passed to the turn as a
    // hint; an unknown value warns rather than fails.
    pub reply_language: Option<String>,
    // Delivery style for the reply (item 2), e.g. `"spoken"` — shapes the
    // reply for text-to-speech (short sentences, no markup).
    pub style: Option<String>,
    // Emit per-turn latency `timing` events even off the voice channel (item 4).
    // Voice turns always get timings; this opts a text turn in for debugging.
    pub debug_timings: Option<bool>,
    // Start this turn speculatively, holding tool execution + persistence until
    // an explicit commit (item 6). Absent/false → a normal committed turn.
    pub speculative: Option<bool>,
}

// Payload for the SSE `done` event. `model`/`provider` echo what the server
// actually used (after any per-turn override) so clients don't have to track
// it themselves. `usage` lets clients compute per-turn cost without a second
// round-trip; zeroed out when the upstream didn't report it.
#[derive(Serialize)]
struct DonePayload {
    conversation_id: String,
    model:           String,
    provider:        String,
    #[serde(default)]
    usage:           crate::types::TokenUsage,
}

// ── POST /api/chat ────────────────────────────────────────────────────────────

pub async fn chat_handler(
    ChatAuth(user): ChatAuth,
    Extension(agent):    Extension<Arc<AgentCore>>,
    Extension(history):  Extension<Arc<HistoryStore>>,
    Extension(auth):     Extension<Arc<LocalAuthService>>,
    Extension(notifs):   Extension<Arc<NotificationBus>>,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(preamble): Extension<Arc<ProfilePreambleCache>>,
    Extension(data_dir): Extension<DataDir>,
    Extension(mcp_servers): Extension<Arc<crate::mcp::McpServerRegistry>>,
    Extension(turn_registry): Extension<Arc<TurnRegistry>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> axum::response::Response {
    // ── Resolve or create conversation ────────────────────────────────────────
    let config = live_cfg.get().await;

    // Channel tag for this turn (and conversations created from it). Resolution
    // order: an explicit `channel` body field wins — this is how a
    // voice client opts into the voice path (`"voice"`). Absent, we fall back to
    // the `X-Mira-Client` header (what the Android app sends): any non-"web"
    // value is a mobile/native client; absent → "web" (backward-compatible).
    // Per-conversation channel, so tagging at creation is sufficient.
    let turn_channel: String = match req.channel.as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
    {
        Some(c) => c,
        None => match headers
            .get("x-mira-client")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") | Some("web") => "web".to_string(),
            Some(_) => "mobile".to_string(),
        },
    };
    let new_channel: &str = &turn_channel;
    let model = req.model_override
        .clone()
        .unwrap_or_else(|| config.default_chat_model().1);

    // ── Restricted Mode caps (Slice 2) ────────────────────────────────────────
    // Instance-wide while a profile is active. Enforce the per-user message rate,
    // the global daily token ceiling, and the global concurrency cap BEFORE any
    // DB write or new-conversation creation, so a throttled or over-quota request
    // costs nothing. On a trip, stream a friendly assistant message (renders as a
    // normal reply) instead of a raw error. The acquired concurrency slot is held
    // for the turn's lifetime (moved into the turn task below).
    let turn_slot = if let Some(rcaps) = crate::policy::restricted::caps() {
        use crate::policy::restricted::CapVerdict;
        let denial = match rcaps.check_rate(&user.id) {
            CapVerdict::Deny { message } => Some(message),
            CapVerdict::Ok => match rcaps.check_daily_ceiling() {
                CapVerdict::Deny { message } => Some(message),
                CapVerdict::Ok => None,
            },
        };
        if let Some(message) = denial {
            return canned_assistant_sse(
                req.conversation_id.clone().unwrap_or_default(),
                model.clone(),
                config.default_chat_model().0,
                message,
            );
        }
        match rcaps.try_acquire_turn_slot() {
            Some(slot) => Some(slot),
            None => return canned_assistant_sse(
                req.conversation_id.clone().unwrap_or_default(),
                model.clone(),
                config.default_chat_model().0,
                "This service is busy right now — please try again in a moment.".to_string(),
            ),
        }
    } else {
        None
    };

    // A speculative turn must leave NO
    // trace until it commits: no conversation created, no user message stored,
    // no title. We defer all of that to the commit (the Done arm). For a brand
    // new conversation we run the turn against a provisional id and create the
    // real row only on commit.
    let speculative = req.speculative.unwrap_or(false);

    // Resolve the conversation up front so we can branch on its `mode`.
    // `new_conv_pending` = a new conversation whose creation is deferred to
    // commit (speculative only).
    let (conv_id, conv_mode, new_conv_pending) = match &req.conversation_id {
        Some(id) => {
            match history.get_conversation(id) {
                Ok(Some(c)) if c.user_id == user.id => (id.clone(), c.mode, false),
                Ok(Some(_)) => {
                    return (StatusCode::FORBIDDEN, "Not your conversation").into_response();
                }
                Ok(None) => {
                    return (StatusCode::NOT_FOUND, "Conversation not found").into_response();
                }
                Err(e) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
                }
            }
        }
        None if speculative => {
            // Defer creation until commit — use a provisional id for the turn.
            (uuid::Uuid::new_v4().to_string(), "chat".to_owned(), true)
        }
        None => {
            // Create a new conversation, titled with the first message.
            let title = derive_title_from_message(&req.message);
            match history.create_conversation(NewConversation {
                user_id:          user.id.clone(),
                channel:          new_channel.to_owned(),
                title:            Some(title),
                model:            Some(model.clone()),
                provider:         None,
                external_user_id: None,
                mode:             None,
            }) {
                Ok(c)  => (c.id, c.mode, false),
                Err(e) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
                }
            }
        }
    };

    // Record the user message immediately (before streaming). When the
    // turn carries attachments (Q1.3 — images), persist them on the
    // metadata blob so a reloaded conversation can render the image
    // chips back in the chat UI. Same on-wire shape as the typed
    // `Attachment` so the frontend deserialises the round-trip directly.
    let user_metadata = req.attachments.as_ref()
        .filter(|v| !v.is_empty())
        .and_then(|att| serde_json::to_string(
            &serde_json::json!({ "attachments": att })
        ).ok());
    // Addendum-4 item 11 — a speculative turn stores nothing until commit; its
    // user message is persisted in the Done arm (with the committed transcript).
    if !speculative {
        if let Err(e) = history.add_message(NewMessage {
            conversation_id: conv_id.clone(),
            role:            MessageRole::User,
            content:         req.message.clone(),
            content_type:    "text".to_owned(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata:        user_metadata.clone(),
        }) {
            warn!("Failed to record user message: {}", e);
        }
    }

    // reset companion missed-check-in counter on any user
    // message. The scheduler increments after each fire; this reset
    // confirms the previous check-in landed and the user is still
    // engaged. Skipped silently when companion isn't installed.
    if let Some(sys) = agent.companion() {
        let _ = sys.store().reset_missed_checkins(&user.id);
    }

    // Capability RBAC — the caller's effective profile gates which
    // provider/model they may *explicitly* select and which tools they may
    // use this turn. Admins resolve to an unrestricted profile. A lookup
    // failure falls back to unrestricted so a transient DB error never locks
    // a user out of chat.
    let caps = auth
        .effective_capabilities(&user.id, &user.role)
        .unwrap_or_default();
    // Gate an explicit provider/model override. The server default (no
    // override) is always permitted; the UI hides disallowed options so a
    // restricted user only ever sends allowed selections (slice 1 scope).
    if let Some(p) = req.provider_override.as_deref() {
        if !caps.allows_provider(p) {
            return (StatusCode::FORBIDDEN,
                format!("Your account is not permitted to use the '{p}' provider.")).into_response();
        }
    }
    if let Some(m) = req.model_override.as_deref() {
        if !caps.allows_model(m) {
            return (StatusCode::FORBIDDEN,
                format!("Your account is not permitted to use the model '{m}'.")).into_response();
        }
    }

    // Build an optional one-shot provider for this turn when the user has
    // selected a specific model / provider from the frontend dropdown. This
    // provider is used for the WHOLE turn (tool rounds + final answer), so the
    // picked model actually answers. Handles every provider via the shared
    // factory — not just openrouter/lmstudio, which used to silently fall back
    // to the default chain for Anthropic/DeepSeek/Gemini/etc.
    // A voice turn uses the voice provider (`voice.provider`)
    // when the request didn't pick an explicit provider; every other channel is
    // unchanged. The voice provider fails over to the default chain (a runtime
    // failure then surfaces as a `warning` event from the tool loop), and carries
    // its own per-turn context budget. An unresolvable voice provider is logged
    // and the turn silently uses the default.
    let mut voice_ctx_budget: Option<usize> = None;
    let turn_provider: Option<Arc<dyn ModelProvider>> = match req.provider_override.as_deref() {
        Some(slug) => {
            let built = crate::gateway::builder::build_single_provider(
                &config, slug, req.model_override.as_deref());
            if built.is_none() {
                warn!("provider '{slug}' selected but not configured (missing key/URL) — using default provider");
            }
            built
        }
        None if turn_channel == "voice" && config.voice.provider.is_some() => {
            match crate::gateway::builder::build_voice_provider(&config, &agent.provider) {
                Some((p, budget)) => { voice_ctx_budget = budget; Some(p) }
                None => {
                    warn!("voice.provider '{}' could not be resolved — using the default provider",
                          config.voice.provider.as_deref().unwrap_or(""));
                    None
                }
            }
        }
        None => None, // use the agent's default provider
    };

    let session_id    = format!("web-{}", conv_id);
    let user_id       = user.id.clone();
    let message       = req.message.clone();
    let conv_id_c     = conv_id.clone();
    // Addendum-4 item 11 — carried into the task so the Done (commit) arm can
    // create the deferred conversation + persist the user message for a
    // speculative turn. `channel`/`model` already flow in via other captures.
    let new_conv_pending_task = new_conv_pending;
    let spec_user_metadata    = user_metadata.clone();
    let spec_channel          = new_channel.to_owned();
    // Snapshot attachments for the agent invocation below. Cheap clone
    // these are small base64 strings; provider-side serialisation
    // owns the further translation work.
    let req_attachments = req.attachments.clone().unwrap_or_default();
    // Echoed back in the SSE `done` payload so clients can render
    // model/provider attribution + look up pricing without a round-trip.
    let provider_name = req.provider_override.clone()
        .unwrap_or_else(|| agent.provider.name().to_owned());
    // Auto-extract config is read from the BOOT snapshot (`agent.config`), NOT
    // LiveConfig — the same source the core post-hook uses for every other
    // channel. Memory settings are restart-required, so reading them from the
    // live config here made web disagree with core until a restart.
    let auto_extract_cfg = agent.config.memory.auto_extract.clone();
    // Captured by the spawn alongside `agent`/`history` so the post-turn
    // extractor (onboarding mode only) can run without re-plumbing extensions.
    let auth_for_task = Arc::clone(&auth);
    let mode_for_task = conv_mode.clone();
    // The extractor must use the same model the user picked in the UI for this
    // A.8: the web turn's LLM extractor uses the fixed `memory_extract`
    // llm-alias (fallback: primary provider), NOT the per-turn model the user
    // picked — so a turn routed to a grounded/remote model can't pull
    // conversation content into the extractor. If the alias can't be resolved,
    // fall back to the per-turn/agent provider so web extraction still runs
    // rather than silently dropping.
    let extractor_provider: Arc<dyn ModelProvider> =
        crate::agent::named_agent::build_provider_for_llm_alias(&agent.config, "memory_extract")
            .unwrap_or_else(|e| {
                // A.8: NEVER fall back to the per-turn model the user picked (it may
                // be a grounded/remote model that must not see conversation content
                // pulled into the extractor). Use the instance PRIMARY provider.
                warn!("web extractor: memory_extract alias resolve failed ({e}); using the primary provider");
                Arc::clone(&agent.provider)
            });

    // Build a per-turn context. Default empty context = normal chat; if the
    // conversation is in onboarding mode, swap in the onboarding prompt,
    // restrict the tool set to the onboarding flow, and inject trusted
    // identity args so onboarding tools can't be steered to another user.
    // Slice H — honour the per-conversation wiki toggle. Look up the
    // current conversation row (cheap; one indexed SELECT) and pass
    // its `skip_wiki` value into the turn context. Missing / failed
    // lookup falls back to false so the wiki hook still runs.
    let conv_skip_wiki = history.get_conversation(&conv_id_c)
        .ok().flatten().map(|c| c.skip_wiki).unwrap_or(false);

    let mut turn_ctx = build_turn_context(
        &agent, &auth, &preamble, &data_dir, &user_id, &conv_id_c, &conv_mode,
        conv_skip_wiki, req.disable_reasoning,
    );
    // Q1.3 — attach the current turn's images. Only the immediate user
    // message gets them; history replay is handled at render time on
    // the frontend (no re-send unless the user re-attaches).
    turn_ctx.attachments = req_attachments;
    // Rehydrate the agent's in-memory session from this conversation's
    // persisted history on a cache miss (process restart / 1-hour idle
    // eviction). Without this, continuing a conversation after a restart would
    // start the agent with empty context even though the history DB — and the
    // UI — still show every prior turn. The agent core handles the seeding;
    // we just tell it which stored conversation this turn belongs to.
    turn_ctx.conversation_id = Some(conv_id_c.clone());

    // per-user MCP tool filter. Apply only when the helper
    // hasn't already set a more specific allow-list (the onboarding
    // flow does). When the MCP registry has zero tools across all
    // users, `allowed_tools_for` returns None and we leave the
    // turn context unrestricted.
    if turn_ctx.allowed_tool_names.is_none() {
        let all = agent.tools.list_tools();
        if let Some(allow) = mcp_servers.allowed_tools_for(&user_id, &all) {
            turn_ctx.allowed_tool_names = Some(allow);
        }
    }

    // Capability RBAC — intersect with the user's tool allow-list. Both this
    // and the MCP filter are *restrictions*, so the effective set is their
    // intersection. When the profile doesn't restrict tools (`caps.tools` is
    // None) this is a no-op. Applied after the MCP filter so it narrows any
    // existing allow-list (onboarding or per-user MCP), never widens it.
    if caps.tools.is_some() {
        let base = turn_ctx
            .allowed_tool_names
            .take()
            .unwrap_or_else(|| agent.tools.list_tools());
        turn_ctx.allowed_tool_names = caps.filter_tools(&base);
    }

    // Skill-authoring gate: don't offer `propose_skill` when the instance
    // master switch is off OR the caller's RBAC profile denies authoring. A
    // dedicated control on top of the tools axis.
    if !config.agent.skill_authoring.enabled || !caps.may_author_skills() {
        let base = turn_ctx
            .allowed_tool_names
            .take()
            .unwrap_or_else(|| agent.tools.list_tools());
        turn_ctx.allowed_tool_names =
            Some(base.into_iter().filter(|t| t != "propose_skill").collect());
    }

    // Snapshot the onboarded state *before* the turn runs. The primary model
    // can call `complete_onboarding` inside its own stream; by the time the
    // post-turn extractor reads the profile, `onboarded_at` is already
    // stamped and its was-before-vs-after comparison returns false. We need
    // the true "before" value to detect the transition. Only meaningful in
    // onboarding mode; non-onboarding turns ignore it.
    let was_onboarded = auth_for_task
        .get_profile(&user_id)
        .ok().flatten()
        .and_then(|p| p.onboarded_at)
        .is_some();

    // Mint this turn's stable id and register it so a later
    // request can cancel/commit it. The id is echoed in the
    // `X-Mira-Turn-Id` response header and opens the stream as `turn_started`.
    // `register` returns the cancellation token (threaded into the turn);
    // for now we only need the registration to exist + be cleaned up on end.
    let turn_id = uuid::Uuid::new_v4().to_string();
    let cancel_token = turn_registry.register(
        &turn_id, &user_id, &turn_channel, speculative, &req.message);
    turn_ctx.turn_id = Some(turn_id.clone());
    // Hand the turn its cancellation token. Firing it — via the cancel
    // route or an SSE disconnect — stops the turn and makes it emit `Cancelled`.
    turn_ctx.cancel_token = Some(cancel_token.clone());
    // Spoken style + reply language steer for this turn.
    // Unknown values surface as `warning` events from the turn, never errors.
    turn_ctx.spoken_style = req.style.clone();
    turn_ctx.reply_language = req.reply_language.clone();
    // Per-turn timing events (always on for voice; core ORs
    // the channel in, so this only needs to carry the explicit debug opt-in).
    turn_ctx.debug_timings = req.debug_timings.unwrap_or(false);
    // The voice provider's per-provider context budget.
    turn_ctx.context_tokens_override = voice_ctx_budget;
    // Speculative turn: thread the commit notifier + restart
    // slot from the registry handle so the turn's commit barrier + restart path
    // see the same shared state the commit route writes. Spawn the auto-discard
    // timeout timer (stands down once committed).
    turn_ctx.speculative = speculative;
    if speculative {
        if let Some(h) = turn_registry.speculative_handles(&turn_id) {
            turn_ctx.commit_notify  = Some(Arc::clone(&h.commit_notify));
            turn_ctx.restart_slot   = Some(Arc::clone(&h.restart_slot));
            turn_ctx.restart_notify = Some(Arc::clone(&h.restart_notify));
            let reg = Arc::clone(&turn_registry);
            let tid = turn_id.clone();
            let committed = Arc::clone(&h.committed);
            let window = config.voice.speculative_timeout_secs;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(window)).await;
                if !committed.load(std::sync::atomic::Ordering::Relaxed) {
                    reg.discard_on_timeout(&tid);
                }
            });
        }
    }
    let channel_for_task = turn_channel.clone();
    let registry_for_task = Arc::clone(&turn_registry);
    let turn_id_for_task = turn_id.clone();
    // Implicit cancel on disconnect is voice-only (a hang-up = barge-in); other
    // channels always run to completion on a dropped stream, exactly as before.
    let implicit_cancel_on_disconnect =
        config.voice.implicit_cancel_on_disconnect && turn_channel == "voice";
    let disconnect_cancel = cancel_token.clone();
    // How long after `done` a cancel may still truncate.
    let cancel_window_secs = config.voice.cancel_window_secs;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);

    // Skill self-authoring: captured for the post-turn auto-approve hook.
    let skill_authoring_cfg = config.agent.skill_authoring.clone();
    let data_dir_sa = data_dir.0.clone();

    // Emit `message.received` (/). Best-effort — when the
    // event bus isn't installed (unit tests) this is a silent no-op.
    if let Some(bus) = agent.event_bus() {
        bus.emit_named(
            crate::events::names::MESSAGE_RECEIVED,
            Some(user_id.clone()),
            serde_json::json!({
                "user_id":         user_id,
                "channel":         "web",
                "conversation_id": conv_id_c,
                "text":            message,
            }),
        );
    }

    tokio::spawn(async move {
        // Hold the Restricted Mode concurrency slot for the whole turn; dropping
        // it here (task end) frees the global permit. `None` when unrestricted.
        let _turn_slot = turn_slot;
        // Mutable because a speculative turn creates its deferred conversation on
        // commit and rebinds this to the real id.
        let mut conv_id_c = conv_id_c;
        // Drop this turn's registry entry whenever the task ends — success,
        // error, or cancellation. A lookup miss then means "already done".
        let _registry_guard = TurnRegistryGuard {
            registry: Arc::clone(&registry_for_task),
            turn_id:  turn_id_for_task.clone(),
        };
        // Implicit cancel on disconnect (voice only). A
        // `Sender::closed()` future resolves when the SSE receiver is dropped,
        // i.e. the client hung up. Firing the token then stops the turn as a
        // barge-in. After a normal finish the receiver also drops, firing this a
        // beat late — a harmless no-op on an already-finished turn.
        if implicit_cancel_on_disconnect {
            let dtx = tx.clone();
            tokio::spawn(async move {
                dtx.closed().await;
                disconnect_cancel.cancel();
            });
        }
        match agent.process_with_context(
            &session_id, &user_id, &channel_for_task, &message, turn_provider, turn_ctx,
        ).await {
            Err(e) => {
                let _ = tx.send(Ok(Event::default().event("error").data(e.to_string()))).await;
            }
            Ok(mut stream_rx) => {
                let mut full_response = String::new();
                let mut tool_events: Vec<serde_json::Value> = Vec::new();
                // Unified "thinking" accumulator — captures the
                // agent's tool calls + results, model reasoning
                // blocks, and wiki-context fetches in arrival
                // order. Serialised to `Message.metadata` on Done
                // so reloading the conversation surfaces the same
                // panel that streamed live during the turn.
                let mut thinking_events: Vec<serde_json::Value> = Vec::new();
                // Non-fatal warnings (e.g. provider failover) surfaced this
                // turn — persisted on the message so the inline callout
                // survives a reload, not just the live stream.
                let mut warnings: Vec<String> = Vec::new();

                loop {
                    match stream_rx.recv().await {
                        None => break,
                        Some(StreamEvent::TurnStarted { turn_id, channel }) => {
                            // First event of the stream. Lets a
                            // client learn the id it can cancel/commit. Older
                            // clients ignore the unknown event name.
                            let payload = serde_json::json!({
                                "turn_id": turn_id, "channel": channel,
                            });
                            let _ = tx.send(Ok(
                                Event::default().event("turn_started").data(payload.to_string())
                            )).await;
                        }
                        Some(StreamEvent::Token(t)) => {
                            full_response.push_str(&t);
                            let _ = tx.send(Ok(
                                Event::default().event("token").data(t)
                            )).await;
                        }
                        Some(StreamEvent::Done { usage }) => {
                            // Addendum-4 item 11 — a speculative turn committed:
                            // NOW create the deferred conversation (if new) and
                            // persist the user message, using the FINAL transcript
                            // (the corrected message on a restart, else original).
                            // A discarded speculative turn never reaches Done, so
                            // it leaves nothing behind.
                            if speculative {
                                let final_msg = registry_for_task
                                    .committed_message(&turn_id_for_task)
                                    .unwrap_or_else(|| message.clone());
                                if new_conv_pending_task {
                                    match history.create_conversation(NewConversation {
                                        user_id:          user_id.clone(),
                                        channel:          spec_channel.clone(),
                                        title:            Some(derive_title_from_message(&final_msg)),
                                        model:            Some(model.clone()),
                                        provider:         None,
                                        external_user_id: None,
                                        mode:             None,
                                    }) {
                                        Ok(c)  => conv_id_c = c.id,
                                        Err(e) => warn!("Failed to create speculative conversation: {}", e),
                                    }
                                }
                                if let Err(e) = history.add_message(NewMessage {
                                    conversation_id: conv_id_c.clone(),
                                    role:            MessageRole::User,
                                    content:         final_msg,
                                    content_type:    "text".to_owned(),
                                    token_count:     None,
                                    model:           None,
                                    tool_calls:      None,
                                    metadata:        spec_user_metadata.clone(),
                                }) {
                                    warn!("Failed to record speculative user message: {}", e);
                                }
                            }
                            // Persist assistant response.
                            let tool_calls_json = if tool_events.is_empty() {
                                None
                            } else {
                                serde_json::to_string(&tool_events).ok()
                            };
                            // Persist the thinking trail (tool calls
                            // + tool results + reasoning + wiki
                            // context, in arrival order) into the
                            // message metadata blob so reloading the
                            // conversation re-renders the same panel
                            // that streamed live. We always write
                            // when there were any events; the chat
                            // UI gates display on `agent.show_thinking`
                            // so disabling that toggle hides the
                            // panel without losing history.
                            let metadata_json = if thinking_events.is_empty() && warnings.is_empty() {
                                None
                            } else {
                                let mut meta = serde_json::Map::new();
                                if !thinking_events.is_empty() {
                                    meta.insert("thinking".into(), serde_json::json!(thinking_events));
                                }
                                if !warnings.is_empty() {
                                    meta.insert("warnings".into(), serde_json::json!(warnings));
                                }
                                serde_json::to_string(&meta).ok()
                            };

                            match history.add_message(NewMessage {
                                conversation_id: conv_id_c.clone(),
                                role:            MessageRole::Assistant,
                                // Repair any artifact URL the model mangled into
                                // its prose (e.g. https://api.artifacts/<sha>) so
                                // a reloaded conversation renders the image on
                                // every client.
                                content:         crate::artifacts::normalize_artifact_urls(&full_response),
                                content_type:    "text".to_owned(),
                                token_count:     Some(usage.completion_tokens as i32),
                                model:           Some(model.clone()),
                                tool_calls:      tool_calls_json,
                                metadata:        metadata_json,
                            }) {
                                Ok(m) => {
                                    // Remember this completed
                                    // turn briefly so a cancel arriving AFTER done
                                    // (the user interrupts while it's still being
                                    // spoken) can still truncate the stored reply.
                                    registry_for_task.record_completed(
                                        &turn_id_for_task, &user_id, &conv_id_c, &m.id,
                                        cancel_window_secs,
                                    );
                                }
                                Err(e) => warn!("Failed to persist assistant message: {}", e),
                            }
                            let _ = history.touch_conversation(&conv_id_c);

                            // Trigger background LLM auto-title on first user message.
                            let first_turn = history
                                .get_messages(&conv_id_c, 5, None)
                                .map(|m| m.len())
                                .unwrap_or(99) <= 2;
                            if first_turn {
                                let hist2   = Arc::clone(&history);
                                let cid     = conv_id_c.clone();
                                let agent2  = Arc::clone(&agent);
                                let msg_c   = message.clone();
                                let preview = derive_title_from_message(&message);
                                let title_context = ProviderRequestContext {
                                    session_id: Some(session_id.clone()),
                                };
                                tokio::spawn(async move {
                                    generate_auto_title_with_context(
                                        agent2,
                                        hist2,
                                        cid.clone(),
                                        msg_c,
                                        preview,
                                        title_context,
                                    ).await;
                                });
                            }

                            // Capture the assistant's final text *before* we
                            // take the `full_response` String below. Used by
                            // the LLM auto-extractor further down. Cheap
                            // clone — typical turn is a few KB.
                            let assistant_text = full_response.clone();

                            // Send `done` *before* running the onboarding
                            // extractor. The extractor is a non-streaming
                            // provider call that can take several seconds on
                            // local reasoning-distilled models (gemma/qwen
                            // burn hundreds of reasoning tokens on it). If we
                            // awaited it before `done`, the frontend would
                            // keep the input locked the whole time — the
                            // stop-button stall the user complained about.
                            let payload = serde_json::to_string(&DonePayload {
                                conversation_id: conv_id_c.clone(),
                                model:           model.clone(),
                                provider:        provider_name.clone(),
                                usage:           usage.clone(),
                            }).unwrap_or_default();
                            let _ = tx.send(Ok(Event::default().event("done").data(payload))).await;

                            // Onboarding-only: detect `onboarded_at` transition
                            // and emit `onboarding_complete`. Two paths can
                            // flip it:
                            // (a) the primary model fired
                            //     `complete_onboarding` during its own stream
                            //     (tool dispatch runs in-line); OR
                            // (b) the post-turn extractor's belt-and-braces
                            //     finalize call flipped it.
                            //
                            // We check (a) here, *before* the extractor, since
                            // the extractor short-circuits on already-onboarded
                            // users and would never report the flip. `was_onboarded`
                            // is the snapshot taken before the turn began — see
                            // above the `tokio::spawn` at the top of this handler.
                            if mode_for_task == "onboarding" {
                                let onboarded_now = auth_for_task
                                    .get_profile(&user_id)
                                    .ok().flatten()
                                    .and_then(|p| p.onboarded_at)
                                    .is_some();
                                let primary_flipped = !was_onboarded && onboarded_now;
                                if primary_flipped {
                                    info!(
                                        "onboarding: primary model's tool call flipped onboarded_at for user={}",
                                        user_id
                                    );
                                    let _ = tx.send(Ok(
                                        Event::default().event("onboarding_complete").data("{}")
                                    )).await;
                                }

                                // Always run the extractor for this turn — it
                                // captures narrated answers that didn't fire as
                                // tool calls, and backs up primary-model
                                // mistakes. If the primary already flipped
                                // onboarded_at, the extractor will early-exit
                                // (its `was_onboarded` check) — that's fine, we
                                // already emitted the signal. If it didn't, the
                                // extractor may flip it now and we emit the
                                // signal then.
                                let extractor_flipped = run_onboarding_extractor(
                                    Arc::clone(&extractor_provider),
                                    Arc::clone(&agent),
                                    Arc::clone(&history),
                                    Arc::clone(&auth_for_task),
                                    conv_id_c.clone(),
                                    user_id.clone(),
                                    crate::providers::ProviderRequestContext { session_id: Some(session_id.clone()) },
                                ).await;
                                if extractor_flipped && !primary_flipped {
                                    let _ = tx.send(Ok(
                                        Event::default().event("onboarding_complete").data("{}")
                                    )).await;
                                }
                            } else if auto_extract_cfg.effective_extractor("web")
                                == crate::config::ExtractorKind::Llm
                                // honour an explicit "don't record this" — the
                                // web handler owns web's LLM extraction, so it must
                                // check consent here too (the core post-hook can't).
                                && !crate::memory::auto_extract::is_no_store_request(&message)
                                // An explicit "remember this" is handled by the core
                                // post-hook for every channel (deterministic store);
                                // skip here so we don't double-extract it.
                                && !crate::memory::auto_extract::is_explicit_remember(&message)
                            {
                                // LLM auto-extract runs only outside onboarding
                                // mode, post-stream, as fire-and-forget. The
                                // core post-hook deliberately skips the LLM path
                                // for "web" (it can't see the per-turn model the
                                // user picked), so this is the sole extractor
                                // writing auto memories for this web turn.
                                let memory    = Arc::clone(&agent.memory);
                                let provider  = Arc::clone(&extractor_provider);
                                let user_msg  = message.clone();
                                let asst_msg  = assistant_text;
                                let uid       = user_id.clone();
                                let cid       = conv_id_c.clone();
                                let cats      = auto_extract_cfg.allowed_categories.clone();
                                let min_conf  = crate::memory::auto_extract::ConfidenceTier::parse(
                                    &auto_extract_cfg.min_confidence,
                                );
                                tokio::spawn(async move {
                                    memory.auto_extract_llm_and_store(
                                        &provider,
                                        &user_msg,
                                        &asst_msg,
                                        &uid,
                                        "web",
                                        Some(&cid),
                                        None,
                                        &cats,
                                        min_conf,
                                        false, // explicit "remember" handled by the core post-hook
                                    ).await;
                                });
                            }
                            break;
                        }
                        Some(StreamEvent::Timing(t)) => {
                            // Forward per-turn latency as a
                            // `timing` SSE event. Only present fields are sent.
                            let mut obj = serde_json::Map::new();
                            obj.insert("phase".into(), serde_json::json!(t.phase));
                            let mut put = |k: &str, v: Option<u64>| {
                                if let Some(n) = v { obj.insert(k.into(), serde_json::json!(n)); }
                            };
                            put("memory_recall_ms", t.memory_recall_ms);
                            put("wiki_ms", t.wiki_ms);
                            put("tool_selection_ms", t.tool_selection_ms);
                            put("prompt_build_ms", t.prompt_build_ms);
                            put("llm_request_to_first_token_ms", t.llm_request_to_first_token_ms);
                            put("total_to_first_token_ms", t.total_to_first_token_ms);
                            put("total_ms", t.total_ms);
                            if let Some(n) = t.tool_rounds { obj.insert("tool_rounds".into(), serde_json::json!(n)); }
                            if let Some(n) = t.llm_tokens_out { obj.insert("llm_tokens_out".into(), serde_json::json!(n)); }
                            if let Some(n) = t.prompt_tokens { obj.insert("prompt_tokens".into(), serde_json::json!(n)); }
                            if let Some(n) = t.cached_tokens { obj.insert("cached_tokens".into(), serde_json::json!(n)); }
                            let _ = tx.send(Ok(Event::default()
                                .event("timing")
                                .data(serde_json::Value::Object(obj).to_string()))).await;
                        }
                        Some(StreamEvent::Restarted { turn_id: tid }) => {
                            // A speculative turn was committed
                            // with a changed message: discard what we streamed and
                            // forward `restarted`; fresh events for the restarted
                            // turn follow on this same stream.
                            full_response.clear();
                            tool_events.clear();
                            thinking_events.clear();
                            warnings.clear();
                            let _ = tx.send(Ok(Event::default()
                                .event("restarted")
                                .data(serde_json::json!({ "turn_id": tid }).to_string()))).await;
                        }
                        Some(StreamEvent::Cancelled { turn_id: tid }) => {
                            // The turn was cancelled (barge-in
                            // or explicit cancel). Persist what was actually
                            // delivered, never the unspoken remainder: the cancel
                            // payload's `spoken_text` wins; absent (implicit
                            // disconnect) we fall back to what streamed so far.
                            // `discard` persists nothing. We deliberately do NOT
                            // run auto-extraction on a cancelled turn — the
                            // generated text past the barge-in point was never
                            // seen, so mining it would record things never said.
                            let req = registry_for_task
                                .take_cancel_request(&turn_id_for_task)
                                .unwrap_or_default();
                            // A Cancelled on a speculative turn
                            // is always an uncommitted abort (a committed turn
                            // finishes with Done), so persist NOTHING regardless
                            // of the payload.
                            if !req.discard && !speculative {
                                let spoken = req.spoken_text
                                    .unwrap_or_else(|| full_response.clone());
                                if !spoken.trim().is_empty() {
                                    let meta = serde_json::json!({
                                        "interrupted":   true,
                                        "spoken_offset": spoken.chars().count(),
                                    });
                                    if let Err(e) = history.add_message(NewMessage {
                                        conversation_id: conv_id_c.clone(),
                                        role:            MessageRole::Assistant,
                                        content:         crate::artifacts::normalize_artifact_urls(&spoken),
                                        content_type:    "text".to_owned(),
                                        token_count:     None,
                                        model:           Some(model.clone()),
                                        tool_calls:      None,
                                        metadata:        Some(meta.to_string()),
                                    }) {
                                        warn!("Failed to persist cancelled assistant message: {}", e);
                                    }
                                    let _ = history.touch_conversation(&conv_id_c);
                                }
                            }
                            let _ = tx.send(Ok(Event::default()
                                .event("cancelled")
                                .data(serde_json::json!({ "turn_id": tid }).to_string()))).await;
                            break;
                        }
                        Some(StreamEvent::Error(e)) => {
                            let _ = tx.send(Ok(Event::default().event("error").data(e))).await;
                            break;
                        }
                        Some(StreamEvent::ToolCapReached { rounds_used }) => {
                            // The voice tool-round cap was
                            // hit; MIRA is answering from what it has. A distinct
                            // event so the client can show a holding phrase.
                            let _ = tx.send(Ok(Event::default()
                                .event("voice_tool_cap_reached")
                                .data(serde_json::json!({ "rounds_used": rounds_used }).to_string()))).await;
                        }
                        Some(StreamEvent::Warning(w)) => {
                            warnings.push(w.clone());
                            let _ = tx.send(Ok(Event::default().event("warning").data(w))).await;
                        }
                        Some(StreamEvent::ToolCall { name, args, call_id }) => {
                            let ev = serde_json::json!({
                                "type": "call",
                                "tool": name,
                                "args": args,
                                // call_id lets clients correlate a result to
                                // its call exactly (vs matching by tool name,
                                // which breaks for parallel/repeated calls).
                                "call_id": call_id,
                            });
                            tool_events.push(ev.clone());
                            thinking_events.push(serde_json::json!({
                                "type":    "tool_call",
                                "name":    name,
                                "args":    args,
                                "call_id": call_id,
                            }));
                            let _ = tx.send(Ok(
                                Event::default().event("tool_call")
                                    .data(serde_json::to_string(&ev).unwrap_or_default())
                            )).await;
                        }
                        Some(StreamEvent::ToolResult { name, output, success, call_id }) => {
                            let ev = serde_json::json!({
                                "type": "result",
                                "tool": name,
                                "success": success,
                                "output": output,
                                "call_id": call_id,
                            });
                            thinking_events.push(serde_json::json!({
                                "type":    "tool_result",
                                "name":    name,
                                "output":  output,
                                "success": success,
                                "call_id": call_id,
                            }));
                            let _ = tx.send(Ok(
                                Event::default().event("tool_result")
                                    .data(serde_json::to_string(&ev).unwrap_or_default())
                            )).await;
                        }
                        Some(StreamEvent::WikiContext { pages }) => {
                            // Forward as SSE so the chat UI can render pills
                            // under the assistant message (Slice H).
                            let payload = serde_json::json!({ "pages": pages });
                            thinking_events.push(serde_json::json!({
                                "type":  "wiki_context",
                                "pages": pages,
                            }));
                            let _ = tx.send(Ok(
                                Event::default().event("wiki_context")
                                    .data(payload.to_string())
                            )).await;
                        }
                        Some(StreamEvent::MemoryContext { served, unavailable }) => {
                            // Forward as SSE so the chat UI can render per-source
                            // memory pills + a "store unavailable" badge (B2d).
                            let payload = serde_json::json!({
                                "served": served, "unavailable": unavailable,
                            });
                            thinking_events.push(serde_json::json!({
                                "type": "memory_context",
                                "served": served, "unavailable": unavailable,
                            }));
                            let _ = tx.send(Ok(
                                Event::default().event("memory_context")
                                    .data(payload.to_string())
                            )).await;
                        }
                        Some(StreamEvent::Reasoning(text)) => {
                            // Private chain-of-thought from reasoning
                            // models (DeepSeek R1, xAI Grok-3-mini,
                            // Anthropic extended thinking). Forwarded
                            // as a dedicated SSE event so the chat UI
                            // can render it as a collapsible block
                            // alongside the assistant message rather
                            // than concatenating it into the visible
                            // answer. The text is the full accumulated
                            // reasoning for a single tool-loop round;
                            // multi-round turns emit multiple events.
                            thinking_events.push(serde_json::json!({
                                "type": "reasoning",
                                "text": text,
                            }));
                            let _ = tx.send(Ok(
                                Event::default().event("reasoning").data(text)
                            )).await;
                        }
                    }
                }

                // Notify other connected clients that this conversation was updated.
                notifs.send(Notification {
                    kind:            NotificationKind::ConversationUpdated,
                    conversation_id: Some(conv_id_c.clone()),
                    channel:         Some("web".to_owned()),
                    user_id:         Some(user_id.clone()),
                    message:         None,
                    category:        None,
                });

                info!("Chat turn complete — conv={} user={}", conv_id_c, user_id);

                // Skill self-authoring: if MIRA proposed a Tier-0 guidance
                // skill this turn and auto-approve is on, activate it now so it's
                // live for the next turn — no restart. Guests can't author.
                if skill_authoring_cfg.enabled
                    && skill_authoring_cfg.auto_approve_tier0
                    && !user_id.starts_with("guest_")
                {
                    let activated = crate::skills::runtime::auto_approve_pending(
                        &agent.tools, data_dir_sa.as_path(), &user_id,
                    );
                    for id in activated {
                        info!("skill self-authoring: auto-approved {id:?} for {user_id}");
                        notifs.send(Notification {
                            kind:            NotificationKind::ConversationUpdated,
                            conversation_id: Some(conv_id_c.clone()),
                            channel:         Some("web".to_owned()),
                            user_id:         Some(user_id.clone()),
                            message:         Some(format!("Saved a new skill: {id}")),
                            category:        None,
                        });
                    }
                }
            }
        }
    });

    let mut response = Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response();
    // Surface the turn id in a header too, so a client can
    // cancel/commit without first parsing the SSE body. Infallible: the id is a
    // UUID, always a valid header value.
    if let Ok(hv) = HeaderValue::from_str(&turn_id) {
        response.headers_mut().insert("x-mira-turn-id", hv);
    }
    response
}

// ── Helpers ───────────────────────────────────────────────────────────────────

// RAII cleanup for a turn's registry entry: removing on drop covers every exit
// path of the turn task (normal end, early `break`, error, or a future
// cancellation) without duplicating the `remove` call at each one.
struct TurnRegistryGuard {
    registry: Arc<TurnRegistry>,
    turn_id:  String,
}

impl Drop for TurnRegistryGuard {
    fn drop(&mut self) {
        self.registry.remove(&self.turn_id);
    }
}

// Body for POST /api/chat/{turn_id}/cancel. Both fields optional, so an empty
// `{}` (or no body) is a plain cancel that keeps whatever was already delivered.
#[derive(Deserialize, Default)]
pub struct CancelTurnBody {
    // What the client actually delivered before barging in (e.g. the text TTS
    // had spoken). Persisted as the assistant turn; absent → keep what streamed.
    pub spoken_text: Option<String>,
    // Drop the turn entirely — persist nothing.
    #[serde(default)]
    pub discard: bool,
}

// ── POST /api/chat/{turn_id}/cancel ─────────────────────────────────────────
//
// Cancel an in-flight turn (barge-in). Idempotent; a cancel
// for an unknown turn is 404 (already finished / never existed), another user's
// turn is 403, and a turn older than `voice.cancel_window_secs` is 409. On
// success the turn stops at its next checkpoint and streams a `cancelled` event
// on its own SSE connection.
pub async fn cancel_turn_handler(
    ChatCancelAuth(user):     ChatCancelAuth,
    Extension(turn_registry): Extension<Arc<TurnRegistry>>,
    Extension(history):       Extension<Arc<HistoryStore>>,
    Extension(live_cfg):      Extension<Arc<LiveConfig>>,
    Path(turn_id):            Path<String>,
    body:                     Option<Json<CancelTurnBody>>,
) -> axum::response::Response {
    let window_secs = live_cfg.get().await.voice.cancel_window_secs;
    let Json(body) = body.unwrap_or_default();
    let req = CancelRequest { spoken_text: body.spoken_text.clone(), discard: body.discard };
    match turn_registry.request_cancel(&turn_id, &user.id, window_secs, req) {
        CancelOutcome::Accepted  => StatusCode::ACCEPTED.into_response(),
        CancelOutcome::Forbidden => (StatusCode::FORBIDDEN, "not your turn").into_response(),
        CancelOutcome::Stale     => (StatusCode::CONFLICT, "turn is too old to cancel").into_response(),
        // The turn may have already finished (the voice
        // engine was still speaking it). If it completed recently, truncate the
        // stored reply to what was actually spoken + mark it interrupted.
        CancelOutcome::NotFound  => {
            match turn_registry.take_completed_for_cancel(&turn_id, &user.id, window_secs) {
                Some(c) => {
                    let spoken = if body.discard { String::new() } else { body.spoken_text.unwrap_or_default() };
                    let meta = serde_json::json!({
                        "interrupted":   true,
                        "spoken_offset": spoken.chars().count(),
                    });
                    let content = crate::artifacts::normalize_artifact_urls(&spoken);
                    match history.update_message_content(&c.message_id, &content, Some(&meta.to_string())) {
                        Ok(true) => StatusCode::ACCEPTED.into_response(),
                        _ => (StatusCode::NOT_FOUND, "turn finished; stored reply not found").into_response(),
                    }
                }
                None => (StatusCode::NOT_FOUND, "no such in-flight turn").into_response(),
            }
        }
    }
}

// Body for POST /api/chat/{turn_id}/commit. An unchanged (or absent) message
// proceeds; a changed message restarts the turn under the same id.
#[derive(Deserialize, Default)]
pub struct CommitTurnBody {
    pub message: Option<String>,
}

// ── POST /api/chat/{turn_id}/commit ─────────────────────────────────────────
//
// Commit a speculative turn. No body (or an unchanged
// `message`) releases the turn's commit barrier: held tool calls execute and
// history is written. A changed `message` discards the speculative work and
// restarts the turn under the same id (a `restarted` event is streamed). 404 for
// an unknown turn, 403 for another user's, 409 for a non-speculative turn.
pub async fn commit_turn_handler(
    ChatAuth(user):           ChatAuth,
    Extension(turn_registry): Extension<Arc<TurnRegistry>>,
    Path(turn_id):            Path<String>,
    body:                     Option<Json<CommitTurnBody>>,
) -> axum::response::Response {
    let Json(body) = body.unwrap_or_default();
    match turn_registry.commit(&turn_id, &user.id, body.message) {
        CommitOutcome::Accepted       => StatusCode::ACCEPTED.into_response(),
        CommitOutcome::NotFound       => (StatusCode::NOT_FOUND, "no such in-flight turn").into_response(),
        CommitOutcome::Forbidden      => (StatusCode::FORBIDDEN, "not your turn").into_response(),
        CommitOutcome::NotSpeculative => (StatusCode::CONFLICT, "turn is not speculative").into_response(),
    }
}

// Stream a single canned assistant message over SSE, in the exact happy-path
// shape (`token` events + a `done` event), so a Restricted Mode cap trip renders
// as a normal assistant reply rather than an error toast. The turn is never run;
// no tokens are spent and nothing is persisted.
fn canned_assistant_sse(
    conversation_id: String,
    model:           String,
    provider:        String,
    message:         String,
) -> axum::response::Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(8);
    let payload = DonePayload {
        conversation_id,
        model,
        provider,
        usage: crate::types::TokenUsage::default(),
    };
    tokio::spawn(async move {
        let _ = tx.send(Ok(Event::default().event("token").data(message))).await;
        if let Ok(json) = serde_json::to_string(&payload) {
            let _ = tx.send(Ok(Event::default().event("done").data(json))).await;
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

// Build a per-turn `TurnContext` based on the conversation's mode.
// // - `mode="onboarding"`: swap in the onboarding prompt, restrict the tool
// set to the onboarding flow, inject trusted identity args, and skip
// the memory hooks (there's nothing meaningful to retrieve or write
// mid-onboarding).
// - Any other mode (default `"chat"`): prepend the per-user profile
// preamble to the base persona so preferences and structured facts
// bias every turn.
// // Failures (missing schema, DB errors) log a warning and fall back to
// the empty context rather than breaking the turn outright.
fn build_turn_context(
    agent:    &Arc<AgentCore>,
    auth:     &Arc<LocalAuthService>,
    preamble: &Arc<ProfilePreambleCache>,
    data_dir: &DataDir,
    user_id:  &str,
    conv_id:  &str,
    conv_mode: &str,
    conv_skip_wiki: bool,
    disable_reasoning: Option<bool>,
) -> TurnContext {
    if conv_mode == "onboarding" {
        return build_onboarding_turn_context(agent, auth, user_id, conv_id);
    }

    // Trusted identity injection: user-tier tools like `recall_history` need
    // `_user_id` to scope results to the caller. The chat handler supplies
    // it here so the model can't forge or omit it.
    let mut inject = serde_json::Map::new();
    inject.insert("_user_id".to_string(),         serde_json::Value::String(user_id.to_owned()));
    inject.insert("_conversation_id".to_string(), serde_json::Value::String(conv_id.to_owned()));
    // The caller's IANA timezone, so tools that accept a zone-less local time
    // (e.g. calendar_create_event with "2026-06-19T15:00:00") resolve it to the
    // right instant instead of guessing UTC.
    if let Some(tz) = auth.get_profile(user_id).ok().flatten().and_then(|p| p.timezone) {
        if !tz.trim().is_empty() {
            inject.insert("_user_tz".to_string(), serde_json::Value::String(tz));
        }
    }

    // Normal chat: layer the user's profile preamble onto the base persona.
    let system_prompt_override = preamble
        .resolve(user_id, auth, data_dir.0.as_path())
        .map(|addition| format!("{}\n\n{}", agent.system_prompt().trim_end(), addition.trim_end()));

    TurnContext {
        system_prompt_override,
        inject_tool_args: inject,
        skip_wiki_hooks:  conv_skip_wiki,
        disable_reasoning,
        ..TurnContext::default()
    }
}

fn build_onboarding_turn_context(
    agent:   &Arc<AgentCore>,
    auth:    &Arc<LocalAuthService>,
    user_id: &str,
    conv_id: &str,
) -> TurnContext {
    let schema = match OnboardingSchema::bundled() {
        Ok(s) => s,
        Err(e) => {
            warn!("Onboarding mode but schema failed to load — falling back to chat: {}", e);
            return TurnContext::default();
        }
    };

    let profile = match auth.get_profile(user_id) {
        Ok(p)  => p,
        Err(e) => { warn!("get_profile failed in onboarding turn: {}", e); None }
    };
    let progress_json = profile.as_ref().and_then(|p| p.onboarding_progress.clone());

    let agent_prompt = agent.system_prompt();
    let system_prompt = build_onboarding_prompt(
        &agent_prompt,
        &schema,
        progress_json.as_deref(),
        profile.as_ref(),
    );

    // Only let the onboarding flow call onboarding tools — no shell, no fs.
    let allowed = agent.tools.list_for_flow("onboarding");

    // Trusted identity injection: the LLM supplies the key and value, we
    // supply who it's for. Overwrites any `_user_id` the model tries to pass.
    let mut inject = serde_json::Map::new();
    inject.insert("_user_id".to_string(),         serde_json::Value::String(user_id.to_owned()));
    inject.insert("_conversation_id".to_string(), serde_json::Value::String(conv_id.to_owned()));

    TurnContext {
        provider_request_context: None,
        system_prompt_override: Some(system_prompt),
        allowed_tool_names:     Some(allowed),
        inject_tool_args:       inject,
        skip_memory_hooks:      true,
        // Onboarding has no wiki content yet — skip the hook entirely.
        skip_wiki_hooks:        true,
        attachments:            Vec::new(),
        reasoning_effort:       None,
        disable_reasoning:      None,
        conversation_id:        Some(conv_id.to_owned()),
        // Onboarding pins exactly the onboarding toolset — adaptive selection
        // must not narrow or replace it.
        tools_flow_restricted:  true,
        // Onboarding is a real user-facing conversation — keep the duty-of-care
        // safety floor on (a distressed user mid-onboarding must still be met).
        suppress_safety_floor:  false,
        suppress_time_context:  false,
        // Use the global tool-round cap.
        max_tool_rounds_override: None,
        // The onboarding path runs inside the chat handler, which mints + sets
        // the real turn id + cancel token after this context is built.
        turn_id:                  None,
        cancel_token:             None,
        // Onboarding is text-only; the voice steer + timings don't apply.
        spoken_style:             None,
        reply_language:           None,
        debug_timings:            false,
        speculative:              false,
        commit_notify:            None,
        restart_slot:             None,
        restart_notify:           None,
        context_tokens_override:  None,
    }
}

// Derive a deterministic conversation title from the user's first message.
// // Used as the initial title at conversation creation and as the fallback
// when the background LLM auto-title call fails or its output is rejected
// by the sanitiser. Distilled / reasoning-tuned local models (Qwen, Gemma
// derivatives) frequently produce reasoning prose instead of a clean title
// no matter how the prompt is shaped, so the heuristic must be good enough
// to stand on its own.
// // Pipeline: strip a common request preamble ("How do I", "Can you", …),
// take the first sentence, truncate to 60 chars at a word boundary, then
// title-case the result.
pub(crate) fn derive_title_from_message(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "New Conversation".to_owned();
    }

    let body = strip_request_preamble(trimmed);

    // First sentence — split on common terminal punctuation. A short first
    // sentence is a much better title than a 60-char slice across two ideas.
    let first = body
        .split(|c: char| matches!(c, '.' | '?' | '!' | '\n' | ';'))
        .next()
        .unwrap_or(body)
        .trim();

    let cap = truncate_at_word_boundary(first, 60);
    title_case(&cap)
}

// Lowercase-prefixed common request openings that add no information to a
// title ("how do I sort a list" → "sort a list"). Match is case-insensitive
// and only one prefix is stripped, so "Please can you …" still has one
// useful word removed.
fn strip_request_preamble(s: &str) -> &str {
    const PREFIXES: &[&str] = &[
        "how do i ", "how do you ", "how can i ", "how can you ",
        "how would i ", "how would you ", "how should i ", "how should you ",
        "how to ",
        "what is ", "what are ", "what's ", "whats ",
        "what does ", "what do ", "what would ", "what should ",
        "why does ", "why do ", "why is ", "why are ",
        "where is ", "where are ", "when is ", "when are ",
        "can you ", "could you ", "would you ", "will you ",
        "please ", "i want to ", "i need to ", "i'd like to ",
        "i would like to ", "i'm trying to ", "im trying to ",
        "i am trying to ", "help me ", "help with ",
        "let's ", "lets ",
        "tell me about ", "tell me ",
        "explain ", "show me ", "give me ",
    ];
    let lower = s.to_ascii_lowercase();
    for p in PREFIXES {
        if lower.starts_with(p) {
            return s[p.len()..].trim_start();
        }
    }
    s
}

// Trim `s` to at most `max_chars` characters, breaking at the last space
// before the limit. Appends `…` if anything was dropped.
fn truncate_at_word_boundary(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    // Walk char boundaries to find the byte index for `max_chars` chars.
    let mut byte_cut = s.len();
    for (i, (b, _)) in s.char_indices().enumerate() {
        if i == max_chars { byte_cut = b; break; }
    }
    let head = &s[..byte_cut];
    let cut = head.rfind(' ').unwrap_or(head.len());
    format!("{}…", &head[..cut])
}

// Capitalise the first character of every whitespace-separated token. Words
// the user already wrote in all-caps (`API`, `HTTP`) are left untouched so
// acronyms survive.
fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            // All-caps acronyms (length ≥ 2) stay as-is.
            if w.len() >= 2 && w.chars().all(|c| !c.is_alphabetic() || c.is_uppercase()) {
                return w.to_owned();
            }
            let mut chars = w.chars();
            match chars.next() {
                None       => String::new(),
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// Post-turn onboarding extractor. Reads the persisted transcript and asks
// the provider to emit structured JSON describing what the user has
// answered / skipped / completed. Applies the resulting ops through the
// tool registry, which is idempotent (`record_profile` on an already-set
// field just overwrites; `skip_topic` / `mark_group_complete` are no-ops
// on repeated calls).
// // Best-effort: any error is logged and swallowed. The user still sees the
// primary model's streamed response regardless of what happens here.
// // Returns `true` when this run flipped `onboarded_at` from null to set, so
// the caller can emit a dedicated `onboarding_complete` SSE event instead of
// relying on the frontend to race a query invalidation against the modal's
// transition watcher.
async fn run_onboarding_extractor(
    provider: Arc<dyn ModelProvider>,
    agent:    Arc<AgentCore>,
    history:  Arc<HistoryStore>,
    auth:     Arc<LocalAuthService>,
    conv_id:  String,
    user_id:  String,
    request_context: crate::providers::ProviderRequestContext,
) -> bool {
    let schema = match OnboardingSchema::bundled() {
        Ok(s)  => s,
        Err(e) => { warn!("onboarding extractor: schema load failed: {}", e); return false; }
    };

    // Snapshot the pre-run onboarded state so we can detect the flip. If the
    // user was already onboarded (shouldn't happen in onboarding mode, but
    // defensive), we skip the rest of the work.
    let was_onboarded = auth.get_profile(&user_id)
        .ok().flatten()
        .and_then(|p| p.onboarded_at)
        .is_some();
    if was_onboarded { return false; }

    // Full transcript — giving the extractor the whole thing is cheap on
    // onboarding scale (≤15 short turns) and catches backfill (e.g. user
    // answered name in turn 1; extractor didn't run until turn 2).
    let transcript = match history.get_messages(&conv_id, 200, None) {
        Ok(msgs) => msgs.into_iter().map(history_to_provider_message).collect::<Vec<_>>(),
        Err(e)   => { warn!("onboarding extractor: history read failed: {}", e); return false; }
    };

    let progress: serde_json::Value = auth
        .get_profile(&user_id)
        .ok().flatten()
        .and_then(|p| p.onboarding_progress)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let updates = extract_updates_from_transcript_with_context(
        &provider,
        &schema,
        &transcript,
        &progress,
        &request_context,
    ).await;

    if !updates.ops.is_empty() {
        info!("onboarding extractor: applying {} ops for user={}", updates.ops.len(), user_id);
        apply_ops(&agent.tools, &user_id, &conv_id, &updates.ops).await;
    }

    // Belt-and-braces: run `finalize_onboarding` directly even if the
    // extractor didn't emit `Op::Finalize`. The guard rejects when coverage
    // is incomplete (untouched required groups), so this is a no-op for
    // mid-flow turns. The individual `record_profile` / `skip_topic` tool
    // handlers already call `try_auto_finalize` — but if the extractor
    // returned zero ops (e.g. a pure "let's wrap up" turn), nothing here
    // would check coverage. This closes that gap.
    let just_flipped = match crate::tools::onboarding::finalize_onboarding(
        auth.as_ref(), &schema, &user_id, None,
    ) {
        Ok(())                                                                    => {
            auth.get_profile(&user_id).ok().flatten()
                .and_then(|p| p.onboarded_at).is_some()
        }
        Err(crate::tools::onboarding::FinalizeError::UntouchedRequiredGroups(g))  => {
            info!(
                "onboarding extractor: finalize guard held — required groups with no activity: {:?} (user={})",
                g, user_id
            );
            false
        }
        Err(crate::tools::onboarding::FinalizeError::Storage(e))                  => {
            warn!("onboarding extractor: finalize storage error: {}", e);
            false
        }
    };

    if just_flipped {
        info!("onboarding extractor: onboarded_at stamped for user={}", user_id);
    }
    just_flipped
}

// Convert a persisted history message to the provider-facing `ChatMessage`
// type. We drop attachments/metadata — the extractor only needs role + text.
fn history_to_provider_message(m: crate::history::Message) -> ProviderChatMessage {
    let role = match m.role {
        MessageRole::System    => ProviderMessageRole::System,
        MessageRole::User      => ProviderMessageRole::User,
        MessageRole::Assistant => ProviderMessageRole::Assistant,
        MessageRole::Tool      => ProviderMessageRole::Tool,
    };
    ProviderChatMessage {
        role,
        content:      m.content,
        tool_calls:   None,
        tool_call_id: None,
        attachments:  None,
    }
}

// Generates a short conversation title via a one-shot LLM call (background).
pub(crate) async fn generate_auto_title(
    agent:         Arc<AgentCore>,
    history:       Arc<HistoryStore>,
    conv_id:       String,
    first_message: String,
    fallback:      String,
) {
    generate_auto_title_with_optional_context(
        agent,
        history,
        conv_id,
        first_message,
        fallback,
        None,
    ).await;
}

// Context-aware variant used by the web chat path. Other callers retain the
// legacy context-free behaviour because they do not have a provider session
// identifier to forward here.
pub(crate) async fn generate_auto_title_with_context(
    agent:         Arc<AgentCore>,
    history:       Arc<HistoryStore>,
    conv_id:       String,
    first_message: String,
    fallback:      String,
    context:       ProviderRequestContext,
) {
    generate_auto_title_with_optional_context(
        agent,
        history,
        conv_id,
        first_message,
        fallback,
        Some(context),
    ).await;
}

async fn generate_auto_title_with_optional_context(
    agent:         Arc<AgentCore>,
    history:       Arc<HistoryStore>,
    conv_id:       String,
    first_message: String,
    fallback:      String,
    context:       Option<ProviderRequestContext>,
) {
    use crate::types::{ChatMessage, GenerationOptions};

    let preview_len = first_message.len().min(500);
    // Few-shot completion. Three examples (rather than one) so the model
    // doesn't latch onto a single demo and regurgitate it verbatim — and
    // so the topical pattern (title summarises the *message*) is reinforced.
    // Earlier versions tried negative directives ("no preamble, no quotes")
    // and small/tuned models obediently bulleted them back as the title,
    // e.g. `* No preamble (no "Here is…")`. The sanitiser below catches
    // residual leakage; the prompt's job is to make the right answer the
    // most likely completion of `Title:`.
    let prompt = format!(
        "/no_think\n\
         Generate a 3-6 word title summarising the topic of the message.\n\n\
         Message: How do I sort a list of dicts in Python by a key?\n\
         Title: Sorting Python Dicts By Key\n\n\
         Message: Plan my weekend trip to Tokyo with a focus on food\n\
         Title: Tokyo Weekend Food Trip\n\n\
         Message: Why does my React component keep re-rendering on every prop change?\n\
         Title: Debugging React Re-Renders\n\n\
         Message: {}\n\
         Title:",
        &first_message[..preview_len]
    );

    let messages = vec![
        ChatMessage::user(prompt),
    ];
    let opts = GenerationOptions {
        temperature: 0.3,
        // Larger than a 6-word title needs: some reasoning models emit a
        // <think> block first and the tokens count against this cap. We
        // strip the block below, but a tight cap truncates the actual title
        // to nothing usable.
        max_tokens:  Some(80),
        ..Default::default()
    };

    let result = match context.as_ref() {
        Some(context) => agent.provider.generate_with_context(&messages, &opts, context).await,
        None => agent.provider.generate(&messages, &opts).await,
    };

    match result {
        Ok(resp) => {
            let title = sanitize_auto_title(&resp.content).unwrap_or(fallback);
            if let Err(e) = history.update_conversation_title(&conv_id, &title) {
                warn!("auto-title update failed: {}", e);
            } else {
                info!("Auto-title set: {:?} for conv={}", title, conv_id);
            }
        }
        Err(e) => {
            warn!("Auto-title generation failed (non-fatal): {}", e);
        }
    }
}

// Pull a usable title out of an auto-title LLM response. Strips
// `<think>…</think>` reasoning blocks (OpenRouter passes them through
// verbatim for Qwen/DeepSeek-style models), then takes the last non-empty
// line as the title proper — preamble, if any, sits above it. Returns
// `None` when nothing plausible survives so the caller can fall back to
// the preview-based default instead of naming the conversation
// "Let me analyze the given information…".
/// ASCII-case-insensitive substring search, returning a byte index. Safe to
/// slice at (the needle is ASCII, so matches land on char boundaries).
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() { return None; }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Strip reasoning-model chain-of-thought blocks. Models wrap it in either
/// `<think>…</think>` OR `<thinking>…</thinking>` (and either case) — note
/// `<think>` is NOT a substring of `<thinking>`, so both must be handled
/// explicitly. An UNCLOSED opener (the title token cap truncated mid-thought)
/// discards everything from the opener on, so a half-emitted thought never
/// becomes the title.
fn strip_reasoning_blocks(raw: &str) -> String {
    const TAGS: [(&str, &str); 2] = [("<thinking>", "</thinking>"), ("<think>", "</think>")];
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        // Earliest opener of any tag.
        let next = TAGS.iter()
            .filter_map(|&(open, close)| find_ci(rest, open).map(|i| (i, open, close)))
            .min_by_key(|&(i, _, _)| i);
        match next {
            None => { out.push_str(rest); break; }
            Some((start, open, close)) => {
                out.push_str(&rest[..start]);
                let after = &rest[start + open.len()..];
                match find_ci(after, close) {
                    Some(end) => rest = &after[end + close.len()..],
                    None      => break, // unclosed → drop everything after the opener
                }
            }
        }
    }
    out
}

fn sanitize_auto_title(raw: &str) -> Option<String> {
    let stripped = strip_reasoning_blocks(raw);

    // Walk lines bottom-up so a trailing title still wins over earlier
    // preamble, but skip any line that smells like restated instructions
    // ("* No preamble", "Here's the title:", etc.).
    stripped
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rev()
        .find_map(clean_title_line)
}

fn clean_title_line(line: &str) -> Option<String> {
    // A line that carries markdown bold (`**…**`) or opens with an ordered-list
    // marker (`1.`, `2)`) is a reliable tell that the model answered the *meta*
    // question — it emitted its reasoning scaffolding ("1. **Analyze User
    // Input:**", "**Generate Title:**") instead of a title. Reject the whole
    // line so the caller keeps scanning (or falls back) rather than persisting
    // the scaffolding as the conversation's name.
    if line.contains("**") {
        return None;
    }
    {
        let t = line.trim_start();
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && t[digits..].starts_with(['.', ')']) {
            return None;
        }
    }

    let stripped = line
        .trim_start_matches(|c: char| matches!(c, '*' | '-' | '•' | '#' | '>' | '·' | '–' | '—'))
        .trim()
        .trim_start_matches("Title:")
        .trim_start_matches("title:")
        .trim_start_matches("TITLE:")
        .trim_start_matches("Message:")
        .trim_start_matches("message:")
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches('.')
        .trim_end_matches(':')
        .trim()
        .to_owned();

    if stripped.is_empty() { return None; }
    if stripped.len() > 80 || stripped.split_whitespace().count() > 12 { return None; }
    // Single tokens are almost always junk ("Title", "Conversation",
    // "Untitled") — a real title needs ≥ 2 words to be useful.
    if stripped.split_whitespace().count() < 2 { return None; }

    // Reject lines that obviously echo the prompt's constraints / template
    // back, or that just regurgitate one of the few-shot example titles
    // verbatim regardless of the user's actual message.
    let lower = stripped.to_ascii_lowercase();
    let echoes = [
        "no preamble", "no quote", "no explanation", "no reasoning",
        "no markdown", "respond with", "here is the title", "here's the title",
        "as instructed", "as requested", "summarising the topic",
        "summarizing the topic", "generate a", "3-6 word", "title summaris",
        "title summariz",
        // Reasoning-distilled models (Qwen, Gemma derivatives) often emit
        // first-person planning or meta-commentary about *the title* rather
        // than a title itself. Catch the most common openings.
        "the user is asking", "the user wants", "the user's message",
        "the message is", "this conversation", "for this conversation",
        "a good title", "a suitable title", "a concise title",
        "a possible title", "a fitting title",
        "let me think", "let me consider", "let me suggest",
        "based on the message", "based on this message", "considering the",
        "thinking about", "i'd suggest", "i would suggest",
        "i think", "i'll go with", "i would go with",
        // Few-shot example titles — model-leaked answers, never a real title.
        "sorting python dicts by key",
        "tokyo weekend food trip",
        "debugging react re-renders",
    ];
    if echoes.iter().any(|p| lower.contains(p)) { return None; }

    Some(stripped)
}

#[cfg(test)]
mod voice_v1_request_tests {
    use super::ChatRequest;

    // Backward-compat: a pre-voice client sends only the old fields. All new
    // voice fields must default to None (today's behaviour) and nothing breaks.
    #[test]
    fn minimal_legacy_payload_leaves_voice_fields_none() {
        let req: ChatRequest = serde_json::from_str(r#"{"message":"hi"}"#).unwrap();
        assert_eq!(req.message, "hi");
        assert!(req.channel.is_none());
        assert!(req.reply_language.is_none());
        assert!(req.style.is_none());
        assert!(req.debug_timings.is_none());
        assert!(req.speculative.is_none());
    }

    // A voice client opts in via the body fields.
    #[test]
    fn voice_payload_parses_all_fields() {
        let req: ChatRequest = serde_json::from_str(
            r#"{"message":"hi","channel":"voice","reply_language":"es",
                "style":"spoken","debug_timings":true,"speculative":true}"#,
        ).unwrap();
        assert_eq!(req.channel.as_deref(), Some("voice"));
        assert_eq!(req.reply_language.as_deref(), Some("es"));
        assert_eq!(req.style.as_deref(), Some("spoken"));
        assert_eq!(req.debug_timings, Some(true));
        assert_eq!(req.speculative, Some(true));
    }

    // Forward-compat: an unknown field from a newer client is ignored, not a
    // 422 (ChatRequest intentionally does not deny unknown fields).
    #[test]
    fn unknown_field_is_ignored() {
        let req: ChatRequest =
            serde_json::from_str(r#"{"message":"hi","some_future_field":42}"#).unwrap();
        assert_eq!(req.message, "hi");
    }

    // Cancel body: empty `{}` is a plain cancel (keep what was delivered);
    // fields parse when present.
    #[test]
    fn cancel_body_parses_empty_and_full() {
        use super::CancelTurnBody;
        let empty: CancelTurnBody = serde_json::from_str("{}").unwrap();
        assert!(empty.spoken_text.is_none());
        assert!(!empty.discard);

        let full: CancelTurnBody =
            serde_json::from_str(r#"{"spoken_text":"so far","discard":true}"#).unwrap();
        assert_eq!(full.spoken_text.as_deref(), Some("so far"));
        assert!(full.discard);
    }

    // Commit body: empty `{}` proceeds (no message); a message restarts.
    #[test]
    fn commit_body_parses_empty_and_message() {
        use super::CommitTurnBody;
        let empty: CommitTurnBody = serde_json::from_str("{}").unwrap();
        assert!(empty.message.is_none());
        let full: CommitTurnBody =
            serde_json::from_str(r#"{"message":"final transcript"}"#).unwrap();
        assert_eq!(full.message.as_deref(), Some("final transcript"));
    }
}

#[cfg(test)]
mod auto_title_tests {
    use super::sanitize_auto_title;

    #[test]
    fn strips_think_block_and_keeps_title() {
        let raw = "<think>let me consider</think>\nMorning Coffee Plans";
        assert_eq!(sanitize_auto_title(raw).as_deref(), Some("Morning Coffee Plans"));
    }

    #[test]
    fn strips_thinking_block_too() {
        // The <thinking>…</thinking> variant (not a substring of <think>) was
        // leaking into titles verbatim — this is the regression under test.
        let raw = "<thinking>the user is asking about X, so…</thinking>\nTokyo Food Trip";
        assert_eq!(sanitize_auto_title(raw).as_deref(), Some("Tokyo Food Trip"));
    }

    #[test]
    fn strips_thinking_case_insensitive_and_inline() {
        let raw = "<Thinking>hmm</Thinking> Budget Spreadsheet Help";
        assert_eq!(sanitize_auto_title(raw).as_deref(), Some("Budget Spreadsheet Help"));
    }

    #[test]
    fn unclosed_thinking_yields_no_title_so_caller_falls_back() {
        // Token cap truncates mid-thought: no closing tag. Must NOT surface the
        // half-thought as the title — returns None so the heuristic fallback wins.
        let raw = "<thinking>let me work out what the best title would be for this";
        assert!(sanitize_auto_title(raw).is_none());
    }

    #[test]
    fn rejects_reasoning_preamble_without_title() {
        let raw = "Let me analyze the given information and compose an appropriate response. I need to expand these highlights";
        assert!(sanitize_auto_title(raw).is_none());
    }

    #[test]
    fn strips_title_prefix_and_quotes() {
        let raw = "Title: \"Weekend Trip\"";
        assert_eq!(sanitize_auto_title(raw).as_deref(), Some("Weekend Trip"));
    }

    #[test]
    fn drops_unterminated_think_remainder() {
        let raw = "<think>still thinking with no closing tag";
        assert!(sanitize_auto_title(raw).is_none());
    }

    #[test]
    fn rejects_bulleted_constraint_echo() {
        // The actual failure that prompted this fix: model bulleted the
        // negative directives back to us, then got truncated mid-sentence.
        let raw = "* No preamble (no \"Here";
        assert!(sanitize_auto_title(raw).is_none());
    }

    #[test]
    fn skips_bulleted_preamble_to_reach_real_title() {
        let raw = "Here are the rules:\n- short\n- descriptive\n\nWeekend Trip Planning";
        assert_eq!(
            sanitize_auto_title(raw).as_deref(),
            Some("Weekend Trip Planning"),
        );
    }

    #[test]
    fn rejects_numbered_bold_reasoning_scaffold() {
        // The exact live failure: the model emitted its own step-list ("1.
        // **Analyze User Input:**") as the title. Both the ordered-list marker
        // and the markdown bold are meta-answer tells → reject, don't persist.
        assert!(sanitize_auto_title("1.  **Analyze User Input:**").is_none());
        assert!(sanitize_auto_title("2) Generate a Title").is_none());
        assert!(sanitize_auto_title("**Generate Title:** something").is_none());
    }

    #[test]
    fn reaches_real_title_below_numbered_bold_scaffold() {
        // Scaffolding lines are skipped bottom-up until a clean title is found.
        let raw = "1. **Analyze the message**\n2. **Compose a title**\nWeekend Trip Planning";
        assert_eq!(
            sanitize_auto_title(raw).as_deref(),
            Some("Weekend Trip Planning"),
        );
    }

    #[test]
    fn rejects_regurgitated_few_shot_example() {
        // Smaller models sometimes copy the first demo title verbatim instead
        // of producing a real one for the user's message.
        assert!(sanitize_auto_title("Sorting Python Dicts By Key").is_none());
        assert!(sanitize_auto_title("Tokyo Weekend Food Trip").is_none());
        assert!(sanitize_auto_title("Debugging React Re-Renders").is_none());
    }

    #[test]
    fn rejects_single_word_titles() {
        // "Conversation", "Untitled", "Title" — never useful, always junk.
        assert!(sanitize_auto_title("Conversation").is_none());
        assert!(sanitize_auto_title("Untitled").is_none());
    }

    #[test]
    fn rejects_prompt_template_leak() {
        // Model parrots the instruction line back as the title.
        assert!(sanitize_auto_title("Generate a 3-6 word title").is_none());
        assert!(sanitize_auto_title("Title summarising the topic of the message").is_none());
    }

    #[test]
    fn strips_message_prefix_when_model_echoes_template() {
        // Some models echo "Message: <text>" before the actual title; if the
        // last non-empty line is the user's message restated, drop it.
        let raw = "Message: Plan my dinner\nTitle: Dinner Planning";
        assert_eq!(sanitize_auto_title(raw).as_deref(), Some("Dinner Planning"));
    }

    #[test]
    fn strips_trailing_punctuation() {
        assert_eq!(sanitize_auto_title("Weekend Plans.").as_deref(), Some("Weekend Plans"));
        assert_eq!(sanitize_auto_title("Topic:").as_deref(), None); // single word after strip
    }

    #[test]
    fn rejects_reasoning_meta_commentary() {
        // Distilled reasoning models (Qwen, Gemma derivatives) emit prose
        // about *how they'd write a title* instead of one. Any line of that
        // shape must drop to fallback.
        assert!(sanitize_auto_title(
            "The user is asking about Python dict sorting"
        ).is_none());
        assert!(sanitize_auto_title(
            "Let me think about a good title for this conversation"
        ).is_none());
        assert!(sanitize_auto_title(
            "A possible title might be: Weekend Trip"
        ).is_none());
        assert!(sanitize_auto_title(
            "Based on the message, I'd suggest something topical"
        ).is_none());
    }
}

#[cfg(test)]
mod derive_title_tests {
    use super::derive_title_from_message;

    #[test]
    fn strips_question_preamble_and_titlecases() {
        assert_eq!(
            derive_title_from_message(
                "How do I sort a list of dicts in Python by a key?"
            ),
            "Sort A List Of Dicts In Python By A Key",
        );
    }

    #[test]
    fn strips_polite_imperative_preamble() {
        assert_eq!(
            derive_title_from_message("Please help me debug a React re-render loop"),
            "Help Me Debug A React Re-render Loop",
        );
    }

    #[test]
    fn keeps_short_message_intact() {
        assert_eq!(derive_title_from_message("Hello there"), "Hello There");
    }

    #[test]
    fn takes_first_sentence_only() {
        assert_eq!(
            derive_title_from_message(
                "Plan my Tokyo trip. I want food, museums, and a day in Kyoto."
            ),
            "Plan My Tokyo Trip",
        );
    }

    #[test]
    fn truncates_at_word_boundary_with_ellipsis() {
        let t = derive_title_from_message(
            "Write a detailed analysis of the macroeconomic impact of \
             inflation on developing countries in the southern hemisphere"
        );
        assert!(t.chars().count() <= 60);
        assert!(t.ends_with('…'), "expected ellipsis suffix, got: {t:?}");
        assert!(!t.contains("  "), "double spaces from cut: {t:?}");
    }

    #[test]
    fn preserves_acronyms() {
        assert_eq!(
            derive_title_from_message("explain HTTP status codes briefly"),
            "HTTP Status Codes Briefly",
        );
    }

    #[test]
    fn handles_empty_after_trim() {
        assert_eq!(derive_title_from_message("   "), "New Conversation");
    }
}
