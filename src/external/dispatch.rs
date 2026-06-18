// SPDX-License-Identifier: AGPL-3.0-or-later

// src/external/dispatch.rs
//
// Process one inbound CPP message end-to-end. The generalized form of the
// built-in webhook dispatchers (Slack/WhatsApp) — same R1+R2 routing, MCP
// filter, history record + auto-title, link-code flow. The only
// difference: the channel string is `external:<provider_kind>` (namespaced
// per provider), and outbound goes back through the provider's send_url
// via a signed CPP call rather than a provider-specific REST API.

use std::sync::Arc;

use tracing::{debug, error, info, warn};

use crate::agent::{AgentCore, TurnContext};
use crate::auth::LocalAuthService;
use crate::history::HistoryStore;
use crate::mcp::McpServerRegistry;
use crate::web::LiveConfig;

use super::api::send_message;

/// Per-account context consumed by the shared `/webhook/external/{id}`
/// handler.
#[derive(Clone, Debug)]
pub struct ExternalAccountCtx {
    pub account_id:      String,
    pub owner_user_id:   String,
    /// Provider slug — e.g. `nctalk`. Namespaces the channel string +
    /// identity links so two providers never collide.
    pub provider_kind:   String,
    /// Where MIRA POSTs outbound replies.
    pub send_url:        String,
    /// HMAC key the provider signs inbound webhooks with (MIRA verifies).
    pub inbound_secret:  String,
    /// HMAC key MIRA signs outbound calls with (the provider verifies).
    pub outbound_secret: String,
    pub mention_only:    bool,
    /// Whether the provider can play synthesized audio. Surfaced in the
    /// voice-settings registry so this channel can offer voice prefs.
    pub supports_voice:  bool,
    pub routing_mode:    crate::channel_accounts::RoutingMode,
}

impl ExternalAccountCtx {
    /// The channel string used everywhere (history, identity, dispatch):
    /// `external:<provider_kind>`.
    pub fn channel_str(&self) -> String {
        format!("external:{}", self.provider_kind)
    }

    /// Build the runtime ctx from a stored `external` channel account. The
    /// single place that maps a row → ctx, shared by startup registration
    /// (ChannelManager) and the webhook's live DB fallback. Rejects an account
    /// that isn't `external` or is missing the mandatory CPP fields.
    pub fn from_account(
        acct: &crate::channel_accounts::ChannelAccount,
    ) -> Result<Self, String> {
        let cfg = acct.external_config().map_err(|e| e.to_string())?;
        if cfg.provider_kind.trim().is_empty()
            || cfg.send_url.trim().is_empty()
            || cfg.inbound_secret.trim().is_empty()
            || cfg.outbound_secret.trim().is_empty()
        {
            return Err(
                "provider_kind, send_url, inbound_secret and outbound_secret are required".into(),
            );
        }
        Ok(Self {
            account_id:      acct.id.clone(),
            owner_user_id:   acct.user_id.clone(),
            provider_kind:   cfg.provider_kind,
            send_url:        cfg.send_url,
            inbound_secret:  cfg.inbound_secret,
            outbound_secret: cfg.outbound_secret,
            mention_only:    cfg.mention_only,
            supports_voice:  cfg.supports_voice,
            routing_mode:    acct.routing_mode,
        })
    }
}

#[derive(Clone)]
pub struct ExternalDispatcherDeps {
    pub agent_core:  Arc<AgentCore>,
    pub history:     Option<Arc<HistoryStore>>,
    pub auth:        Option<Arc<LocalAuthService>>,
    pub live_config: Option<Arc<LiveConfig>>,
    pub mcp_servers: Option<Arc<McpServerRegistry>>,
    pub http_client: reqwest::Client,
    pub identity:    Option<Arc<crate::channel_identity::IdentityStore>>,
    pub link_codes:  Option<Arc<crate::channel_identity::LinkCodeStore>>,
    /// Channel-account store, for resolving an account the startup `accounts`
    /// snapshot doesn't have — i.e. one created after the server booted. Lets a
    /// freshly-installed CPP provider (or a manually-added External account) go
    /// live without a restart. `None` → snapshot-only (older wiring/tests).
    pub channel_store: Option<Arc<crate::channel_accounts::ChannelAccountStore>>,
    /// TTS service for spoken replies on voice-capable external channels.
    /// `None` (or a channel that isn't `supports_voice`, or a voice policy
    /// of Never) → text-only.
    pub tts:         Option<crate::tts::TtsService>,
    /// STT service for transcribing inbound voice notes a provider relays
    /// (CPP `audio` field). `None` → audio-only messages are dropped.
    pub stt:         Option<crate::stt::SttService>,
}

/// Decoded inbound audio (a user's voice note), carried from the webhook
/// handler — already base64-decoded + size-capped at the edge — into the
/// async processor where transcription happens.
pub struct InboundAudioBytes {
    /// MIME hint for the STT format sniffer, e.g. "audio/ogg".
    pub mime:  String,
    pub bytes: Vec<u8>,
}

pub struct InboundExternal {
    pub conversation_id: String,
    pub sender_id:       String,
    pub display_name:    Option<String>,
    pub text:            String,
    /// Present when the provider relayed a voice note instead of (or with
    /// empty) text. Transcribed to text before routing.
    pub audio:           Option<InboundAudioBytes>,
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub async fn process_external_message(
    deps: ExternalDispatcherDeps,
    ctx:  ExternalAccountCtx,
    msg:  InboundExternal,
) {
    let mut msg = msg;

    // Voice note → transcribe before routing. The provider relayed audio (and
    // either no text, or we prefer the spoken content). STT runs here, in the
    // already-spawned task, so the webhook ack isn't blocked. `inbound_was_voice`
    // lets an `OnVoiceInput` voice policy opt into a spoken reply below.
    let inbound_was_voice = msg.audio.is_some();
    if msg.text.trim().is_empty() {
        if let Some(audio) = msg.audio.take() {
            match transcribe_inbound(&deps, &ctx, audio).await {
                Some(t) if !t.trim().is_empty() => {
                    info!(account = %ctx.account_id, "transcribed inbound voice note ({} chars)", t.len());
                    msg.text = t;
                }
                _ => {
                    debug!(account = %ctx.account_id, "skip: voice note transcription empty/failed");
                    return;
                }
            }
        }
    }

    let content = msg.text.trim();
    if content.is_empty() {
        debug!(account = %ctx.account_id, "skip: empty text");
        return;
    }

    // Kill switch.
    if let Some(cfg) = deps.live_config.as_ref() {
        if !cfg.get().await.channels.external.enabled {
            debug!(account = %ctx.account_id, "skip: channels.external.enabled is false");
            return;
        }
    }

    if ctx.mention_only && !contains_word(content, "mira") {
        debug!(account = %ctx.account_id, "skip: mention_only set + 'mira' not in text");
        return;
    }

    let channel_str = ctx.channel_str();
    info!("External [acct={} kind={}] conv={} sender={}: {}",
          ctx.account_id, ctx.provider_kind, msg.conversation_id, msg.sender_id,
          &content[..content.len().min(80)]);

    let effective_text = content.to_string();

    // ── R1+R2 routing — channel string is `external:<kind>` so identity
    // links + sessions are namespaced per provider. ───────────────────
    use crate::channel_accounts::RoutingMode;
    let resolved_user_id: String = match ctx.routing_mode {
        RoutingMode::Personal => ctx.owner_user_id.clone(),
        RoutingMode::Shared | RoutingMode::GuestOk => 'resolve: {
            let idstore = match deps.identity.as_ref() {
                Some(s) => s,
                None => {
                    warn!(account = %ctx.account_id,
                          "shared/guest_ok but no IdentityStore wired — degrading");
                    if matches!(ctx.routing_mode, RoutingMode::GuestOk) {
                        break 'resolve ctx.owner_user_id.clone();
                    }
                    return;
                }
            };
            match idstore.lookup(&channel_str, &msg.sender_id) {
                Ok(Some(uid)) => uid,
                Ok(None) => {
                    if let Some(code) = crate::channel_identity::link_codes::looks_like_link_code(&effective_text) {
                        match deps.link_codes.as_ref().and_then(|cs| cs.consume(code, &channel_str).ok().flatten()) {
                            Some(uid) => {
                                if let Err(e) = idstore.link(&uid, &channel_str, &msg.sender_id) {
                                    warn!(account = %ctx.account_id, "link claim ok but persist failed: {}", e);
                                    reply(&deps, &ctx, &msg.conversation_id,
                                        "Sorry — link was accepted but I couldn't save it. Try again or ask the admin.").await;
                                    return;
                                }
                                info!(account = %ctx.account_id, user = %uid, external = %msg.sender_id,
                                      "external identity linked via code");
                                reply(&deps, &ctx, &msg.conversation_id,
                                    "✅ Linked! You can talk to me normally now.").await;
                                return;
                            }
                            None => {
                                reply(&deps, &ctx, &msg.conversation_id,
                                    "That link code didn't match — generate a fresh one in MIRA → Settings → My Channels and try again within 10 minutes.").await;
                                return;
                            }
                        }
                    }
                    if matches!(ctx.routing_mode, RoutingMode::GuestOk) {
                        format!("guest:{}:{}", channel_str, msg.sender_id)
                    } else {
                        reply(&deps, &ctx, &msg.conversation_id,
                            "Hi! I don't recognise you yet. Open MIRA → Settings → My Channels, generate a LINK-XXXX-XXXX code for this channel, and send it to me here.").await;
                        return;
                    }
                }
                Err(e) => {
                    warn!(account = %ctx.account_id, "identity lookup failed: {}", e);
                    return;
                }
            }
        }
    };

    let session_id = format!("ext-{}-{}-{}", ctx.provider_kind, resolved_user_id, msg.conversation_id);

    if let Some(bus) = deps.agent_core.event_bus() {
        bus.emit_named(
            crate::events::names::MESSAGE_RECEIVED,
            Some(msg.sender_id.clone()),
            serde_json::json!({
                "user_id":         msg.sender_id,
                "channel":         channel_str,
                "conversation_id": session_id,
                "text":            effective_text,
            }),
        );
    }

    let _ = deps.auth;
    let mut inject = serde_json::Map::new();
    inject.insert("_user_id".to_string(), serde_json::Value::String(resolved_user_id.clone()));
    let mut turn_ctx = TurnContext { inject_tool_args: inject, ..TurnContext::default() };
    if let Some(reg) = deps.mcp_servers.as_ref() {
        let all = deps.agent_core.tools.list_tools();
        if let Some(allow) = reg.allowed_tools_for(&resolved_user_id, &all) {
            turn_ctx.allowed_tool_names = Some(allow);
        }
    }

    // Resolve the persisted thread up-front so the agent can rehydrate this
    // conversation's context on a cache miss (restart / idle eviction); the
    // record-turn below reuses the same id.
    let history_conv = deps.history.as_ref().and_then(|hist| {
        let default_title = match &msg.display_name {
            Some(n) => format!("{} — {}", titlecase_kind(&ctx.provider_kind), n),
            None    => format!("{} — {}", titlecase_kind(&ctx.provider_kind), msg.conversation_id),
        };
        hist.find_or_create_external_conversation(
            &resolved_user_id, &channel_str, &msg.conversation_id, Some(default_title.as_str()),
        ).map_err(|e| warn!("find_or_create_external_conversation failed: {}", e)).ok()
    });
    if let Some(ref conv) = history_conv {
        turn_ctx.conversation_id = Some(conv.id.clone());
    }

    let rx = match deps.agent_core
        .process_with_context(&session_id, &resolved_user_id, &channel_str, &effective_text, None, turn_ctx)
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            error!("AgentCore failed for External {} (acct={}): {}", msg.conversation_id, ctx.account_id, e);
            return;
        }
    };
    let (response_text, _events) = AgentCore::collect_response(rx).await;

    // Record turn — external_user_id = conversation_id (one thread per
    // provider conversation under the resolved user).
    if let (Some(ref hist), Some(ref conv)) = (deps.history.as_ref(), history_conv.as_ref()) {
        let _ = hist.record_turn(&conv.id, &effective_text, &response_text, None, None);
        let first_turn = hist.get_messages(&conv.id, 5, None).map(|m| m.len()).unwrap_or(99) <= 2;
        if first_turn {
            let agent2  = Arc::clone(&deps.agent_core);
            let hist2   = Arc::clone(hist);
            let cid     = conv.id.clone();
            let msg_c   = effective_text.clone();
            let preview = crate::server::handlers::chat::derive_title_from_message(&effective_text);
            tokio::spawn(async move {
                crate::server::handlers::chat::generate_auto_title(agent2, hist2, cid, msg_c, preview).await;
            });
        }
    }

    if response_text.is_empty() {
        debug!(account = %ctx.account_id, "agent returned empty response — sending nothing");
        return;
    }
    // The agent's reply: attach synthesized audio when this channel is
    // voice-capable and the resolved user's voice policy opts in (`Always`,
    // or `OnVoiceInput` when they sent a voice note). Link-code / system
    // replies above stay text-only via `reply()`.
    reply_voiced(&deps, &ctx, &msg.conversation_id, &resolved_user_id, &response_text, inbound_was_voice).await;
}

/// Transcribe an inbound voice note to text via the configured STT backend.
/// Returns `None` (caller drops the message) when STT isn't wired or the
/// backend errors — same graceful-degrade posture as the rest of the path.
async fn transcribe_inbound(
    deps:  &ExternalDispatcherDeps,
    ctx:   &ExternalAccountCtx,
    audio: InboundAudioBytes,
) -> Option<String> {
    let stt = deps.stt.as_ref().or_else(|| {
        warn!(account = %ctx.account_id, "inbound voice note but STT not configured — dropping");
        None
    })?;
    let req = crate::stt::TranscribeRequest {
        format:      crate::stt::AudioInputFormat::from_mime(&audio.mime),
        audio_bytes: audio.bytes,
        language:    None,
    };
    match stt.transcribe(req, None, Some(&ctx.channel_str())).await {
        Ok(t)  => Some(t.text),
        Err(e) => {
            warn!(account = %ctx.account_id, "inbound voice transcription failed: {e}");
            None
        }
    }
}

/// Text-only outbound — used for system/link-code replies.
async fn reply(deps: &ExternalDispatcherDeps, ctx: &ExternalAccountCtx, conversation_id: &str, text: &str) {
    if let Err(e) = send_message(
        &deps.http_client, &ctx.send_url, &ctx.outbound_secret,
        &ctx.account_id, conversation_id, text, now_unix(),
    ).await {
        warn!(account = %ctx.account_id, "External outbound failed: {}", e);
    }
}

/// Outbound for the agent's reply: synthesize + attach audio when the
/// channel `supports_voice` AND the user's per-channel voice policy opts in
/// — `Always`, or `OnVoiceInput` when the user themselves sent a voice note
/// (`voice_input`). Falls back to text on any synth/transport issue (text is
/// always included in the CPP body either way).
async fn reply_voiced(
    deps: &ExternalDispatcherDeps,
    ctx:  &ExternalAccountCtx,
    conversation_id: &str,
    user_id: &str,
    text: &str,
    voice_input: bool,
) {
    if ctx.supports_voice {
        if let Some(tts) = deps.tts.as_ref() {
            let channel_str = ctx.channel_str();
            // Resolve the user's per-channel voice prefs (policy + voice id),
            // same layering as the other channels: per-user over server
            // defaults. `OnVoiceInput` mirrors Telegram/Signal — it only
            // speaks back when the inbound message was itself a voice note.
            let server_defaults = tts.voice_prefs_defaults();
            let user_prefs = deps.auth.as_ref()
                .and_then(|a| a.get_user(user_id).ok().flatten())
                .map(|u| crate::voice::parse_user_prefs(u.voice_prefs.as_deref()))
                .unwrap_or_default();
            let resolved = crate::voice::resolve_voice(&channel_str, Some(&user_prefs), &server_defaults);
            let want_voice = match resolved.policy {
                crate::voice::ResponsePolicy::Always       => true,
                crate::voice::ResponsePolicy::OnVoiceInput  => voice_input,
                crate::voice::ResponsePolicy::Never         => false,
            };
            if want_voice {
                if let Some(buf) = crate::server::handlers::telegram::synth_voice_for_channel(
                    Some(tts), &channel_str, text, resolved.voice_id.as_deref(),
                ).await {
                    use base64::Engine;
                    let audio = super::types::OutboundAudio {
                        content_type: buf.codec.content_type().to_string(),
                        extension:    buf.codec.extension().to_string(),
                        data_base64:  base64::engine::general_purpose::STANDARD.encode(&buf.bytes),
                    };
                    match super::api::send_message_with_audio(
                        &deps.http_client, &ctx.send_url, &ctx.outbound_secret,
                        &ctx.account_id, conversation_id, text, audio, now_unix(),
                    ).await {
                        Ok(()) => return,
                        Err(e) => warn!(account = %ctx.account_id,
                            "External voiced outbound failed ({e}) — falling back to text"),
                    }
                }
            }
        }
    }
    // Text-only fallback / default.
    reply(deps, ctx, conversation_id, text).await;
}

fn contains_word(haystack: &str, word: &str) -> bool {
    haystack.split(|c: char| !c.is_alphanumeric())
        .any(|tok| tok.eq_ignore_ascii_case(word))
}

/// Best-effort prettifier for the conversation-title prefix: `nctalk` →
/// `Nctalk`. Providers can pick a clean slug; this just capitalises it.
fn titlecase_kind(kind: &str) -> String {
    let mut c = kind.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None    => String::new(),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(kind: &str) -> ExternalAccountCtx {
        ExternalAccountCtx {
            account_id: "a".into(), owner_user_id: "o".into(),
            provider_kind: kind.into(), send_url: "http://x".into(),
            inbound_secret: "in".into(), outbound_secret: "out".into(),
            mention_only: false, supports_voice: false,
            routing_mode: crate::channel_accounts::RoutingMode::Personal,
        }
    }

    #[test]
    fn channel_str_is_namespaced() {
        assert_eq!(ctx("nctalk").channel_str(), "external:nctalk");
        assert_eq!(ctx("irc").channel_str(), "external:irc");
    }

    #[test]
    fn titlecase_kind_capitalises() {
        assert_eq!(titlecase_kind("nctalk"), "Nctalk");
        assert_eq!(titlecase_kind(""), "");
    }

    #[test]
    fn contains_word_boundary_aware() {
        assert!(contains_word("hey mira", "mira"));
        assert!(!contains_word("miracle", "mira"));
    }

    fn external_account(send_url: &str) -> crate::channel_accounts::ChannelAccount {
        use crate::channel_accounts::{ChannelAccount, ChannelKind, ExternalAccountConfig, RoutingMode};
        let cfg = ExternalAccountConfig {
            provider_kind: "nctalk".into(),
            send_url: send_url.into(),
            inbound_secret: "ins".into(),
            outbound_secret: "outs".into(),
            mention_only: true,
            supports_voice: true,
        };
        ChannelAccount {
            id: "acct-1".into(),
            user_id: "owner-1".into(),
            channel: ChannelKind::External,
            account_label: "talk".into(),
            external_id: None,
            config_json: serde_json::to_string(&cfg).unwrap(),
            enabled: true,
            routing_mode: RoutingMode::Personal,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn from_account_maps_a_valid_external_row() {
        let ctx = ExternalAccountCtx::from_account(&external_account("https://nc/cpp")).unwrap();
        assert_eq!(ctx.account_id, "acct-1");
        assert_eq!(ctx.owner_user_id, "owner-1");
        assert_eq!(ctx.provider_kind, "nctalk");
        assert_eq!(ctx.send_url, "https://nc/cpp");
        assert_eq!(ctx.channel_str(), "external:nctalk");
        assert!(ctx.mention_only && ctx.supports_voice);
    }

    #[test]
    fn from_account_rejects_missing_send_url() {
        // The webhook's live fallback must not register an account that can't
        // round-trip (no send_url → MIRA can't reply).
        assert!(ExternalAccountCtx::from_account(&external_account("")).is_err());
    }
}
