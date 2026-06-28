// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/chat.rs
//! POST /api/chat — streaming chat over SSE with history persistence and auto-title.

use std::sync::Arc;

use axum::{
    extract::Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Sse},
    Extension,
};
use axum::response::sse::{Event, KeepAlive};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

use crate::agent::{AgentCore, StreamEvent, TurnContext};
use crate::auth::{AuthUser, LocalAuthService};
use crate::history::{HistoryStore, NewConversation, NewMessage, MessageRole};
use crate::notifications::{NotificationBus, Notification, NotificationKind};
use crate::onboarding::{
    apply_ops, build_onboarding_prompt, extract_updates_from_transcript,
    OnboardingSchema, ProfilePreambleCache,
};
use crate::server::handlers::onboarding::DataDir;
use crate::providers::ModelProvider;
use crate::providers::lmstudio::LmStudioProvider;
use crate::providers::openrouter::OpenRouterProvider;
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
    AuthUser(user): AuthUser,
    Extension(agent):    Extension<Arc<AgentCore>>,
    Extension(history):  Extension<Arc<HistoryStore>>,
    Extension(auth):     Extension<Arc<LocalAuthService>>,
    Extension(notifs):   Extension<Arc<NotificationBus>>,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(preamble): Extension<Arc<ProfilePreambleCache>>,
    Extension(data_dir): Extension<DataDir>,
    Extension(mcp_servers): Extension<Arc<crate::mcp::McpServerRegistry>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> axum::response::Response {
    // ── Resolve or create conversation ────────────────────────────────────────
    let config = live_cfg.get().await;

    // Channel tag for conversations created from this turn. Native clients
    // send `X-Mira-Client: android` (any non-"web" value is treated as a
    // mobile/native client); absent → "web" (backward-compatible). Per-
    // conversation channel, so tagging at creation is sufficient. We honour
    // the header (what the Android app sends today) rather than a body field.
    let new_channel: &str = match headers
        .get("x-mira-client")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("") | Some("web") => "web",
        Some(_) => "mobile",
    };
    let model = req.model_override
        .clone()
        .unwrap_or_else(|| config.providers.lmstudio.default_model.clone());

    // Resolve the conversation up front so we can branch on its `mode`.
    let (conv_id, conv_mode) = match &req.conversation_id {
        Some(id) => {
            match history.get_conversation(id) {
                Ok(Some(c)) if c.user_id == user.id => (id.clone(), c.mode),
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
                Ok(c)  => (c.id, c.mode),
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
    if let Err(e) = history.add_message(NewMessage {
        conversation_id: conv_id.clone(),
        role:            MessageRole::User,
        content:         req.message.clone(),
        content_type:    "text".to_owned(),
        token_count:     None,
        model:           None,
        tool_calls:      None,
        metadata:        user_metadata,
    }) {
        warn!("Failed to record user message: {}", e);
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
    // selected a specific model / provider from the frontend dropdown.
    let turn_provider: Option<Arc<dyn ModelProvider>> = match req.provider_override.as_deref() {
        Some("openrouter") => {
            if let Some(ref api_key) = config.providers.openrouter.api_key {
                let model = req.model_override.clone()
                    .unwrap_or_else(|| config.providers.openrouter.default_model.clone());
                Some(Arc::new(OpenRouterProvider::new(api_key.clone(), model))
                    as Arc<dyn ModelProvider>)
            } else {
                warn!("OpenRouter selected but no API key configured — falling back to default");
                None
            }
        }
        Some("lmstudio") => {
            let model = req.model_override.clone()
                .unwrap_or_else(|| config.providers.lmstudio.default_model.clone());
            Some(Arc::new(
                LmStudioProvider::new(config.providers.lmstudio.url.clone(), model)
                    .with_token_caps(
                        config.agent.max_tool_round_tokens,
                        config.agent.max_response_tokens,
                    )
            ) as Arc<dyn ModelProvider>)
        }
        _ => None, // use the agent's default provider
    };

    let session_id    = format!("web-{}", conv_id);
    let user_id       = user.id.clone();
    let message       = req.message.clone();
    let conv_id_c     = conv_id.clone();
    // Snapshot attachments for the agent invocation below. Cheap clone
    // these are small base64 strings; provider-side serialisation
    // owns the further translation work.
    let req_attachments = req.attachments.clone().unwrap_or_default();
    // Echoed back in the SSE `done` payload so clients can render
    // model/provider attribution + look up pricing without a round-trip.
    let provider_name = req.provider_override.clone()
        .unwrap_or_else(|| agent.provider.name().to_owned());
    // Snapshot the LLM auto-extract config for this turn. The mode switch
    // is read once so mid-stream config reloads don't skew behaviour.
    let auto_extract_cfg = config.memory.auto_extract.clone();
    // Captured by the spawn alongside `agent`/`history` so the post-turn
    // extractor (onboarding mode only) can run without re-plumbing extensions.
    let auth_for_task = Arc::clone(&auth);
    let mode_for_task = conv_mode.clone();
    // The extractor must use the same model the user picked in the UI for this
    // turn. Otherwise selecting e.g. gemma in the dropdown but having a
    // different default provider wired up at startup would cause chats to go
    // to gemma while the extractor silently hit the default model. Clone the
    // per-turn provider (if any) for the extractor; fall back to the agent
    // default when no override was sent.
    let extractor_provider: Arc<dyn ModelProvider> = turn_provider
        .clone()
        .unwrap_or_else(|| Arc::clone(&agent.provider));

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

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);

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
        match agent.process_with_context(
            &session_id, &user_id, "web", &message, turn_provider, turn_ctx,
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
                        Some(StreamEvent::Token(t)) => {
                            full_response.push_str(&t);
                            let _ = tx.send(Ok(
                                Event::default().event("token").data(t)
                            )).await;
                        }
                        Some(StreamEvent::Done { usage }) => {
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

                            if let Err(e) = history.add_message(NewMessage {
                                conversation_id: conv_id_c.clone(),
                                role:            MessageRole::Assistant,
                                content:         full_response.clone(),
                                content_type:    "text".to_owned(),
                                token_count:     Some(usage.completion_tokens as i32),
                                model:           Some(model.clone()),
                                tool_calls:      tool_calls_json,
                                metadata:        metadata_json,
                            }) {
                                warn!("Failed to persist assistant message: {}", e);
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
                                tokio::spawn(async move {
                                    generate_auto_title(agent2, hist2, cid, msg_c, preview).await;
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
                                ).await;
                                if extractor_flipped && !primary_flipped {
                                    let _ = tx.send(Ok(
                                        Event::default().event("onboarding_complete").data("{}")
                                    )).await;
                                }
                            } else if auto_extract_cfg.effective_extractor("web")
                                == crate::config::ExtractorKind::Llm
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
                                    ).await;
                                });
                            }
                            break;
                        }
                        Some(StreamEvent::Error(e)) => {
                            let _ = tx.send(Ok(Event::default().event("error").data(e))).await;
                            break;
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
            }
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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

    let updates = extract_updates_from_transcript(
        &provider,
        &schema,
        &transcript,
        &progress,
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
        "Generate a 3-6 word title summarising the topic of the message.\n\n\
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

    match agent.provider.generate(&messages, &opts).await {
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
fn sanitize_auto_title(raw: &str) -> Option<String> {
    let mut stripped = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("<think>") {
        stripped.push_str(&rest[..start]);
        let after = &rest[start + "<think>".len()..];
        match after.find("</think>") {
            Some(end) => rest = &after[end + "</think>".len()..],
            None      => { rest = ""; break; }
        }
    }
    stripped.push_str(rest);

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
mod auto_title_tests {
    use super::sanitize_auto_title;

    #[test]
    fn strips_think_block_and_keeps_title() {
        let raw = "<think>let me consider</think>\nMorning Coffee Plans";
        assert_eq!(sanitize_auto_title(raw).as_deref(), Some("Morning Coffee Plans"));
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
