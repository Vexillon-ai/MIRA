// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/dispatcher.rs
//! Send a single companion check-in to a user.
//!
//! The scheduler decides *when* (via `policy::evaluate`); this module
//! does the actual fire: resolve the channel, find-or-create the
//! conversation, generate an opener via the agent, persist + notify.
//!
//! v1 supports the **web** channel end-to-end. Other
//! channels (Signal/Telegram) are accepted in the preferred-channels
//! config but skipped at delivery time with a warn — the outbound
//! bridge for those lands in a small follow-up. The check-in still
//! gets persisted into history so the next time the user opens the
//! web UI they see what the companion would have sent.

use std::sync::Arc;

use chrono::Utc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::agent::{AgentCore, StreamEvent, TurnContext};
use crate::auth::LocalAuthService;
use crate::automations::store::AutomationsStore;
use crate::calendar::CalendarStore;
use crate::channel::telegram_channel::TelegramChannel;
use crate::channel_accounts::ChannelAccountStore;
use crate::companion::briefing;
use crate::companion::{CompanionError, CompanionStore, Result};
use crate::notifications::{Notification, NotificationBus, NotificationKind};
use crate::history::{HistoryStore, MessageRole, NewConversation, NewMessage};
use crate::providers::signal_cli::SignalCliClient;
use crate::wiki::WikiRegistry;
use crate::MiraError;

// The cue we feed into `AgentCore::process` to nudge the model to
// open a check-in. This isn't surfaced to the user — the agent core
// puts it on the wire as a "user-role" turn, but we don't persist
// it as a user message in history, so the visible conversation
// only contains the assistant's opener (and the user's reply
// thereafter).
const CHECKIN_CUE: &str = "\
[Companion-mode tick. The user did not just message you. They might \
be available to chat. Open the conversation with a warm, brief \
message tailored to what you know about them from the wiki (style, \
likes, routines). Keep it natural — share something small or ask a \
gentle question. One message; let them reply when they're ready. \
Do not refer to this prompt or to the scheduler. Do not call any \
tools — just write the opening message.]";

// Conversation title used when the dispatcher has to create one.
// Same title across check-ins so they all roll up into one thread,
// preventing a new conversation row per fire.
const CHECKIN_CONV_TITLE: &str = "Companion check-ins";

// Concrete outcome of one fire, returned to the scheduler so it can
// stamp `last_checkin_at` only on success.
#[derive(Debug)]
pub enum DispatchOutcome {
    Sent { conversation_id: String, channel: String, chars: usize },
    SkippedNoChannel,
    Failed(String),
}

// Outcome of delivering a message to a user's last-used messaging channel.
// Lets the Guardian distinguish "operator is web-only" from "their channel is
// down" (isolation, §4.5).
#[derive(Debug, Clone)]
pub enum DeliveryOutcome {
    // Delivered to this channel.
    Delivered(String),
    // No messaging channel configured (web-only operator) — not isolation.
    NoChannel,
    // A configured channel (`0`) failed to deliver (`1`) — this is isolation.
    Failed(String, String),
}

// Holds the dependencies the scheduler needs to send a check-in.
// Built once by the gateway and cloned cheaply (it's just `Arc`s).
#[derive(Clone)]
pub struct CompanionDispatcher {
    agent: Arc<AgentCore>,
    history: Arc<HistoryStore>,
    store: Arc<CompanionStore>,
    notifications: Option<Arc<NotificationBus>>,
    // Outbound Signal delivery. All three must be present for a
    // check-in on channel="signal" to actually reach the user's phone;
    // missing any of them falls through to the warn-and-history-only
    // path so the generated message is preserved and operators see
    // why the bridge no-op'd.
    auth: Option<Arc<LocalAuthService>>,
    signal_port: Option<u16>,
    signal_bot_number: Option<String>,
    // Outbound Telegram delivery. The store gives us the user's bot
    // token; the chat_id comes from the most recent inbound telegram
    // conversation's `external_user_id` (which equals the user's tg
    // user id for 1-on-1 chats — the only case companion-mode targets).
    channel_accounts: Option<Arc<ChannelAccountStore>>,
    // E2 — outbound email delivery. The account row gives us the
    // SMTP creds; the recipient is the bot owner's `users.email`
    // (matches the Signal pattern of "send to the user's own phone
    // for proactive check-ins"). When either is missing, the
    // email arm returns an error and the assistant message lives
    // in history only.
    email_accounts: Option<Arc<crate::email::EmailAccountStore>>,
    email_loop_cache: Option<Arc<crate::email::ReplyLoopCache>>,
    // Q1.6 — Daily briefing snapshot sources. All optional; when
    // None the relevant section of the briefing is empty.
    calendar:    Option<Arc<CalendarStore>>,
    automations: Option<Arc<AutomationsStore>>,
    wiki:        Option<Arc<WikiRegistry>>,
    // Honoured by `deliver_telegram` to gate outbound on the MIRA-wide
    // `channels.telegram.enabled` kill switch. Without this, flipping
    // the global toggle off would still let companion check-ins push
    // out via per-account bot tokens.
    live_config: Option<Arc<crate::web::LiveConfig>>,
    // TTS service + the owner's per-channel voice prefs decide whether a
    // proactive message goes out as a voice note. `None` → text only
    // (the prior behaviour). Mirrors the normal reply path so a user with
    // "voice: always" on a channel gets spoken check-ins too.
    tts: Option<crate::tts::TtsService>,
}

impl CompanionDispatcher {
    pub fn new(
        agent: Arc<AgentCore>,
        history: Arc<HistoryStore>,
        store: Arc<CompanionStore>,
    ) -> Self {
        Self {
            agent, history, store,
            notifications: None,
            auth: None,
            signal_port: None,
            signal_bot_number: None,
            channel_accounts: None,
            email_accounts: None,
            email_loop_cache: None,
            calendar: None,
            automations: None,
            wiki: None,
            live_config: None,
            tts: None,
        }
    }

    // Wire the TTS service so proactive messages can be delivered as voice
    // notes when the user's per-channel voice preference asks for it.
    pub fn with_tts(mut self, tts: Option<crate::tts::TtsService>) -> Self {
        self.tts = tts;
        self
    }

    // Wire the outbound Email bridge. Both args must be present
    // for the email arm to actually deliver — `accounts` for SMTP
    // creds + recipient lookup, `loop_cache` so companion sends
    // share the same reply-loop guard as the inbound dispatch
    // path. Either being `None` keeps companion on the
    // history-only fallback for email.
    pub fn with_email(
        mut self,
        accounts:   Option<Arc<crate::email::EmailAccountStore>>,
        loop_cache: Option<Arc<crate::email::ReplyLoopCache>>,
    ) -> Self {
        self.email_accounts   = accounts;
        self.email_loop_cache = loop_cache;
        self
    }

    // Wire the live config so the dispatcher can honour the MIRA-wide
    // `channels.telegram.enabled` kill switch on every send.
    pub fn with_live_config(mut self, cfg: Option<Arc<crate::web::LiveConfig>>) -> Self {
        self.live_config = cfg;
        self
    }

    pub fn with_notifications(mut self, bus: Arc<NotificationBus>) -> Self {
        self.notifications = Some(bus);
        self
    }

    // Wire the outbound Signal bridge. `auth` is used to resolve the
    // recipient's phone number from `users.phone`; `port` +
    // `bot_number` match `config.channels.signal.{rest_port,phone_number}`.
    // Any of these being `None` keeps the dispatcher on the
    // history-only fallback for signal — useful in tests and on
    // servers that haven't configured signal-cli.
    pub fn with_signal(
        mut self,
        auth: Option<Arc<LocalAuthService>>,
        port: Option<u16>,
        bot_number: Option<String>,
    ) -> Self {
        self.auth = auth;
        self.signal_port = port;
        self.signal_bot_number = bot_number.filter(|s| !s.is_empty());
        self
    }

    // Wire the outbound Telegram bridge. `accounts` is the global
    // channel-account store; at dispatch time we find the recipient
    // user's enabled telegram account row (provides bot_token) and
    // derive the chat_id from their most recent telegram conversation.
    // `None` keeps the dispatcher on history-only fallback for telegram.
    pub fn with_telegram(mut self, accounts: Option<Arc<ChannelAccountStore>>) -> Self {
        self.channel_accounts = accounts;
        self
    }

    // Q1.6 — wire the Daily Briefing snapshot sources. Each is
    // optional: a user with only a wiki configured still gets a
    // meaningful briefing (just no calendar/automation sections).
    pub fn with_briefing_sources(
        mut self,
        calendar:    Option<Arc<CalendarStore>>,
        automations: Option<Arc<AutomationsStore>>,
        wiki:        Option<Arc<WikiRegistry>>,
    ) -> Self {
        self.calendar    = calendar;
        self.automations = automations;
        self.wiki        = wiki;
        self
    }

    // Send one check-in to `user_id`. On success, stamps
    // `last_checkin_at` in the settings store; on failure, leaves it
    // untouched (so the scheduler's policy will let us retry).
    pub async fn send_checkin(&self, user_id: &str) -> Result<DispatchOutcome> {
        let settings = self.store.get(user_id)?
            .ok_or_else(|| CompanionError::NotEnabled(user_id.to_string()))?;

        // 1. Pick a channel: first reachable in preferred_channels,
        //  else last-used from history, else fall back to "web".
        let channel = pick_channel(&self.history, user_id, &settings.preferred_channels);
        if channel.is_empty() {
            warn!("companion dispatch: no channel resolved for user '{user_id}'");
            return Ok(DispatchOutcome::SkippedNoChannel);
        }

        // 2. Find or create the companion thread on that channel.
        let conv_id = find_or_create_checkin_thread(&self.history, user_id, &channel)
            .map_err(|e| CompanionError::Invalid(format!("conversation: {e}")))?;

        // 3. Run the agent with the cue — drain to text — persist the
        //  assistant turn explicitly (we suppress the cue, so the
        //  agent's normal "user-role" wire message doesn't show up
        //  in history; we add the assistant text in this code path
        //  ourselves).
        let turn_ctx = TurnContext {
            // Constrain the tool set — this is a check-in, not a
            // task. Leaving the full chat palette in lets the model
            // accidentally fire web_fetch or recall when it should
            // just be warm and brief.
            allowed_tool_names: Some(Vec::new()),
            // Rehydrate the check-in thread from history so a restart / idle
            // eviction doesn't blank the agent's context — and so it can see
            // its own recent check-ins and avoid repeating the same opener.
            conversation_id: Some(conv_id.clone()),
            ..TurnContext::default()
        };

        // Fold the user's recent real conversations into the cue so the opener
        // can reference what was recently discussed — and so the memory recall
        // (which searches with this input) surfaces recent-topic memories
        // instead of the same generic top-N every time.
        let cue = match recent_conversation_digest(&self.history, user_id, &conv_id) {
            Some(digest) => format!("{CHECKIN_CUE}\n\n{digest}"),
            None         => CHECKIN_CUE.to_string(),
        };

        let mut rx = match self.agent
            .process_with_context(&conv_id, user_id, &channel, &cue, None, turn_ctx)
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                warn!("companion dispatch: agent.process failed for '{user_id}': {e}");
                return Ok(DispatchOutcome::Failed(format!("agent: {e}")));
            }
        };

        let assistant_text = match drain_to_text(&mut rx).await {
            Ok(s) if !s.trim().is_empty() => s,
            Ok(_)  => {
                warn!("companion dispatch: empty opener generated for '{user_id}'");
                return Ok(DispatchOutcome::Failed("empty opener".into()));
            }
            Err(e) => {
                warn!("companion dispatch: drain failed for '{user_id}': {e}");
                return Ok(DispatchOutcome::Failed(format!("drain: {e}")));
            }
        };

        // 4. Persist the assistant message into history. The cue
        //  itself never gets persisted as a user turn (the agent
        //  core only writes user turns via the chat handler, which
        //  we bypassed).
        if let Err(e) = self.history.add_message(NewMessage {
            conversation_id: conv_id.clone(),
            role:            MessageRole::Assistant,
            content:         assistant_text.clone(),
            content_type:    "text".to_owned(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata:        Some(serde_json::json!({"companion_checkin": true}).to_string()),
        }) {
            warn!("companion dispatch: persist failed for '{user_id}': {e}");
            // Continue — message generation succeeded, persistence is
            // best-effort. The next slice's audit log catches this.
        }
        let _ = self.history.touch_conversation(&conv_id);

        // 5. Push the assistant text out over the user's channel
        //  transport. Web is "already done" — the bus event below
        //  wakes any open tab. Signal looks up the user's phone and
        //  pushes via signal-cli. Telegram pushes via the bot
        //  bridge. Other channels warn-and-noop until their bridges
        //  land; the assistant text is preserved in history regardless.
        // Track the primary-channel delivery outcome separately so we
        // can return Failed to the caller (and the UI) when the user's
        // preferred channel didn't actually deliver. Earlier the
        // dispatcher logged the failure and still returned Sent — the
        // UI then claimed "✅ sent" even though Tarek's phone never
        // saw the message. Found in the real incident:
        // https://wiki/competitive-research-and-roadmap (Q1.6 follow-up).
        let mut delivery_error: Option<String> = None;
        match channel.as_str() {
            "web" => { /* delivery happens via the bus event below */ }
            "signal" => {
                if let Err(e) = self.deliver_signal(user_id, &assistant_text).await {
                    warn!("companion dispatch: signal delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("signal: {e}"));
                }
            }
            "telegram" => {
                if let Err(e) = self.deliver_telegram(user_id, &assistant_text).await {
                    warn!("companion dispatch: telegram delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("telegram: {e}"));
                }
            }
            "discord" => {
                if let Err(e) = self.deliver_discord(user_id, &assistant_text).await {
                    warn!("companion dispatch: discord delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("discord: {e}"));
                }
            }
            "matrix" => {
                if let Err(e) = self.deliver_matrix(user_id, &assistant_text).await {
                    warn!("companion dispatch: matrix delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("matrix: {e}"));
                }
            }
            "whatsapp" => {
                if let Err(e) = self.deliver_whatsapp(user_id, &assistant_text).await {
                    warn!("companion dispatch: whatsapp delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("whatsapp: {e}"));
                }
            }
            "slack" => {
                if let Err(e) = self.deliver_slack(user_id, &assistant_text).await {
                    warn!("companion dispatch: slack delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("slack: {e}"));
                }
            }
            "email" => {
                if let Err(e) = self.deliver_email(user_id, &assistant_text).await {
                    warn!("companion dispatch: email delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("email: {e}"));
                }
            }
            // CPP plugin channels: `external:<provider_kind>`.
            ext if ext.starts_with("external:") => {
                if let Err(e) = self.deliver_external(user_id, ext, &assistant_text).await {
                    warn!("companion dispatch: external delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("{ext}: {e}"));
                }
            }
            other => {
                warn!(
                    "companion dispatch: outbound delivery on '{other}' not yet \
                     implemented — message saved to history only"
                );
                delivery_error = Some(format!("{other}: outbound not implemented"));
            }
        }

        // Always emit the NotificationBus event regardless of primary-
        // channel outcome — open web tabs still need to refresh, and
        // Web Push fan-out (Q1.2) might still reach the user even if
        // their preferred channel failed.
        if let Some(bus) = &self.notifications {
            bus.send(Notification {
                kind: NotificationKind::ConversationUpdated,
                conversation_id: Some(conv_id.clone()),
                channel:         Some(channel.clone()),
                user_id:         Some(user_id.to_string()),
                message:         Some(snippet(&assistant_text)),
            });
        }

        // Stamp the success in companion_settings only when delivery
        // actually worked — otherwise the policy gates (min-gap,
        // missed-checkin counter, etc) treat the broken delivery as a
        // successful contact and stop firing.
        if delivery_error.is_none() {
            if let Err(e) = self.store.mark_checkin(user_id, Utc::now()) {
                warn!("companion dispatch: mark_checkin failed for '{user_id}': {e}");
            }
        }

        if let Some(err) = delivery_error {
            // Honest failure outcome. The text is still in history so the
            // user can read it the next time they open the web UI;
            // they're just not getting pinged on the channel they
            // expected.
            return Ok(DispatchOutcome::Failed(err));
        }

        info!(
            "companion dispatch: sent check-in for '{user_id}' on '{channel}' \
             ({} chars, conv={conv_id})",
            assistant_text.chars().count(),
        );
        Ok(DispatchOutcome::Sent {
            conversation_id: conv_id,
            channel,
            chars: assistant_text.chars().count(),
        })
    }

    // Q1.6 — Daily Briefing fire. Gathers a structured snapshot
    // (calendar / wiki / automations), asks the agent to render it
    // into a warm summary in the user's persona, then dispatches
    // via the same channel-routing path send_checkin uses. Stamps
    // `last_briefing_at` on success so the scheduler's once-per-day
    // guard works on the next tick.
    pub async fn send_briefing(&self, user_id: &str) -> Result<DispatchOutcome> {
        let settings = self.store.get(user_id)?
            .ok_or_else(|| CompanionError::NotEnabled(user_id.to_string()))?;

        // Resolve user tz the same way the scheduler does (auth
        // profile → fallback to UTC). Used by the snapshot's
        // local-day boundaries + the local-clock formatting.
        let user_tz = self.auth.as_ref()
            .and_then(|a| a.get_profile(user_id).ok().flatten())
            .and_then(|p| p.timezone);

        let snapshot = briefing::gather_snapshot(
            user_id,
            user_tz.as_deref(),
            self.calendar.as_ref(),
            self.automations.as_ref(),
            self.wiki.as_ref(),
            Some(&self.history),
            Utc::now(),
        );

        // Render the persona-aware cue. Names default to neutral
        // placeholders when the auth profile doesn't have them.
        let (agent_name, user_name) = self.resolve_names(user_id);
        let cue = briefing::brief_cue(&snapshot, &agent_name, &user_name);

        // From here on, the dispatch logic mirrors send_checkin —
        // pick channel, find-or-create thread, run the agent, persist
        // assistant message, route to Signal/Telegram/web. Threading
        // shares the check-in conversation so a user sees morning
        // briefings + warm openers in one chronological view.
        let channel = pick_channel(&self.history, user_id, &settings.preferred_channels);
        if channel.is_empty() {
            warn!("companion briefing: no channel resolved for user '{user_id}'");
            return Ok(DispatchOutcome::SkippedNoChannel);
        }

        let conv_id = find_or_create_checkin_thread(&self.history, user_id, &channel)
            .map_err(|e| CompanionError::Invalid(format!("conversation: {e}")))?;

        // Fold recent real conversations into the briefing cue too (see
        // send_checkin) — for both the opener's context and recall quality.
        let cue = match recent_conversation_digest(&self.history, user_id, &conv_id) {
            Some(digest) => format!("{cue}\n\n{digest}"),
            None         => cue,
        };

        let turn_ctx = TurnContext {
            // Same restriction as check-ins — the briefing is content,
            // not a tool-calling moment. Empty allowlist + the
            // explicit "do not call tools" line in the cue.
            allowed_tool_names: Some(Vec::new()),
            // Rehydrate the shared check-in/briefing thread from history (see
            // send_checkin) so restarts don't blank context and the briefing
            // sees the recent check-in/briefing exchange.
            conversation_id: Some(conv_id.clone()),
            ..TurnContext::default()
        };

        let mut rx = match self.agent
            .process_with_context(&conv_id, user_id, &channel, &cue, None, turn_ctx)
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                warn!("companion briefing: agent.process failed for '{user_id}': {e}");
                return Ok(DispatchOutcome::Failed(format!("agent: {e}")));
            }
        };
        let assistant_text = match drain_to_text(&mut rx).await {
            Ok(s) if !s.trim().is_empty() => s,
            Ok(_) => {
                warn!("companion briefing: empty briefing generated for '{user_id}'");
                return Ok(DispatchOutcome::Failed("empty briefing".into()));
            }
            Err(e) => {
                warn!("companion briefing: drain failed for '{user_id}': {e}");
                return Ok(DispatchOutcome::Failed(format!("drain: {e}")));
            }
        };

        // Persist + tag in metadata so the chat UI can later style
        // briefings differently from regular assistant messages.
        if let Err(e) = self.history.add_message(NewMessage {
            conversation_id: conv_id.clone(),
            role:            MessageRole::Assistant,
            content:         assistant_text.clone(),
            content_type:    "text".to_owned(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata:        Some(serde_json::json!({"companion_briefing": true}).to_string()),
        }) {
            warn!("companion briefing: persist failed for '{user_id}': {e}");
        }
        let _ = self.history.touch_conversation(&conv_id);

        // Channel delivery — same honest-outcome handling as check-ins.
        let mut delivery_error: Option<String> = None;
        match channel.as_str() {
            "web" => { /* delivery via the bus event below */ }
            "signal" => {
                if let Err(e) = self.deliver_signal(user_id, &assistant_text).await {
                    warn!("companion briefing: signal delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("signal: {e}"));
                }
            }
            "telegram" => {
                if let Err(e) = self.deliver_telegram(user_id, &assistant_text).await {
                    warn!("companion briefing: telegram delivery failed for '{user_id}': {e}");
                    delivery_error = Some(format!("telegram: {e}"));
                }
            }
            other => {
                warn!(
                    "companion briefing: outbound on '{other}' not yet implemented \
                     — message saved to history only"
                );
                delivery_error = Some(format!("{other}: outbound not implemented"));
            }
        }

        // Wake any open web tabs + fan out via push regardless — web
        // push is the secondary path that might still reach the user
        // even when the primary channel failed.
        if let Some(bus) = &self.notifications {
            bus.send(Notification {
                kind: NotificationKind::ConversationUpdated,
                conversation_id: Some(conv_id.clone()),
                channel:         Some(channel.clone()),
                user_id:         Some(user_id.to_string()),
                message:         Some(snippet(&assistant_text)),
            });
        }

        // Stamp last_briefing_at only when delivery actually succeeded
        // otherwise the one-per-local-day gate suppresses tomorrow's
        // briefing too because it thinks today's already fired.
        if delivery_error.is_none() {
            if let Err(e) = self.store.mark_briefing(user_id, Utc::now()) {
                warn!("companion briefing: mark_briefing failed for '{user_id}': {e}");
            }
        }

        if let Some(err) = delivery_error {
            return Ok(DispatchOutcome::Failed(err));
        }

        info!(
            "companion briefing: sent for '{user_id}' on '{channel}' \
             ({} chars, conv={conv_id})",
            assistant_text.chars().count(),
        );
        Ok(DispatchOutcome::Sent {
            conversation_id: conv_id,
            channel,
            chars: assistant_text.chars().count(),
        })
    }

    fn resolve_names(&self, user_id: &str) -> (String, String) {
        let profile = self.auth.as_ref()
            .and_then(|a| a.get_profile(user_id).ok().flatten());
        let agent_name = profile.as_ref()
            .and_then(|p| p.agent_name.clone())
            .unwrap_or_else(|| "MIRA".to_string());
        let user_name = profile.as_ref()
            .and_then(|p| p.preferred_name.clone())
            .or_else(|| profile.as_ref().and_then(|p| p.full_name.clone()))
            .unwrap_or_else(|| "the user".to_string());
        (agent_name, user_name)
    }

    // Push `text` to `user_id`'s Signal number via signal-cli. Mirrors
    // the path used by `automations::dispatch::Dispatcher::deliver_outbound`
    // but kept inline here (no shared trait) because the companion's
    // delivery requirements are narrower — text-only, no voice/TTS,
    // no `to_override`, no rate-limiter.
    async fn deliver_signal(&self, user_id: &str, text: &str) -> std::result::Result<(), MiraError> {
        let (Some(port), Some(bot)) = (self.signal_port, self.signal_bot_number.as_ref()) else {
            return Err(MiraError::ConfigError(
                "signal_port/signal_bot_number not configured".into(),
            ));
        };
        let auth = self.auth.as_ref().ok_or_else(|| MiraError::ConfigError(
            "auth service required to look up recipient phone".into(),
        ))?;
        let user = auth.get_user(user_id)
            .map_err(|e| MiraError::ConfigError(format!("get_user: {e}")))?
            .ok_or_else(|| MiraError::ConfigError(format!("user {user_id} not found")))?;
        let phone = user.phone.ok_or_else(|| MiraError::ConfigError(format!(
            "user {user_id} has no phone — set users.phone to enable Signal delivery"
        )))?;
        let client = SignalCliClient::new(port, bot.clone());

        // Voice note when the owner's signal voice preference is "always".
        // signal-cli takes attachments as file paths, so we synth → write a
        // temp OGG/Opus → send_with_attachments (the temp file is held alive
        // across the send, then dropped/deleted). Falls back to text on any
        // synth/transport failure. Mirrors the automations + reply paths.
        if let Some(tts) = &self.tts {
            let resolved = self.resolve_owner_voice(user_id, "signal");
            if matches!(resolved.policy, crate::voice::ResponsePolicy::Always) {
                if let Some(buf) = crate::providers::signal_cli::sse_listener::synth_signal_voice(
                    Some(tts), text, resolved.voice_id.as_deref(),
                ).await {
                    match crate::providers::signal_cli::sse_listener::write_voice_tempfile(&buf.bytes) {
                        Ok(tmp) => {
                            let path = tmp.path().to_string_lossy().to_string();
                            match client.send_with_attachments(vec![phone.clone()], text, &[path]).await {
                                Ok(()) => {
                                    info!("companion dispatch: pushed voice check-in to signal:{phone} for user '{user_id}'");
                                    return Ok(());
                                }
                                Err(e) => warn!(
                                    "companion dispatch: signal voice send failed ({e}) — falling back to text"
                                ),
                            }
                        }
                        Err(e) => warn!(
                            "companion dispatch: signal voice tempfile failed ({e}) — falling back to text"
                        ),
                    }
                }
            }
        }

        client.send(vec![phone.clone()], text).await
            .map_err(|e| MiraError::ConfigError(format!("signal send: {e}")))?;
        info!("companion dispatch: pushed check-in to signal:{phone} for user '{user_id}'");
        Ok(())
    }

    // Push `text` to `user_id` via their Telegram bot. The bot_token
    // comes from the user's enabled `channel_accounts` row; the chat_id
    // is derived from their most-recent telegram conversation's
    // `external_user_id` (== tg user id for 1-on-1 chats, the only
    // case proactive check-ins target). Group-chat companion mode is
    // out of scope — those chats aren't a "warm message Mom" surface.
    async fn deliver_telegram(&self, user_id: &str, text: &str) -> std::result::Result<(), MiraError> {
        // MIRA-wide kill switch — Settings → Channels → Telegram.
        // Short-circuit before touching the channel-accounts store so a
        // disabled global toggle is honoured even when the user has
        // per-account rows wired up.
        if let Some(cfg) = &self.live_config {
            if !cfg.get().await.channels.telegram.enabled {
                return Err(MiraError::ConfigError(
                    "telegram is disabled globally (Settings → Channels → Telegram)".into(),
                ));
            }
        }
        let accounts = self.channel_accounts.as_ref().ok_or_else(|| MiraError::ConfigError(
            "channel_accounts store not wired".into(),
        ))?;
        // Resolve the bot token to send through. Personal bot first: the
        // user's own enabled telegram account. If they have none, fall back
        // to a shared admin-managed bot (R1+R2) — a linked user owns no bot
        // but proactive messages should still reach them through the shared
        // one. The chat_id resolution below works either way because inbound
        // already records the conversation under the resolved (linked) user.
        let bot_token = accounts
            .outbound_telegram_token(user_id)
            .map_err(|e| MiraError::ConfigError(format!("resolve telegram token: {e}")))?
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no enabled telegram account and no shared \
                 telegram bot is configured — set one up in Channels"
            )))?;
        // Resolve chat_id from the user's most recent telegram thread
        // that has an `external_user_id` stamped (i.e. an actual inbound
        // conversation from a TG user — not a bot-side thread like the
        // "Companion check-ins" thread which `find_or_create_checkin_thread`
        // touches as a side-effect of a fire, leaving it with
        // external_user_id=None). Without the filter, the most-recently-
        // touched thread wins → which is often the bot's own thread →
        // no chat_id → silent delivery failure that we then misreport as
        // a success. Found in the real incident:
        // https://wiki/competitive-research-and-roadmap (Q1.6 follow-up).
        let convs = self.history.list_conversations(user_id, Some("telegram"), 50, 0)
            .map_err(|e| MiraError::ConfigError(format!("list_conversations: {e}")))?;
        let chat_id_str = convs.into_iter()
            .filter_map(|c| c.external_user_id)
            .find(|s| !s.trim().is_empty())
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no prior telegram conversation with a known \
                 chat id — they must message the bot once from their phone so \
                 we learn it"
            )))?;
        let chat_id: i64 = chat_id_str.parse()
            .map_err(|e| MiraError::ConfigError(
                format!("chat_id parse {chat_id_str:?}: {e}")
            ))?;
        let tg = TelegramChannel::new(bot_token);

        // Voice note when the owner's telegram voice preference is "always".
        // Companion messages have no inbound audio, so OnVoiceInput never
        // applies here — only Always opts into spoken check-ins. Falls back
        // to text on any synth/transport failure.
        if let Some(tts) = &self.tts {
            let resolved = self.resolve_owner_voice(user_id, "telegram");
            if matches!(resolved.policy, crate::voice::ResponsePolicy::Always) {
                if let Some(buf) = crate::server::handlers::telegram::synth_voice_for_channel(
                    Some(tts), "telegram", text, resolved.voice_id.as_deref(),
                ).await {
                    match tg.send_voice_to_chat(chat_id, &buf.bytes, text).await {
                        Ok(()) => {
                            info!("companion dispatch: pushed voice check-in to telegram:{chat_id} for user '{user_id}'");
                            return Ok(());
                        }
                        Err(e) => warn!(
                            "companion dispatch: telegram voice send failed ({e}) — falling back to text"
                        ),
                    }
                }
            }
        }

        tg.send_to_chat(chat_id, text).await
            .map_err(|e| MiraError::ConfigError(format!("telegram send: {e}")))?;
        info!("companion dispatch: pushed check-in to telegram:{chat_id} for user '{user_id}'");
        Ok(())
    }

    // D3 — push a proactive check-in to `user_id` over Discord. Mirrors
    // `deliver_telegram`: honour the global kill switch, resolve a bot
    // token (personal-first, shared-bot fallback for linked users), find
    // the Discord channel id from the user's most-recent inbound thread,
    // and POST the text via the REST API. Discord has no proactive voice
    // path yet (D2/D3 are text-only), so this is text-only.
    //     // Note the conversation key: Discord inbound records
    // `external_user_id = channel_id` (the DM or guild channel the user
    // talks to the bot in), which is exactly what `post_message` posts
    // to — no DM-channel-open round-trip needed.
    async fn deliver_discord(&self, user_id: &str, text: &str) -> std::result::Result<(), MiraError> {
        if let Some(cfg) = &self.live_config {
            if !cfg.get().await.channels.discord.enabled {
                return Err(MiraError::ConfigError(
                    "discord is disabled globally (Settings → Channels → Discord)".into(),
                ));
            }
        }
        let accounts = self.channel_accounts.as_ref().ok_or_else(|| MiraError::ConfigError(
            "channel_accounts store not wired".into(),
        ))?;
        let bot_token = accounts
            .outbound_discord_token(user_id)
            .map_err(|e| MiraError::ConfigError(format!("resolve discord token: {e}")))?
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no enabled discord account and no shared \
                 discord bot is configured — set one up in Channels"
            )))?;
        // Most-recent inbound thread with a stamped channel id. Filtering
        // out NULL external_user_id skips bot-side threads (e.g. the
        // check-in thread) the same way the telegram path does.
        let convs = self.history.list_conversations(user_id, Some("discord"), 50, 0)
            .map_err(|e| MiraError::ConfigError(format!("list_conversations: {e}")))?;
        let channel_id = convs.into_iter()
            .filter_map(|c| c.external_user_id)
            .find(|s| !s.trim().is_empty())
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no prior discord conversation with a known \
                 channel id — they must message the bot once so we learn it"
            )))?;
        // Check-ins are infrequent — a per-call client is fine and avoids
        // threading another field through the dispatcher builder.
        let client = reqwest::Client::new();
        crate::discord::api::post_message(&client, &bot_token, &channel_id, text)
            .await
            .map_err(|e| MiraError::ConfigError(format!("discord send: {e}")))?;
        info!("companion dispatch: pushed check-in to discord:{channel_id} for user '{user_id}'");
        Ok(())
    }

    // Push a proactive check-in to `user_id` over Matrix. Mirrors
    // `deliver_discord`: kill switch, personal-first creds with shared-bot
    // fallback, room id from the most-recent inbound thread (Matrix stores
    // `external_user_id = room_id`, directly postable), send via REST.
    // Text-only.
    async fn deliver_matrix(&self, user_id: &str, text: &str) -> std::result::Result<(), MiraError> {
        if let Some(cfg) = &self.live_config {
            if !cfg.get().await.channels.matrix.enabled {
                return Err(MiraError::ConfigError(
                    "matrix is disabled globally (Settings → Channels → Matrix)".into(),
                ));
            }
        }
        let accounts = self.channel_accounts.as_ref().ok_or_else(|| MiraError::ConfigError(
            "channel_accounts store not wired".into(),
        ))?;
        let (homeserver, token) = accounts
            .outbound_matrix_creds(user_id)
            .map_err(|e| MiraError::ConfigError(format!("resolve matrix creds: {e}")))?
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no enabled matrix account and no shared \
                 matrix bot is configured — set one up in Channels"
            )))?;
        let convs = self.history.list_conversations(user_id, Some("matrix"), 50, 0)
            .map_err(|e| MiraError::ConfigError(format!("list_conversations: {e}")))?;
        let room_id = convs.into_iter()
            .filter_map(|c| c.external_user_id)
            .find(|s| !s.trim().is_empty())
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no prior matrix conversation with a known \
                 room id — they must message the bot once so we learn it"
            )))?;
        let client = reqwest::Client::new();
        // txn seed: a fixed-per-(room) value is fine for proactive sends;
        // the homeserver dedupes on (token, txnId) and check-ins are rare.
        crate::matrix::api::send_message(&client, &homeserver, &token, &room_id, text, 0)
            .await
            .map_err(|e| MiraError::ConfigError(format!("matrix send: {e}")))?;
        info!("companion dispatch: pushed check-in to matrix:{room_id} for user '{user_id}'");
        Ok(())
    }

    // Push a proactive check-in to `user_id` over WhatsApp. Like the other
    // channels: kill switch, personal-first creds with shared-bot fallback,
    // recipient phone from the most-recent inbound thread, send via the
    // Cloud API. NOTE the 24-hour window — Meta only allows a free-form
    // text reply within 24h of the user's last inbound message; a check-in
    // fired outside that window will be rejected (logged as a 131047
    // error). Template messages (the supported way to re-engage) are not
    // yet implemented; see design-docs/whatsapp-channel.md.
    async fn deliver_whatsapp(&self, user_id: &str, text: &str) -> std::result::Result<(), MiraError> {
        if let Some(cfg) = &self.live_config {
            if !cfg.get().await.channels.whatsapp.enabled {
                return Err(MiraError::ConfigError(
                    "whatsapp is disabled globally (Settings → Channels → WhatsApp)".into(),
                ));
            }
        }
        let accounts = self.channel_accounts.as_ref().ok_or_else(|| MiraError::ConfigError(
            "channel_accounts store not wired".into(),
        ))?;
        let (phone_number_id, token) = accounts
            .outbound_whatsapp_creds(user_id)
            .map_err(|e| MiraError::ConfigError(format!("resolve whatsapp creds: {e}")))?
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no enabled whatsapp account and no shared \
                 whatsapp bot is configured — set one up in Channels"
            )))?;
        let convs = self.history.list_conversations(user_id, Some("whatsapp"), 50, 0)
            .map_err(|e| MiraError::ConfigError(format!("list_conversations: {e}")))?;
        let to = convs.into_iter()
            .filter_map(|c| c.external_user_id)
            .find(|s| !s.trim().is_empty())
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no prior whatsapp conversation with a known \
                 phone — they must message the bot once so we learn it"
            )))?;
        let client = reqwest::Client::new();
        crate::whatsapp::api::send_text(&client, &phone_number_id, &token, &to, text)
            .await
            .map_err(|e| MiraError::ConfigError(format!(
                "whatsapp send (note: free-form replies only work within 24h \
                 of the user's last message): {e}"
            )))?;
        info!("companion dispatch: pushed check-in to whatsapp:{to} for user '{user_id}'");
        Ok(())
    }

    // Push a proactive check-in to `user_id` over Slack. Same shape as
    // the other webhook channels: kill switch, personal-first token with
    // shared-bot fallback, channel id from the most-recent inbound thread,
    // send via chat.postMessage. Text-only. Slack has no 24h-window
    // restriction, so proactive sends just work.
    async fn deliver_slack(&self, user_id: &str, text: &str) -> std::result::Result<(), MiraError> {
        if let Some(cfg) = &self.live_config {
            if !cfg.get().await.channels.slack.enabled {
                return Err(MiraError::ConfigError(
                    "slack is disabled globally (Settings → Channels → Slack)".into(),
                ));
            }
        }
        let accounts = self.channel_accounts.as_ref().ok_or_else(|| MiraError::ConfigError(
            "channel_accounts store not wired".into(),
        ))?;
        let bot_token = accounts
            .outbound_slack_token(user_id)
            .map_err(|e| MiraError::ConfigError(format!("resolve slack token: {e}")))?
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no enabled slack account and no shared \
                 slack bot is configured — set one up in Channels"
            )))?;
        let convs = self.history.list_conversations(user_id, Some("slack"), 50, 0)
            .map_err(|e| MiraError::ConfigError(format!("list_conversations: {e}")))?;
        let channel = convs.into_iter()
            .filter_map(|c| c.external_user_id)
            .find(|s| !s.trim().is_empty())
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no prior slack conversation with a known \
                 channel — they must message the bot once so we learn it"
            )))?;
        let client = reqwest::Client::new();
        crate::slack::api::post_message(&client, &bot_token, &channel, text)
            .await
            .map_err(|e| MiraError::ConfigError(format!("slack send: {e}")))?;
        info!("companion dispatch: pushed check-in to slack:{channel} for user '{user_id}'");
        Ok(())
    }

    // Push a proactive check-in to `user_id` over a CPP (External) channel.
    // `chan` is the full `external:<kind>` string the conversation was
    // recorded under. Resolves the account's send_url + outbound_secret
    // (personal-first, shared fallback) and POSTs a signed CPP outbound.
    async fn deliver_external(&self, user_id: &str, chan: &str, text: &str) -> std::result::Result<(), MiraError> {
        if let Some(cfg) = &self.live_config {
            if !cfg.get().await.channels.external.enabled {
                return Err(MiraError::ConfigError(
                    "external channels are disabled globally (Settings → Channels)".into(),
                ));
            }
        }
        let accounts = self.channel_accounts.as_ref().ok_or_else(|| MiraError::ConfigError(
            "channel_accounts store not wired".into(),
        ))?;
        let (account_id, send_url, secret, supports_voice) = accounts
            .outbound_external_creds(user_id)
            .map_err(|e| MiraError::ConfigError(format!("resolve external creds: {e}")))?
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no enabled external account and no shared \
                 external provider is configured"
            )))?;
        let convs = self.history.list_conversations(user_id, Some(chan), 50, 0)
            .map_err(|e| MiraError::ConfigError(format!("list_conversations: {e}")))?;
        let conversation_id = convs.into_iter()
            .filter_map(|c| c.external_user_id)
            .find(|s| !s.trim().is_empty())
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no prior {chan} conversation with a known \
                 id — they must message the bot once so we learn it"
            )))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let client = reqwest::Client::new();
        // Voice check-in when the provider declares it can play audio
        // (`supports_voice`) *and* the owner's per-channel voice policy for
        // this `external:<kind>` channel is "Always". Mirrors the inbound
        // reply path (external::dispatch::reply_voiced) and the signal/
        // telegram arms above. Falls through to text on any synth/send
        // failure so the check-in still lands.
        if supports_voice {
            if let Some(tts) = &self.tts {
                let resolved = self.resolve_owner_voice(user_id, chan);
                if matches!(resolved.policy, crate::voice::ResponsePolicy::Always) {
                    if let Some(buf) = crate::server::handlers::telegram::synth_voice_for_channel(
                        Some(tts), chan, text, resolved.voice_id.as_deref(),
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
                                info!("companion dispatch: pushed voice check-in to {chan}:{conversation_id} for user '{user_id}'");
                                return Ok(());
                            }
                            Err(e) => warn!(
                                "companion dispatch: external voice send failed ({e}) — falling back to text"
                            ),
                        }
                    }
                }
            }
        }
        crate::external::api::send_message(
            &client, &send_url, &secret, &account_id, &conversation_id, text, now,
        ).await.map_err(|e| MiraError::ConfigError(format!("external send: {e}")))?;
        info!("companion dispatch: pushed check-in to {chan}:{conversation_id} for user '{user_id}'");
        Ok(())
    }

    // Resolve the owner's voice preference for `channel`: their per-user
    // prefs layered over the server defaults, same as the normal reply
    // path. Defaults to "Never" when neither TTS nor auth is wired.
    fn resolve_owner_voice(&self, user_id: &str, channel: &str) -> crate::voice::ResolvedVoice {
        let server_defaults = self.tts.as_ref()
            .map(|t| t.voice_prefs_defaults())
            .unwrap_or_default();
        let user_prefs = self.auth.as_ref()
            .and_then(|a| a.get_user(user_id).ok().flatten())
            .map(|u| crate::voice::parse_user_prefs(u.voice_prefs.as_deref()))
            .unwrap_or_default();
        crate::voice::resolve_voice(channel, Some(&user_prefs), &server_defaults)
    }

    // E2 — push `text` to `user_id`'s personal email via the user's
    // first enabled email_account's SMTP server. The account row
    // is treated as MIRA's "from" identity; the recipient is the
    // bot owner's `users.email` (same posture as Signal using
    // `users.phone`). Subject is a fixed "Check-in from MIRA" so
    // reply threading clusters companion check-ins in the
    // recipient's mail client.
    async fn deliver_email(&self, user_id: &str, text: &str) -> std::result::Result<(), MiraError> {
        let accounts = self.email_accounts.as_ref().ok_or_else(|| MiraError::ConfigError(
            "email_accounts store not wired".into(),
        ))?;
        let loop_cache = self.email_loop_cache.as_ref().ok_or_else(|| MiraError::ConfigError(
            "email reply-loop cache not wired".into(),
        ))?;
        let auth = self.auth.as_ref().ok_or_else(|| MiraError::ConfigError(
            "auth service not wired (needed to resolve user.email)".into(),
        ))?;
        let user = auth.get_user(user_id)
            .map_err(|e| MiraError::ConfigError(format!("get_user: {e}")))?
            .ok_or_else(|| MiraError::ConfigError(format!("user {user_id} not found")))?;
        let to = user.email.ok_or_else(|| MiraError::ConfigError(format!(
            "user {user_id} has no email — set users.email to enable Email delivery"
        )))?;
        // Pick the user's first enabled email account as the "from".
        let account = accounts.list_for_user(user_id)
            .map_err(|e| MiraError::ConfigError(format!("list email accounts: {e}")))?
            .into_iter()
            .find(|a| a.enabled)
            .ok_or_else(|| MiraError::ConfigError(format!(
                "user {user_id} has no enabled email account — set one up on the Email page"
            )))?;
        let subject = "Check-in from MIRA";
        let msg = crate::email::OutboundMessage {
            to:          &to,
            subject,
            body:        text,
            in_reply_to: None,
            references:  &[],
        };
        // Live config gives the OAuth client_ids for refresh; falls
        // through gracefully for password accounts since
        // `send_for_account` only touches OAuth fields when
        // `auth_mode` starts with "oauth_".
        let live = match &self.live_config {
            Some(lc) => lc.get().await,
            None => {
                return Err(MiraError::ConfigError(
                    "companion email send needs live_config (for OAuth refresh)".into()
                ));
            }
        };
        crate::email::smtp_send_for_account(
            &account, msg, loop_cache.as_ref(), accounts.as_ref(), &live.email_oauth,
        ).await
            .map_err(|e| MiraError::ConfigError(format!("email send: {e}")))?;
        info!("companion dispatch: pushed check-in to email:{to} for user '{user_id}'");
        Ok(())
    }

    // Deliver an arbitrary message to `user_id`'s most-recent **messaging**
    // channel (Signal/Telegram/Discord/Matrix/WhatsApp/Slack/email/external),
    // skipping web/cli/tui. Used by the MIRA-Guardian watch loop. Web is covered
    // separately by the NotificationBus. The 3-way outcome lets the Guardian
    // distinguish "no channel" (web-only operator) from "channel failed"
    // (isolation) — see §4.5.
    pub async fn deliver_to_user(&self, user_id: &str, text: &str) -> DeliveryOutcome {
        let Some(channel) = self.last_messaging_channel(user_id) else {
            return DeliveryOutcome::NoChannel;
        };
        let res = match channel.as_str() {
            "signal"   => self.deliver_signal(user_id, text).await,
            "telegram" => self.deliver_telegram(user_id, text).await,
            "discord"  => self.deliver_discord(user_id, text).await,
            "matrix"   => self.deliver_matrix(user_id, text).await,
            "whatsapp" => self.deliver_whatsapp(user_id, text).await,
            "slack"    => self.deliver_slack(user_id, text).await,
            "email"    => self.deliver_email(user_id, text).await,
            ext if ext.starts_with("external:") => self.deliver_external(user_id, ext, text).await,
            _ => return DeliveryOutcome::NoChannel,
        };
        match res {
            Ok(()) => DeliveryOutcome::Delivered(channel),
            Err(e) => {
                warn!("guardian alert delivery on '{channel}' failed for '{user_id}': {e}");
                DeliveryOutcome::Failed(channel, e.to_string())
            }
        }
    }

    // The user's most-recent conversation channel that is a real messaging
    // channel (not web/cli/tui), if any. Scans recent conversations newest-first.
    fn last_messaging_channel(&self, user_id: &str) -> Option<String> {
        let convs = self.history.list_conversations(user_id, None, 25, 0).ok()?;
        convs.into_iter().map(|c| c.channel).find(|ch| {
            matches!(ch.as_str(),
                "signal" | "telegram" | "discord" | "matrix" | "whatsapp" | "slack" | "email")
                || ch.starts_with("external:")
        })
    }
}

// ── Internals ────────────────────────────────────────────────────────────────

// Resolve the channel for a check-in: try each entry in
// `preferred_channels` in order; fall through to the most-recent
// channel the user has actually used (per history); fall back to
// `web`. Returns an empty string if neither preference nor history
// yields anything (genuinely uninitialised users).
fn pick_channel(history: &HistoryStore, user_id: &str, preferred: &[String]) -> String {
    // 1. Configured preference — for v1 we accept any non-empty
    //  string. The dispatcher's delivery half handles unsupported
    //  channels by warning + falling through to history-only.
    if let Some(first) = preferred.iter().find(|s| !s.is_empty()) {
        return first.clone();
    }
    // 2. Last-used: most-recent conversation for this user.
    if let Ok(convs) = history.list_conversations(user_id, None, 1, 0) {
        if let Some(c) = convs.first() {
            if !c.channel.is_empty() { return c.channel.clone(); }
        }
    }
    // 3. Final fallback.
    "web".to_string()
}

// Find a conversation on `channel` for `user_id` titled
// `CHECKIN_CONV_TITLE`, or create one. Reusing the same title across
// fires keeps all check-ins in one thread — the user sees a coherent
// running conversation rather than 30 sibling threads after a month.
fn find_or_create_checkin_thread(
    history: &HistoryStore,
    user_id: &str,
    channel: &str,
) -> std::result::Result<String, MiraError> {
    // Look at the user's recent conversations on this channel; pick
    // the most recent one whose title matches. Bounded scan (20)
    // because we don't expect more than that on a typical first
    // boot.
    let convs = history.list_conversations(user_id, Some(channel), 20, 0)?;
    if let Some(c) = convs.iter().find(|c|
        c.title.as_deref().map(|t| t == CHECKIN_CONV_TITLE).unwrap_or(false)
    ) {
        return Ok(c.id.clone());
    }
    let conv = history.create_conversation(NewConversation {
        user_id: user_id.to_string(),
        channel: channel.to_string(),
        title: Some(CHECKIN_CONV_TITLE.to_string()),
        model: None,
        provider: None,
        external_user_id: None,
        mode: None,
    })?;
    debug!(
        "companion dispatch: created new check-in thread '{}' for '{user_id}' on '{channel}'",
        conv.id,
    );
    Ok(conv.id)
}

// Build a compact digest of the user's *recent real conversations* (across
// channels), for injection into a check-in/briefing cue.
// // Two purposes: (1) it gives the opener concrete recent context to reference
// naturally, and (2) because the agent's memory pre-hook searches with the
// turn `input`, folding recent topics into the cue makes recall surface
// memories tied to what was *recently* discussed instead of the same generic
// top-N every time — the two big causes of stale, repetitive check-ins.
// // The dedicated check-in thread (`CHECKIN_CONV_TITLE`) and `exclude_conv_id`
// are skipped so we summarise actual conversations, not prior openers. Returns
// `None` when there's nothing recent to show.
fn recent_conversation_digest(
    history:         &HistoryStore,
    user_id:         &str,
    exclude_conv_id: &str,
) -> Option<String> {
    // How much context to gather — small on purpose; this rides in a prompt.
    const MAX_CONVS:        usize = 2;
    const MSGS_PER_CONV:    i64   = 6;
    const MAX_MSG_CHARS:    usize = 200;

    let convs = history.list_conversations(user_id, None, 12, 0).ok()?;
    let mut sections: Vec<String> = Vec::new();
    for conv in convs.iter() {
        if sections.len() >= MAX_CONVS { break; }
        if conv.id == exclude_conv_id { continue; }
        if conv.title.as_deref() == Some(CHECKIN_CONV_TITLE) { continue; }

        let msgs = match history.get_recent_messages(&conv.id, MSGS_PER_CONV) {
            Ok(m) if !m.is_empty() => m,
            _ => continue,
        };
        let mut lines: Vec<String> = Vec::new();
        for m in &msgs {
            let who = match m.role {
                MessageRole::User      => "them",
                MessageRole::Assistant => "you",
                _ => continue, // skip system/tool rows
            };
            let mut text = m.content.trim().replace('\n', " ");
            if text.is_empty() { continue; }
            if text.chars().count() > MAX_MSG_CHARS {
                text = text.chars().take(MAX_MSG_CHARS).collect::<String>() + "…";
            }
            lines.push(format!("  {who}: {text}"));
        }
        if lines.is_empty() { continue; }
        let title = conv.title.as_deref().unwrap_or("(untitled)");
        sections.push(format!("• \"{title}\" ({}):\n{}", conv.channel, lines.join("\n")));
    }

    if sections.is_empty() { return None; }
    Some(format!(
        "[Recent context — what you and the user discussed lately. Draw on it \
         naturally if it fits; do NOT recite or quote it back verbatim.]\n{}",
        sections.join("\n"),
    ))
}

async fn drain_to_text(
    rx: &mut mpsc::Receiver<StreamEvent>,
) -> std::result::Result<String, MiraError> {
    let mut text = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Token(t) => text.push_str(&t),
            StreamEvent::Done { .. } => break,
            StreamEvent::Error(e) => return Err(MiraError::ConfigError(e)),
            // Companion dispatch ignores everything else — tool
            // events shouldn't be emitted anyway (we passed an empty
            // tool allowlist) and wiki-context pills are informational.
            _ => {}
        }
    }
    Ok(text)
}

fn snippet(s: &str) -> String {
    let mut out: String = s.chars().take(120).collect();
    if s.chars().count() > 120 { out.push('…'); }
    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryStore;
    use tempfile::tempdir;

    fn fresh_history() -> (tempfile::TempDir, Arc<HistoryStore>) {
        let dir = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("history.db")).unwrap();
        (dir, Arc::new(store))
    }

    #[test]
    fn pick_channel_honours_preference() {
        let (_dir, h) = fresh_history();
        let ch = pick_channel(&h, "alice", &["signal".into(), "web".into()]);
        assert_eq!(ch, "signal");
    }

    #[test]
    fn pick_channel_falls_back_to_last_used_when_no_pref() {
        let (_dir, h) = fresh_history();
        h.create_conversation(NewConversation {
            user_id: "alice".into(), channel: "telegram".into(),
            title: Some("chat".into()),
            ..Default::default()
        }).unwrap();
        let ch = pick_channel(&h, "alice", &[]);
        assert_eq!(ch, "telegram");
    }

    #[test]
    fn pick_channel_final_fallback_is_web() {
        let (_dir, h) = fresh_history();
        // No prefs, no conversations → "web".
        let ch = pick_channel(&h, "ghost", &[]);
        assert_eq!(ch, "web");
    }

    #[test]
    fn pick_channel_ignores_empty_pref_strings() {
        let (_dir, h) = fresh_history();
        h.create_conversation(NewConversation {
            user_id: "alice".into(), channel: "telegram".into(),
            title: Some("chat".into()),
            ..Default::default()
        }).unwrap();
        let ch = pick_channel(&h, "alice", &["".into(), "signal".into()]);
        // First non-empty entry wins.
        assert_eq!(ch, "signal");
    }

    #[test]
    fn find_or_create_returns_same_thread_across_calls() {
        let (_dir, h) = fresh_history();
        let id1 = find_or_create_checkin_thread(&h, "alice", "web").unwrap();
        let id2 = find_or_create_checkin_thread(&h, "alice", "web").unwrap();
        assert_eq!(id1, id2, "second call should reuse the existing thread");
    }

    #[test]
    fn find_or_create_separates_threads_per_channel() {
        let (_dir, h) = fresh_history();
        let id_web = find_or_create_checkin_thread(&h, "alice", "web").unwrap();
        let id_sig = find_or_create_checkin_thread(&h, "alice", "signal").unwrap();
        assert_ne!(id_web, id_sig);
        // Both should still have the same title.
        let cw = h.get_conversation(&id_web).unwrap().unwrap();
        let cs = h.get_conversation(&id_sig).unwrap().unwrap();
        assert_eq!(cw.title.as_deref(), Some(CHECKIN_CONV_TITLE));
        assert_eq!(cs.title.as_deref(), Some(CHECKIN_CONV_TITLE));
    }

    #[test]
    fn find_or_create_ignores_unrelated_conversations() {
        let (_dir, h) = fresh_history();
        h.create_conversation(NewConversation {
            user_id: "alice".into(), channel: "web".into(),
            title: Some("Pong project chat".into()),
            ..Default::default()
        }).unwrap();
        let id = find_or_create_checkin_thread(&h, "alice", "web").unwrap();
        let conv = h.get_conversation(&id).unwrap().unwrap();
        assert_eq!(conv.title.as_deref(), Some(CHECKIN_CONV_TITLE));
    }

    #[test]
    fn snippet_truncates_long_input() {
        let s: String = "x".repeat(500);
        let out = snippet(&s);
        assert_eq!(out.chars().count(), 121); // 120 + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn snippet_leaves_short_input_alone() {
        assert_eq!(snippet("hi"), "hi");
    }
}
