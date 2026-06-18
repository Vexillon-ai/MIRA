// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/automations.rs
//! Agent-callable automations tools.
//!
//! Five tools that let the agent set up its own follow-ups, webhooks, and
//! event subscriptions during a turn. All five share these properties:
//!
//! - **Owner attribution**: rows land with `owner_kind = Agent` and the
//! trusted `_user_id` injected by the chat handler. The agent cannot
//! create-on-behalf-of another user.
//! - **Quota + rationale gate**: every create routes through
//! [`crate::automations::agent_gate`], which enforces the per-user quota
//! and the `agent_rationale_required` config knob.
//! - **Approval mode**: when `agent_creates_pending` is on, agent-authored
//! rows land in `pending_approval` and won't fire until the user
//! approves them via the web UI.
//!
//! Cancel/list operate on rows the agent itself authored — the agent
//! cannot cancel rows the user created. (The HTTP API is the right surface
//! for cross-owner mutations.)

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::automations::{
    Action, AutomationsStore, NewEventSubscription, NewSchedule, NewWebhook, OwnerKind,
    QuietHours, TriggerSpec,
    agent_gate::{
        gate_create_event_subscription, gate_create_schedule, gate_create_webhook, GateError,
    },
};
use crate::config::MiraConfig;
use crate::MiraError;

// ── Shared helpers ───────────────────────────────────────────────────────────

fn caller(args: &ToolArgs) -> Result<String, MiraError> {
    args.get("_user_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| MiraError::ToolError(
            "automations tool called without caller identity".into(),
        ))
}

fn require_str<'a>(args: &'a ToolArgs, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("'{key}' is required"))
}

fn parse_action(v: &Value) -> Result<Action, String> {
    let action: Action = serde_json::from_value(v.clone())
        .map_err(|e| format!("invalid action: {e}"))?;
    // Reject inconsistent prompt actions up front so we fail at *creation*
    // (where the model can fix it) rather than silently on every fire — a
    // `named` schedule with no name used to fail each tick + trip the watchdog.
    if let Action::Prompt(p) = &action {
        use crate::automations::ConversationStrategy::*;
        let name_empty = p.conversation_name.as_deref().map(str::trim).unwrap_or("").is_empty();
        match p.conversation_strategy {
            Named if name_empty => return Err(
                "conversation_strategy=named requires a non-empty conversation_name \
                 (or use \"new\" for a fresh thread each fire)".into()
            ),
            Existing if p.conversation_id.is_none() => return Err(
                "conversation_strategy=existing requires conversation_id".into()
            ),
            _ => {}
        }
    }
    Ok(action)
}

fn parse_trigger(v: &Value) -> Result<TriggerSpec, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("invalid trigger: {e}"))
}

fn gate_to_string(e: &GateError) -> String {
    e.to_string()
}

// ── Shared sub-schemas ───────────────────────────────────────────────────────
//
// `TriggerSpec` and `Action` are tagged enums on the Rust side. We expose
// them to the model as a single object whose `kind` field discriminates the
// variant; per-variant fields are listed alongside with `[kind=…]` markers
// in their description so the model can tell which fields belong to which
// variant. Strict `oneOf` would be cleaner but every provider we target
// (LM Studio, OpenRouter, Ollama) handles it inconsistently — listing the
// flat union has been the most reliable shape in practice.

fn trigger_schema() -> Value {
    json!({
        "type": "object",
        "required": ["kind"],
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["one_off", "interval", "cron"],
                "description": "Discriminator. Pick `one_off` for a single firing, `interval` for periodic, `cron` for a calendar-style expression."
            },
            "at": {
                "type": "integer",
                "description": "[kind=one_off] Unix seconds (UTC) when to fire."
            },
            "every_secs": {
                "type": "integer",
                "minimum": 1,
                "description": "[kind=interval] Period in seconds. Example: 300 for every 5 minutes."
            },
            "expr": {
                "type": "string",
                "description": "[kind=cron] Cron expression. Standard 5-field Unix cron `min hour dom mon dow` works (e.g. `0 9 * * *` = 09:00 daily, `0 9 * * 1-5` = 09:00 weekdays); 6-field Quartz with seconds also accepted (`sec min hour dom mon dow`). Timezone is taken from the schedule's `timezone` field, defaulting to UTC."
            }
        }
    })
}

fn action_schema() -> Value {
    json!({
        "type": "object",
        "required": ["kind"],
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["prompt", "tool_call", "internal", "http_post", "channel_message"],
                "description": "Action discriminator. DELIVERY SEMANTICS — only `prompt` and `channel_message` send anything to the user; the others do not. \
`prompt` runs a full agent turn in a conversation and posts the agent's reply (the agent can call tools mid-turn). USE THIS when the user asks for a recurring or scheduled message — including \"send me a random string every 2 minutes\", \"give me a joke every morning\", etc. The `prompt` field is the *instruction to the agent*, e.g. \"Generate a random 6-character string and send it to me.\" \
`channel_message` posts a fire-and-forget templated string with no agent turn — only useful when the message body is fully known up front. \
`tool_call` runs ONE backend tool and stores the result in the audit log ONLY — the user NEVER sees it; do not pick this for \"send me X\" requests. \
`internal` runs a built-in heartbeat task (admin maintenance only). \
`http_post` calls an outbound webhook (no user delivery)."
            },

            // ── Action::Prompt fields ────────────────────────────────
            "conversation_strategy": {
                "type": "string",
                "enum": ["existing", "new", "named"],
                "description": "[kind=prompt] REQUIRED. `new` opens a fresh thread each fire; `named` finds-or-creates by `conversation_name`; `existing` requires `conversation_id` (rare — only when the agent wants to keep posting into the exact same thread it is in now)."
            },
            "conversation_id": {
                "type": "string",
                "description": "[kind=prompt, conversation_strategy=existing] UUID of the conversation to resume."
            },
            "conversation_name": {
                "type": "string",
                "description": "[kind=prompt, conversation_strategy=named|new] Title to find-or-create or label a new thread. Recommended for `named`."
            },
            "channel": {
                "type": "string",
                "description": "[kind=prompt, kind=channel_message] REQUIRED. One of `web`, `signal`, `telegram`, `email`, `tui`. **Default to the channel the user is talking to you on right now** — your system prompt tells you which one. Only fall back to `web` when there is no current channel context (e.g. a system-initiated follow-up)."
            },
            "prompt": {
                "type": "string",
                "description": "[kind=prompt] REQUIRED. The user-side message text dropped into the conversation when the schedule fires."
            },
            "tools_allowed": {
                "type": "array",
                "items": {"type": "string"},
                "description": "[kind=prompt] Optional whitelist of tool names available during the fired turn. Omit for the default registry."
            },
            "max_iterations": {
                "type": "integer",
                "default": 10,
                "description": "[kind=prompt] Max tool-loop rounds. Default 10."
            },

            // ── Action::ToolCall fields ──────────────────────────────
            "tool": {
                "type": "string",
                "description": "[kind=tool_call] Registered backend tool name. NOTE: the tool's output is stored in the audit log only — it is NOT delivered to the user. To send a result to the user, pick `kind=prompt` instead and let the agent call this tool inside the turn."
            },
            "args": {
                "type": "object",
                "description": "[kind=tool_call, kind=internal] Free-shape arguments for the target tool/task."
            },

            // ── Action::Internal fields ──────────────────────────────
            "task": {
                "type": "string",
                "description": "[kind=internal] Built-in heartbeat task name (e.g. `log_cleanup`)."
            },

            // ── Action::HttpPost fields ──────────────────────────────
            "url": {
                "type": "string",
                "description": "[kind=http_post] Outbound URL."
            },
            "body_template": {
                "type": "string",
                "description": "[kind=http_post] Liquid-ish body template; `{{payload.…}}` resolves the inbound payload context."
            },
            "headers": {
                "type": "object",
                "description": "[kind=http_post] Extra request headers."
            },
            "timeout_secs": {
                "type": "integer",
                "description": "[kind=http_post] Request timeout. Default 10."
            },
            "secret": {
                "type": "string",
                "description": "[kind=http_post] Optional HMAC-SHA256 secret used to sign the rendered body."
            },
            "max_retries": {
                "type": "integer",
                "description": "[kind=http_post] Extra retry count on 5xx/transport errors. Default 3."
            },

            // ── Action::ChannelMessage fields ────────────────────────
            "to": {
                "type": "string",
                "description": "[kind=channel_message] Optional recipient address (e.g. phone for signal). Ignored for `web`."
            },
            "text_template": {
                "type": "string",
                "description": "[kind=channel_message] Body template; same substitution rules as http_post.body_template."
            }
        }
    })
}

// ── automations_schedule_followup ────────────────────────────────────────────

pub struct ScheduleFollowupTool {
    store:  Arc<AutomationsStore>,
    config: Arc<MiraConfig>,
}

impl ScheduleFollowupTool {
    pub fn new(store: Arc<AutomationsStore>, config: Arc<MiraConfig>) -> Self {
        Self { store, config }
    }
}

#[async_trait]
impl Tool for ScheduleFollowupTool {
    fn name(&self) -> &str { "automations_schedule_followup" }
    fn description(&self) -> &str {
        "Schedule a follow-up the agent will run later. \
         \n\nMost common case: user asks for a recurring or scheduled \
         message (\"send me a joke every 5 minutes\", \"text me a random \
         string every 2 minutes\", \"remind me at 9am\"). \
         For ANY of these, use `action.kind=prompt` — only `prompt` and \
         `channel_message` actually deliver a message to the user. \
         `tool_call` runs a tool but the result goes to the audit log \
         only; the user sees nothing. If you need to run a tool AND \
         show the result, pick `prompt` and let the agent call the tool \
         from inside the turn. \
         \n\nFor `kind=prompt` you need: `conversation_strategy` \
         (`new` for a fresh thread each fire, `named` to reuse one by \
         title), `channel` (match the channel the user is talking to \
         you on right now — `signal` if they're texting via Signal, \
         `telegram` for Telegram, `web` for the in-app chat), and \
         `prompt` (the *instruction* you want the agent to act on each \
         fire — write it as a directive, not as a finished message). \
         \n\nWorked example — \"send me a joke every 5 minutes\" while \
         talking on Signal: \
         \n  trigger = {\"kind\":\"interval\",\"every_secs\":300} \
         \n  action  = {\"kind\":\"prompt\",\"conversation_strategy\":\"new\",\
\"channel\":\"signal\",\"prompt\":\"Tell me a fresh joke.\"} \
         \n\nWorked example — \"send me a random 6-char string every 2 minutes\": \
         \n  trigger = {\"kind\":\"interval\",\"every_secs\":120} \
         \n  action  = {\"kind\":\"prompt\",\"conversation_strategy\":\"new\",\
\"channel\":\"<the channel you are on now>\",\"prompt\":\"Generate a random 6-character alphanumeric string and send it to me.\"} \
         \n\nThe schedule is owned by the agent and the calling user; \
         quota and approval rules from user config apply. Times are unix \
         seconds (UTC) for one_off; seconds for interval; standard 5-field \
         cron for cron."
    }
    fn tier(&self) -> Tier { Tier::Pure }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name", "trigger", "action", "rationale"],
            "properties": {
                "name":        { "type": "string", "description": "Short label shown in the user's automations UI." },
                "trigger":     trigger_schema(),
                "timezone":    { "type": "string", "description": "IANA tz; defaults to UTC." },
                "action":      action_schema(),
                "rationale":   { "type": "string", "description": "Why this follow-up is being created. Required when the user has agent_rationale_required on." },
                "description": { "type": "string" },
                "expires_at":  { "type": "integer", "description": "Unix seconds; row stops firing past this time." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = caller(&args)?;
        let name = match require_str(&args, "name") { Ok(s) => s.to_string(), Err(e) => return Ok(ToolResult::failure(e)) };
        let rationale = match require_str(&args, "rationale") {
            Ok(s)  => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e)),
        };
        let trigger = match args.get("trigger") {
            Some(v) => match parse_trigger(v) { Ok(t) => t, Err(e) => return Ok(ToolResult::failure(e)) },
            None    => return Ok(ToolResult::failure("'trigger' is required")),
        };
        let action = match args.get("action") {
            Some(v) => match parse_action(v) { Ok(a) => a, Err(e) => return Ok(ToolResult::failure(e)) },
            None    => return Ok(ToolResult::failure("'action' is required")),
        };
        let timezone = args.get("timezone")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "UTC".to_string());
        let description = args.get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let expires_at = args.get("expires_at").and_then(|v| v.as_i64());

        // Validate the trigger eagerly so the model gets a clear error
        // instead of a row that silently never fires.
        if let Err(e) = crate::automations::next_run_at::next_run_at(&trigger, &timezone, 0) {
            return Ok(ToolResult::failure(format!("invalid trigger: {e}")));
        }

        let status_override = match gate_create_schedule(
            &self.store, &self.config.automations, &user_id,
            OwnerKind::Agent, Some(&rationale),
        ) {
            Ok(v)  => v,
            Err(e) => return Ok(ToolResult::failure(gate_to_string(&e))),
        };

        let new = NewSchedule {
            user_id:     user_id.clone(),
            owner_kind:  OwnerKind::Agent,
            name,
            description,
            rationale:   Some(rationale),
            trigger,
            timezone,
            quiet_hours: None::<QuietHours>,
            action,
            expires_at,
            status:      status_override,
        };
        match self.store.create_schedule(new) {
            Ok(s) => {
                let body = json!({
                    "id":         s.id,
                    "name":       s.name,
                    "status":     s.status.as_str(),
                    "next_run_at": s.next_run_at,
                    "pending_approval": matches!(
                        s.status, crate::automations::types::ScheduleStatus::PendingApproval
                    ),
                });
                Ok(ToolResult::success(body.to_string()))
            }
            Err(e) => Ok(ToolResult::failure(format!("create_schedule: {e}"))),
        }
    }
}

// ── automations_list_self_schedules ──────────────────────────────────────────

pub struct ListSelfSchedulesTool {
    store: Arc<AutomationsStore>,
}

impl ListSelfSchedulesTool {
    pub fn new(store: Arc<AutomationsStore>) -> Self { Self { store } }
}

#[async_trait]
impl Tool for ListSelfSchedulesTool {
    fn name(&self) -> &str { "automations_list_self_schedules" }
    fn description(&self) -> &str {
        "List schedules the agent itself authored for this user. Useful \
         before deciding whether to create another or cancel an existing \
         one. Pass `only_active: true` to filter out paused / pending / \
         expired rows."
    }
    fn tier(&self) -> Tier { Tier::Pure }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "only_active": { "type": "boolean", "description": "Only return rows whose status is `active`." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = caller(&args)?;
        let only_active = args.get("only_active").and_then(|v| v.as_bool()).unwrap_or(false);
        match self.store.list_schedules(Some(&user_id)) {
            Ok(rows) => {
                let filtered: Vec<_> = rows.into_iter()
                    .filter(|s| matches!(s.owner_kind, OwnerKind::Agent))
                    .filter(|s| !only_active
                        || matches!(s.status, crate::automations::types::ScheduleStatus::Active))
                    .map(|s| json!({
                        "id":          s.id,
                        "name":        s.name,
                        "status":      s.status.as_str(),
                        "trigger":     s.trigger,
                        "timezone":    s.timezone,
                        "next_run_at": s.next_run_at,
                        "last_run_at": s.last_run_at,
                        "rationale":   s.rationale,
                        "created_at":  s.created_at,
                    }))
                    .collect();
                Ok(ToolResult::success(json!({"schedules": filtered}).to_string()))
            }
            Err(e) => Ok(ToolResult::failure(format!("list_schedules: {e}"))),
        }
    }
}

// ── automations_cancel_schedule ──────────────────────────────────────────────

pub struct CancelScheduleTool {
    store: Arc<AutomationsStore>,
}

impl CancelScheduleTool {
    pub fn new(store: Arc<AutomationsStore>) -> Self { Self { store } }
}

#[async_trait]
impl Tool for CancelScheduleTool {
    fn name(&self) -> &str { "automations_cancel_schedule" }
    fn description(&self) -> &str {
        "Cancel an agent-authored schedule for this user. The agent can \
         only cancel rows it created (owner_kind = agent); user-authored \
         and system rows must be edited via the UI. `reason` is recorded \
         in logs for audit."
    }
    fn tier(&self) -> Tier { Tier::Pure }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["id", "reason"],
            "properties": {
                "id":     { "type": "string" },
                "reason": { "type": "string", "description": "Audit-trail explanation; surfaced in logs." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = caller(&args)?;
        let id = match require_str(&args, "id") { Ok(s) => s.to_string(), Err(e) => return Ok(ToolResult::failure(e)) };
        let reason = match require_str(&args, "reason") { Ok(s) => s.to_string(), Err(e) => return Ok(ToolResult::failure(e)) };
        let existing = match self.store.get_schedule(&id) {
            Ok(Some(s)) => s,
            Ok(None)    => return Ok(ToolResult::failure("schedule not found")),
            Err(e)      => return Ok(ToolResult::failure(format!("get_schedule: {e}"))),
        };
        if existing.user_id != user_id {
            return Ok(ToolResult::failure("schedule belongs to a different user"));
        }
        if !matches!(existing.owner_kind, OwnerKind::Agent) {
            return Ok(ToolResult::failure(
                "agent can only cancel schedules it authored (owner_kind = agent)",
            ));
        }
        match self.store.delete_schedule(&id) {
            Ok(true)  => {
                tracing::info!(target: "automations", "agent cancelled schedule {id}: {reason}");
                Ok(ToolResult::success(json!({"cancelled": true, "id": id}).to_string()))
            }
            Ok(false) => Ok(ToolResult::failure("schedule not found")),
            Err(e)    => Ok(ToolResult::failure(format!("delete_schedule: {e}"))),
        }
    }
}

// ── automations_register_webhook ─────────────────────────────────────────────

pub struct RegisterWebhookTool {
    store:  Arc<AutomationsStore>,
    config: Arc<MiraConfig>,
}

impl RegisterWebhookTool {
    pub fn new(store: Arc<AutomationsStore>, config: Arc<MiraConfig>) -> Self {
        Self { store, config }
    }
}

#[async_trait]
impl Tool for RegisterWebhookTool {
    fn name(&self) -> &str { "automations_register_webhook" }
    fn description(&self) -> &str {
        "Register a webhook the agent expects an external system to call. \
         Returns the public ingest URL path and the HMAC secret (one-time; \
         the secret cannot be recovered later). The agent should hand both \
         to the user along with instructions on how to wire them into the \
         third-party system."
    }
    fn tier(&self) -> Tier { Tier::Pure }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name", "action", "rationale"],
            "properties": {
                "name":        { "type": "string" },
                "description": { "type": "string" },
                "rationale":   { "type": "string", "description": "Why this webhook is being registered." },
                "predicate":   { "type": "object", "description": "Optional MIRA predicate evaluated against {payload, headers, now}." },
                "action":      action_schema(),
                "payload_template":   { "type": "string", "description": "Optional template applied to the payload before dispatch." },
                "rate_limit_per_min": { "type": "integer" },
                "debounce_secs":      { "type": "integer" },
                "expires_at":         { "type": "integer" }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = caller(&args)?;
        let name = match require_str(&args, "name") { Ok(s) => s.to_string(), Err(e) => return Ok(ToolResult::failure(e)) };
        let rationale = match require_str(&args, "rationale") {
            Ok(s)  => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e)),
        };
        let action = match args.get("action") {
            Some(v) => match parse_action(v) { Ok(a) => a, Err(e) => return Ok(ToolResult::failure(e)) },
            None    => return Ok(ToolResult::failure("'action' is required")),
        };
        let predicate = args.get("predicate").cloned().filter(|v| !v.is_null());
        let description = args.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());
        let payload_template = args.get("payload_template").and_then(|v| v.as_str()).map(|s| s.to_string());
        let rate_limit_per_min = args.get("rate_limit_per_min").and_then(|v| v.as_i64());
        let debounce_secs = args.get("debounce_secs").and_then(|v| v.as_i64());
        let expires_at = args.get("expires_at").and_then(|v| v.as_i64());

        let status_override = match gate_create_webhook(
            &self.store, &self.config.automations, &user_id,
            OwnerKind::Agent, Some(&rationale),
        ) {
            Ok(v)  => v,
            Err(e) => return Ok(ToolResult::failure(gate_to_string(&e))),
        };
        let new = NewWebhook {
            user_id:           user_id.clone(),
            owner_kind:        OwnerKind::Agent,
            name,
            description,
            rationale:         Some(rationale),
            predicate,
            payload_template,
            action,
            rate_limit_per_min,
            debounce_secs,
            expires_at,
            status:            status_override,
        };
        match self.store.create_webhook(new) {
            Ok(w) => {
                let body = json!({
                    "id":     w.id,
                    "url":    format!("/webhook/incoming/{}", w.token),
                    "secret": w.secret,
                    "status": w.status.as_str(),
                    "pending_approval": matches!(
                        w.status, crate::automations::types::AutomationStatus::PendingApproval
                    ),
                });
                Ok(ToolResult::success(body.to_string()))
            }
            Err(e) => Ok(ToolResult::failure(format!("create_webhook: {e}"))),
        }
    }
}

// ── automations_subscribe_event ──────────────────────────────────────────────

pub struct SubscribeEventTool {
    store:  Arc<AutomationsStore>,
    config: Arc<MiraConfig>,
}

impl SubscribeEventTool {
    pub fn new(store: Arc<AutomationsStore>, config: Arc<MiraConfig>) -> Self {
        Self { store, config }
    }
}

#[async_trait]
impl Tool for SubscribeEventTool {
    fn name(&self) -> &str { "automations_subscribe_event" }
    fn description(&self) -> &str {
        "Subscribe to an internal MIRA event so the agent fires an action \
         whenever it occurs. Use this for EVENT-triggered notifications, \
         NOT `automations_schedule_followup` (which is time-based and \
         requires user approval per fire). \
         \n\nKey events: \
         \n  - `agent.worker.completed` — fires when a background task \
         spawned via `spawn_background_task` finishes. Use this for \
         \"notify me when task X is done\" patterns IF the prebuilt \
         `notify_channels` arg on spawn_background_task isn't enough. \
         Predicate keys on `payload.task_id`. Worked example — Signal \
         ping when task 019e0... completes: \
         \n    event_name = \"agent.worker.completed\" \
         \n    predicate  = {\"eq\": [\"payload.task_id\", \"019e0...\"]} \
         \n    action     = {\"kind\":\"channel_message\", \"channel\":\"signal\", \
         \"text_template\":\"Task done: {{payload.summary_or_error}}\"} \
         \n  - `tool.failed` — fires when any tool call fails. \
         \n  - `conversation.idle` — fires when an active conversation \
         goes silent past a configured threshold. \
         \n  - `memory.threshold` — fires when memory pressure crosses \
         a configured limit. \
         \n\nOptional `predicate` filters against {event, payload, user, \
         now}. Fetch the full catalog via GET /api/events/names if you \
         need to know what's emitted."
    }
    fn tier(&self) -> Tier { Tier::Pure }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name", "event_name", "action", "rationale"],
            "properties": {
                "name":        { "type": "string" },
                "description": { "type": "string" },
                "rationale":   { "type": "string" },
                "event_name":  { "type": "string", "description": "Internal event name to listen for." },
                "predicate":   { "type": "object" },
                "action":      action_schema(),
                "expires_at":  { "type": "integer" }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = caller(&args)?;
        let name = match require_str(&args, "name") { Ok(s) => s.to_string(), Err(e) => return Ok(ToolResult::failure(e)) };
        let event_name = match require_str(&args, "event_name") {
            Ok(s)  => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e)),
        };
        let rationale = match require_str(&args, "rationale") {
            Ok(s)  => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e)),
        };
        let action = match args.get("action") {
            Some(v) => match parse_action(v) { Ok(a) => a, Err(e) => return Ok(ToolResult::failure(e)) },
            None    => return Ok(ToolResult::failure("'action' is required")),
        };
        let predicate = args.get("predicate").cloned().filter(|v| !v.is_null());
        let description = args.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());
        let expires_at = args.get("expires_at").and_then(|v| v.as_i64());

        let status_override = match gate_create_event_subscription(
            &self.store, &self.config.automations, &user_id,
            OwnerKind::Agent, Some(&rationale),
        ) {
            Ok(v)  => v,
            Err(e) => return Ok(ToolResult::failure(gate_to_string(&e))),
        };
        let new = NewEventSubscription {
            user_id:     user_id.clone(),
            owner_kind:  OwnerKind::Agent,
            name,
            description,
            rationale:   Some(rationale),
            event_name,
            predicate,
            action,
            expires_at,
            status:      status_override,
            // Generic agent-created subscription tool — by default we
            // keep the row so the agent can react to repeated events.
            // The narrower spawn_background_task helper sets
            // delete_after_fire=true explicitly because its predicate
            // keys on a unique task_id.
            delete_after_fire: false,
        };
        match self.store.create_event_subscription(new) {
            Ok(s) => {
                let body = json!({
                    "id":         s.id,
                    "name":       s.name,
                    "event_name": s.event_name,
                    "status":     s.status.as_str(),
                    "pending_approval": matches!(
                        s.status, crate::automations::types::AutomationStatus::PendingApproval
                    ),
                });
                Ok(ToolResult::success(body.to_string()))
            }
            Err(e) => Ok(ToolResult::failure(format!("create_event_subscription: {e}"))),
        }
    }
}
