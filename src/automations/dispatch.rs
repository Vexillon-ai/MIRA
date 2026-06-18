// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/dispatch.rs
//! Action dispatcher.
//!
//! Every activation — whether produced by a schedule, a webhook, or an
//! event subscription — flows through [`Dispatcher::dispatch`]. 
//! wires up all five action variants:
//!
//! - `Internal`        — built-in heartbeat handler.
//! - `Prompt`          — drop a user message into a conversation, run agent.
//! - `ToolCall`        — invoke a registered backend tool.
//! - `HttpPost`        — outbound webhook (templated body).
//! - `ChannelMessage`  — fire-and-forget notification on a user-facing channel.
//!
//! The non-Internal handlers depend on subsystems built elsewhere
//! (`AgentCore`, `HistoryStore`, `NotificationBus`, `reqwest`). To keep this
//! module testable without standing all of that up, every dependency is
//! `Option<Arc<…>>`. A handler whose dependency is missing returns a clear
//! `ConfigError`, which the worker records as a failure — never a panic.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::agent::{AgentCore, StreamEvent, TurnContext};
use crate::auth::LocalAuthService;
use crate::channel::telegram_channel::TelegramChannel;
use crate::channel_accounts::ChannelAccountStore;
use crate::history::{HistoryStore, MessageRole, NewConversation, NewMessage};
use crate::notifications::{Notification, NotificationBus, NotificationKind};
use crate::providers::signal_cli::SignalCliClient;
use crate::providers::signal_cli::sse_listener::{synth_signal_voice, write_voice_tempfile};
use crate::tts::TtsService;
use crate::voice::{parse_user_prefs, resolve_voice, ResponsePolicy};
use crate::MiraError;

use super::heartbeats::{HeartbeatContext, HeartbeatRegistry};
use super::rate_limit::{ChannelRateLimiter, RateDecision};
use super::store::AutomationsStore;
use super::template;
use super::types::{Action, ConversationStrategy, PromptAction, RunOutcome};

// ── Activation / outcome ─────────────────────────────────────────────────────

// What the worker tells the dispatcher about the activation it's about
// to run. `source_kind` is `"schedule"` for Slices 1–4; webhook and event
// activators reuse the same shape and pass the inbound payload
// via `payload` so action templates can read `{{payload.…}}`.
// // added `chain_ids` for loop detection: each entry-point starts a
// fresh chain (empty list); a sub-activation triggered downstream appends
// its parent's `source_id` to the chain. The dispatcher rejects an
// activation when the chain length exceeds the configured cap or
// `source_id` already appears in the chain (a cycle).
pub struct Activation<'a> {
    pub source_kind: &'static str,
    pub source_id:   &'a str,
    pub user_id:     &'a str,
    pub action:      &'a Action,
    // Optional inbound payload. Schedules pass `None`; webhook + event
    // activations pass the parsed body. Rendered into `tpl_ctx.payload`.
    pub payload:     Option<&'a serde_json::Value>,
    // Ancestor `source_id`s in this chain. Empty for an entry-point
    // activation; length = current depth.
    pub chain_ids:   &'a [String],
}

impl<'a> Activation<'a> {
    // Convenience constructor for entry-point activations (depth 0).
    pub fn root(
        source_kind: &'static str,
        source_id:   &'a str,
        user_id:     &'a str,
        action:      &'a Action,
        payload:     Option<&'a serde_json::Value>,
    ) -> Self {
        Self { source_kind, source_id, user_id, action, payload, chain_ids: &[] }
    }

    // Current chain depth (0 = entry-point activation).
    pub fn depth(&self) -> u32 { self.chain_ids.len() as u32 }
}

pub struct DispatchOutcome {
    pub outcome:        RunOutcome,
    pub output_snippet: Option<String>,
    pub error:          Option<String>,
}

// ── Dispatcher ───────────────────────────────────────────────────────────────

// Owns references to every subsystem an action might touch. Optional
// fields let tests stand up only what they need; production wiring in
// `gateway::builder` populates them all.
pub struct Dispatcher {
    pub heartbeats:    Arc<HeartbeatRegistry>,
    pub ctx:           Arc<HeartbeatContext>,
    pub store:         Arc<AutomationsStore>,
    // Required by `Action::Prompt` and `Action::ToolCall`. `None` in tests
    // that exercise only the time-driven half.
    pub agent:         Option<Arc<AgentCore>>,
    // Required by `Action::Prompt` (find/create/persist conversation) and
    // `Action::ChannelMessage` for `channel="web"`.
    pub history:       Option<Arc<HistoryStore>>,
    // Required by `Action::ChannelMessage` for `channel="web"` so the open
    // browser tab refreshes when the schedule fires.
    pub notifications: Option<Arc<NotificationBus>>,
    // Maximum chain depth. 0 disables the check entirely.
    // Production wiring reads this from `AutomationsConfig.max_chain_depth`;
    // tests construct dispatchers directly and may set 0 to bypass.
    pub max_chain_depth: u32,
    // Per-user, per-channel sliding-window cap on `channel_message`
    // dispatches. `None` disables limiting (used by tests that don't
    // care about throttle behavior).
    pub rate_limiter: Option<Arc<ChannelRateLimiter>>,
    // Auth service used by the outbound bridge to look up a user's
    // per-channel address (e.g. their Signal phone) when a `Prompt`
    // or `ChannelMessage` action targets `signal`/`telegram`. `None`
    // disables outbound delivery — the message still lives in history,
    // it just doesn't get pushed to the channel.
    pub auth: Option<Arc<LocalAuthService>>,
    // signal-cli REST port (matches `config.channels.signal.rest_port`).
    // Required alongside `signal_bot_number` for outbound Signal pushes.
    pub signal_port: Option<u16>,
    // MIRA's own Signal phone number (the one signal-cli is registered
    // with; matches `config.channels.signal.phone_number`). Acts as the
    // `from`-address; `None` disables outbound Signal delivery.
    pub signal_bot_number: Option<String>,
    // Per-user channel-account store. Outbound Telegram delivery
    // (`Action::Prompt`/`ChannelMessage` with channel=telegram) reads
    // the recipient's bot_token from here. `None` keeps the
    // history-only fallback so scheduled telegram prompts still
    // persist their generated text even when the bridge isn't wired.
    pub channel_accounts: Option<Arc<ChannelAccountStore>>,
    // E2 — per-user email account store + shared reply-loop cache.
    // Used by the `"email"` arm of `deliver_outbound` for SMTP
    // sends; `None` keeps that arm on the warn-and-noop fallback.
    pub email_accounts:   Option<Arc<crate::email::EmailAccountStore>>,
    pub email_loop_cache: Option<Arc<crate::email::ReplyLoopCache>>,
    // TTS service used to synthesise voice notes when the recipient's
    // voice policy is `Always` (or `OnVoiceInput` and the trigger had
    // voice input — automations never do, so the latter falls back to
    // text). `None` disables voice replies; the message still goes out
    // as text.
    pub tts: Option<TtsService>,
    // Honoured by the `"telegram"` arm of `deliver_outbound` to gate
    // outbound on the MIRA-wide `channels.telegram.enabled` kill
    // switch. Without this, scheduled telegram dispatches would still
    // push out via per-account bot tokens after the operator flipped
    // the global toggle off.
    pub live_config: Option<Arc<crate::web::LiveConfig>>,
}

impl Dispatcher {
    // Run the action and write a row to `automation_runs`. Never panics —
    // handler errors are caught and recorded so the worker stays alive.
    pub async fn dispatch(&self, act: Activation<'_>) -> DispatchOutcome {
        let started_at = Utc::now().timestamp();

        // ── chain-depth + cycle gate ────────────────────────────
        if self.max_chain_depth > 0 && act.depth() >= self.max_chain_depth {
            let err = format!(
                "chain depth {} exceeds max {} (chain={:?})",
                act.depth(), self.max_chain_depth, act.chain_ids,
            );
            warn!("automations: refusing activation: {err}");
            if let Err(e) = self.store.record_run(
                act.source_kind, act.source_id, act.user_id,
                started_at, Some(started_at),
                RunOutcome::Failure, None, Some(&err), None,
            ) {
                error!("automations: record_run(chain-depth) failed: {e}");
            }
            return DispatchOutcome {
                outcome: RunOutcome::Failure,
                output_snippet: None,
                error: Some(err),
            };
        }
        if act.chain_ids.iter().any(|id| id == act.source_id) {
            let err = format!(
                "cycle detected: {} already in chain {:?}",
                act.source_id, act.chain_ids,
            );
            warn!("automations: refusing activation: {err}");
            if let Err(e) = self.store.record_run(
                act.source_kind, act.source_id, act.user_id,
                started_at, Some(started_at),
                RunOutcome::Failure, None, Some(&err), None,
            ) {
                error!("automations: record_run(cycle) failed: {e}");
            }
            return DispatchOutcome {
                outcome: RunOutcome::Failure,
                output_snippet: None,
                error: Some(err),
            };
        }

        // Per-action context payload for templates. Webhook + event
        // activations pass their parsed body via `act.payload`; schedules
        // pass `None`, so `{{payload.…}}` resolves to empty.
        let payload_value = act.payload.cloned().unwrap_or_else(|| json!({}));
        let tpl_ctx = json!({
            "payload": payload_value,
            "now":     started_at,
            "source":  { "kind": act.source_kind, "id": act.source_id },
            "user":    { "id":   act.user_id },
        });

        // One-line audit log per activation so a tail of the running daemon
        // shows exactly what the dispatcher is doing without trawling the
        // /api/automations/runs view. Especially useful for ChannelMessage
        // and HttpPost which otherwise have very quiet happy paths.
        let action_tag = match act.action {
            Action::Internal { task, .. }       => format!("internal({task})"),
            Action::Prompt(p)                   => format!("prompt(channel={}, strategy={:?})", p.channel, p.conversation_strategy),
            Action::ToolCall { tool, .. }       => format!("tool_call({tool})"),
            Action::HttpPost { url, .. }        => format!("http_post({url})"),
            Action::ChannelMessage { channel, .. } => format!("channel_message({channel})"),
        };
        info!(
            "automations: dispatching action={} source={} id={} user={}",
            action_tag, act.source_kind, act.source_id, act.user_id,
        );

        let result = match act.action {
            Action::Internal { task, args } => {
                self.run_internal(task, args).await
            }
            Action::Prompt(p) => {
                self.run_prompt(act.user_id, p).await
            }
            Action::ToolCall { tool, args } => {
                self.run_tool_call(act.user_id, tool, args).await
            }
            Action::HttpPost { url, headers, body_template, timeout_secs, secret, max_retries } => {
                self.run_http_post(
                    url, headers, body_template, *timeout_secs,
                    secret.as_deref(), *max_retries, &tpl_ctx,
                ).await
            }
            Action::ChannelMessage { channel, to, conversation_id, text_template } => {
                self.run_channel_message(
                    act.user_id, channel, to.as_deref(),
                    conversation_id.as_deref(), text_template, &tpl_ctx,
                ).await
            }
        };

        let finished_at = Utc::now().timestamp();
        let outcome = match &result {
            Ok(_)  => RunOutcome::Success,
            Err(_) => RunOutcome::Failure,
        };
        let (snippet, err) = match &result {
            Ok(s)  => (Some(s.clone()), None),
            Err(e) => (None, Some(e.to_string())),
        };

        // Surface action-level failures at warn so they're visible in the
        // running tail without having to GET /api/automations/runs. Until
        // this was here, a schedule whose action errored (bad
        // conversation_id, AgentCore not wired, etc.) only logged
        // `claimed N due schedule(s)` and then went silent — making
        // remote debugging impossible without DB access.
        if let Some(e) = err.as_deref() {
            warn!(
                "automations: action failed (source={} id={} user={}): {}",
                act.source_kind, act.source_id, act.user_id, e
            );
        }

        if let Err(e) = self.store.record_run(
            act.source_kind, act.source_id, act.user_id,
            started_at, Some(finished_at),
            outcome, snippet.as_deref(), err.as_deref(),
            None,
        ) {
            error!("automations: record_run failed: {e}");
        }

        DispatchOutcome {
            outcome,
            output_snippet: snippet,
            error: err,
        }
    }

    // ── Internal ───────────────────────────────────────────────

    async fn run_internal(
        &self,
        task: &str,
        args: &serde_json::Value,
    ) -> Result<String, MiraError> {
        let handler = self.heartbeats.get(task)
            .ok_or_else(|| MiraError::ConfigError(
                format!("unknown internal task: {task}")
            ))?;
        debug!("automations: running internal task {task}");
        let out = handler.run(&self.ctx, args).await?;
        Ok(out.summary)
    }

    // ── Prompt ──────────────────────────────────────────────────────────

    async fn run_prompt(
        &self,
        user_id: &str,
        p:       &PromptAction,
    ) -> Result<String, MiraError> {
        let agent = self.agent.as_ref().ok_or_else(|| MiraError::ConfigError(
            "prompt action requires AgentCore (not wired in this build)".into()
        ))?;
        let history = self.history.as_ref().ok_or_else(|| MiraError::ConfigError(
            "prompt action requires HistoryStore".into()
        ))?;

        let conv_id = self.resolve_conversation(user_id, &p.channel, p, history).await?;

        // The prompt is a *turn instruction* the agent acts on, not user
        // content the human typed — so it does NOT get persisted into the
        // conversation log. Earlier behavior persisted it as a `User`
        // message, which made every fire look like the user had texted
        // themselves and the agent had replied to that ghost message.
        // The agent still receives the prompt verbatim via `agent.process`
        // (so it knows what to do this turn) and the in-memory session
        // store keeps it for context across consecutive fires.
        debug!(
            "automations: prompt action firing in conv {conv_id} \
             (prompt persisted to session only, not history)"
        );

        // Wrap the prompt with framing that makes the role unambiguous.
        // `agent.process` injects `input` as a user-role chat message, so a
        // bare prompt like "Why did the chicken cross the road?" reads to
        // the model as the user *telling it a joke* — and the model replies
        // "ha, classic!" instead of delivering a fresh joke. The framing
        // below tells the model this is a scheduled task fired by the
        // automations subsystem and to act on it, not respond to it.
        let framed_input = format!(
            "[Scheduled automation triggered. The user set this task ahead \
             of time and the scheduler is delivering it now. Execute the \
             task below and address the user directly with the result. Do \
             NOT treat the task text as a message the user just sent — it \
             is an instruction for you to carry out.]\n\n\
             Task: {}",
            p.prompt
        );

        // Restrict the tool palette for this turn:
        // - If the action specifies `tools_allowed`, honor it as the
        //   whitelist (caller knows exactly what the task needs).
        // - Otherwise fall back to `list_for_flow("chat")`, which is the
        //   same User+Admin tier filter the normal chat path uses. This
        //   drops `record_profile`, `skip_topic`, `mark_group_complete`,
        //   `complete_onboarding`, and `resolve_timezone` (all marked
        //   `ToolVisibility::system("onboarding")`) so a scheduled "tell
        //   me a joke" turn isn't shipping a 7K-token tool registry that
        //   includes onboarding-only schemas the model can't usefully
        //   call here.
        let allowed = p.tools_allowed
            .clone()
            .unwrap_or_else(|| agent.tools.list_for_flow("chat"));
        let turn_ctx = TurnContext {
            allowed_tool_names: Some(allowed),
            ..TurnContext::default()
        };

        // Fire the agent loop. The receiver streams Token/ToolCall/Done; we
        // only need the final assistant text to write back and for the run
        // snippet, so collect tokens until Done/Error.
        let mut rx = agent
            .process_with_context(
                &conv_id, user_id, &p.channel, &framed_input, None, turn_ctx,
            )
            .await
            .map_err(|e| MiraError::ConfigError(format!("agent.process: {e}")))?;

        let assistant_text = drain_to_text(&mut rx).await?;

        if let Err(e) = history.add_message(NewMessage {
            conversation_id: conv_id.clone(),
            role:            MessageRole::Assistant,
            content:         assistant_text.clone(),
            content_type:    "text".to_owned(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata:        Some(json!({"automation": true}).to_string()),
        }) {
            warn!("automations: persist assistant message failed: {e}");
        }
        let _ = history.touch_conversation(&conv_id);

        // Nudge the open web UI so the new message renders without a refresh.
        if let Some(bus) = self.notifications.as_ref() {
            bus.send(Notification {
                kind:            NotificationKind::ConversationUpdated,
                conversation_id: Some(conv_id.clone()),
                channel:         Some(p.channel.clone()),
                user_id:         Some(user_id.to_string()),
                message:         Some(snippet(&assistant_text)),
            });
        }

        // Outbound bridge — for non-web channels, also push the assistant
        // text to the user via the channel's transport so the message
        // actually arrives on their phone, not just into the web history.
        if let Err(e) = self.deliver_outbound(&p.channel, user_id, None, &assistant_text).await {
            warn!(
                "automations: outbound delivery to {} failed for user {}: {e}",
                p.channel, user_id,
            );
        }

        Ok(snippet(&assistant_text))
    }

    // Push `text` to `user_id` over `channel`. `to_override` lets a
    // `ChannelMessage` action send to an explicit recipient instead of
    // the schedule owner's registered address (`Action::Prompt` always
    // passes `None`). `web` is a no-op (the message is already in
    // history + NotificationBus). `signal` looks up the user's phone
    // (or uses `to_override`) and pushes via signal-cli. Telegram and
    // email warn-and-noop until their per-user lookups are wired.
    async fn deliver_outbound(
        &self,
        channel:     &str,
        user_id:     &str,
        to_override: Option<&str>,
        text:        &str,
    ) -> Result<(), MiraError> {
        match channel {
            // Web: nothing to push — the assistant message is already in
            // history and the NotificationBus event has woken any open tab.
            "web" => Ok(()),

            "signal" => {
                // Warn-and-noop when the dispatcher wasn't built with
                // signal config (tests, server runs with Signal disabled).
                // Returning Ok keeps the schedule's run row green — the
                // assistant text is in history regardless, and the warn
                // log makes the missing config visible to operators.
                let (Some(port), Some(bot_number)) = (
                    self.signal_port,
                    self.signal_bot_number.as_ref(),
                ) else {
                    warn!(
                        "automations: signal delivery skipped — \
                         signal_port/signal_bot_number not configured \
                         (message saved to history but not pushed)"
                    );
                    return Ok(());
                };

                // Look up the recipient phone + the recipient's voice
                // prefs in one auth round-trip so we don't double-fetch.
                // `to_override` (set by `ChannelMessage{to: …}`) skips the
                // user lookup entirely — the override is sent as plain
                // text since we don't know whose voice prefs to apply.
                let (phone, voice_prefs_json) = match to_override {
                    Some(p) => (p.to_string(), None),
                    None => {
                        let auth = self.auth.as_ref().ok_or_else(|| MiraError::ConfigError(
                            "signal delivery requires auth service when no `to` is set".into()
                        ))?;
                        let user = auth.get_user(user_id)
                            .map_err(|e| MiraError::ConfigError(format!("get_user: {e}")))?
                            .ok_or_else(|| MiraError::ConfigError(format!("user {user_id} not found")))?;
                        let phone = user.phone.ok_or_else(|| MiraError::ConfigError(format!(
                            "user {user_id} has no phone — set users.phone to enable Signal delivery"
                        )))?;
                        (phone, user.voice_prefs)
                    }
                };

                // Resolve voice policy the same way the SSE listener does
                // for inbound replies: per-user prefs over server defaults
                // over the built-in Never fallback. Automations have no
                // inbound message, so `OnVoiceInput` collapses to "no
                // voice" — only `Always` triggers TTS here.
                let want_voice = if to_override.is_some() {
                    false
                } else {
                    let server_defaults = self.tts.as_ref()
                        .map(|t| t.voice_prefs_defaults())
                        .unwrap_or_default();
                    let user_prefs = parse_user_prefs(voice_prefs_json.as_deref());
                    let resolved = resolve_voice("signal", Some(&user_prefs), &server_defaults);
                    let policy = resolved.policy;
                    let voice_id = resolved.voice_id.clone();
                    debug!(
                        "automations: signal reply policy: {} → want_voice={} (voice_id={:?})",
                        policy.as_str(),
                        matches!(policy, ResponsePolicy::Always),
                        voice_id,
                    );
                    matches!(policy, ResponsePolicy::Always)
                };

                let client = SignalCliClient::new(port, bot_number.clone());

                // Voice attempt — synth + send_with_attachments. Any
                // failure (TTS unavailable, transcode error, RPC error)
                // falls through to plain text below so the user still
                // gets the message.
                if want_voice {
                    let voice_id = self.tts.as_ref().and_then(|t| {
                        let server_defaults = t.voice_prefs_defaults();
                        let user_prefs = parse_user_prefs(voice_prefs_json.as_deref());
                        resolve_voice("signal", Some(&user_prefs), &server_defaults).voice_id
                    });
                    if let Some(buf) = synth_signal_voice(
                        self.tts.as_ref(),
                        text,
                        voice_id.as_deref(),
                    ).await {
                        match write_voice_tempfile(&buf.bytes) {
                            Ok(tmp) => {
                                let path = tmp.path().to_string_lossy().to_string();
                                match client.send_with_attachments(
                                    vec![phone.clone()], text, &[path],
                                ).await {
                                    Ok(()) => {
                                        info!(
                                            "automations: pushed voice reply to signal:{phone} for user {user_id}"
                                        );
                                        return Ok(());
                                    }
                                    Err(e) => warn!(
                                        "automations: signal send_with_attachments to {phone} \
                                         failed: {e} — falling back to text"
                                    ),
                                }
                            }
                            Err(e) => warn!(
                                "automations: signal voice tempfile write failed: {e} — falling back to text"
                            ),
                        }
                    }
                }

                // Plain text (either voice off, or voice attempt failed).
                client.send(vec![phone.clone()], text).await
                    .map_err(|e| MiraError::ConfigError(format!("signal send: {e}")))?;
                info!("automations: pushed assistant reply to signal:{phone} for user {user_id}");
                Ok(())
            }

            "telegram" => {
                // MIRA-wide kill switch (Settings → Channels →
                // Telegram). Short-circuit before any account lookup so
                // a disabled global toggle is honoured even when the
                // recipient has per-account rows wired up.
                if let Some(cfg) = &self.live_config {
                    if !cfg.get().await.channels.telegram.enabled {
                        return Err(MiraError::ConfigError(
                            "telegram is disabled globally \
                             (Settings → Channels → Telegram)".into(),
                        ));
                    }
                }
                // bot_token comes from the user's enabled
                // channel_accounts row; chat_id is parsed from
                // `to_override` if set (admin-supplied recipient),
                // otherwise from the user's most-recent telegram
                // conversation's external_user_id (== tg user id for
                // 1-on-1 chats).
                let Some(accounts) = self.channel_accounts.as_ref() else {
                    warn!(
                        "automations: telegram delivery skipped — channel_accounts \
                         store not wired (message saved to history but not pushed)"
                    );
                    return Ok(());
                };
                // Personal bot first; fall back to a shared admin-managed
                // bot (R1+R2) so a linked user with no bot of their own
                // still gets scheduled/triggered telegram dispatches.
                let bot_token = accounts.outbound_telegram_token(user_id)
                    .map_err(|e| MiraError::ConfigError(format!("resolve telegram token: {e}")))?
                    .ok_or_else(|| MiraError::ConfigError(format!(
                        "user {user_id} has no enabled telegram account and no \
                         shared telegram bot is configured"
                    )))?;
                let chat_id: i64 = match to_override {
                    Some(s) => s.parse().map_err(|e| MiraError::ConfigError(
                        format!("to_override chat_id parse: {e}")
                    ))?,
                    None => {
                        let history = self.history.as_ref().ok_or_else(|| MiraError::ConfigError(
                            "history required to resolve telegram chat_id".into()
                        ))?;
                        // Filter NULL external_user_id conversations
                        // (bot-created threads from automations'
                        // `find_or_create` side-effects) before
                        // picking — same fix as
                        // companion::dispatcher::deliver_telegram, see
                        // that comment for the failure mode.
                        history.list_conversations(user_id, Some("telegram"), 50, 0)
                            .map_err(|e| MiraError::ConfigError(
                                format!("list_conversations: {e}")
                            ))?
                            .into_iter()
                            .filter_map(|c| c.external_user_id)
                            .find(|s| !s.trim().is_empty())
                            .ok_or_else(|| MiraError::ConfigError(format!(
                                "user {user_id} has no prior telegram conversation \
                                 with a known chat id — they must message the bot \
                                 once from their phone first"
                            )))?
                            .parse()
                            .map_err(|e| MiraError::ConfigError(
                                format!("chat_id parse: {e}")
                            ))?
                    }
                };
                let tg = TelegramChannel::new(bot_token);

                // Voice note when the recipient's telegram voice preference
                // is "Always" — same policy resolution as the signal arm
                // above and the companion dispatcher (per-user prefs layered
                // over server defaults). Automations have no inbound audio so
                // OnVoiceInput collapses to text; only Always opts in. Admin-
                // supplied recipients (to_override) get text since we can't
                // assume their prefs. Falls back to text on any synth/send
                // failure so the message still lands.
                if to_override.is_none() {
                    if let Some(tts) = self.tts.as_ref() {
                        let server_defaults = tts.voice_prefs_defaults();
                        let user_prefs = self.auth.as_ref()
                            .and_then(|a| a.get_user(user_id).ok().flatten())
                            .map(|u| parse_user_prefs(u.voice_prefs.as_deref()))
                            .unwrap_or_default();
                        let resolved = resolve_voice("telegram", Some(&user_prefs), &server_defaults);
                        if matches!(resolved.policy, ResponsePolicy::Always) {
                            if let Some(buf) = crate::server::handlers::telegram::synth_voice_for_channel(
                                Some(tts), "telegram", text, resolved.voice_id.as_deref(),
                            ).await {
                                match tg.send_voice_to_chat(chat_id, &buf.bytes, text).await {
                                    Ok(()) => {
                                        info!("automations: pushed voice reply to telegram:{chat_id} for user {user_id}");
                                        return Ok(());
                                    }
                                    Err(e) => warn!(
                                        "automations: telegram voice send failed ({e}) — falling back to text"
                                    ),
                                }
                            }
                        }
                    }
                }

                tg.send_to_chat(chat_id, text).await
                    .map_err(|e| MiraError::ConfigError(format!("telegram send: {e}")))?;
                info!("automations: pushed assistant reply to telegram:{chat_id} for user {user_id}");
                Ok(())
            }

            "discord" => {
                // D3 — scheduled/triggered Discord dispatch. Mirrors the
                // telegram arm: global kill switch, personal-first token
                // with shared-bot fallback, channel id from `to_override`
                // or the recipient's most-recent inbound thread. Discord
                // stores `external_user_id = channel_id` (postable
                // directly), so no parse + no DM-open round-trip. Text-only
                // (Discord has no proactive voice path yet).
                if let Some(cfg) = &self.live_config {
                    if !cfg.get().await.channels.discord.enabled {
                        return Err(MiraError::ConfigError(
                            "discord is disabled globally \
                             (Settings → Channels → Discord)".into(),
                        ));
                    }
                }
                let Some(accounts) = self.channel_accounts.as_ref() else {
                    warn!(
                        "automations: discord delivery skipped — channel_accounts \
                         store not wired (message saved to history but not pushed)"
                    );
                    return Ok(());
                };
                let bot_token = accounts.outbound_discord_token(user_id)
                    .map_err(|e| MiraError::ConfigError(format!("resolve discord token: {e}")))?
                    .ok_or_else(|| MiraError::ConfigError(format!(
                        "user {user_id} has no enabled discord account and no \
                         shared discord bot is configured"
                    )))?;
                let channel_id: String = match to_override {
                    Some(s) => s.to_string(),
                    None => {
                        let history = self.history.as_ref().ok_or_else(|| MiraError::ConfigError(
                            "history required to resolve discord channel id".into()
                        ))?;
                        history.list_conversations(user_id, Some("discord"), 50, 0)
                            .map_err(|e| MiraError::ConfigError(
                                format!("list_conversations: {e}")
                            ))?
                            .into_iter()
                            .filter_map(|c| c.external_user_id)
                            .find(|s| !s.trim().is_empty())
                            .ok_or_else(|| MiraError::ConfigError(format!(
                                "user {user_id} has no prior discord conversation \
                                 with a known channel id — they must message the \
                                 bot once first"
                            )))?
                    }
                };
                let client = reqwest::Client::new();
                crate::discord::api::post_message(&client, &bot_token, &channel_id, text)
                    .await
                    .map_err(|e| MiraError::ConfigError(format!("discord send: {e}")))?;
                info!("automations: pushed assistant reply to discord:{channel_id} for user {user_id}");
                Ok(())
            }

            "matrix" => {
                // Scheduled/triggered Matrix dispatch. Same shape as the
                // discord arm: kill switch, personal-first creds with
                // shared-bot fallback, room id from `to_override` or the
                // recipient's most-recent inbound thread (Matrix stores
                // external_user_id = room_id, directly postable). Text-only.
                if let Some(cfg) = &self.live_config {
                    if !cfg.get().await.channels.matrix.enabled {
                        return Err(MiraError::ConfigError(
                            "matrix is disabled globally \
                             (Settings → Channels → Matrix)".into(),
                        ));
                    }
                }
                let Some(accounts) = self.channel_accounts.as_ref() else {
                    warn!(
                        "automations: matrix delivery skipped — channel_accounts \
                         store not wired (message saved to history but not pushed)"
                    );
                    return Ok(());
                };
                let (homeserver, token) = accounts.outbound_matrix_creds(user_id)
                    .map_err(|e| MiraError::ConfigError(format!("resolve matrix creds: {e}")))?
                    .ok_or_else(|| MiraError::ConfigError(format!(
                        "user {user_id} has no enabled matrix account and no \
                         shared matrix bot is configured"
                    )))?;
                let room_id: String = match to_override {
                    Some(s) => s.to_string(),
                    None => {
                        let history = self.history.as_ref().ok_or_else(|| MiraError::ConfigError(
                            "history required to resolve matrix room id".into()
                        ))?;
                        history.list_conversations(user_id, Some("matrix"), 50, 0)
                            .map_err(|e| MiraError::ConfigError(
                                format!("list_conversations: {e}")
                            ))?
                            .into_iter()
                            .filter_map(|c| c.external_user_id)
                            .find(|s| !s.trim().is_empty())
                            .ok_or_else(|| MiraError::ConfigError(format!(
                                "user {user_id} has no prior matrix conversation \
                                 with a known room id — they must message the \
                                 bot once first"
                            )))?
                    }
                };
                let client = reqwest::Client::new();
                crate::matrix::api::send_message(&client, &homeserver, &token, &room_id, text, 0)
                    .await
                    .map_err(|e| MiraError::ConfigError(format!("matrix send: {e}")))?;
                info!("automations: pushed assistant reply to matrix:{room_id} for user {user_id}");
                Ok(())
            }

            "whatsapp" => {
                // Scheduled/triggered WhatsApp dispatch. Same shape as the
                // other arms. NOTE the 24h window — a free-form send to a
                // user who hasn't messaged in 24h is rejected by Meta
                // (131047); template messages aren't implemented yet.
                if let Some(cfg) = &self.live_config {
                    if !cfg.get().await.channels.whatsapp.enabled {
                        return Err(MiraError::ConfigError(
                            "whatsapp is disabled globally \
                             (Settings → Channels → WhatsApp)".into(),
                        ));
                    }
                }
                let Some(accounts) = self.channel_accounts.as_ref() else {
                    warn!(
                        "automations: whatsapp delivery skipped — channel_accounts \
                         store not wired (message saved to history but not pushed)"
                    );
                    return Ok(());
                };
                let (phone_number_id, token) = accounts.outbound_whatsapp_creds(user_id)
                    .map_err(|e| MiraError::ConfigError(format!("resolve whatsapp creds: {e}")))?
                    .ok_or_else(|| MiraError::ConfigError(format!(
                        "user {user_id} has no enabled whatsapp account and no \
                         shared whatsapp bot is configured"
                    )))?;
                let to: String = match to_override {
                    Some(s) => s.to_string(),
                    None => {
                        let history = self.history.as_ref().ok_or_else(|| MiraError::ConfigError(
                            "history required to resolve whatsapp recipient".into()
                        ))?;
                        history.list_conversations(user_id, Some("whatsapp"), 50, 0)
                            .map_err(|e| MiraError::ConfigError(
                                format!("list_conversations: {e}")
                            ))?
                            .into_iter()
                            .filter_map(|c| c.external_user_id)
                            .find(|s| !s.trim().is_empty())
                            .ok_or_else(|| MiraError::ConfigError(format!(
                                "user {user_id} has no prior whatsapp conversation \
                                 with a known phone — they must message the bot \
                                 once first"
                            )))?
                    }
                };
                let client = reqwest::Client::new();
                crate::whatsapp::api::send_text(&client, &phone_number_id, &token, &to, text)
                    .await
                    .map_err(|e| MiraError::ConfigError(format!("whatsapp send: {e}")))?;
                info!("automations: pushed assistant reply to whatsapp:{to} for user {user_id}");
                Ok(())
            }

            "slack" => {
                // Scheduled/triggered Slack dispatch. Same shape as the
                // other webhook arms. No 24h-window restriction.
                if let Some(cfg) = &self.live_config {
                    if !cfg.get().await.channels.slack.enabled {
                        return Err(MiraError::ConfigError(
                            "slack is disabled globally \
                             (Settings → Channels → Slack)".into(),
                        ));
                    }
                }
                let Some(accounts) = self.channel_accounts.as_ref() else {
                    warn!(
                        "automations: slack delivery skipped — channel_accounts \
                         store not wired (message saved to history but not pushed)"
                    );
                    return Ok(());
                };
                let bot_token = accounts.outbound_slack_token(user_id)
                    .map_err(|e| MiraError::ConfigError(format!("resolve slack token: {e}")))?
                    .ok_or_else(|| MiraError::ConfigError(format!(
                        "user {user_id} has no enabled slack account and no \
                         shared slack bot is configured"
                    )))?;
                let channel: String = match to_override {
                    Some(s) => s.to_string(),
                    None => {
                        let history = self.history.as_ref().ok_or_else(|| MiraError::ConfigError(
                            "history required to resolve slack channel".into()
                        ))?;
                        history.list_conversations(user_id, Some("slack"), 50, 0)
                            .map_err(|e| MiraError::ConfigError(
                                format!("list_conversations: {e}")
                            ))?
                            .into_iter()
                            .filter_map(|c| c.external_user_id)
                            .find(|s| !s.trim().is_empty())
                            .ok_or_else(|| MiraError::ConfigError(format!(
                                "user {user_id} has no prior slack conversation \
                                 with a known channel — they must message the \
                                 bot once first"
                            )))?
                    }
                };
                let client = reqwest::Client::new();
                crate::slack::api::post_message(&client, &bot_token, &channel, text)
                    .await
                    .map_err(|e| MiraError::ConfigError(format!("slack send: {e}")))?;
                info!("automations: pushed assistant reply to slack:{channel} for user {user_id}");
                Ok(())
            }

            "tui" => {
                warn!(
                    "automations: outbound delivery for channel=tui not yet wired \
                     (message saved to history but not pushed to user)"
                );
                Ok(())
            }

            "email" => {
                // E2 — automations push via SMTP. `to_override` is
                // the explicit recipient (admin-supplied or the
                // schedule's `to` field); without it we resolve the
                // bot owner's `users.email` (matches the companion
                // pattern). The "from" is the user's first enabled
                // email account.
                let Some(accounts) = self.email_accounts.as_ref() else {
                    warn!("automations: email delivery skipped — email_accounts store not wired");
                    return Ok(());
                };
                let loop_cache = self.email_loop_cache.as_ref().ok_or_else(|| MiraError::ConfigError(
                    "email reply-loop cache not wired".into()
                ))?;
                let account = accounts.list_for_user(user_id)
                    .map_err(|e| MiraError::ConfigError(format!("list email accounts: {e}")))?
                    .into_iter()
                    .find(|a| a.enabled)
                    .ok_or_else(|| MiraError::ConfigError(format!(
                        "user {user_id} has no enabled email account"
                    )))?;
                let to: String = match to_override {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => {
                        let auth = self.auth.as_ref().ok_or_else(|| MiraError::ConfigError(
                            "auth service required to resolve user.email".into()
                        ))?;
                        let user = auth.get_user(user_id)
                            .map_err(|e| MiraError::ConfigError(format!("get_user: {e}")))?
                            .ok_or_else(|| MiraError::ConfigError(format!("user {user_id} not found")))?;
                        user.email.ok_or_else(|| MiraError::ConfigError(format!(
                            "user {user_id} has no email — set users.email or supply a `to` on the schedule"
                        )))?
                    }
                };
                let subject = "Notification from MIRA";
                let msg = crate::email::OutboundMessage {
                    to:          &to,
                    subject,
                    body:        text,
                    in_reply_to: None,
                    references:  &[],
                };
                let live = match self.live_config.as_ref() {
                    Some(lc) => lc.get().await,
                    None => return Err(MiraError::ConfigError(
                        "automations email send needs live_config (for OAuth refresh)".into()
                    )),
                };
                crate::email::smtp_send_for_account(
                    &account, msg, loop_cache.as_ref(), accounts.as_ref(), &live.email_oauth,
                ).await
                    .map_err(|e| MiraError::ConfigError(format!("email send: {e}")))?;
                info!("automations: pushed assistant reply to email:{to} for user {user_id}");
                Ok(())
            }

            // CPP plugin channels: `external:<provider_kind>`. Resolve the
            // account's send_url + outbound_secret (personal-first, shared
            // fallback) and POST a signed CPP outbound to the provider.
            ext if ext.starts_with("external:") => {
                if let Some(cfg) = &self.live_config {
                    if !cfg.get().await.channels.external.enabled {
                        return Err(MiraError::ConfigError(
                            "external channels are disabled globally \
                             (Settings → Channels)".into(),
                        ));
                    }
                }
                let Some(accounts) = self.channel_accounts.as_ref() else {
                    warn!(
                        "automations: external delivery skipped — channel_accounts \
                         store not wired (message saved to history but not pushed)"
                    );
                    return Ok(());
                };
                let (account_id, send_url, secret, supports_voice) = accounts.outbound_external_creds(user_id)
                    .map_err(|e| MiraError::ConfigError(format!("resolve external creds: {e}")))?
                    .ok_or_else(|| MiraError::ConfigError(format!(
                        "user {user_id} has no enabled external account and no \
                         shared external provider is configured"
                    )))?;
                let conversation_id: String = match to_override {
                    Some(s) => s.to_string(),
                    None => {
                        let history = self.history.as_ref().ok_or_else(|| MiraError::ConfigError(
                            "history required to resolve external conversation id".into()
                        ))?;
                        history.list_conversations(user_id, Some(ext), 50, 0)
                            .map_err(|e| MiraError::ConfigError(
                                format!("list_conversations: {e}")
                            ))?
                            .into_iter()
                            .filter_map(|c| c.external_user_id)
                            .find(|s| !s.trim().is_empty())
                            .ok_or_else(|| MiraError::ConfigError(format!(
                                "user {user_id} has no prior {ext} conversation \
                                 with a known id — they must message the bot \
                                 once first"
                            )))?
                    }
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
                let client = reqwest::Client::new();
                // Voice when the provider declares it can play audio *and* the
                // owner's per-channel policy for this external:<kind> channel
                // is "Always". Mirrors the telegram arm above. Admin-supplied
                // recipients (to_override) get text — we can't assume their
                // prefs. Falls back to text on any synth/send failure.
                if supports_voice && to_override.is_none() {
                    if let Some(tts) = self.tts.as_ref() {
                        let server_defaults = tts.voice_prefs_defaults();
                        let user_prefs = self.auth.as_ref()
                            .and_then(|a| a.get_user(user_id).ok().flatten())
                            .map(|u| parse_user_prefs(u.voice_prefs.as_deref()))
                            .unwrap_or_default();
                        let resolved = resolve_voice(ext, Some(&user_prefs), &server_defaults);
                        if matches!(resolved.policy, ResponsePolicy::Always) {
                            if let Some(buf) = crate::server::handlers::telegram::synth_voice_for_channel(
                                Some(tts), ext, text, resolved.voice_id.as_deref(),
                            ).await {
                                use base64::Engine;
                                let audio = crate::external::types::OutboundAudio {
                                    content_type: buf.codec.content_type().to_string(),
                                    extension:    buf.codec.extension().to_string(),
                                    data_base64:  base64::engine::general_purpose::STANDARD.encode(&buf.bytes),
                                };
                                match crate::external::api::send_message_with_audio(
                                    &client, &send_url, &secret, &account_id, &conversation_id, text, audio, now,
                                ).await {
                                    Ok(()) => {
                                        info!("automations: pushed voice reply to {ext}:{conversation_id} for user {user_id}");
                                        return Ok(());
                                    }
                                    Err(e) => warn!(
                                        "automations: external voice send failed ({e}) — falling back to text"
                                    ),
                                }
                            }
                        }
                    }
                }
                crate::external::api::send_message(
                    &client, &send_url, &secret, &account_id, &conversation_id, text, now,
                ).await.map_err(|e| MiraError::ConfigError(format!("external send: {e}")))?;
                info!("automations: pushed assistant reply to {ext}:{conversation_id} for user {user_id}");
                Ok(())
            }

            other => Err(MiraError::ConfigError(format!(
                "unknown channel {other} — cannot deliver outbound"
            ))),
        }
    }

    // Resolve `PromptAction.conversation_strategy` to a conversation id.
    //     // - `Existing` requires `conversation_id` to already exist; an unknown
    // id is treated as an error rather than silently creating a new
    // thread, since "existing" semantically means "this exact thread".
    // - `New` always creates a fresh conversation. `conversation_name`
    // becomes the title if set.
    // - `Named` looks up by `conversation_name` for this user/channel; the
    // most recently updated match wins. None matches → create one.
    async fn resolve_conversation(
        &self,
        user_id: &str,
        channel: &str,
        p:       &PromptAction,
        history: &Arc<HistoryStore>,
    ) -> Result<String, MiraError> {
        match p.conversation_strategy {
            ConversationStrategy::Existing => {
                let id = p.conversation_id.as_deref().ok_or_else(|| {
                    MiraError::ConfigError(
                        "conversation_strategy=existing requires conversation_id".into()
                    )
                })?;
                let exists = history.get_conversation(id)
                    .map_err(|e| MiraError::ConfigError(format!("history lookup: {e}")))?
                    .is_some();
                if !exists {
                    return Err(MiraError::ConfigError(format!(
                        "conversation {id} not found"
                    )));
                }
                Ok(id.to_string())
            }
            ConversationStrategy::New => {
                self.create_fresh_titled_conversation(user_id, channel, p, history)
            }
            ConversationStrategy::Named => {
                // `Named` keys a reusable thread by title. With no usable name
                // it can't key anything — the old code returned a hard error,
                // so the schedule failed on every fire and tripped the watchdog
                // (this is the failure you saw). Degrade gracefully instead:
                // behave like `New` (fresh, auto-titled thread).
                let name = p.conversation_name.as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let Some(name) = name else {
                    warn!(
                        "automations: strategy=named with no conversation_name \
                         (user {user_id}) — creating a fresh titled thread instead \
                         of failing the fire"
                    );
                    return self.create_fresh_titled_conversation(user_id, channel, p, history);
                };
                let mut convs = history.list_conversations(
                    user_id, Some(channel), 200, 0,
                ).map_err(|e| MiraError::ConfigError(format!("list_conversations: {e}")))?;
                convs.retain(|c| c.title.as_deref() == Some(name));
                if let Some(c) = convs.into_iter().next() {
                    return Ok(c.id);
                }
                let conv = history.create_conversation(NewConversation {
                    user_id:          user_id.to_string(),
                    channel:          channel.to_string(),
                    title:            Some(name.to_string()),
                    model:            None,
                    provider:         None,
                    external_user_id: None,
                    mode:             None,
                }).map_err(|e| MiraError::ConfigError(format!("create_conversation: {e}")))?;
                Ok(conv.id)
            }
        }
    }

    // Create a fresh conversation titled from the prompt: an immediate
    // heuristic title (so it never shows as "Untitled") plus a background
    // LLM upgrade — the same auto-title behaviour as the web chat handler,
    // which automation turns otherwise bypass.
    fn create_fresh_titled_conversation(
        &self,
        user_id: &str,
        channel: &str,
        p:       &PromptAction,
        history: &Arc<HistoryStore>,
    ) -> Result<String, MiraError> {
        let named = p.conversation_name.clone().filter(|s| !s.trim().is_empty());
        let heuristic = crate::server::handlers::chat::derive_title_from_message(&p.prompt);
        let title = named.clone().or_else(|| Some(heuristic.clone()));
        let conv = history.create_conversation(NewConversation {
            user_id:          user_id.to_string(),
            channel:          channel.to_string(),
            title,
            model:            None,
            provider:         None,
            external_user_id: None,
            mode:             None,
        }).map_err(|e| MiraError::ConfigError(format!("create_conversation: {e}")))?;

        if named.is_none() {
            if let Some(agent) = self.agent.as_ref() {
                let agent2 = Arc::clone(agent);
                let hist2  = Arc::clone(history);
                let cid    = conv.id.clone();
                let prompt = p.prompt.clone();
                tokio::spawn(async move {
                    crate::server::handlers::chat::generate_auto_title(
                        agent2, hist2, cid, prompt, heuristic,
                    ).await;
                });
            }
        }
        Ok(conv.id)
    }

    // ── ToolCall ────────────────────────────────────────────────────────

    async fn run_tool_call(
        &self,
        user_id: &str,
        tool:    &str,
        args:    &serde_json::Value,
    ) -> Result<String, MiraError> {
        let agent = self.agent.as_ref().ok_or_else(|| MiraError::ConfigError(
            "tool_call action requires AgentCore (not wired in this build)".into()
        ))?;

        // Inject `_user_id` so the tool's audit row records the right
        // actor (the chat path does this; we mirror it here).
        let mut merged = match args {
            serde_json::Value::Object(_) => args.clone(),
            _                            => serde_json::Value::Object(Default::default()),
        };
        if let serde_json::Value::Object(map) = &mut merged {
            map.insert("_user_id".into(), serde_json::Value::String(user_id.into()));
        }

        let result = agent.tools.execute(tool, merged).await?;
        if !result.success {
            return Err(MiraError::ToolError(
                result.error.unwrap_or_else(|| "tool failed".into())
            ));
        }
        Ok(snippet(&result.output))
    }

    // ── HttpPost ────────────────────────────────────────────────────────

    // POST `body_template` (rendered against `tpl_ctx`) to `url`, retrying
    // on transient failures. Behaviour:
    //     // - **Retries** (network errors, 5xx, 408, 429): exponential backoff
    // starting at 250ms, doubling each attempt, capped at 5s. Total
    // attempts = `max_retries + 1`.
    // - **Terminal** (other 4xx): never retried — the receiver is telling
    // us the request is wrong, so retrying just wastes their time.
    // - **HMAC**: when `secret` is `Some`, the rendered body is signed
    // with HMAC-SHA256 and sent as `X-Mira-Signature: sha256=<hex>`.
    // `X-Mira-Timestamp` carries the unix-seconds the request started,
    // so receivers can apply a replay window.
    // - **User-Agent**: `mira/<crate-version>` is always sent unless the
    // caller's headers already specify one.
    async fn run_http_post(
        &self,
        url:           &str,
        headers:       &std::collections::HashMap<String, String>,
        body_template: &str,
        timeout_secs:  u64,
        secret:        Option<&str>,
        max_retries:   u32,
        tpl_ctx:       &serde_json::Value,
    ) -> Result<String, MiraError> {
        let body = template::render(body_template, tpl_ctx);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .build()
            .map_err(|e| MiraError::ConfigError(format!("reqwest build: {e}")))?;

        // Caller-supplied User-Agent wins; otherwise we badge ourselves so
        // server logs make MIRA traffic recognisable.
        let has_user_agent = headers.keys().any(|k| k.eq_ignore_ascii_case("user-agent"));

        let signature = secret.map(|s| {
            crate::security::hmac::compute_hmac(s.as_bytes(), body.as_bytes())
        });
        let timestamp = Utc::now().timestamp().to_string();

        let attempts_total = max_retries.saturating_add(1);
        let mut last_err: String = "no attempts made".into();

        for attempt in 0..attempts_total {
            // Backoff before retry (skipped on first attempt).
            if attempt > 0 {
                let ms: u64 = 250u64.saturating_mul(1 << (attempt - 1).min(5));
                tokio::time::sleep(Duration::from_millis(ms.min(5_000))).await;
            }

            let mut req = client.post(url).body(body.clone());
            for (k, v) in headers {
                req = req.header(k, v);
            }
            if !has_user_agent {
                req = req.header("user-agent",
                    concat!("mira/", env!("CARGO_PKG_VERSION")));
            }
            if let Some(sig) = signature.as_deref() {
                req = req
                    .header("x-mira-signature", format!("sha256={sig}"))
                    .header("x-mira-timestamp", &timestamp);
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(format!(
                            "POST {url} → {} (attempt {}/{})",
                            status.as_u16(),
                            attempt + 1,
                            attempts_total,
                        ));
                    }
                    let retry = is_retryable_status(status);
                    let body_snip = resp.text().await.unwrap_or_default();
                    last_err = format!(
                        "non-2xx: {} body={}",
                        status, snippet(&body_snip),
                    );
                    if !retry {
                        return Err(MiraError::ConfigError(format!(
                            "http_post {last_err} (terminal, no retry)"
                        )));
                    }
                    warn!(
                        "automations: http_post {url} attempt {}/{} {} — retrying",
                        attempt + 1, attempts_total, status,
                    );
                }
                Err(e) => {
                    last_err = format!("network: {e}");
                    warn!(
                        "automations: http_post {url} attempt {}/{} network error: {e}",
                        attempt + 1, attempts_total,
                    );
                    // Network errors (connect/timeout/DNS) are always retryable
                    // fall through to the next iteration.
                }
            }
        }

        Err(MiraError::ConfigError(format!(
            "http_post {url} exhausted {attempts_total} attempt(s): {last_err}"
        )))
    }

    // ── ChannelMessage ──────────────────────────────────────────────────

    // supports `channel="web"` end-to-end (writes into a per-user
    // "Notifications" thread + emits a NotificationBus event). Other
    // channels (`signal`, `telegram`, `email`) are stubbed: we record a
    // success with a clear message but don't yet bridge to the per-account
    // daemons. Those land alongside the Webhooks slice when account
    // look-up plumbing is needed for inbound signed POSTs anyway.
    async fn run_channel_message(
        &self,
        user_id:         &str,
        channel:         &str,
        to:              Option<&str>,
        conversation_id: Option<&str>,
        text_template:   &str,
        tpl_ctx:         &serde_json::Value,
    ) -> Result<String, MiraError> {
        // Spam guard. Per-user, per-channel sliding-window cap. Runs
        // *before* any side-effect (history write, NotificationBus emit,
        // outbound API call) so a flood is caught at the front door.
        if let Some(limiter) = self.rate_limiter.as_ref() {
            let now = Utc::now().timestamp();
            match limiter.check_and_record(user_id, channel, now) {
                RateDecision::Allowed { .. } => {}
                RateDecision::Denied { cap, retry_after_secs } => {
                    let err = format!(
                        "channel_message: rate limit exceeded for user={user_id} \
                         channel={channel} (cap={cap}/min, retry in {retry_after_secs}s)"
                    );
                    warn!("automations: {err}");
                    return Err(MiraError::ConfigError(err));
                }
            }
        }

        let text = template::render(text_template, tpl_ctx);
        match channel {
            "web" => {
                let history = self.history.as_ref().ok_or_else(|| MiraError::ConfigError(
                    "channel=web requires HistoryStore".into()
                ))?;
                let bus = self.notifications.as_ref().ok_or_else(|| MiraError::ConfigError(
                    "channel=web requires NotificationBus".into()
                ))?;

                // Originating-conversation mode: when the action carries an
                // explicit `conversation_id` (e.g. `spawn_background_task`
                // delivering its result back to the chat the user started
                // it from), write straight into that thread. We verify
                // ownership so a malformed action can't post into another
                // user's conversation.
                //
                // Fallback: persist into a per-user "Notifications" thread
                // (auto-create-if-missing) so unscoped fire-and-forget
                // messages still have a stable place to land.
                let conv_id = if let Some(target) = conversation_id {
                    let owns = history.get_conversation(target)
                        .map_err(|e| MiraError::ConfigError(format!("get_conversation: {e}")))?
                        .map(|c| c.user_id == user_id)
                        .unwrap_or(false);
                    if !owns {
                        warn!(
                            "automations: conversation_id={} not owned by user={} — \
                             falling back to Notifications thread",
                            target, user_id,
                        );
                        notifications_thread_id(history, user_id)?
                    } else {
                        target.to_string()
                    }
                } else {
                    notifications_thread_id(history, user_id)?
                };

                history.add_message(NewMessage {
                    conversation_id: conv_id.clone(),
                    role:            MessageRole::Assistant,
                    content:         text.clone(),
                    content_type:    "text".to_owned(),
                    token_count:     None,
                    model:           None,
                    tool_calls:      None,
                    metadata:        Some(json!({"automation": true, "kind": "channel_message"}).to_string()),
                }).map_err(|e| MiraError::ConfigError(format!("add_message: {e}")))?;
                let _ = history.touch_conversation(&conv_id);

                bus.send(Notification {
                    kind:            NotificationKind::InboundMessage,
                    conversation_id: Some(conv_id),
                    channel:         Some("web".to_string()),
                    user_id:         Some(user_id.to_string()),
                    message:         Some(snippet(&text)),
                });
                Ok(format!("web → {}", snippet(&text)))
            }
            "signal" | "telegram" | "discord" | "matrix" | "whatsapp" | "slack" | "email" => {
                // Signal + Telegram + Discord all have outbound bridges
                // (signal-cli / Bot API / Discord REST). Signal + Telegram
                // honour the recipient's voice preference (voice note when
                // policy=Always); Discord + Email are text-only. Reuse
                // `deliver_outbound` so `Prompt(channel=…)` and
                // `ChannelMessage{channel:…}` share one path; tui still
                // warn-and-noops.
                self.deliver_outbound(channel, user_id, to, &text).await?;
                Ok(format!("{channel} → {}", snippet(&text)))
            }
            // CPP plugin channels: `external:<provider_kind>` — same path.
            ext if ext.starts_with("external:") => {
                self.deliver_outbound(ext, user_id, to, &text).await?;
                Ok(format!("{ext} → {}", snippet(&text)))
            }
            other => Err(MiraError::ConfigError(format!(
                "channel_message: unknown channel '{other}'"
            ))),
        }
    }

    // ── dead-letter notification ───────────────────────────────

    // Synthesise a `channel_message` and dispatch it as a notification to
    // the schedule's owner when it transitions to `failed`. Best-effort —
    // the worker logs but doesn't propagate any error from this path so a
    // flaky notification subsystem never masks the underlying schedule
    // failure.
    pub async fn notify_dead_letter(
        &self,
        user_id:        &str,
        schedule_id:    &str,
        schedule_name:  &str,
        failure_count:  i64,
        last_error:     &str,
    ) -> DispatchOutcome {
        let text = format!(
            "Schedule “{}” has been disabled after {} consecutive failures. Last error: {}",
            schedule_name,
            failure_count,
            snippet(last_error),
        );
        let action = Action::ChannelMessage {
            channel:         "web".into(),
            to:              None,
            conversation_id: None, // dead letters always go to the Notifications thread
            text_template:   text,
        };
        let activation = Activation {
            source_kind: "dead_letter",
            source_id:   schedule_id,
            user_id,
            action:      &action,
            payload:     None,
            chain_ids:   &[],
        };
        self.dispatch(activation).await
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

// Drain the agent's event stream into a single assistant text. Returns the
// concatenated tokens — `Error` events become an `Err`, `Warning` and tool
// events are ignored (they are still streamed to live UIs by the agent).
async fn drain_to_text(
    rx: &mut mpsc::Receiver<StreamEvent>,
) -> Result<String, MiraError> {
    let mut text = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Token(t) => text.push_str(&t),
            StreamEvent::Done { .. } => break,
            StreamEvent::Error(e) => return Err(MiraError::ConfigError(e)),
            StreamEvent::Warning(_)
            | StreamEvent::ToolCall { .. }
            | StreamEvent::ToolResult { .. }
            | StreamEvent::WikiContext { .. }
            | StreamEvent::Reasoning(_) => {}
        }
    }
    Ok(text)
}

// Status codes the HttpPost retry path treats as transient. 5xx is the
// classic case; 408 (request timeout) and 429 (rate-limited) are also
// safe to retry after a backoff.
fn is_retryable_status(s: reqwest::StatusCode) -> bool {
    s.is_server_error()
        || s == reqwest::StatusCode::REQUEST_TIMEOUT
        || s == reqwest::StatusCode::TOO_MANY_REQUESTS
}

// Find-or-create the per-user "Notifications" web thread used as the
// fallback target for `Action::ChannelMessage{channel:"web", conversation_id:None}`.
// Pulled out of `run_channel_message` so the orphan-sweep path can reuse it.
fn notifications_thread_id(
    history: &Arc<HistoryStore>,
    user_id: &str,
) -> Result<String, MiraError> {
    let thread_name = "Notifications";
    let mut convs = history.list_conversations(user_id, Some("web"), 200, 0)
        .map_err(|e| MiraError::ConfigError(format!("list_conversations: {e}")))?;
    convs.retain(|c| c.title.as_deref() == Some(thread_name));
    if let Some(c) = convs.into_iter().next() {
        return Ok(c.id);
    }
    let new = history.create_conversation(NewConversation {
        user_id:          user_id.to_string(),
        channel:          "web".to_string(),
        title:            Some(thread_name.to_string()),
        model:            None,
        provider:         None,
        external_user_id: None,
        mode:             None,
    })
    .map_err(|e| MiraError::ConfigError(format!("create_conversation: {e}")))?;
    Ok(new.id)
}

// Truncate to a small audit-friendly snippet so we don't bloat the runs
// table with multi-KB strings.
fn snippet(s: &str) -> String {
    const LIMIT: usize = 500;
    if s.len() <= LIMIT {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(LIMIT).collect();
        t.push('…');
        t
    }
}
