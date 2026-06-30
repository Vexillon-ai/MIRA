// SPDX-License-Identifier: AGPL-3.0-or-later

//! Agent-task lifecycle tools — `spawn_background_task`,
//! `get_task_result`.
//!
//! These are the primitives that turn MIRA's multi-agent runtime into
//! something a chat-tier LLM can actually use:
//!
//!   - `spawn_background_task` kicks off a worker under a known Skill
//!     (e.g. `com.mira.research`, `com.mira.claudecode`) and returns a
//!     `task_id` immediately. The model can then end its turn cleanly
//!     ("started research, will let you know when it's done") instead
//!     of trying to fit a long-running operation inside a single
//!     response.
//!
//!   - `get_task_result` looks up a previously spawned task by id and
//!     returns its current status + result_summary. Useful when a
//!     follow-up turn asks "how did the research go?" — the model can
//!     fetch the answer rather than guessing.
//!
//! Notification on completion is wired via the auto-subscribe path in
//! Step 4: `spawn_background_task` registers an
//! `automations_subscribe_event` row for `agent.worker.completed`
//! filtered by `task_id`, so when the supervisor emits the completion
//! event the user gets a `ChannelMessage` on the same channel they
//! kicked the task off from.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{info, warn};

use super::{Tier, Tool, ToolArgs, ToolResult, ToolVisibility};
use crate::agent::{
    Agent, AgentId, AgentRegistry, Supervisor,
};
use crate::automations::{
    Action, AutomationsStore, NewEventSubscription, OwnerKind,
    agent_gate::gate_create_event_subscription,
};
use crate::config::MiraConfig;
use crate::MiraError;
// Per-task budget defaults + ceiling now live in config
// (`agent.default_task_budget_usd` / `agent.max_task_budget_usd`) so a long
// research run can be allowed more spend without a recompile.

// ── spawn_background_task ────────────────────────────────────────────────────

pub struct SpawnBackgroundTaskTool {
    supervisor:     Arc<Supervisor>,
    agent_registry: Arc<AgentRegistry>,
    automations:    Option<Arc<AutomationsStore>>,
    config:         Arc<MiraConfig>,
    /// 0.111.0 — when wired, allocates a per-task artifact dir before
    /// spawn so the executor can run with cwd set + the brief can
    /// reference an absolute output path.
    task_artifacts: Option<Arc<crate::task_artifacts::TaskArtifactsStore>>,
    /// Phase B slice 2 — when wired, the `agent` arg can target a saved
    /// named agent by handle. Used to validate the handle exists + is
    /// enabled and to read its default budget before spawning.
    agent_defs: Option<Arc<crate::agent::AgentDefinitionStore>>,
    /// Capability RBAC — when wired, the spawning user's effective
    /// `max_task_budget_usd` clamps the per-task budget so a restricted
    /// user can't authorise more autonomous spend than their profile allows.
    auth_db: Option<Arc<crate::auth::AuthDb>>,
}

impl SpawnBackgroundTaskTool {
    pub fn new(
        supervisor:     Arc<Supervisor>,
        agent_registry: Arc<AgentRegistry>,
        automations:    Option<Arc<AutomationsStore>>,
        config:         Arc<MiraConfig>,
    ) -> Self {
        Self {
            supervisor, agent_registry, automations, config,
            task_artifacts: None, agent_defs: None, auth_db: None,
        }
    }

    /// 0.111.0 — wire the artifacts store. Called at gateway startup.
    pub fn with_task_artifacts(
        mut self, store: Arc<crate::task_artifacts::TaskArtifactsStore>,
    ) -> Self {
        self.task_artifacts = Some(store);
        self
    }

    /// Phase B slice 2 — wire the named-agent definition store so the
    /// `agent` arg can resolve a handle. Called at gateway startup.
    pub fn with_agent_defs(
        mut self, store: Arc<crate::agent::AgentDefinitionStore>,
    ) -> Self {
        self.agent_defs = Some(store);
        self
    }

    /// Capability RBAC — wire the auth DB so the spawning user's effective
    /// budget cap clamps the per-task budget. Called at gateway startup.
    pub fn with_auth_db(mut self, db: Arc<crate::auth::AuthDb>) -> Self {
        self.auth_db = Some(db);
        self
    }
}

#[async_trait]
impl Tool for SpawnBackgroundTaskTool {
    fn name(&self) -> &str { "spawn_background_task" }

    fn description(&self) -> &str {
        "Kick off a long-running task in a background subagent. Returns a \
         `task_id` immediately so you can end this turn cleanly while the \
         work continues. When the task finishes, the user is automatically \
         pinged on the same channel with the result (no follow-up wiring \
         needed). \
         \n\nUse this — instead of trying to do long work inline — whenever \
         the user says \"go away and let me know when you're done\", or \
         when the task obviously won't fit in a single tool call (deep \
         research, building a project, multi-source synthesis). \
         \n\nTwo ways to target a subagent — provide exactly one: \
         \n  - `skill` — a built-in packaged skill (see list below). \
         \n  - `agent` — a user-defined **named agent** by its handle (a \
         saved persona + tool set + model). Call `list_named_agents` first \
         to discover handles; pass the handle here WITHOUT the leading `@`. \
         The named agent's configured budget applies unless you override \
         `budget_usd`. \
         \n\nKnown skills: \
         \n  - `com.mira.research` — multi-source web research with \
         synthesis. Brief should be a research question. \
         \n  - `com.mira.claudecode` — drives the Claude Code CLI (Anthropic) \
         as a coding subagent to build/edit code. Brief should describe \
         what to build. (Requires the `claude` CLI on the server — falls \
         through with an error if absent.) \
         \n  - `com.mira.opencode` — drives the OpenCode CLI (sst/opencode, \
         multi-provider via OpenRouter / Anthropic / OpenAI / Gemini) as \
         a coding subagent. Same brief shape as claudecode. (Requires \
         the `opencode` CLI on the server.) \
         \n\nThe `brief` field is the FULL instruction the subagent will \
         act on; it doesn't get to see this conversation. Be specific. \
         \n\nMulti-channel notification — when the user asks to be \
         notified on a channel BESIDES the one they're chatting on (e.g. \
         \"build it and ping me on Signal when done\"), pass \
         `notify_channels: [\"signal\", ...]`. One additional one-shot \
         delivery subscription is registered per channel. The originating \
         channel always receives the result regardless. Do NOT use \
         `automations_schedule_followup` for this — that creates a TIME-\
         based recurring schedule, not an event-triggered notification, \
         and the user has to approve every fire."
    }

    fn tier(&self) -> Tier { Tier::System }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["brief"],
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Reverse-DNS Skill id, e.g. `com.mira.research` or `com.mira.claudecode`. Provide this OR `agent`, not both."
                },
                "agent": {
                    "type": "string",
                    "description": "Handle of a user-defined named agent (from `list_named_agents`), without the leading `@`. Provide this OR `skill`, not both."
                },
                "brief": {
                    "type": "string",
                    "description": "What the subagent should do. The subagent doesn't see this chat — be self-contained and specific. For research: the question. For coding: the build description."
                },
                "budget_usd": {
                    "type": "number",
                    "description": "Optional USD cap for this task. Default 2.0, hard max 10.0.",
                    "minimum": 0.05,
                    "maximum": 10.0
                },
                "notify_channels": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["web", "signal", "telegram"] },
                    "description": "Extra channels to deliver the completion result on, in addition to the channel the user is chatting on right now. Use when the user explicitly asks for notification on another channel — e.g. they're on web and ask for a Signal ping. The originating channel is always notified; entries here are merged in (deduped). Empty/omitted = single-channel delivery."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = args.get("_user_id").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .ok_or_else(|| MiraError::ToolError(
                "spawn_background_task called without caller identity".into(),
            ))?;
        let conv_id = args.get("_conversation_id").and_then(|v| v.as_str())
            .map(String::from);
        let channel = args.get("_channel").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| "web".to_string());

        let brief = match args.get("brief").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return Ok(ToolResult::failure("'brief' is required")),
        };

        let skill_arg = args.get("skill").and_then(|v| v.as_str())
            .map(str::trim).filter(|s| !s.is_empty());
        let agent_arg = args.get("agent").and_then(|v| v.as_str())
            .map(str::trim).filter(|s| !s.is_empty());

        // Resolve the target: a named agent (`agent`) or a built-in skill
        // (`skill`). Exactly one. A named agent maps onto the `named:<handle>`
        // skill-id convention the supervisor's resolver understands, and
        // contributes its configured budget as the default.
        let (skill_id, agent_default_budget) = match (agent_arg, skill_arg) {
            (Some(_), Some(_)) => return Ok(ToolResult::failure(
                "provide exactly one of 'agent' or 'skill', not both",
            )),
            (None, None) => return Ok(ToolResult::failure(
                "one of 'agent' (named agent handle) or 'skill' (built-in skill id) is required",
            )),
            (Some(handle), None) => {
                let Some(store) = self.agent_defs.as_ref() else {
                    return Ok(ToolResult::failure(
                        "named agents aren't available on this MIRA host (definition store not wired)",
                    ));
                };
                match store.get_by_name(handle) {
                    Ok(Some(def)) if def.enabled => (
                        crate::agent::skill_id_for_handle(handle),
                        def.budget_usd,
                    ),
                    Ok(Some(_)) => return Ok(ToolResult::failure(format!(
                        "named agent '@{handle}' is disabled — enable it first"
                    ))),
                    Ok(None) => return Ok(ToolResult::failure(format!(
                        "no named agent with handle '@{handle}'. Call `list_named_agents` to see what's available."
                    ))),
                    Err(e) => return Ok(ToolResult::failure(format!(
                        "failed to look up named agent '@{handle}': {e}"
                    ))),
                }
            }
            (None, Some(skill)) => (skill.to_string(), None),
        };

        // Budget precedence: explicit arg > named-agent default > config
        // default, then clamped to the host's hard ceiling.
        let mut budget_usd = args.get("budget_usd").and_then(|v| v.as_f64())
            .or(agent_default_budget)
            .unwrap_or(self.config.agent.default_task_budget_usd)
            .clamp(0.05, self.config.agent.max_task_budget_usd);

        // Capability RBAC — clamp to the spawning user's effective per-task
        // budget cap (admins resolve to no cap). Best-effort: a lookup error
        // leaves the host-ceiling-clamped value untouched.
        if let Some(db) = self.auth_db.as_ref() {
            if let Ok(caps) = db.effective_capabilities_for(&user_id) {
                let capped = caps.cap_task_budget(budget_usd).max(0.05);
                if capped < budget_usd {
                    info!("spawn_background_task: budget {budget_usd} → {capped} (capability cap for {user_id})");
                    budget_usd = capped;
                }
            }
        }

        // Extra channels the model wants completion delivered on. The
        // originating `channel` is always included implicitly; entries
        // here that match it are skipped to avoid double delivery.
        // Unknown channel names are accepted up front and will surface
        // as dispatch errors at fire time, same as user-created
        // channel_message subscriptions — better than failing the
        // spawn for a typo since the worker is the expensive part.
        let mut extra_channels: Vec<String> = args.get("notify_channels")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty() && s != &channel.to_lowercase())
                .collect::<Vec<_>>())
            .unwrap_or_default();

        // 0.112.0 — auto-detect notification preferences buried in
        // the brief. The chat model often parses "ping me on Signal"
        // as part of the brief instead of a tool argument; this
        // belt-and-braces guard catches that. Heuristic: a known
        // notification verb (ping/notify/alert/etc.) followed within
        // ~80 chars by a known channel name. Anything found is
        // merged into extra_channels (after the originating-channel
        // filter) and logged so we can monitor model behaviour.
        if extra_channels.is_empty() {
            let detected = detect_notification_channels(&brief);
            for ch in detected {
                if ch != channel.to_lowercase() && !extra_channels.contains(&ch) {
                    info!(
                        "spawn_background_task: auto-added '{ch}' to notify_channels \
                         based on brief content (model didn't set it explicitly)"
                    );
                    extra_channels.push(ch);
                }
            }
        }

        // Resolve the executor up front so the model sees a clear
        // "this skill isn't installed" error instead of the worker
        // failing inside the manager loop.
        let executor = match self.supervisor.executor_for(&skill_id) {
            Some(e) => e,
            None => return Ok(ToolResult::failure(format!(
                "skill '{skill_id}' has no executor registered. Either it's \
                 mistyped or the adapter isn't installed on this MIRA host."
            ))),
        };

        // Top-level tasks need a parent agent; create an ephemeral root
        // for this task. Root has unlimited budget; the worker carries
        // the finite per-task cap. (Future improvement: hold one root
        // per (user, channel) so sibling tasks share a session budget.)
        let root_handle = self.agent_registry.register(Agent::new_root());
        let root_id     = root_handle.read().expect("root read").id;

        // 0.111.0 — pre-generate the worker's AgentId so we can
        // allocate the artifact dir before spawn AND have the brief
        // reference the dir's absolute path. The same id flows through
        // as task_id in completion events.
        let task_id_v = AgentId::new();
        let (effective_brief, effective_context) = match self.task_artifacts.as_ref() {
            Some(store) => {
                match store.allocate(
                    &skill_id, &task_id_v.0.to_string(),
                    Some(&user_id), Some(&channel), &brief,
                ) {
                    Ok(dir) => {
                        let dir_str = dir.display().to_string();
                        let addendum = format!(
                            "\n\n---\n\
                             **Output directory**: `{dir_str}` \
                             (also exported as `$MIRA_TASK_OUTPUT_DIR`).\n\
                             - Write all deliverables under `output/` inside that directory.\n\
                             - **First action before any other work**: choose a 2-4 word \
                             kebab-case slug describing this task (e.g. `pong-game-modern`, \
                             `auth-bug-investigation`) and write that single line to \
                             `$MIRA_TASK_OUTPUT_DIR/SLUG`. MIRA renames the directory to \
                             `<slug>_<task_id_short>` after completion so the user can find \
                             the result later.\n\
                             - Optional: write debug logs under `logs/`.\n\
                             - Don't write anywhere outside `$MIRA_TASK_OUTPUT_DIR` unless \
                             the user explicitly asked you to edit a specific path."
                        );
                        let context = serde_json::json!({"output_dir": dir_str});
                        let verify = coding_addendum_for(&skill_id);
                        (format!("{brief}{addendum}{verify}"), Some(context))
                    }
                    Err(e) => {
                        warn!("artifact dir allocation failed (continuing without): {e}");
                        let verify = coding_addendum_for(&skill_id);
                        (format!("{brief}{verify}"), None)
                    }
                }
            }
            None => {
                let verify = coding_addendum_for(&skill_id);
                (format!("{brief}{verify}"), None)
            }
        };

        let handle = self.supervisor.spawn_worker_full_with_id(
            root_id,
            0,
            skill_id.clone(),
            effective_brief,
            effective_context,
            budget_usd,
            None, // deadline
            executor,
            None, // llm_choice — let the executor pick its default
            Some(user_id.clone()),
            Some(task_id_v),
        );
        let task_id = handle.agent_id;
        // Drop the JoinHandle on the completion oneshot — we don't await
        // it from the chat turn. The supervisor's manager loop will
        // emit `agent.worker.completed` (Step 3) once the worker
        // terminates, which the auto-subscription below picks up.
        drop(handle);

        // Auto-register a completion delivery: when the supervisor emits
        // `agent.worker.completed` with this task_id, fire a
        // ChannelMessage to the user on the channel they were on.
        let mut subscription_id: Option<String> = None;
        let mut extra_subscription_ids: Vec<(String, String)> = Vec::new();
        if let Some(store) = self.automations.as_ref() {
            match register_completion_delivery(
                store, &self.config, &user_id, &channel,
                conv_id.as_deref(), task_id, &skill_id,
            ).await {
                Ok(id) => { subscription_id = Some(id); }
                Err(e) => {
                    warn!("spawn_background_task: completion auto-subscribe \
                           failed (task continues, user won't auto-receive \
                           result): {e}");
                }
            }
            // Register additional one-shot deliveries for each extra
            // channel the model requested. `conv_id` is intentionally
            // not propagated — it's a web-only concept; cross-channel
            // delivery should land on the channel's default thread for
            // that user (signal/telegram route by address, not conv).
            for ch in &extra_channels {
                match register_completion_delivery(
                    store, &self.config, &user_id, ch,
                    None, task_id, &skill_id,
                ).await {
                    Ok(id) => { extra_subscription_ids.push((ch.clone(), id)); }
                    Err(e) => {
                        warn!("spawn_background_task: extra-channel ({ch}) \
                               auto-subscribe failed (other channels still \
                               wired): {e}");
                    }
                }
            }
        } else {
            warn!("spawn_background_task: no automations store, completion \
                   notification skipped — user must call get_task_result \
                   manually");
        }

        info!(
            "spawn_background_task: skill={} task_id={} budget=${:.2} channel={} subscription={:?} extra={:?}",
            skill_id, task_id, budget_usd, channel, subscription_id, extra_subscription_ids,
        );

        let extra_channel_list: Vec<String> =
            extra_subscription_ids.iter().map(|(c, _)| c.clone()).collect();
        let body = json!({
            "task_id":              task_id.to_string(),
            "skill":                skill_id,
            "status":               "running",
            "budget_usd":           budget_usd,
            "delivery_channel":     channel,
            "delivery_subscription": subscription_id,
            "extra_delivery_channels": extra_channel_list,
            "note": if extra_subscription_ids.is_empty() {
                "The user will be pinged on this channel when the task finishes. \
                 Use `get_task_result` to inspect status before then.".to_string()
            } else {
                format!(
                    "The user will be pinged on {} and {} when the task finishes. \
                     Use `get_task_result` to inspect status before then.",
                    channel,
                    extra_channel_list.join(", "),
                )
            },
        });
        Ok(ToolResult::success(body.to_string()))
    }
}

/// Register a `agent.worker.completed` event subscription that posts the
/// completion summary back to the user on the originating channel.
async fn register_completion_delivery(
    store:    &AutomationsStore,
    config:   &MiraConfig,
    user_id:  &str,
    channel:  &str,
    conv_id:  Option<&str>,
    task_id:  AgentId,
    skill_id: &str,
) -> Result<String, String> {
    // The body the user sees. `{{payload.…}}` placeholders are resolved
    // by the automation dispatcher when the event fires. The supervisor
    // pre-renders `status_emoji`, `status_label`, and `summary_or_error`
    // so this single template handles both success ("✅ … finished")
    // and failure ("⚠️ … failed") cleanly.
    let text_template = format!(
        "{{{{payload.status_emoji}}}} Task `{skill_id}` {{{{payload.status_label}}}}\n\n\
         {{{{payload.summary_or_error}}}}\n\n\
         _(task_id: {task_id})_"
    );

    // Deliver back into the originating conversation when the chat
    // handler injected a `_conversation_id` (web). This keeps the result
    // visible in the same thread the user started the task from instead
    // of dropping it into a sibling "Notifications" conversation. For
    // `signal`/`telegram` the field is ignored (those channels route by
    // address, not conversation_id), so passing it costs nothing.
    let action = Action::ChannelMessage {
        channel:         channel.to_string(),
        to:              None, // route to caller's identity for this channel
        conversation_id: conv_id.map(str::to_string),
        text_template,
    };

    // Predicate: only fire when the inbound payload's task_id matches
    // ours. The supervisor emits the event with the worker's AgentId
    // as `task_id` (see Step 3). The first arg `payload.task_id` is a
    // dotted path; the second is a literal — strings without a dot are
    // treated as literals by the evaluator (`predicate.rs::resolve`).
    let predicate = json!({
        "eq": ["payload.task_id", task_id.to_string()]
    });

    // Run the gate so quota is enforced (and any rationale-required
    // policy gets surfaced) — but ignore its `PendingApproval` advice.
    // The user explicitly called `spawn_background_task` which IS the
    // act of authorising delivery; making them approve a hidden helper
    // subscription afterwards would just confuse them. The completion
    // subscription is plumbing for the task they already approved.
    let _ = gate_create_event_subscription(
        store, &config.automations, user_id,
        OwnerKind::Agent, Some("Auto-delivery for spawn_background_task"),
    ).map_err(|e| e.to_string())?;

    let new = NewEventSubscription {
        user_id:           user_id.to_string(),
        owner_kind:        OwnerKind::Agent,
        name:              format!("Task {} delivery", task_id),
        description:       Some(format!("Deliver result of task {} on {}", task_id, channel)),
        rationale:         Some("Auto-registered by spawn_background_task".to_string()),
        event_name:        crate::events::names::AGENT_WORKER_COMPLETED.to_string(),
        predicate:         Some(predicate),
        action,
        expires_at:        None,
        status:            Some(crate::automations::AutomationStatus::Active),
        // One-shot: predicate keys on this exact task_id which is
        // unique per worker. Once it fires, the row is dead weight —
        // tear it down so we don't accumulate one row per spawned
        // task forever.
        delete_after_fire: true,
    };
    store.create_event_subscription(new)
        .map(|sub| sub.id)
        .map_err(|e| format!("create_event_subscription: {e}"))
}

// ── 0.112.0 — heuristics ────────────────────────────────────────────────────

/// Notification verbs that, when paired with a channel name nearby in
/// the same sentence, signal "the user wants to be told via that
/// channel". Tuned conservatively — false positives auto-add an extra
/// notification (annoying but cheap), false negatives silently miss
/// (the failure mode that prompted this whole heuristic).
const NOTIFY_TRIGGERS: &[&str] = &[
    "ping",        "ping me",     "notify",       "notif",
    "alert",       "tell me",     "let me know",  "message me",
    "contact me",  "reach me",    "send me a",    "drop me a",
    "update me",   "shoot me",    "text me",
];

/// Channel names we know how to route to. Order matters only for log
/// readability — detection short-circuits per channel anyway.
const KNOWN_CHANNELS: &[&str] = &["signal", "telegram"];

/// How far after a trigger word to look for a channel name. 80 chars
/// covers "ping me on signal once it's done" + a handful of qualifiers
/// without dragging in unrelated mentions later in the brief.
const NOTIFY_WINDOW_CHARS: usize = 80;

/// Scan a brief for "{trigger} ... {channel}" patterns and return the
/// channel names that matched. De-duplicated. Designed to be cheap
/// enough to run on every spawn (substring scan, no regex).
pub(crate) fn detect_notification_channels(brief: &str) -> Vec<String> {
    let lower = brief.to_lowercase();
    let mut found = std::collections::HashSet::new();
    for trigger in NOTIFY_TRIGGERS {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(trigger) {
            let pos = from + rel;
            let end = (pos + trigger.len() + NOTIFY_WINDOW_CHARS).min(lower.len());
            let window = &lower[pos..end];
            for ch in KNOWN_CHANNELS {
                // Word-boundary-ish check: channel preceded by a
                // non-letter (or start of window). Avoids matching
                // "designal" → "signal".
                let bytes = window.as_bytes();
                if let Some(off) = window.find(ch) {
                    let prev_ok = off == 0
                        || !bytes[off - 1].is_ascii_alphabetic();
                    let after = off + ch.len();
                    let next_ok = after >= bytes.len()
                        || !bytes[after].is_ascii_alphabetic();
                    if prev_ok && next_ok {
                        found.insert((*ch).to_string());
                    }
                }
            }
            from = pos + trigger.len();
            if from >= lower.len() { break; }
        }
    }
    let mut out: Vec<String> = found.into_iter().collect();
    out.sort();
    out
}

/// 0.112.0 — coding-skill brief addendum. Appended to every spawn
/// targeting com.mira.claudecode or com.mira.opencode. Forces the
/// subagent to verify before reporting completion, which is the
/// single biggest quality win for one-shot game/app builds.
pub(crate) const CODING_VERIFICATION_ADDENDUM: &str = "\n\n---\n\
**Acceptance criteria — do not mark this task complete until ALL pass:**\n\
1. The code builds / interprets / loads without errors. For web apps, \
   open `index.html` (start a local HTTP server: `python3 -m http.server 8000`, \
   then `curl -s http://localhost:8000/` and grep the response for HTML correctness).\n\
2. Watch for runtime errors: tail the dev server log, grep JS source for \
   obvious bugs (uncaught references, mismatched function names, missing \
   imports), or run a headless browser smoke test if available \
   (`chromium --headless --disable-gpu --dump-dom http://localhost:8000/`).\n\
3. Test the **primary user flow** end-to-end at least once. For a game: \
   verify input handlers fire (paddles move on key events), the main loop \
   renders something non-trivial, and the win/lose path doesn't crash. \
   For a tool: run it with realistic input and verify the output.\n\
4. If anything fails, **fix it and retry**. Do NOT submit a half-finished \
   implementation with a hopeful summary.\n\n\
If you genuinely can't run the code in this environment (no browser, no \
runtime, etc.), write a `VERIFY.md` in your output dir with step-by-step \
manual test instructions, and explicitly say so in your final summary — \
honesty about what you couldn't verify is far better than false claims of \
success.";

/// Returns the verification addendum if `skill_id` is a known coding
/// skill, empty string otherwise. Centralised so the empty-string
/// branches in execute() stay readable.
fn coding_addendum_for(skill_id: &str) -> &'static str {
    match skill_id {
        "com.mira.claudecode" | "com.mira.opencode" => CODING_VERIFICATION_ADDENDUM,
        _ => "",
    }
}

// ── get_task_result ──────────────────────────────────────────────────────────

pub struct GetTaskResultTool {
    agent_registry: Arc<AgentRegistry>,
}

impl GetTaskResultTool {
    pub fn new(agent_registry: Arc<AgentRegistry>) -> Self {
        Self { agent_registry }
    }
}

#[async_trait]
impl Tool for GetTaskResultTool {
    fn name(&self) -> &str { "get_task_result" }

    fn description(&self) -> &str {
        "Look up a task by id (returned by `spawn_background_task`) and \
         report its current status. When the status is `completed`, \
         `result_summary` carries what the subagent produced. When the \
         status is `running` or `pending`, the task is still in flight — \
         the user will be pinged automatically when it finishes; tell \
         them so rather than busy-polling."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["task_id"],
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "UUID returned by spawn_background_task."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let raw = match args.get("task_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(ToolResult::failure("'task_id' is required")),
        };
        let id = match uuid::Uuid::parse_str(raw) {
            Ok(u) => AgentId(u),
            Err(_) => return Ok(ToolResult::failure(format!(
                "task_id '{raw}' is not a valid UUID"
            ))),
        };

        let handle = match self.agent_registry.get(id) {
            Some(h) => h,
            None => return Ok(ToolResult::failure(format!(
                "no task with id {raw} (it may have been pruned, or never existed)"
            ))),
        };
        let agent = handle.read().expect("agent read");

        let body = json!({
            "task_id":        agent.id.to_string(),
            "skill":          agent.skill_id,
            "status":         status_str(agent.status),
            "result_summary": agent.result_summary,
            "failure_reason": agent.failure_reason,
            "fault_code":     agent.fault.as_ref().map(|f| f.code()),
            "current_step":   agent.current_step,
            "spent_usd":      agent.budget.spent_usd,
            "budget_usd":     agent.budget.max_usd,
        });
        Ok(ToolResult::success(body.to_string()))
    }
}

// ── list_named_agents ─────────────────────────────────────────────────────────

/// Phase B slice 2 — lets the model discover user-defined named agents so it
/// can delegate to one via `spawn_background_task` with `agent: "<handle>"`.
pub struct ListNamedAgentsTool {
    agent_defs: Option<Arc<crate::agent::AgentDefinitionStore>>,
}

impl ListNamedAgentsTool {
    pub fn new(agent_defs: Option<Arc<crate::agent::AgentDefinitionStore>>) -> Self {
        Self { agent_defs }
    }
}

#[async_trait]
impl Tool for ListNamedAgentsTool {
    fn name(&self) -> &str { "list_named_agents" }

    fn description(&self) -> &str {
        "List the user-defined **named agents** available on this MIRA host. \
         Each is a saved persona + tool set + model, addressed by a handle. \
         When the user asks to use a named agent (e.g. \"have the researcher \
         look into X\"), or when delegating fits a saved agent better than a \
         built-in skill, call this to get the handle, then \
         `spawn_background_task` with `agent: \"<handle>\"`. Returns only \
         enabled agents; an empty list means none are configured."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
        let Some(store) = self.agent_defs.as_ref() else {
            return Ok(ToolResult::success(
                json!({ "agents": [], "note": "named agents not available on this host" }).to_string(),
            ));
        };
        let defs = match store.list() {
            Ok(d) => d,
            Err(e) => return Ok(ToolResult::failure(format!("failed to list named agents: {e}"))),
        };
        let agents: Vec<Value> = defs.into_iter()
            .filter(|d| d.enabled)
            .map(|d| json!({
                "handle":      d.name,
                "description": d.description,
                "tools":       if d.allowed_tools.is_empty() { Value::String("default set".into()) }
                               else { json!(d.allowed_tools) },
                "model_alias": d.model_alias,
                "budget_usd":  d.budget_usd,
            }))
            .collect();
        Ok(ToolResult::success(json!({ "agents": agents }).to_string()))
    }
}

// ── create_named_agent ────────────────────────────────────────────────────────

/// Turn a friendly name into a valid handle slug: lowercase, spaces/underscores
/// → dashes, drop other invalid chars, collapse repeats, ensure it starts with
/// a letter. `validate_name` in the store is the final gate.
pub(crate) fn slugify_handle(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            prev_dash = false;
        } else if (c == ' ' || c == '_' || c == '-') && !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    // Must start with a letter — strip leading digits/dashes.
    trimmed.trim_start_matches(|c: char| !c.is_ascii_lowercase()).to_string()
}

/// Lets the model save a new reusable named agent on the user's request.
pub struct CreateNamedAgentTool {
    agent_defs: Option<Arc<crate::agent::AgentDefinitionStore>>,
}

impl CreateNamedAgentTool {
    pub fn new(agent_defs: Option<Arc<crate::agent::AgentDefinitionStore>>) -> Self {
        Self { agent_defs }
    }
}

#[async_trait]
impl Tool for CreateNamedAgentTool {
    fn name(&self) -> &str { "create_named_agent" }

    fn description(&self) -> &str {
        "Create a new reusable **named agent** (a saved persona + tool set + \
         model) that can later be invoked by handle via `spawn_background_task` \
         (`agent: \"<handle>\"`) or referenced in a workflow. Use this when the \
         user asks you to create or save an agent. `name` becomes the handle — \
         use a short slug (lowercase letters, digits, dashes); a friendlier name \
         is auto-slugified (e.g. \"MasterResearcher\" → \"masterresearcher\"). \
         Write a clear `system_prompt` describing the agent's role, method, and \
         output format. `allowed_tools` is the list of tool names it may use \
         (e.g. web_search, web_fetch, url_preview, code_run, and any \
         `mcp__puppeteer__*` browser tools) — leave empty to inherit MIRA's \
         default toolset. Optional `model_alias` (primary/coding/research/cheap) \
         and `budget_usd` per run. Returns the created handle; tell the user how \
         to invoke it."
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name", "system_prompt"],
            "properties": {
                "name":          { "type": "string", "description": "Handle/slug for the agent. Lowercase letters, digits, dashes; auto-slugified from a friendlier name." },
                "description":   { "type": "string", "description": "One-line summary shown in the agents list." },
                "system_prompt": { "type": "string", "description": "The persona / instructions the agent runs with — role, method, and desired output format." },
                "allowed_tools": { "type": "array", "items": { "type": "string" }, "description": "Tool names the agent may use. Empty = inherit MIRA's default toolset." },
                "model_alias":   { "type": "string", "description": "Optional LLM alias: primary | coding | research | cheap. Omit for the default." },
                "budget_usd":    { "type": "number", "description": "Optional per-run USD cap." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let Some(store) = self.agent_defs.as_ref() else {
            return Ok(ToolResult::failure(
                "named agents are not available on this host (no definitions store)",
            ));
        };
        let raw_name = args.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        if raw_name.is_empty() {
            return Ok(ToolResult::failure("create_named_agent: `name` is required"));
        }
        let system_prompt = args.get("system_prompt").and_then(|v| v.as_str()).unwrap_or("").trim();
        if system_prompt.is_empty() {
            return Ok(ToolResult::failure("create_named_agent: `system_prompt` is required"));
        }
        let handle = slugify_handle(raw_name);
        let allowed_tools: Vec<String> = args.get("allowed_tools").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let model_alias = args.get("model_alias").and_then(|v| v.as_str())
            .map(str::to_string).filter(|s| !s.is_empty());
        let budget_usd = args.get("budget_usd").and_then(|v| v.as_f64());

        let new = crate::agent::NewAgentDefinition {
            name: handle.clone(),
            description: args.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            system_prompt: system_prompt.to_string(),
            allowed_tools,
            model_alias,
            budget_usd,
            enabled: true,
        };
        match store.create(new) {
            Ok(def) => {
                info!("named agent created via tool: @{} ({})", def.name, def.id);
                Ok(ToolResult::success(json!({
                    "created": true,
                    "handle":  def.name,
                    "id":      def.id,
                    "invoke_hint": format!("@{} <your request>", def.name),
                }).to_string()))
            }
            Err(e) => Ok(ToolResult::failure(format!("create_named_agent: {e}"))),
        }
    }
}

fn status_str(s: crate::agent::AgentStatus) -> &'static str {
    use crate::agent::AgentStatus::*;
    match s {
        Pending     => "pending",
        Running     => "running",
        Paused      => "paused",
        Completed   => "completed",
        Failed      => "failed",
        Interrupted => "interrupted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        AgentRegistry, MiraSkillResolver, Supervisor,
        WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure, WorkerTask,
    };
    use crate::automations::AutomationsStore;

    #[test]
    fn slugify_handle_cases() {
        assert_eq!(slugify_handle("MasterResearcher"), "masterresearcher");
        assert_eq!(slugify_handle("Master Researcher"), "master-researcher");
        assert_eq!(slugify_handle("code_reviewer"), "code-reviewer");
        assert_eq!(slugify_handle("  Trends 2024  "), "trends-2024");
        // Must start with a letter — leading digits stripped.
        assert_eq!(slugify_handle("2fast"), "fast");
    }

    #[tokio::test]
    async fn create_named_agent_round_trip() {
        let store = std::sync::Arc::new(crate::agent::AgentDefinitionStore::open_memory().unwrap());
        let tool = CreateNamedAgentTool::new(Some(store.clone()));
        let r = tool.execute(serde_json::json!({
            "name": "MasterResearcher",
            "description": "Multi-source researcher",
            "system_prompt": "You research topics and write cited reports.",
            "allowed_tools": ["web_search", "web_fetch"],
        })).await.unwrap();
        assert!(r.success, "got {r:?}");
        assert!(r.output.contains("\"handle\":\"masterresearcher\""), "{}", r.output);
        // Persisted + invocable by handle.
        assert!(store.get_by_name("masterresearcher").unwrap().is_some());
        // Duplicate is a clean failure, not a panic.
        let dup = tool.execute(serde_json::json!({
            "name": "masterresearcher", "system_prompt": "x",
        })).await.unwrap();
        assert!(!dup.success);
        assert!(dup.error.unwrap().contains("already exists"));
    }

    #[tokio::test]
    async fn create_named_agent_requires_prompt_and_store() {
        // No store → clean failure.
        let none = CreateNamedAgentTool::new(None);
        let r = none.execute(serde_json::json!({"name": "x", "system_prompt": "y"})).await.unwrap();
        assert!(!r.success);
        // Missing system_prompt → clean failure.
        let store = std::sync::Arc::new(crate::agent::AgentDefinitionStore::open_memory().unwrap());
        let tool = CreateNamedAgentTool::new(Some(store));
        let r = tool.execute(serde_json::json!({"name": "researcher"})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("system_prompt"));
    }

    // ── 0.112.0 — notification heuristic ───────────────────────────

    #[test]
    fn detect_signal_from_ping_phrasing() {
        let r = detect_notification_channels(
            "Build me a thing and ping me on Signal when done",
        );
        assert_eq!(r, vec!["signal".to_string()]);
    }

    #[test]
    fn detect_handles_send_me_a_message_via_signal() {
        let brief = "...send me a message either on this channel or via Signal \
                     to tell me the plan is ready for review.";
        let r = detect_notification_channels(brief);
        assert!(r.contains(&"signal".to_string()), "got: {r:?}");
    }

    #[test]
    fn detect_skips_unrelated_signal_mention() {
        // The Signal protocol is the topic, not the notification channel.
        let r = detect_notification_channels(
            "Research the Signal protocol's message ordering guarantees.",
        );
        assert!(r.is_empty(), "should not auto-add signal here, got: {r:?}");
    }

    #[test]
    fn detect_picks_up_telegram_too() {
        let r = detect_notification_channels(
            "Notify me on Telegram once it's deployed.",
        );
        assert_eq!(r, vec!["telegram".to_string()]);
    }

    #[test]
    fn detect_returns_both_when_both_requested() {
        let r = detect_notification_channels(
            "Ping me on Signal AND let me know via Telegram",
        );
        let mut sorted = r.clone(); sorted.sort();
        assert_eq!(sorted, vec!["signal".to_string(), "telegram".to_string()]);
    }

    #[test]
    fn detect_empty_when_no_trigger() {
        // No notification verb — just a content mention.
        let r = detect_notification_channels(
            "Build a Signal client. Use any framework you want.",
        );
        assert!(r.is_empty(), "got: {r:?}");
    }

    #[test]
    fn detect_word_boundary_avoids_substring_false_positive() {
        // "designal" should NOT match "signal".
        let r = detect_notification_channels(
            "Notify me about any designal patterns you find",
        );
        assert!(r.is_empty(), "got: {r:?}");
    }

    #[test]
    fn coding_addendum_only_for_coding_skills() {
        assert!(!coding_addendum_for("com.mira.claudecode").is_empty());
        assert!(!coding_addendum_for("com.mira.opencode").is_empty());
        assert!(coding_addendum_for("com.mira.research").is_empty());
        assert!(coding_addendum_for("com.example.unknown").is_empty());
    }

    use crate::config::MiraConfig;
    use crate::events::EventBus;
    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    /// End-to-end exercise of the spawn → event → subscription registration
    /// path. We don't run the dispatcher here (that's exercised in the
    /// automations module's own tests), but we verify the subscription is
    /// in the store with the right event_name + predicate, and that the
    /// completion event reaches subscribers on the bus.
    #[tokio::test]
    async fn spawn_background_task_registers_completion_subscription() {
        struct EchoExec;
        #[async_trait]
        impl WorkerTask for EchoExec {
            async fn run(&self, a: WorkerAssignment, _: WorkerContext)
                -> Result<WorkerComplete, WorkerFailure>
            {
                Ok(WorkerComplete {
                    result_summary: format!("echo: {}", a.task),
                    artifacts: vec![],
                })
            }
        }

        // Build the runtime pieces a real gateway would.
        let dir = tempdir().unwrap();
        let bus = Arc::new(EventBus::new());
        let mut bus_rx = bus.subscribe();
        let registry = Arc::new(AgentRegistry::new());
        let resolver = MiraSkillResolver::new()
            .with_skill("com.mira.test", Arc::new(EchoExec) as Arc<dyn WorkerTask>);
        let sup = Arc::new(
            Supervisor::new(Arc::clone(&registry))
                .with_event_bus(Arc::clone(&bus))
                .with_resolver(Arc::new(resolver))
        );
        let store = Arc::new(
            AutomationsStore::open(&dir.path().join("automations.db")).unwrap()
        );
        let config = Arc::new(MiraConfig::default());

        // Act — fire the tool with a minimal inject map.
        let tool = SpawnBackgroundTaskTool::new(
            Arc::clone(&sup),
            Arc::clone(&registry),
            Some(Arc::clone(&store)),
            Arc::clone(&config),
        );

        let args = json!({
            "_user_id":         "alice",
            "_conversation_id": "conv-1",
            "_channel":         "web",
            "skill":            "com.mira.test",
            "brief":            "do the thing",
        });
        let result = tool.execute(args).await.unwrap();
        assert!(result.success, "tool should succeed: {result:?}");
        let body: Value = serde_json::from_str(&result.output).unwrap();
        let task_id = body["task_id"].as_str().unwrap().to_string();
        let sub_id = body["delivery_subscription"].as_str().unwrap().to_string();
        assert_eq!(body["status"], "running");
        assert_eq!(body["delivery_channel"], "web");

        // Assert — the auto-registered subscription is present, owned
        // by the user, listening for the right event, with a predicate
        // that targets THIS task_id.
        let subs = store.active_subscriptions_for(
            crate::events::names::AGENT_WORKER_COMPLETED,
        ).unwrap();
        let our = subs.iter().find(|s| s.id == sub_id).expect("subscription persisted");
        assert_eq!(our.user_id, "alice");
        let pred = our.predicate.as_ref().expect("predicate set");
        let pred_str = pred.to_string();
        assert!(pred_str.contains(&task_id), "predicate must filter by task_id; got {pred_str}");

        // The auto-delivery action must thread `_conversation_id` through
        // so the dispatcher delivers the result back into the chat the
        // user started the task from, not a sibling "Notifications"
        // thread. Regression test for the routing fix.
        match &our.action {
            crate::automations::Action::ChannelMessage { conversation_id, .. } => {
                assert_eq!(
                    conversation_id.as_deref(), Some("conv-1"),
                    "action.conversation_id should match the originating chat",
                );
            }
            other => panic!("expected ChannelMessage action, got {other:?}"),
        }

        // The worker completes asynchronously; wait for the bus event.
        let ev = timeout(Duration::from_millis(500), async {
            loop {
                let e = bus_rx.recv().await.unwrap();
                if e.name == crate::events::names::AGENT_WORKER_COMPLETED {
                    return e;
                }
            }
        }).await.expect("worker.completed event never fired");

        assert_eq!(ev.user_id.as_deref(), Some("alice"));
        assert_eq!(ev.payload["status"], "completed");
        assert_eq!(ev.payload["task_id"], task_id);
        assert_eq!(ev.payload["summary"], "echo: do the thing");

        // And get_task_result returns the final state.
        let getter = GetTaskResultTool::new(Arc::clone(&registry));
        // Status update is async; poll briefly.
        for _ in 0..20 {
            let res = getter.execute(json!({ "task_id": task_id })).await.unwrap();
            assert!(res.success);
            let body: Value = serde_json::from_str(&res.output).unwrap();
            if body["status"] == "completed" {
                assert_eq!(body["result_summary"], "echo: do the thing");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("get_task_result never reported completed");
    }

    /// Unknown skill → tool returns a clear failure rather than spawning
    /// a worker that immediately fails inside the manager loop.
    #[tokio::test]
    async fn spawn_background_task_rejects_unknown_skill() {
        let dir = tempdir().unwrap();
        let bus = Arc::new(EventBus::new());
        let registry = Arc::new(AgentRegistry::new());
        // Empty resolver — no skills registered.
        let sup = Arc::new(
            Supervisor::new(Arc::clone(&registry))
                .with_event_bus(Arc::clone(&bus))
                .with_resolver(Arc::new(MiraSkillResolver::new()))
        );
        let store = Arc::new(
            AutomationsStore::open(&dir.path().join("automations.db")).unwrap()
        );
        let config = Arc::new(MiraConfig::default());

        let tool = SpawnBackgroundTaskTool::new(
            sup, registry, Some(store), config,
        );
        let res = tool.execute(json!({
            "_user_id": "bob",
            "_channel": "web",
            "skill":    "com.mira.does_not_exist",
            "brief":    "x",
        })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("has no executor registered"));
    }

    /// Reject obvious junk input (missing skill, missing brief).
    #[tokio::test]
    async fn spawn_background_task_validates_inputs() {
        let dir = tempdir().unwrap();
        let registry = Arc::new(AgentRegistry::new());
        let sup = Arc::new(
            Supervisor::new(Arc::clone(&registry))
                .with_event_bus(Arc::new(EventBus::new()))
        );
        let store = Arc::new(
            AutomationsStore::open(&dir.path().join("automations.db")).unwrap()
        );
        let config = Arc::new(MiraConfig::default());
        let tool = SpawnBackgroundTaskTool::new(
            sup, registry, Some(store), config,
        );

        // No identity injection.
        let r = tool.execute(json!({ "skill": "com.mira.test", "brief": "x" })).await;
        assert!(r.is_err(), "must refuse unauthenticated callers");

        // Missing skill.
        let r = tool.execute(json!({ "_user_id": "u", "brief": "x" })).await.unwrap();
        assert!(!r.success);

        // Missing brief.
        let r = tool.execute(json!({ "_user_id": "u", "skill": "com.mira.test" })).await.unwrap();
        assert!(!r.success);
    }

    /// The `agent` arg resolves a named agent to the `named:<handle>` skill id
    /// and spawns through the supervisor. Also covers the agent/skill XOR and
    /// disabled/unknown-handle guards.
    #[tokio::test]
    async fn spawn_background_task_routes_named_agent() {
        use crate::agent::{AgentDefinitionStore, NewAgentDefinition};

        struct EchoExec;
        #[async_trait]
        impl WorkerTask for EchoExec {
            async fn run(&self, a: WorkerAssignment, _: WorkerContext)
                -> Result<WorkerComplete, WorkerFailure>
            {
                Ok(WorkerComplete { result_summary: format!("ran: {}", a.task), artifacts: vec![] })
            }
        }

        let dir = tempdir().unwrap();
        let registry = Arc::new(AgentRegistry::new());
        // The supervisor's resolver maps the `named:echo` id to the executor,
        // exactly as the gateway's ChainedResolver+NamedAgentResolver would.
        let resolver = MiraSkillResolver::new()
            .with_skill("named:echo", Arc::new(EchoExec) as Arc<dyn WorkerTask>);
        let sup = Arc::new(
            Supervisor::new(Arc::clone(&registry))
                .with_event_bus(Arc::new(EventBus::new()))
                .with_resolver(Arc::new(resolver))
        );
        let autostore = Arc::new(
            AutomationsStore::open(&dir.path().join("automations.db")).unwrap()
        );
        let config = Arc::new(MiraConfig::default());

        let defs = Arc::new(AgentDefinitionStore::open(&dir.path().join("defs.db")).unwrap());
        defs.create(NewAgentDefinition {
            name: "echo".into(), description: "".into(), system_prompt: "".into(),
            allowed_tools: vec![], model_alias: None, budget_usd: Some(4.0), enabled: true,
        }).unwrap();
        defs.create(NewAgentDefinition {
            name: "off".into(), description: "".into(), system_prompt: "".into(),
            allowed_tools: vec![], model_alias: None, budget_usd: None, enabled: false,
        }).unwrap();

        let tool = SpawnBackgroundTaskTool::new(
            Arc::clone(&sup), Arc::clone(&registry), Some(autostore), config,
        ).with_agent_defs(Arc::clone(&defs));

        // Happy path — `agent` resolves, spawns under `named:echo`, and the
        // agent's configured budget (4.0) becomes the default.
        let res = tool.execute(json!({
            "_user_id": "alice", "_channel": "web",
            "agent": "echo", "brief": "do it",
        })).await.unwrap();
        assert!(res.success, "named agent spawn should succeed: {res:?}");
        let body: Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(body["skill"], "named:echo");
        assert_eq!(body["budget_usd"], 4.0);

        // XOR guard — both provided.
        let r = tool.execute(json!({
            "_user_id": "alice", "agent": "echo", "skill": "com.mira.x", "brief": "b",
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("exactly one"));

        // Disabled agent → clear failure.
        let r = tool.execute(json!({
            "_user_id": "alice", "agent": "off", "brief": "b",
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("disabled"));

        // Unknown handle → clear failure.
        let r = tool.execute(json!({
            "_user_id": "alice", "agent": "ghost", "brief": "b",
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("no named agent"));
    }
}
