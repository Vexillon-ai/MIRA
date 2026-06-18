// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/tool_loop.rs
//! Tool-calling loop for [`AgentCore`].
//!
//! Supports three protocols:
//!
//! * **OpenAI** — the model returns structured `tool_calls` in
//! [`GenerationResponse`]. Each call is executed and its result is appended
//! as a `role: tool` message before the next generate round.
//!
//! * **Hermes / Qwen / Gemini XML** — the model emits function calls as text
//! tokens embedded in `content`:
//! ```text
//! <tool_call>{"name": "foo", "arguments": {"k": "v"}}</tool_call>
//! ```
//! `<tool_code>` and `<function_call>` are accepted as aliases — different
//! fine-tunes emit different tags. Used by Qwen2.5/3, Hermes-2-Pro,
//! Gemini-style distillations, and many open-weight derivatives. We parse
//! each block, synthesise call ids, and route through the same execution
//! path as the OpenAI branch. The visible content has the XML stripped
//! before it's added to history.
//!
//! * **ReAct** — the model uses plain text patterns:
//! ```text
//! Thought: <reasoning>
//! Action: <tool_name>
//! Action Input: <json or plain text>
//! ```
//! The loop parses these patterns, executes the tool, and appends an
//! `Observation:` block before the next generate round.
//!
//! The `"auto"` mode tries OpenAI structured calls, then Hermes XML, then
//! falls back to ReAct text parsing. `"openai"` runs structured + Hermes
//! (both are function-style); `"react"` runs ReAct only.

use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::agent::stream::StreamEvent;
use crate::events::{names as event_names, EventBus};
use crate::providers::ModelProvider;
use crate::tools::ToolRegistry;
use crate::types::{ChatMessage, GenerationOptions, MessageRole, TokenUsage, ToolCall};
use crate::MiraError;

// Sidecar so failed tool calls can publish a `tool.failed` event without
// the loop needing to know the bus + user details. Both fields are optional
// emit no-ops when the bus isn't installed (unit tests).
#[derive(Clone, Copy)]
pub struct ToolEventCtx<'a> {
    pub bus:     Option<&'a Arc<EventBus>>,
    pub user_id: &'a str,
}

impl<'a> ToolEventCtx<'a> {
    pub const NONE: Self = Self { bus: None, user_id: "" };
}

// ─────────────────────────────────────────────────────────────────────────────

// Tool-calling protocol selection.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolMode {
    // Disabled — never call tools, return model output directly.
    Disabled,
    // OpenAI structured `tool_calls` only.
    OpenAi,
    // ReAct text parsing only.
    React,
    // Try OpenAI first, fall back to ReAct if no structured calls found.
    Auto,
}

impl ToolMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "openai" => Self::OpenAi,
            "react"  => Self::React,
            "auto"   => Self::Auto,
            _        => Self::Disabled,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

// Run the complete tool loop and stream events to `tx`.
// // Returns the final assembled text response after all tool rounds complete.
pub async fn run_tool_loop(
    provider:   &Arc<dyn ModelProvider>,
    tools:      &Arc<ToolRegistry>,
    messages:   &mut Vec<ChatMessage>,
    options:    &GenerationOptions,
    mode:       &ToolMode,
    max_rounds: usize,
    tx:         &mpsc::Sender<StreamEvent>,
) -> Result<(String, TokenUsage), MiraError> {
    run_tool_loop_with_context(
        provider, tools, messages, options, mode, max_rounds, tx,
        None, &serde_json::Map::new(), ToolEventCtx::NONE,
    ).await
}

// Context-aware tool loop. `allowed_tool_names = Some(&[...])` restricts the
// loop to that exact set — tool calls outside it are returned as an error
// result so the model can correct itself. `inject_args` is merged into every
// tool call's arguments before execution (useful for stamping caller
// identity the LLM doesn't get to see or change).
pub async fn run_tool_loop_with_context(
    provider:           &Arc<dyn ModelProvider>,
    tools:              &Arc<ToolRegistry>,
    messages:           &mut Vec<ChatMessage>,
    options:            &GenerationOptions,
    mode:               &ToolMode,
    max_rounds:         usize,
    tx:                 &mpsc::Sender<StreamEvent>,
    allowed_tool_names: Option<&[String]>,
    inject_args:        &serde_json::Map<String, serde_json::Value>,
    event_ctx:          ToolEventCtx<'_>,
) -> Result<(String, TokenUsage), MiraError> {

    if *mode == ToolMode::Disabled || tools.is_empty() {
        // gate the single streaming call too, otherwise
        // the no-tools path bypasses the policy engine entirely.
        gate_llm_call(tools, provider, inject_args).await?;
        return run_streaming_no_tools(provider, messages, options, tx).await;
    }

    // Build OpenAI-format tool specs for every tool the model is allowed to
    // call this turn, and inject them into `options`. Without this, providers
    // send the chat-completions request with no `tools` field and the model
    // can only "describe" tool use in prose — which is exactly what small
    // local models (Qwen, Hermes, etc.) do when they don't get a schema.
    let tool_specs = build_tool_specs(tools, allowed_tool_names);

    // Cross-round duplicate tracking. LM Studio's sampling switch (qwen
    // distills, Hermes) forces ONE `<tool_call>` per response, so models
    // that want to emit a batch of 3-4 calls get split across 3-4 rounds.
    // A reasoning-distilled model then keeps re-reading its earlier calls
    // in reasoning and sometimes re-issues them, burning rounds until we
    // hit `max_rounds` and error out. When we see the same `(name, args)`
    // pair twice, refuse to dispatch and return a stern observation — this
    // breaks the loop rather than burning the whole rounds budget.
    let mut seen_calls: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    // Surface a provider failover once per turn (the primary may fail on
    // every round; we only want to tell the user once).
    let mut fallback_warned = false;

    for round in 0..max_rounds {
        debug!("Tool loop round {}/{}", round + 1, max_rounds);

        // Build per-round options.
        //
        // `tool_choice: "auto"` on every round. We used to force
        // `"required"` on round 0 of flow-restricted turns (onboarding) to
        // make the model record answers instead of narrating, but that
        // strategy now backfires:
        //
        // * Reasoning-distilled local models (gemma-4-26b-a4b,
        //   qwen3.5-*-distilled) spiral when `"required"` tells them they
        //   MUST emit a tool call on a turn where no call is naturally
        //   warranted — e.g. they've already asked the next question and
        //   have nothing to record yet. We saw gemma degenerate into
        //   `agent_style:persona_name,agent_style:persona_name,…` loops
        //   until it hit max_tokens, producing no parseable tool call and
        //   leaving the user with no response.
        //
        // * The onboarding extractor (`src/onboarding/extractor.rs`) now
        //   handles narrated answers server-side after the turn finishes.
        //   The primary model's tool calls are best-effort; the extractor
        //   is the source of truth. Forcing tool calls from the primary
        //   model is redundant AND harmful.
        //
        // Non-onboarding tool flows were already fine on `"auto"`, so we
        // unify the behaviour.
        let mut opts_with_tools = options.clone();
        if !tool_specs.is_empty() {
            opts_with_tools.tools = Some(tool_specs.clone());
            opts_with_tools.tool_choice = Some(serde_json::json!("auto"));
        }

        // LLM-call policy gate. Fires for every tool-loop
        // round when an engine is wired AND the inject_args carry an
        // `_agent_id`. Skips cleanly otherwise (tests, legacy callers).
        gate_llm_call(tools, provider, inject_args).await?;

        // Generate (non-streaming during tool rounds, streaming on final answer)
        let resp = provider.generate(messages, &opts_with_tools).await?;
        // Provider failover surfacing — the configured/primary provider failed
        // and a fallback answered. Tell the user once so a silently-degraded
        // (e.g. local) reply isn't mistaken for their chosen model.
        if !fallback_warned {
            if let Some(fb) = &resp.fallback {
                let _ = tx.send(StreamEvent::Warning(fallback_warning_message(fb))).await;
                fallback_warned = true;
            }
        }
        // Surface reasoning models' chain-of-thought as a dedicated
        // stream event so the chat UI can render it separately. Done
        // here per-round; reasoning across multiple tool-loop rounds
        // arrives as multiple Reasoning events.
        if let Some(reasoning) = resp.reasoning.as_ref().filter(|s| !s.is_empty()) {
            let _ = tx.send(StreamEvent::Reasoning(reasoning.clone())).await;
        }
        // Some reasoning models (Qwen distills, etc.) leak `<think>…</think>`
        // blocks into `content`. Those blocks can contain anything — including
        // the model *quoting* prior prompts with `<tool_call>` XML inside them
        // which would then be mis-parsed as real tool invocations. Strip
        // reasoning first so every downstream parser / stripper sees only
        // user-visible prose + genuine tool-call markup.
        let content = strip_think_blocks(&resp.content);
        let _usage  = resp.usage.clone();

        // ── Try OpenAI structured tool_calls ──────────────────────────────────
        let has_openai_calls = resp.tool_calls.as_ref().map_or(false, |tc| !tc.is_empty());
        if (*mode == ToolMode::OpenAi || *mode == ToolMode::Auto) && has_openai_calls {
            let tool_calls = dedupe_tool_calls(resp.tool_calls.unwrap());
            // Append the assistant message with tool_calls to history
            messages.push(ChatMessage {
                role:         MessageRole::Assistant,
                content:      content.clone(),
                tool_calls:   Some(tool_calls.clone()),
                tool_call_id: None,
                attachments:  None,
            });

            for tc in &tool_calls {
                let merged = merge_injected_args(tc.arguments.clone(), inject_args);
                let _ = tx.send(StreamEvent::ToolCall {
                    name:    tc.name.clone(),
                    args:    merged.to_string(),
                    call_id: tc.call_id.clone(),
                }).await;

                let (output, success) = dispatch_tool_call(
                    tools, &tc.name, merged, allowed_tool_names, &mut seen_calls,
                    event_ctx,
                ).await;

                info!("Tool '{}' → success={} output_len={}", tc.name, success, output.len());
                let _ = tx.send(StreamEvent::ToolResult {
                    name:    tc.name.clone(),
                    output:  output.clone(),
                    success,
                    call_id: tc.call_id.clone(),
                }).await;

                messages.push(ChatMessage::tool(output, tc.call_id.clone()));
            }
            continue; // next round
        }

        // ── Try Hermes / Qwen-style `<tool_call>` XML in content ──────────
        // Models like Qwen2.5/3 and Hermes-2-Pro emit function calls as
        // text tokens rather than native structured output. Parse the
        // blocks, execute each, and append the result in the format these
        // models were trained to read back.
        //
        // Two choices here that matter:
        //
        // * We keep the **raw** assistant content (with XML intact). Most
        //   local providers (LM Studio / Ollama / llama.cpp via
        //   OpenAI-compat) don't forward structured `tool_calls` or
        //   `tool_call_id` — they serialize messages as `{role, content}`
        //   and nothing else. If we strip the XML, the model sees an
        //   empty assistant turn followed by an out-of-nowhere tool
        //   message it can't correlate, and tends to retry the same call.
        //
        // * Results are appended as a **user-role** message wrapping the
        //   output in `<tool_response>`. That's the Qwen/Hermes training
        //   convention and it survives the `{role, content}`-only
        //   serialization that `role=tool` does not.
        if *mode == ToolMode::OpenAi || *mode == ToolMode::Auto {
            let hermes_calls = parse_hermes_tool_calls(&content);
            if !hermes_calls.is_empty() {
                let tool_calls: Vec<ToolCall> = dedupe_tool_calls(
                    hermes_calls.into_iter()
                        .map(|(name, args)| ToolCall {
                            name,
                            arguments: args,
                            call_id: uuid::Uuid::new_v4().to_string(),
                        })
                        .collect()
                );

                messages.push(ChatMessage {
                    role:         MessageRole::Assistant,
                    content:      content.clone(),
                    tool_calls:   Some(tool_calls.clone()),
                    tool_call_id: None,
                attachments:  None,
                });

                for tc in &tool_calls {
                    let merged = merge_injected_args(tc.arguments.clone(), inject_args);
                    let _ = tx.send(StreamEvent::ToolCall {
                        name:    tc.name.clone(),
                        args:    merged.to_string(),
                        call_id: tc.call_id.clone(),
                    }).await;

                    let (output, success) = dispatch_tool_call(
                        tools, &tc.name, merged, allowed_tool_names, &mut seen_calls,
                        event_ctx,
                    ).await;

                    info!("Tool '{}' (Hermes) → success={} output_len={}", tc.name, success, output.len());
                    let _ = tx.send(StreamEvent::ToolResult {
                        name:    tc.name.clone(),
                        output:  output.clone(),
                        success,
                        call_id: tc.call_id.clone(),
                    }).await;

                    // Prefer the structured form: parse the tool's output as
                    // JSON when possible so the model sees
                    // <tool_response>{"name": "...", "content": {"iana":...}}</tool_response>
                    // rather than a doubly-escaped JSON-inside-JSON string.
                    // Small models parse the structured form far more reliably.
                    let content_val = serde_json::from_str::<serde_json::Value>(&output)
                        .unwrap_or_else(|_| serde_json::Value::String(output.clone()));
                    let wrapped = format!(
                        "<tool_response>{}</tool_response>",
                        serde_json::json!({ "name": tc.name, "content": content_val }),
                    );
                    messages.push(ChatMessage::user(wrapped));
                }
                continue; // next round
            }
        }

        // ── Try ReAct text parsing ─────────────────────────────────────────
        if *mode == ToolMode::React || *mode == ToolMode::Auto {
            if let Some((tool_name, tool_args)) = parse_react_action(&content) {
                let call_id = uuid::Uuid::new_v4().to_string();

                // Append the assistant's reasoning + action to history
                messages.push(ChatMessage::assistant(content.clone()));

                let merged = merge_injected_args(tool_args, inject_args);
                let _ = tx.send(StreamEvent::ToolCall {
                    name:    tool_name.clone(),
                    args:    merged.to_string(),
                    call_id: call_id.clone(),
                }).await;

                let (output, success) = dispatch_tool_call(
                    tools, &tool_name, merged, allowed_tool_names, &mut seen_calls,
                    event_ctx,
                ).await;

                info!("Tool '{}' (ReAct) → success={}", tool_name, success);
                let _ = tx.send(StreamEvent::ToolResult {
                    name:    tool_name.clone(),
                    output:  output.clone(),
                    success,
                    call_id: call_id.clone(),
                }).await;

                // Append observation for next round
                let observation = format!("Observation: {}", output);
                messages.push(ChatMessage::user(observation));
                continue; // next round
            }
        }

        // ── No tool calls found — this is the final answer ────────────────
        // Replay the probe's content as pseudo-streamed tokens rather than
        // doing a second generate_stream call. The original two-call design
        // was non-deterministic: the probe's tool-call detection could pass
        // (no calls found) while the stream's fresh generation emitted raw
        // Hermes XML, which then leaked verbatim to the UI. Replaying what
        // we already parsed guarantees the user sees exactly the content
        // we ran tool-call detection against.
        let visible = strip_hermes_tool_calls(&content);

        // No tool calls found → this is the final answer. Don't try to
        // "correct" the model with a nudge. On onboarding flows the
        // post-turn extractor will back-fill anything the model narrated
        // but didn't record; on free-form flows the model's prose is
        // genuinely the next thing to say. The nudge used to over-trigger
        // on natural follow-up questions and pushed the model into
        // confused tool-spew loops.
        debug!("No tool calls in round {} — replaying probe as stream", round + 1);
        replay_as_stream(&visible, tx).await;
        return Ok((visible, resp.usage));
    }

    // Reaching here means we exhausted `max_rounds` without the model ever
    // producing a plain-text final answer. Rather than surfacing a fatal
    // `ToolRoundLimitReached` to the user (which on onboarding turns would
    // abort the turn and freeze the UI), nudge the model with one final
    // non-tool generation. The onboarding extractor runs post-turn anyway,
    // so any missed tool calls get back-filled server-side.
    warn!("Tool loop hit max_rounds={} — asking model for a final prose reply", max_rounds);
    messages.push(ChatMessage::user(
        "You've used all your tool calls for this turn. Stop calling tools now and \
         respond to me directly in plain prose.".to_string()
    ));
    let mut final_opts = options.clone();
    final_opts.tools       = None;
    final_opts.tool_choice = None;
    run_streaming_no_tools(provider, messages, &final_opts, tx).await
}

// Single dispatch path for tool execution. Enforces three invariants:
// // 1. **Flow restriction** — a call to a tool outside `allowed_tool_names`
// is returned as a soft failure telling the model what it *can* call,
// instead of erroring.
// 2. **Cross-round dedupe** — if we've already dispatched this exact
// `(name, args)` pair earlier in the same turn, refuse to run it again
// and return a stern observation. Reasoning-distilled local models
// (qwen3.5-27b-distilled, etc.) split one intended batch of calls
// across multiple rounds because LM Studio's sampling switch forces
// one `<tool_call>` per response — and then keep re-reading their own
// earlier calls in reasoning and re-issuing them. The intra-round
// `dedupe_tool_calls` doesn't catch this; the cross-round set does.
// 3. **Uniform error shape** — any execution error is returned as a
// `success=false` result with a short description, so the loop never
// panics on a tool bug.
async fn dispatch_tool_call(
    tools:     &ToolRegistry,
    name:      &str,
    args:      serde_json::Value,
    allowed:   Option<&[String]>,
    seen:      &mut std::collections::HashSet<(String, String)>,
    event_ctx: ToolEventCtx<'_>,
) -> (String, bool) {
    if !tool_allowed(name, allowed) {
        let body = format!(
            "Tool '{}' is not available in this flow — call only: {}",
            name,
            allowed.map(|a| a.join(", ")).unwrap_or_default(),
        );
        emit_tool_failed(event_ctx, name, &body, &args);
        return (body, false);
    }

    let key = (name.to_owned(), args.to_string());
    if !seen.insert(key) {
        warn!("tool_loop: duplicate '{}' call blocked (cross-round)", name);
        let body = format!(
            "You already called '{}' with these exact arguments earlier in this turn. \
             Do NOT call it again. Either call a DIFFERENT tool (with different arguments) \
             or reply to the user in plain prose.",
            name,
        );
        // Don't emit `tool.failed` for duplicate-block — the original call
        // already emitted (or succeeded), and a follow-up would just spam
        // subscribers without adding signal.
        return (body, false);
    }

    match tools.execute(name, args.clone()).await {
        Ok(r) => {
            if r.success {
                (r.output, true)
            } else {
                let msg = r.error.as_deref().unwrap_or("").trim();
                let body = if !msg.is_empty() {
                    format!("Tool error: {msg}")
                } else if !r.output.is_empty() {
                    r.output
                } else {
                    format!("Tool '{name}' reported failure with no error message.")
                };
                emit_tool_failed(event_ctx, name, &body, &args);
                (body, false)
            }
        }
        Err(e) => {
            let body = format!("Tool error: {}", e);
            emit_tool_failed(event_ctx, name, &body, &args);
            (body, false)
        }
    }
}

// Publish a `tool.failed` event when an event bus is wired. Quiet when not.
fn emit_tool_failed(
    ctx:   ToolEventCtx<'_>,
    name:  &str,
    error: &str,
    args:  &serde_json::Value,
) {
    let Some(bus) = ctx.bus else { return; };
    let user_id = if ctx.user_id.is_empty() { None } else { Some(ctx.user_id.to_string()) };
    bus.emit_named(
        event_names::TOOL_FAILED,
        user_id,
        serde_json::json!({
            "tool":  name,
            "error": error,
            "args":  args,
        }),
    );
}

// Strip `<think>…</think>` blocks from model content. Qwen reasoning
// distillations emit these as chain-of-thought tokens; LM Studio normally
// routes them to `reasoning_content`, but not every config does, and
// OpenRouter passes them through verbatim. Running parsers against raw
// content means a quoted `<tool_call>` inside reasoning gets executed as
// a real call — a subtle but nasty bug. Unterminated `<think>` drops
// everything after it (reasoning truncation > leaking markup to the user).
fn strip_think_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<think>".len()..];
        match after.find("</think>") {
            Some(end) => rest = &after[end + "</think>".len()..],
            None      => return out, // unterminated; drop the rest
        }
    }
    out.push_str(rest);
    out
}

// Collapse a batch of tool calls so each `(name, arguments)` combination
// runs at most once. Small models — especially when forced into
// `tool_choice: "required"` on a turn where no call is actually warranted
// will sometimes spew dozens of duplicate `<tool_call>` blocks trying
// to "fill" the forced slot. Executing all of them burns rounds, confuses
// the state (e.g. marking the same group complete 40 times), and chews
// the rounds budget in one turn. First-come-wins preserves the earliest
// call's `call_id` so the model's own reference to the id stays valid.
fn dedupe_tool_calls(calls: Vec<ToolCall>) -> Vec<ToolCall> {
    let mut seen: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let mut out = Vec::with_capacity(calls.len());
    for c in calls {
        let key = (c.name.clone(), c.arguments.to_string());
        if seen.insert(key) {
            out.push(c);
        } else {
            debug!("Dedupe: dropping duplicate tool call '{}'", c.name);
        }
    }
    out
}

// Build OpenAI-format [`ToolSpec`]s from the registry, filtered to the
// caller-allowed names. If `allowed` is `None`, every registered tool is
// advertised — free-chat mode. Each spec carries `{type:"function",
// function:{name, description, parameters}}` which is what the chat
// completions API expects.
fn build_tool_specs(
    tools: &ToolRegistry,
    allowed: Option<&[String]>,
) -> Vec<crate::types::ToolSpec> {
    let all = tools.list_tools();
    all.into_iter()
        .filter(|n| match allowed {
            None      => true,
            Some(set) => set.iter().any(|a| a == n),
        })
        .filter_map(|name| {
            tools.get(&name).map(|t| crate::types::ToolSpec::function(
                t.name(),
                t.description(),
                t.args_schema(),
            ))
        })
        .collect()
}

fn tool_allowed(name: &str, allowed: Option<&[String]>) -> bool {
    match allowed {
        None      => true,
        Some(set) => set.iter().any(|n| n == name),
    }
}

// Merge `inject` keys into `args`. If `args` is not an object (e.g. ReAct
// wrapped a raw string as `{"input": "..."}`), returns it unchanged — the
// caller's inject keys only make sense for object-shaped args. Existing
// keys in `args` win over injected ones so the LLM can't accidentally
// overwrite an injected identity field, but the reverse (inject overwriting
// LLM-supplied value) would let an LLM's made-up `_user_id` be ignored
// which is what we want.
fn merge_injected_args(
    args:   serde_json::Value,
    inject: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    if inject.is_empty() {
        return args;
    }
    match args {
        serde_json::Value::Object(mut map) => {
            for (k, v) in inject {
                // Inject wins: any LLM-supplied value for these reserved keys
                // is overwritten with the trusted value.
                map.insert(k.clone(), v.clone());
            }
            serde_json::Value::Object(map)
        }
        other => other,
    }
}

// ─────────────────────────────────────────────────────────────────────────────

// call the policy engine with an `LlmCall` event before
// the tool loop issues a `provider.generate*()` call. Three early-out
// conditions, each cheap:
// // 1. The registry has no engine wired (legacy / dev builds) — skip.
// 2. `inject_args` doesn't carry an `_agent_id` — no agent context
// = no per-Skill rules can match. Skip.
// 3. The id is malformed UUID — treated as absent (fail-open, same
// reasoning as `ToolRegistry::execute`).
// // On `Allow` returns Ok, the tool-loop call proceeds normally. On
// `Deny` returns `MiraError::PolicyDenied`, which propagates up out
// of `run_tool_loop_with_context` exactly like a provider error
// would — callers see the standard `policy/<rule> denied: <reason>`
// message.
async fn gate_llm_call(
    tools:       &Arc<crate::tools::ToolRegistry>,
    provider:    &Arc<dyn ModelProvider>,
    inject_args: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), MiraError> {
    let Some(engine) = tools.policy_engine() else { return Ok(()); };
    let Some(agent_id) = inject_args.get("_agent_id")
        .and_then(|v| v.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(crate::agent::instance::AgentId)
    else { return Ok(()); };
    let skill_id = inject_args.get("_skill_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    // 1.5 — pull the per-agent cap from inject_args when the caller
    // populated it. Supervisors that know `Agent.budget.max_usd`
    // should add `_agent_budget_usd` to their inject map; absent or
    // 0.0 means "rule falls back to its default."
    let agent_budget_usd = inject_args.get("_agent_budget_usd")
        .and_then(|v| v.as_f64())
        .filter(|&n| n > 0.0);

    let ctx = crate::policy::LlmCallContext {
        agent_id,
        skill_id,
        provider: provider.name().to_owned(),
        // Model name isn't known at request time (most providers
        // resolve it server-side); empty string means admin
        // `model_equals` rules just won't match. That's correct
        // behaviour — better than fabricating a placeholder.
        model:    String::new(),
        // Cost tally isn't tracked across the tool loop yet (the
        // supervisor's manager loop is the canonical accountant);
        // pass 0.0 and let `session_budget` rules trigger off
        // configured caps rather than running totals.
        running_cost_usd: 0.0,
        session_cost_usd: 0.0,
        agent_budget_usd,
    };
    crate::policy::check_llm_call(engine, &ctx).await
}

// Emit `content` as a sequence of `StreamEvent::Token` chunks so the UI
// renders progressively. Used on the fall-through path of the tool loop
// where the answer is already known (from the probe generation) and we
// want to reuse it instead of asking the provider to generate again.
// Word-sized chunks preserve a typed-out feel without the 30x slowdown
// of character-by-character.
async fn replay_as_stream(content: &str, tx: &mpsc::Sender<StreamEvent>) {
    if content.is_empty() { return; }
    for piece in content.split_inclusive(char::is_whitespace) {
        let _ = tx.send(StreamEvent::Token(piece.to_owned())).await;
        tokio::task::yield_now().await;
    }
}

// Generate and stream tokens directly when no tools are involved (or after
// the tool loop is complete and the final answer is being produced).
async fn run_streaming_no_tools(
    provider: &Arc<dyn ModelProvider>,
    messages: &[ChatMessage],
    options:  &GenerationOptions,
    tx:       &mpsc::Sender<StreamEvent>,
) -> Result<(String, TokenUsage), MiraError> {
    let tx_inner = tx.clone();
    let mut on_token = move |tok: String| {
        let _ = tx_inner.try_send(StreamEvent::Token(tok));
    };
    let resp = provider.generate_stream(messages, options, &mut on_token).await?;
    if let Some(fb) = &resp.fallback {
        let _ = tx.send(StreamEvent::Warning(fallback_warning_message(fb))).await;
    }
    if let Some(reasoning) = resp.reasoning.as_ref().filter(|s| !s.is_empty()) {
        // Reasoning surfaces after the final answer streaming is
        // complete (reasoning is buffered by the provider until then).
        let _ = tx.send(StreamEvent::Reasoning(reasoning.clone())).await;
    }
    Ok((resp.content, resp.usage))
}

// User-facing message for a provider failover, shared by the tool-loop and
// the pure-streaming paths.
fn fallback_warning_message(fb: &crate::types::FallbackNotice) -> String {
    format!(
        "Your configured provider “{}” was unavailable ({}). This reply was generated by “{}” instead — check Settings → Providers if you expected “{}”.",
        fb.from, fb.reason, fb.to, fb.from,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// ReAct parser
// ─────────────────────────────────────────────────────────────────────────────

// Extract `(tool_name, args)` from a ReAct-style text block, or `None` if
// the response does not contain an `Action:` directive.
// // Recognised patterns (case-insensitive):
// ```text
// Action: tool_name
// Action Input: {"key": "value"}
// ```
// or the shorthand aliases `TOOL:` / `TOOL_INPUT:`.
pub fn parse_react_action(text: &str) -> Option<(String, serde_json::Value)> {
    let lines: Vec<&str> = text.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        let tool_name: Option<String> = {
            let lower = trimmed.to_lowercase();
            if lower.starts_with("action:") {
                Some(trimmed[7..].trim().to_string())
            } else if lower.starts_with("tool:") {
                Some(trimmed[5..].trim().to_string())
            } else {
                None
            }
        };

        if let Some(name) = tool_name {
            if name.is_empty() { continue; }

            // Search the next non-blank lines for Action Input
            for j in (i + 1)..lines.len() {
                let input_line = lines[j].trim();
                if input_line.is_empty() { continue; }

                let lower = input_line.to_lowercase();
                let input_str = if lower.starts_with("action input:") {
                    input_line[13..].trim()
                } else if lower.starts_with("tool_input:") {
                    input_line[11..].trim()
                } else {
                    // First non-blank, non-header line → treat as raw input
                    input_line
                };

                let args = serde_json::from_str(input_str)
                    .unwrap_or_else(|_| serde_json::json!({ "input": input_str }));

                return Some((name, args));
            }

            // Action line found but no input — use empty args
            return Some((name, serde_json::Value::Object(Default::default())));
        }
    }

    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Hermes / Qwen `<tool_call>` parser
// ─────────────────────────────────────────────────────────────────────────────

// Extract every `<tool_call>{...}</tool_call>` block from `text`, returning
// `(name, arguments)` for each successfully-parsed call. `<function_call>`
// is accepted as an alias since some Qwen variants emit it. Malformed or
// unnamed blocks are skipped silently — we don't want a stray tag to abort
// the whole turn.
// // The JSON inside a block may be wrapped in ```json … ``` fences; we strip
// them before parsing. `arguments` and `parameters` are treated as synonyms
// for the args field, matching both the Hermes and Qwen conventions.
pub fn parse_hermes_tool_calls(text: &str) -> Vec<(String, serde_json::Value)> {
    // Tag conventions we've seen in the wild:
    // <tool_call>…</tool_call>         — Hermes-2-Pro, Qwen2.5/3 default
    // <tool_code>…</tool_code>         — Gemini + several Qwen distillations
    // <function_call>…</function_call> — older Hermes / function-calling fine-tunes
    const PAIRS: &[(&str, &str)] = &[
        ("<tool_call>",     "</tool_call>"),
        ("<tool_code>",     "</tool_code>"),
        ("<function_call>", "</function_call>"),
    ];

    let mut out = Vec::new();
    let mut rest = text;
    loop {
        // Find the earliest opening tag anywhere in the remainder.
        let earliest = PAIRS.iter()
            .filter_map(|(open, close)| rest.find(open).map(|i| (i, *open, *close)))
            .min_by_key(|(i, _, _)| *i);
        let Some((start, open, close)) = earliest else { break; };

        let after_open = &rest[start + open.len()..];
        let Some(end) = after_open.find(close) else { break; };

        let mut payload = after_open[..end].trim();
        if let Some(stripped) = payload.strip_prefix("```json") {
            payload = stripped.trim();
        } else if let Some(stripped) = payload.strip_prefix("```") {
            payload = stripped.trim();
        }
        if let Some(stripped) = payload.strip_suffix("```") {
            payload = stripped.trim();
        }

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(payload) {
            let name = val.get("name").and_then(|v| v.as_str()).map(str::to_owned);
            let args = val.get("arguments")
                .or_else(|| val.get("parameters"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            if let Some(name) = name.filter(|n| !n.is_empty()) {
                out.push((name, args));
            }
        }

        rest = &after_open[end + close.len()..];
    }

    // Fallback: some small models emit a bare JSON tool-call object (no XML
    // wrapper). Scan the raw text for `{"name": "...", "arguments": {...}}`
    // blocks when the tagged pass produced nothing. A match only counts if
    // both the name and a non-null args object parse cleanly — prose with
    // incidental braces won't trigger a false tool call.
    if out.is_empty() {
        out.extend(parse_bare_json_tool_calls(text));
    }

    out
}

// Scan `text` for bare JSON tool-call objects (no wrapping XML tag). Returns
// at most a handful of matches — each must contain a `name` string and an
// `arguments`/`parameters` object to be accepted, so ordinary JSON that
// happens to appear in prose (e.g. an example block) is skipped.
fn parse_bare_json_tool_calls(text: &str) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    let bytes   = text.as_bytes();
    let mut i   = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' { i += 1; continue; }
        // Find the matching closing brace — track depth + string state.
        let mut depth     = 0i32;
        let mut in_str    = false;
        let mut escaped   = false;
        let mut end: Option<usize> = None;
        for (j, &b) in bytes.iter().enumerate().skip(i) {
            if escaped      { escaped = false; continue; }
            if b == b'\\'   { escaped = true; continue; }
            if b == b'"'    { in_str = !in_str; continue; }
            if in_str       { continue; }
            if b == b'{'    { depth += 1; }
            else if b == b'}' { depth -= 1; if depth == 0 { end = Some(j); break; } }
        }
        let Some(close) = end else { break; };
        let slice = &text[i..=close];
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(slice) {
            let name = val.get("name").and_then(|v| v.as_str()).map(str::to_owned);
            let args = val.get("arguments").or_else(|| val.get("parameters")).cloned();
            if let (Some(name), Some(args)) = (name.filter(|n| !n.is_empty()), args) {
                if args.is_object() {
                    out.push((name, args));
                }
            }
        }
        i = close + 1;
    }
    out
}

// Return `text` with every `<tool_call>…</tool_call>` (or `<function_call>…`)
// block removed. Runs of surrounding whitespace are collapsed to a single
// newline so the remaining prose reads cleanly. Unterminated opening tags
// are dropped along with everything after them, on the principle that raw
// markup leaking to the user is worse than a slightly shorter message.
pub fn strip_hermes_tool_calls(text: &str) -> String {
    // Tag conventions we've seen in the wild:
    // <tool_call>…</tool_call>         — Hermes-2-Pro, Qwen2.5/3 default
    // <tool_code>…</tool_code>         — Gemini + several Qwen distillations
    // <function_call>…</function_call> — older Hermes / function-calling fine-tunes
    // <think>…</think>                 — Qwen reasoning distillations; LM Studio
    //                                    normally routes these to `reasoning_content`,
    //                                    but some configs leak them into the main
    //                                    content stream where they'd otherwise
    //                                    show up verbatim in the user-facing reply.
    const PAIRS: &[(&str, &str)] = &[
        ("<tool_call>",     "</tool_call>"),
        ("<tool_code>",     "</tool_code>"),
        ("<function_call>", "</function_call>"),
        ("<think>",         "</think>"),
    ];

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let earliest = PAIRS.iter()
            .filter_map(|(open, close)| rest.find(open).map(|i| (i, *open, *close)))
            .min_by_key(|(i, _, _)| *i);
        let Some((start, open, close)) = earliest else {
            out.push_str(rest);
            break;
        };

        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        match after_open.find(close) {
            Some(end) => rest = &after_open[end + close.len()..],
            None      => break, // unterminated — drop the tail
        }
    }

    // Mirror `parse_hermes_tool_calls`: if the model emitted a bare-JSON tool
    // call (no XML wrapper), also strip that so the raw JSON doesn't leak
    // into the user-visible message.
    let cleaned = strip_bare_json_tool_calls(&out);
    cleaned.trim().to_string()
}

fn strip_bare_json_tool_calls(text: &str) -> String {
    let calls = parse_bare_json_tool_calls(text);
    if calls.is_empty() { return text.to_owned(); }

    // Rebuild by walking the text and skipping any top-level JSON object
    // whose parse matches a detected tool-call shape.
    let bytes = text.as_bytes();
    let mut out   = String::with_capacity(text.len());
    let mut i     = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        let mut depth     = 0i32;
        let mut in_str    = false;
        let mut escaped   = false;
        let mut end: Option<usize> = None;
        for (j, &b) in bytes.iter().enumerate().skip(i) {
            if escaped      { escaped = false; continue; }
            if b == b'\\'   { escaped = true; continue; }
            if b == b'"'    { in_str = !in_str; continue; }
            if in_str       { continue; }
            if b == b'{'    { depth += 1; }
            else if b == b'}' { depth -= 1; if depth == 0 { end = Some(j); break; } }
        }
        let Some(close) = end else {
            out.push_str(&text[i..]);
            break;
        };
        let slice = &text[i..=close];
        let looks_like_call = serde_json::from_str::<serde_json::Value>(slice).ok()
            .map(|v| v.get("name").and_then(|n| n.as_str()).is_some()
                  && (v.get("arguments").is_some() || v.get("parameters").is_some()))
            .unwrap_or(false);
        if looks_like_call {
            i = close + 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hermes_single_call() {
        let text = r#"<tool_call>{"name": "foo", "arguments": {"a": 1}}</tool_call>"#;
        let calls = parse_hermes_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "foo");
        assert_eq!(calls[0].1["a"], 1);
    }

    #[test]
    fn parse_hermes_multiple_calls_with_text_between() {
        let text = r#"
<tool_call>{"name": "record_profile", "arguments": {"key": "full_name", "value": "Tarek"}}</tool_call>
Nice to meet you!
<tool_call>{"name": "record_profile", "arguments": {"key": "pronouns", "value": "he/him"}}</tool_call>
        "#;
        let calls = parse_hermes_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1["key"], "full_name");
        assert_eq!(calls[1].1["key"], "pronouns");
    }

    #[test]
    fn parse_hermes_accepts_parameters_alias() {
        let text = r#"<tool_call>{"name": "foo", "parameters": {"x": "y"}}</tool_call>"#;
        let calls = parse_hermes_tool_calls(text);
        assert_eq!(calls[0].1["x"], "y");
    }

    #[test]
    fn parse_hermes_strips_code_fences() {
        let text = "<tool_call>```json\n{\"name\": \"foo\", \"arguments\": {}}\n```</tool_call>";
        let calls = parse_hermes_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "foo");
    }

    #[test]
    fn parse_hermes_accepts_function_call_alias() {
        let text = r#"<function_call>{"name": "foo", "arguments": {}}</function_call>"#;
        assert_eq!(parse_hermes_tool_calls(text).len(), 1);
    }

    #[test]
    fn parse_hermes_accepts_tool_code_alias() {
        // The exact format emitted by the Qwen 27B distillation that
        // motivated this code path. Two calls, no prose between them.
        let text = r#"<tool_code>{"name": "record_profile", "arguments": {"key": "full_name", "value": "Tarek El Diab"}}</tool_code><tool_code>{"name": "record_profile", "arguments": {"key": "preferred_name", "value": "Tarek"}}</tool_code>"#;
        let calls = parse_hermes_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1["key"], "full_name");
        assert_eq!(calls[1].1["key"], "preferred_name");
    }

    #[test]
    fn parse_hermes_skips_malformed_blocks() {
        let text = "<tool_call>not json</tool_call><tool_call>{\"name\": \"ok\"}</tool_call>";
        let calls = parse_hermes_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "ok");
    }

    #[test]
    fn parse_hermes_plain_text_returns_empty() {
        assert!(parse_hermes_tool_calls("Just a normal reply.").is_empty());
    }

    #[test]
    fn parse_bare_json_tool_call_no_xml_wrapper() {
        // Some small models skip the <tool_call> tags entirely.
        let text = r#"Let me record that.
{"name": "record_profile", "arguments": {"key": "preferred_name", "value": "Tarek"}}"#;
        let calls = parse_hermes_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "record_profile");
        assert_eq!(calls[0].1["key"], "preferred_name");
    }

    #[test]
    fn strip_hermes_removes_bare_json_tool_call() {
        let text = r#"Here you go: {"name": "foo", "arguments": {"k": 1}}"#;
        let cleaned = strip_hermes_tool_calls(text);
        assert!(!cleaned.contains("\"name\""), "bare JSON leaked: {:?}", cleaned);
        assert!(cleaned.contains("Here you go"));
    }

    #[test]
    fn strip_think_blocks_removes_reasoning() {
        let text = "<think>I should call record_profile.</think>Hi Tarek!";
        assert_eq!(strip_think_blocks(text), "Hi Tarek!");
    }

    #[test]
    fn strip_think_blocks_removes_quoted_tool_call_xml() {
        // The nasty case: reasoning block quotes prior tool_call XML.
        // Without stripping, the Hermes parser would mis-parse this as a
        // real tool invocation.
        let text = r#"<think>The prompt quoted <tool_call>{"name":"x","arguments":{}}</tool_call> at me.</think>Real response."#;
        let cleaned = strip_think_blocks(text);
        assert!(!cleaned.contains("<tool_call>"));
        assert_eq!(cleaned, "Real response.");
    }

    #[test]
    fn strip_think_blocks_handles_unterminated() {
        let text = "Hello.<think>never closed";
        assert_eq!(strip_think_blocks(text), "Hello.");
    }

    #[test]
    fn strip_think_blocks_passes_through_when_absent() {
        assert_eq!(strip_think_blocks("just text"), "just text");
    }

    #[test]
    fn strip_hermes_removes_blocks_keeps_prose() {
        let text = r#"<tool_call>{"name":"a","arguments":{}}</tool_call>Nice to meet you.<tool_call>{"name":"b","arguments":{}}</tool_call>"#;
        assert_eq!(strip_hermes_tool_calls(text), "Nice to meet you.");
    }

    #[test]
    fn strip_hermes_drops_unterminated_tag_tail() {
        let text = "Hello.<tool_call>{\"name\":\"oops\"";
        assert_eq!(strip_hermes_tool_calls(text), "Hello.");
    }

    #[test]
    fn parse_react_standard_format() {
        let text = "Thought: I should look up the weather.\nAction: get_weather\nAction Input: {\"city\": \"London\"}";
        let (name, args) = parse_react_action(text).unwrap();
        assert_eq!(name, "get_weather");
        assert_eq!(args["city"], "London");
    }

    #[test]
    fn parse_react_tool_alias() {
        let text = "TOOL: shell\nTOOL_INPUT: {\"command\": \"ls\"}";
        let (name, args) = parse_react_action(text).unwrap();
        assert_eq!(name, "shell");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn parse_react_plain_text_input_wrapped_in_object() {
        let text = "Action: summarize\nAction Input: Summarize the meeting notes";
        let (name, args) = parse_react_action(text).unwrap();
        assert_eq!(name, "summarize");
        assert_eq!(args["input"], "Summarize the meeting notes");
    }

    #[test]
    fn parse_react_no_action_returns_none() {
        let text = "I think the answer is 42.";
        assert!(parse_react_action(text).is_none());
    }

    #[test]
    fn parse_react_empty_action_name_skipped() {
        let text = "Action:   \nAction: real_tool\nAction Input: {}";
        let (name, _) = parse_react_action(text).unwrap();
        assert_eq!(name, "real_tool");
    }

    #[test]
    fn parse_react_action_no_input_returns_empty_object() {
        let text = "Action: ping";
        let (name, args) = parse_react_action(text).unwrap();
        assert_eq!(name, "ping");
        assert!(args.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_react_blank_lines_between_action_and_input() {
        let text = "Action: echo\n\n\nAction Input: {\"msg\": \"hello\"}";
        let (name, args) = parse_react_action(text).unwrap();
        assert_eq!(name, "echo");
        assert_eq!(args["msg"], "hello");
    }

    #[test]
    fn tool_mode_from_str() {
        assert_eq!(ToolMode::from_str("openai"),   ToolMode::OpenAi);
        assert_eq!(ToolMode::from_str("react"),    ToolMode::React);
        assert_eq!(ToolMode::from_str("auto"),     ToolMode::Auto);
        assert_eq!(ToolMode::from_str("disabled"), ToolMode::Disabled);
        assert_eq!(ToolMode::from_str("unknown"),  ToolMode::Disabled);
    }

    // ── dispatch_tool_call: failure-surface tests ─────────────────────────────
    //
    // These lock in the contract that a failed `ToolResult` must reach the
    // caller (and through them, the LLM) with the actual error text — never
    // an empty string. Empty-string returns previously caused the model to
    // hallucinate a plausible-looking output for tool calls that had in fact
    // failed.

    use crate::tools::{Tool, ToolArgs, ToolRegistry, ToolResult};

    struct StubTool {
        name:  &'static str,
        reply: ToolResult,
    }

    #[async_trait::async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str { self.name }
        fn description(&self) -> &str { "stub" }
        fn args_schema(&self) -> serde_json::Value { serde_json::json!({}) }
        async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
            Ok(self.reply.clone())
        }
    }

    fn registry_with(tool: StubTool) -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(tool);
        r
    }

    #[tokio::test]
    async fn dispatch_success_passes_output_through() {
        let tools = registry_with(StubTool {
            name:  "ok_tool",
            reply: ToolResult::success("hello world"),
        });
        let mut seen = std::collections::HashSet::new();
        let (out, ok) = dispatch_tool_call(&tools, "ok_tool", serde_json::json!({}), None, &mut seen, ToolEventCtx::NONE).await;
        assert!(ok);
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn dispatch_failure_surfaces_error_message() {
        // The bug we're guarding against: ToolResult::failure(...) leaves
        // `output` empty; the tool's diagnostic lives in `error`. If
        // dispatch_tool_call forwarded `output` blindly, the LLM would see
        // an empty string and confabulate a result. The contract here is:
        // failure surfaces the error text prefixed with "Tool error:".
        let tools = registry_with(StubTool {
            name:  "boom",
            reply: ToolResult::failure("sandbox refused: EPERM"),
        });
        let mut seen = std::collections::HashSet::new();
        let (out, ok) = dispatch_tool_call(&tools, "boom", serde_json::json!({}), None, &mut seen, ToolEventCtx::NONE).await;
        assert!(!ok, "failed tool must report success=false");
        assert!(!out.is_empty(),
            "failed tool must not return empty output (would invite hallucination)");
        assert!(out.contains("sandbox refused: EPERM"),
            "failed tool's error must reach the caller, got: {out}");
    }

    #[tokio::test]
    async fn dispatch_failure_with_no_message_uses_fallback_label() {
        // Pathological case: a tool reports success=false but populates
        // neither `error` nor `output`. We still must not return an empty
        // string — emit a recognisable placeholder so the LLM sees *some*
        // signal that the call failed.
        let tools = registry_with(StubTool {
            name:  "silent_fail",
            reply: ToolResult { success: false, output: String::new(), error: None },
        });
        let mut seen = std::collections::HashSet::new();
        let (out, ok) = dispatch_tool_call(&tools, "silent_fail", serde_json::json!({}), None, &mut seen, ToolEventCtx::NONE).await;
        assert!(!ok);
        assert!(!out.is_empty(), "fallback label must not be empty, got: {out:?}");
        assert!(out.contains("silent_fail"), "fallback should name the failing tool, got: {out}");
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_registry_error() {
        let tools = ToolRegistry::new();
        let mut seen = std::collections::HashSet::new();
        let (out, ok) = dispatch_tool_call(&tools, "missing", serde_json::json!({}), None, &mut seen, ToolEventCtx::NONE).await;
        assert!(!ok);
        assert!(out.starts_with("Tool error:"), "got: {out}");
    }

    #[tokio::test]
    async fn dispatch_disallowed_tool_returns_explanatory_failure() {
        let tools = registry_with(StubTool {
            name:  "shell",
            reply: ToolResult::success("ignored"),
        });
        let allowed = vec!["calendar".to_string()];
        let mut seen = std::collections::HashSet::new();
        let (out, ok) = dispatch_tool_call(&tools, "shell", serde_json::json!({}), Some(&allowed), &mut seen, ToolEventCtx::NONE).await;
        assert!(!ok);
        assert!(out.contains("not available"), "got: {out}");
    }

    // ── gate_llm_call integration tests ─────────────────

    use crate::agent::instance::AgentId;
    use crate::policy::{AllowAllEngine, DenyAllEngine, PolicyEngine};
    use crate::providers::ModelProvider;
    use crate::types::{ChatMessage, GenerationOptions, GenerationResponse, ProviderId, TokenUsage};

    // Provider that returns a canned content string.
    struct StubProvider(&'static str);
    #[async_trait::async_trait]
    impl ModelProvider for StubProvider {
        fn name(&self) -> &str { "stub" }
        async fn generate(&self, _msgs: &[ChatMessage], _opts: &GenerationOptions)
            -> Result<GenerationResponse, MiraError>
        {
            Ok(GenerationResponse {
                content:    self.0.into(),
                tool_calls: None,
                reasoning: None,
                usage:      TokenUsage::default(),
                provider_id: ProviderId::Local("stub".into()),
                model_name:  "stub".into(),
                fallback: None,
            })
        }
        async fn health_check(&self) -> bool { true }
    }

    fn engineless_registry() -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::new())
    }
    fn registry_with_engine(eng: Arc<dyn PolicyEngine>) -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::new().with_policy_engine(eng))
    }

    #[tokio::test]
    async fn gate_llm_call_skips_when_registry_has_no_engine() {
        let provider: Arc<dyn ModelProvider> = Arc::new(StubProvider("ok"));
        let tools = engineless_registry();
        // Even with a populated inject map, no engine = no consult.
        let mut inject = serde_json::Map::new();
        inject.insert("_agent_id".into(), AgentId::new().to_string().into());
        gate_llm_call(&tools, &provider, &inject).await
            .expect("no engine should always allow");
    }

    #[tokio::test]
    async fn gate_llm_call_skips_when_inject_args_lack_agent_id() {
        let provider: Arc<dyn ModelProvider> = Arc::new(StubProvider("ok"));
        // Engine that would deny if asked.
        let tools = registry_with_engine(Arc::new(DenyAllEngine::new("would fire")));
        let inject = serde_json::Map::new();
        gate_llm_call(&tools, &provider, &inject).await
            .expect("missing _agent_id should skip the consult");
    }

    #[tokio::test]
    async fn gate_llm_call_skips_when_agent_id_is_unparseable() {
        let provider: Arc<dyn ModelProvider> = Arc::new(StubProvider("ok"));
        let tools = registry_with_engine(Arc::new(DenyAllEngine::new("would fire")));
        let mut inject = serde_json::Map::new();
        inject.insert("_agent_id".into(), "not-a-uuid".into());
        gate_llm_call(&tools, &provider, &inject).await
            .expect("garbage UUID should skip the consult (fail-open)");
    }

    #[tokio::test]
    async fn gate_llm_call_returns_policy_denied_when_engine_denies() {
        let provider: Arc<dyn ModelProvider> = Arc::new(StubProvider("ok"));
        let tools = registry_with_engine(Arc::new(DenyAllEngine::new("blocked here")));
        let mut inject = serde_json::Map::new();
        inject.insert("_agent_id".into(), AgentId::new().to_string().into());
        inject.insert("_skill_id".into(), "com.example.x".into());
        let err = gate_llm_call(&tools, &provider, &inject).await.unwrap_err();
        match err {
            MiraError::PolicyDenied { rule, reason } => {
                assert_eq!(rule, "test/deny-all");
                assert_eq!(reason, "blocked here");
            }
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gate_llm_call_returns_ok_when_engine_allows() {
        let provider: Arc<dyn ModelProvider> = Arc::new(StubProvider("ok"));
        let tools = registry_with_engine(Arc::new(AllowAllEngine));
        let mut inject = serde_json::Map::new();
        inject.insert("_agent_id".into(), AgentId::new().to_string().into());
        gate_llm_call(&tools, &provider, &inject).await.unwrap();
    }

    #[tokio::test]
    async fn gate_llm_call_threads_agent_budget_usd_from_inject_args() {
        // 1.5 — when inject_args carries `_agent_budget_usd`, it
        // must reach the LlmCall event so PerAgentBudgetRule can
        // compare against it.
        struct Recording {
            seen: std::sync::Mutex<Vec<crate::policy::PolicyEvent>>,
        }
        #[async_trait::async_trait]
        impl PolicyEngine for Recording {
            async fn evaluate(&self, e: &crate::policy::PolicyEvent)
                -> crate::policy::PolicyDecision
            {
                self.seen.lock().unwrap().push(e.clone());
                crate::policy::PolicyDecision::Allow
            }
        }
        let eng = Arc::new(Recording { seen: std::sync::Mutex::new(vec![]) });
        let dyn_eng: Arc<dyn PolicyEngine> = eng.clone();
        let tools = Arc::new(ToolRegistry::new().with_policy_engine(dyn_eng));
        let provider: Arc<dyn ModelProvider> = Arc::new(StubProvider("ok"));

        let mut inject = serde_json::Map::new();
        inject.insert("_agent_id".into(), AgentId::new().to_string().into());
        inject.insert("_agent_budget_usd".into(), serde_json::json!(2.50));
        gate_llm_call(&tools, &provider, &inject).await.unwrap();

        match &eng.seen.lock().unwrap()[0] {
            crate::policy::PolicyEvent::LlmCall { agent_budget_usd, .. } => {
                assert_eq!(*agent_budget_usd, Some(2.50));
            }
            other => panic!("expected LlmCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gate_llm_call_filters_zero_or_negative_budget_sentinels() {
        // Callers sometimes default `_agent_budget_usd` to 0.0 when
        // the cap isn't actually known. The tool loop filters these
        // out before populating the event, so the rule sees None
        // and falls back to its default rather than enforcing $0.
        struct Recording {
            seen: std::sync::Mutex<Vec<crate::policy::PolicyEvent>>,
        }
        #[async_trait::async_trait]
        impl PolicyEngine for Recording {
            async fn evaluate(&self, e: &crate::policy::PolicyEvent)
                -> crate::policy::PolicyDecision
            {
                self.seen.lock().unwrap().push(e.clone());
                crate::policy::PolicyDecision::Allow
            }
        }
        let eng = Arc::new(Recording { seen: std::sync::Mutex::new(vec![]) });
        let dyn_eng: Arc<dyn PolicyEngine> = eng.clone();
        let tools = Arc::new(ToolRegistry::new().with_policy_engine(dyn_eng));
        let provider: Arc<dyn ModelProvider> = Arc::new(StubProvider("ok"));

        for sentinel in [0.0, -1.0, -0.0001] {
            let mut inject = serde_json::Map::new();
            inject.insert("_agent_id".into(), AgentId::new().to_string().into());
            inject.insert("_agent_budget_usd".into(), serde_json::json!(sentinel));
            gate_llm_call(&tools, &provider, &inject).await.unwrap();
        }

        let seen = eng.seen.lock().unwrap();
        for (i, e) in seen.iter().enumerate() {
            match e {
                crate::policy::PolicyEvent::LlmCall { agent_budget_usd, .. } => {
                    assert_eq!(*agent_budget_usd, None,
                        "sentinel value at index {i} leaked into event");
                }
                other => panic!("expected LlmCall, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn gate_llm_call_passes_provider_name_into_event() {
        // Provider name should reach the engine so admin
        // `provider_equals` rules can match.
        struct Recording {
            seen: std::sync::Mutex<Vec<crate::policy::PolicyEvent>>,
        }
        #[async_trait::async_trait]
        impl PolicyEngine for Recording {
            async fn evaluate(&self, e: &crate::policy::PolicyEvent)
                -> crate::policy::PolicyDecision
            {
                self.seen.lock().unwrap().push(e.clone());
                crate::policy::PolicyDecision::Allow
            }
        }
        let eng = Arc::new(Recording { seen: std::sync::Mutex::new(vec![]) });
        let dyn_eng: Arc<dyn PolicyEngine> = eng.clone();
        let tools = Arc::new(ToolRegistry::new().with_policy_engine(dyn_eng));
        let provider: Arc<dyn ModelProvider> = Arc::new(StubProvider("ok"));

        let mut inject = serde_json::Map::new();
        inject.insert("_agent_id".into(), AgentId::new().to_string().into());
        gate_llm_call(&tools, &provider, &inject).await.unwrap();

        let seen = eng.seen.lock().unwrap();
        match &seen[0] {
            crate::policy::PolicyEvent::LlmCall { provider, model, .. } => {
                assert_eq!(provider, "stub");
                assert_eq!(model,    "");
            }
            other => panic!("expected LlmCall, got {other:?}"),
        }
    }
}
