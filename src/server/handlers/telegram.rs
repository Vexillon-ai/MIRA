// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/telegram.rs
//! POST /webhook/telegram/{account_id} — inbound Telegram message handler.
//!
//! Telegram is now multi-tenant: each `ChannelAccount` of kind `Telegram`
//! owns its own bot token and webhook URL of the form
//! `/webhook/telegram/{account_id}`. The path param resolves to a
//! [`TelegramAccountCtx`] which carries the owner user id, the outbound
//! bot token, and the per-bot secret used to verify the
//! `X-Telegram-Bot-Api-Secret-Token` header.
//!
//! Flow:
//! 1. Lookup the account by id (404 if unknown).
//! 2. Verify the secret-token header (401 on mismatch).
//! 3. Parse the payload; stamp history with the owner's user id.
//! 4. Stream through AgentCore and reply via the Bot API.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing::{debug, error, info, warn};

use crate::agent::{AgentCore, TurnContext};
use crate::auth::LocalAuthService;
use crate::gateway::channel_manager::TelegramAccountCtx;
use crate::history::HistoryStore;
use crate::stt::SttService;
use crate::stt::types::{AudioInputFormat, TranscribeRequest};
use crate::tts::TtsService;
use crate::tts::types::{AudioBuffer, AudioCodec, OutputFormat};
use crate::voice::{parse_user_prefs, resolve_voice, ResponsePolicy};

// ── Payload types ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TelegramWebhookPayload {
    pub update_id: i64,
    pub message:   Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat:       TelegramChat,
    pub from:       Option<TelegramUser>,
    pub text:       Option<String>,
    // Caption attached to a media/voice message — Telegram puts the text
    // here when the user adds a comment alongside an attachment.
    pub caption:    Option<String>,
    // Present only on voice-note messages. Carries the `file_id` we
    // pass to `getFile` to resolve a download URL.
    pub voice:      Option<TelegramVoice>,
}

// Subset of the Telegram `Voice` object we care about. `mime_type` is
// usually `audio/ogg` (Opus), but we don't trust it implicitly — STT
// sniffs the container.
#[derive(Debug, Deserialize)]
pub struct TelegramVoice {
    pub file_id:   String,
    #[allow(dead_code)]
    pub duration:  Option<i64>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct TelegramUser {
    pub id:         i64,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name:  Option<String>,
    #[serde(default)]
    pub username:   Option<String>,
}

// ── Multi-tenant state ───────────────────────────────────────────────────────

/// HTTP client for all Telegram Bot API calls.
///
/// Built with explicit timeouts so a degraded network path can never wedge a
/// caller: `connect_timeout` bounds connection establishment (a dead route —
/// e.g. the broken IPv6 egress on WSL2 — fails fast so the poll loop backs off
/// and retries instead of hanging silently), and `timeout` bounds the whole
/// request, which is what protects the *send* path (`sendMessage`/`sendVoice`)
/// — a stalled reply used to hang indefinitely and look like "no response".
/// The long-poll `getUpdates` sets its own longer per-request timeout, which
/// overrides this default for that one call.
pub fn telegram_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// Injected state for the shared Telegram webhook. Holds the AgentCore +
// a snapshot of every enabled Telegram account keyed by id.
#[derive(Clone)]
pub struct TelegramState {
    pub agent_core:  Arc<AgentCore>,
    pub http_client: reqwest::Client,
    pub history:     Option<Arc<HistoryStore>>,
    // Account id → per-account context. Populated at startup by
    // [`ChannelManager`](crate::gateway::channel_manager::ChannelManager).
    pub accounts:    Arc<std::collections::HashMap<String, TelegramAccountCtx>>,
    // TTS service used to synthesise reply audio when
    // `tts.routing.telegram` pins a backend. `None` disables outbound
    // voice notes regardless of the routing config.
    pub tts:         Option<TtsService>,
    // STT service used to transcribe inbound voice messages. `None`
    // drops voice-only messages with a warning rather than silently
    // ignoring them.
    pub stt:         Option<SttService>,
    // Auth service — used by the reply dispatcher to look up the *recipient's*
    // per-channel voice prefs (response policy + voice id override), and by the
    // identity lookups above. `None` falls back to server defaults only.
    pub auth:        Option<Arc<LocalAuthService>>,
    // Live config used to honour the MIRA-wide
    // `channels.telegram.enabled` kill switch at request time, so
    // toggling it off in Settings takes effect immediately for both
    // inbound webhooks and the polling daemon without a restart.
    pub live_config: Option<Arc<crate::web::LiveConfig>>,
    // MCP host registry, used to derive the per-user
    // `allowed_tool_names` filter for the inbound turn. `None`
    // leaves the turn unrestricted (matches pre-Slice-4 behaviour).
    pub mcp_servers: Option<Arc<crate::mcp::McpServerRegistry>>,
    // R1+R2 — sender-id → MIRA user lookup for `Shared`/`GuestOk` bots.
    // `None` forces `Personal` semantics (every inbound runs as the bot
    // owner — matches pre-R1+R2 behaviour).
    pub identity:    Option<Arc<crate::channel_identity::IdentityStore>>,
    // R1+R2 — pending one-time link codes, read when an unmapped sender
    // posts a `LINK-XXXX-XXXX` string so we can claim the identity.
    pub link_codes:  Option<Arc<crate::channel_identity::LinkCodeStore>>,
}

// ── Handler ──────────────────────────────────────────────────────────────────

pub async fn telegram_handler(
    State(state):  State<TelegramState>,
    Path(account): Path<String>,
    headers:       HeaderMap,
    Json(payload): Json<TelegramWebhookPayload>,
) -> impl IntoResponse {
    // MIRA-wide kill switch — checked at request time so flipping the
    // toggle in Settings takes effect without a restart. Per-account
    // enable lives on the channel_account row; this gate is the
    // global override.
    if let Some(cfg) = &state.live_config {
        if !cfg.get().await.channels.telegram.enabled {
            info!("Telegram webhook ignored — globally disabled (acct={})", account);
            return (StatusCode::SERVICE_UNAVAILABLE, "telegram disabled");
        }
    }
    // Resolve target account.
    let ctx = match state.accounts.get(&account) {
        Some(c) => c.clone(),
        None    => {
            warn!("Telegram webhook: unknown account id {}", account);
            return (StatusCode::NOT_FOUND, "unknown account");
        }
    };

    // Per-account secret verification. If the account didn't register a
    // secret we fall through (operator's choice — unauthenticated
    // webhook accepted, matching the bot's BotFather setup).
    if let Some(ref expected) = ctx.secret_token {
        let provided = headers
            .get("x-telegram-bot-api-secret-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let ok = bool::from(provided.as_bytes().ct_eq(expected.as_bytes()));
        if !ok {
            warn!("Telegram webhook: bad secret on account {}", account);
            return (StatusCode::UNAUTHORIZED, "unauthorized");
        }
    }

    let msg = match payload.message {
        Some(m) => m,
        None    => {
            info!("Telegram update {} has no message (acct={})", payload.update_id, account);
            return (StatusCode::OK, "no message");
        }
    };

    process_message_for_account(&state, &ctx, msg).await;
    (StatusCode::OK, "ok")
}

// Per-message work shared between the webhook handler and the polling
// daemon. Extracted so the same path drives both transports: extract
// text (or transcribe voice), invoke AgentCore, record the turn into
// history, then dispatch the reply over the channel. Errors are logged
// but swallowed — neither caller has a recovery action.
pub async fn process_message_for_account(
    state: &TelegramState,
    ctx:   &TelegramAccountCtx,
    msg:   TelegramMessage,
) {
    let chat_id = msg.chat.id;
    let tg_user = msg.from.as_ref().map(|u| u.id.to_string())
        .unwrap_or_else(|| chat_id.to_string());
    let inbound_was_voice = msg.voice.is_some();
    let account = &ctx.account_id;

    // Caption travels with media/voice attachments; plain `text` is set for
    // text-only messages. For voice notes we treat the caption as the
    // user's typed comment alongside the audio (mirroring Signal's combined
    // text+voice handling).
    let typed_text = msg.text.as_deref()
        .or(msg.caption.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut effective_text = typed_text.unwrap_or_default();

    if let Some(ref vn) = msg.voice {
        match transcribe_telegram_voice(state, ctx, vn).await {
            Ok(transcript) => {
                info!(
                    "Telegram voice from {} (acct={}) transcribed ({} chars)",
                    tg_user, account, transcript.len(),
                );
                effective_text = if effective_text.is_empty() {
                    transcript
                } else {
                    format!("{}\n\n[voice]: {}", effective_text, transcript)
                };
            }
            Err(e) => {
                warn!(
                    "Telegram voice from {} (acct={}) failed to transcribe: {} \
                     — using caption only",
                    tg_user, account, e,
                );
                if effective_text.is_empty() {
                    return;
                }
            }
        }
    }

    if effective_text.is_empty() {
        info!("Telegram message {} has no usable text or voice", msg.message_id);
        return;
    }

    info!("Telegram [acct={}] chat={} user={}: {}",
          account, chat_id, tg_user,
          &effective_text[..effective_text.len().min(80)]);

    // ── R1+R2: resolve which MIRA user this turn runs as ───────────────
    //
    // SECURITY: a Telegram bot is reachable by anyone who knows its @username,
    // so we must never run a turn as a MIRA user we haven't *verified* owns the
    // sending chat.
    //
    // `Personal` — a single-owner bot. It serves ONLY the owner's verified
    //   chat: the owner links their own chat once (send a `LINK-XXXX-XXXX`
    //   code), and from then on any other sender is refused. Before this lock,
    //   Personal ran every inbound as the owner — meaning a stranger who found
    //   the bot acted as the owner (full memory + admin). Now an unlinked chat
    //   is prompted to link (owner) or ignored (everyone else).
    // `Shared`/`GuestOk` — multi-user: look the sender's Telegram id up in the
    //   identity table; on a miss redeem a code, prompt to link (Shared), or
    //   fall through to a `guest:telegram:<id>` identity (GuestOk).
    use crate::channel_accounts::RoutingMode;
    use crate::channel_identity::link_codes::looks_like_link_code;
    let resolved_user_id: String = match ctx.routing_mode {
        RoutingMode::Personal => 'personal: {
            let Some(idstore) = state.identity.as_ref() else {
                // No identity store wired (minimal/test build) — can't verify
                // the sender. Degrade to the legacy owner-as-sender behaviour
                // with a loud warning; production always wires the store, so
                // the lock applies there.
                warn!("Telegram acct={} is Personal but no IdentityStore is wired — \
                       cannot verify sender; running as owner (INSECURE fallback)", account);
                break 'personal ctx.owner_user_id.clone();
            };
            // Personal-bot ownership is tracked PER BOT, in a namespaced
            // identity scope (`telegram:personal:<account>`) — NOT the global
            // `telegram` map that Shared bots use. This decouples a personal bot
            // from a person's "main" MIRA identity, so one phone can be a
            // different MIRA user on each bot (e.g. `admin` on the shared family
            // bot, `Tarek` on a private bot) without one link stealing the
            // other. Only this bot's owner can register the chat, by sending a
            // fresh LINK code; the scope key is hidden from the My Channels UI.
            let scope = format!("telegram:personal:{}", account);
            match idstore.lookup(&scope, &tg_user) {
                // The owner's verified chat for THIS bot — the only one served.
                Ok(Some(uid)) if uid == ctx.owner_user_id => uid,
                // Not verified for this bot yet (`None`), or a stale/non-owner
                // row. The owner (re)claims it with a fresh LINK code; anyone
                // without a valid owner code is refused, so a stranger is never
                // served as the owner.
                Ok(_) => {
                    if let Some(code) = looks_like_link_code(&effective_text) {
                        match state.link_codes.as_ref()
                            .and_then(|cs| cs.consume(code, "telegram").ok().flatten())
                        {
                            Some(uid) if uid == ctx.owner_user_id => {
                                // relink (upsert) registers this chat as the
                                // bot's owner under the per-bot scope.
                                if let Err(e) = idstore.relink(&uid, &scope, &tg_user) {
                                    warn!("Telegram owner-link persist failed: {}", e);
                                    send_telegram_message(&state.http_client, &ctx.bot_token, chat_id,
                                        "Link accepted but couldn't be saved — try again.").await;
                                    return;
                                }
                                info!("Telegram personal bot {} bound to owner chat: user={} tg_user={}",
                                    account, uid, tg_user);
                                send_telegram_message(&state.http_client, &ctx.bot_token, chat_id,
                                    "✅ This bot is now secured to your account — go ahead and talk to me.").await;
                                return;
                            }
                            Some(_) => {
                                send_telegram_message(&state.http_client, &ctx.bot_token, chat_id,
                                    "That code isn't for this bot — a personal bot can only be linked by its owner.").await;
                                return;
                            }
                            None => {
                                send_telegram_message(&state.http_client, &ctx.bot_token, chat_id,
                                    "That link code didn't match — generate a fresh one in MIRA → Settings → My Channels (valid 10 minutes).").await;
                                return;
                            }
                        }
                    }
                    send_telegram_message(&state.http_client, &ctx.bot_token, chat_id,
                        "🔒 This bot isn't linked yet. If you're the owner, open MIRA → Settings → My Channels → Link Telegram and send me the LINK-XXXX-XXXX code.").await;
                    return;
                }
                Err(e) => {
                    warn!("Telegram identity lookup failed: {}", e);
                    return;
                }
            }
        }
        RoutingMode::Shared | RoutingMode::GuestOk => 'resolve: {
            let idstore = match state.identity.as_ref() {
                Some(s) => s,
                None => {
                    warn!("Telegram acct={} is shared/guest_ok but no IdentityStore wired — degrading", account);
                    if matches!(ctx.routing_mode, RoutingMode::GuestOk) {
                        break 'resolve ctx.owner_user_id.clone();
                    }
                    return;
                }
            };
            match idstore.lookup("telegram", &tg_user) {
                Ok(Some(uid)) => uid,
                Ok(None) => {
                    if let Some(code) = looks_like_link_code(&effective_text) {
                        match state.link_codes.as_ref()
                            .and_then(|cs| cs.consume(code, "telegram").ok().flatten())
                        {
                            Some(uid) => {
                                if let Err(e) = idstore.link(&uid, "telegram", &tg_user) {
                                    warn!("Telegram link claim succeeded but persist failed: {}", e);
                                    send_telegram_message(&state.http_client, &ctx.bot_token, chat_id,
                                        "Sorry — link was accepted but I couldn't save it. Try again or ask the admin.").await;
                                    return;
                                }
                                info!("Telegram identity linked via code: user={} tg_user={}", uid, tg_user);
                                send_telegram_message(&state.http_client, &ctx.bot_token, chat_id,
                                    "✅ Linked! You can talk to me normally now.").await;
                                return;
                            }
                            None => {
                                send_telegram_message(&state.http_client, &ctx.bot_token, chat_id,
                                    "That link code didn't match — generate a fresh one in MIRA → Settings → My Channels and try again within 10 minutes.").await;
                                return;
                            }
                        }
                    }
                    if matches!(ctx.routing_mode, RoutingMode::GuestOk) {
                        format!("guest:telegram:{}", tg_user)
                    } else {
                        send_telegram_message(&state.http_client, &ctx.bot_token, chat_id,
                            "Hi! I don't recognise you yet. Open MIRA → Settings → My Channels → Link Telegram, copy the LINK-XXXX-XXXX code, and send it to me here.").await;
                        return;
                    }
                }
                Err(e) => {
                    warn!("Telegram identity lookup failed: {}", e);
                    return;
                }
            }
        }
    };

    // Session id bakes in the resolved MIRA user so each linked sender
    // gets their own session even when sharing one admin-managed bot.
    let session_id = format!("tg-{}-{}", resolved_user_id, chat_id);

    if let Some(bus) = state.agent_core.event_bus() {
        bus.emit_named(
            crate::events::names::MESSAGE_RECEIVED,
            Some(tg_user.clone()),
            serde_json::json!({
                "user_id":         tg_user,
                "channel":         "telegram",
                "conversation_id": session_id,
                "text":            effective_text,
            }),
        );
    }

    // Trusted identity injection. The agent's MIRA user_id is
    // `resolved_user_id` (computed above from the account's routing
    // mode), NOT the raw inbound Telegram sender id.
    //
    // - Personal mode: resolved_user_id == ctx.owner_user_id. Messaging
    // your own bot acts as you (your settings, wiki, admin role).
    // Stamping the raw `tg_user` here would make the agent see a
    // non-existent MIRA user → tools refuse with "not an admin".
    // - Shared / GuestOk: resolved_user_id is the MIRA user the sender
    // linked to (or a `guest:telegram:<id>` identity). This is what
    // makes one admin-managed bot safely serve many users — each
    // inbound runs as the linked person, not the bot owner.
    let mut inject = serde_json::Map::new();
    inject.insert(
        "_user_id".to_string(),
        serde_json::Value::String(resolved_user_id.clone()),
    );
    let mut turn_ctx = TurnContext { inject_tool_args: inject, ..TurnContext::default() };
    // per-user MCP filter for inbound Telegram turns. The
    // resolved user's MCP tools are the only ones offered to the agent
    // here. No-op when MCP isn't wired or the user has no servers.
    if let Some(reg) = state.mcp_servers.as_ref() {
        let all = state.agent_core.tools.list_tools();
        if let Some(allow) = reg.allowed_tools_for(&resolved_user_id, &all) {
            turn_ctx.allowed_tool_names = Some(allow);
        }
    }

    // Resolve the persisted thread up-front so the agent can rehydrate this
    // conversation's context on a cache miss (restart / idle eviction); the
    // record-turn below reuses the same id. external_user_id keys on the
    // Telegram user id so each contact gets their own thread under the owner;
    // the default title is stamped on CREATE only.
    let history_conv = state.history.as_ref().and_then(|hist| {
        let default_title = telegram_default_title(&msg);
        hist.find_or_create_external_conversation(
            &resolved_user_id, "telegram", &tg_user, Some(default_title.as_str()),
        ).map_err(|e| warn!("find_or_create_external_conversation failed: {}", e)).ok()
    });
    if let Some(ref conv) = history_conv {
        turn_ctx.conversation_id = Some(conv.id.clone());
    }

    let rx = match state.agent_core
        .process_with_context(
            &session_id, &resolved_user_id, "telegram", &effective_text,
            None, turn_ctx,
        )
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            error!("AgentCore failed for Telegram chat {} (acct={}): {}", chat_id, account, e);
            return;
        }
    };

    let (response_text, _events) = AgentCore::collect_response(rx).await;

    // Record turn — reuses the thread resolved before the turn above.
    if let (Some(ref hist), Some(ref conv)) = (state.history.as_ref(), history_conv.as_ref()) {
        let _ = hist.record_turn(&conv.id, &effective_text, &response_text, None, None);

        // First-message auto-title — same pattern as the web chat handler.
        // Only fires when the persisted thread has 2 messages or fewer (this
        // turn's user+assistant pair), so it doesn't keep re-titling old
        // threads on every fresh message.
        let first_turn = hist
            .get_messages(&conv.id, 5, None)
            .map(|m| m.len())
            .unwrap_or(99) <= 2;
        if first_turn {
            let agent2  = Arc::clone(&state.agent_core);
            let hist2   = Arc::clone(hist);
            let cid     = conv.id.clone();
            let msg_c   = effective_text.clone();
            let preview = crate::server::handlers::chat::derive_title_from_message(&effective_text);
            tokio::spawn(async move {
                crate::server::handlers::chat::generate_auto_title(
                    agent2, hist2, cid, msg_c, preview,
                ).await;
            });
        }
    }

    dispatch_telegram_reply(state, ctx, chat_id, &resolved_user_id, &response_text, inbound_was_voice).await;
}

// Build the initial conversation title for an inbound Telegram chat.
// Used only on row creation; auto-title may overwrite later. Prefer
// the @username (stable, looks right in the sidebar), fall back to
// "FirstName LastName", fall back to the numeric chat id so we always
// have something better than "Untitled".
fn telegram_default_title(msg: &TelegramMessage) -> String {
    let from = msg.from.as_ref();
    if let Some(handle) = from.and_then(|u| u.username.as_deref()).filter(|s| !s.is_empty()) {
        return format!("Telegram — @{handle}");
    }
    let first = from.and_then(|u| u.first_name.as_deref()).unwrap_or("");
    let last  = from.and_then(|u| u.last_name.as_deref()).unwrap_or("");
    let name  = format!("{first} {last}");
    let name  = name.trim();
    if !name.is_empty() {
        return format!("Telegram — {name}");
    }
    format!("Telegram — chat {}", msg.chat.id)
}

// ─────────────────────────────────────────────────────────────────────────────
// Inbound voice — getFile → download → STT.
// ─────────────────────────────────────────────────────────────────────────────
//
// Telegram voice messages need two HTTP round-trips to actually fetch the
// audio: `getFile` returns the relative `file_path` (only valid for ~1h),
// then `https://api.telegram.org/file/bot{token}/{file_path}` serves the
// raw OGG/Opus bytes.

#[derive(Debug, Deserialize)]
struct GetFileResponse {
    ok:     bool,
    result: Option<GetFileResult>,
}

#[derive(Debug, Deserialize)]
struct GetFileResult {
    file_path: Option<String>,
}

async fn transcribe_telegram_voice(
    state: &TelegramState,
    ctx:   &TelegramAccountCtx,
    voice: &TelegramVoice,
) -> Result<String, String> {
    let stt = state.stt.as_ref()
        .ok_or_else(|| "STT not configured at server start".to_string())?;

    // 1. getFile → resolve file_path
    let url = format!(
        "https://api.telegram.org/bot{}/getFile?file_id={}",
        ctx.bot_token, voice.file_id,
    );
    let resp = state.http_client.get(&url).send().await
        .map_err(|e| format!("getFile send: {}", e.without_url()))?;
    let parsed: GetFileResponse = resp.json().await
        .map_err(|e| format!("getFile parse: {}", e))?;
    if !parsed.ok {
        return Err("getFile returned ok=false".to_string());
    }
    let file_path = parsed.result.and_then(|r| r.file_path)
        .ok_or_else(|| "getFile result missing file_path".to_string())?;

    // 2. Download bytes from the file CDN.
    let download_url = format!(
        "https://api.telegram.org/file/bot{}/{}",
        ctx.bot_token, file_path,
    );
    let bytes = state.http_client.get(&download_url).send().await
        .map_err(|e| format!("download send: {}", e.without_url()))?
        .error_for_status()
        .map_err(|e| format!("download status: {}", e))?
        .bytes().await
        .map_err(|e| format!("download body: {}", e))?
        .to_vec();

    // 3. Transcribe.
    let mime = voice.mime_type.as_deref().unwrap_or("audio/ogg");
    let format = AudioInputFormat::from_mime(mime);
    let req = TranscribeRequest {
        audio_bytes: bytes,
        format,
        language:    None,
    };
    let transcript = stt.transcribe(req, None, Some("telegram")).await
        .map_err(|e| e.to_string())?;
    Ok(transcript.text)
}

// ─────────────────────────────────────────────────────────────────────────────
// Reply dispatch — voice prefs first, falls through to plain sendMessage.
// ─────────────────────────────────────────────────────────────────────────────
//
// Voice replies are gated by the layered prefs resolver, keyed on the
// RECIPIENT (the MIRA user this turn resolved to), not the bot owner:
// 1. Recipient's `users.voice_prefs.telegram` (if set)
// 2. Server defaults (`tts.voice_prefs.telegram`)
// 3. Built-in fallback: ResponsePolicy::Never
//
// On a Personal bot the recipient IS the owner, so this is unchanged. On a
// Shared bot each linked member gets THEIR own voice — otherwise everyone
// hears the owner's voice (the "Annika set Michael but still hears Emily" bug).
// A guest session (`guest:telegram:<id>`) has no profile → server defaults.
//
// `OnVoiceInput` requires the inbound message to itself be a voice note.
//
// Even when policy says "send voice," we still need (a) TTS wired up, and
// (b) a routed backend that can produce OGG/Opus. Failure modes — wrong
// codec, synth error, network fault — fall back to plain text so the user
// always gets a reply.

async fn dispatch_telegram_reply(
    state:             &TelegramState,
    ctx:               &TelegramAccountCtx,
    chat_id:           i64,
    recipient_id:      &str,
    text:              &str,
    inbound_was_voice: bool,
) {
    let resolved = resolve_voice_for_user(state, recipient_id, "telegram");
    let want_voice = match resolved.policy {
        ResponsePolicy::Always       => true,
        ResponsePolicy::OnVoiceInput => inbound_was_voice,
        ResponsePolicy::Never        => false,
    };
    debug!(
        "Telegram reply policy: {} (inbound_was_voice={}) → want_voice={}",
        resolved.policy.as_str(), inbound_was_voice, want_voice,
    );

    if want_voice {
        if let Some(buf) = synth_voice_for_channel(
            state.tts.as_ref(),
            "telegram",
            text,
            resolved.voice_id.as_deref(),
        ).await {
            match send_telegram_voice(&state.http_client, &ctx.bot_token, chat_id, &buf, text).await {
                Ok(())  => return,
                Err(e)  => warn!(
                    "Telegram sendVoice for chat {} failed: {} — falling back to text",
                    chat_id, e.without_url(),
                ),
            }
        }
    }
    send_telegram_message(&state.http_client, &ctx.bot_token, chat_id, text).await;
}

// Resolve the voice prefs for a given channel using a specific MIRA user's
// per-user prefs (when available) layered over server defaults. The caller
// passes the RECIPIENT (the user this turn resolved to) so a shared bot honours
// each member's own voice choice; on a Personal bot that's the owner. An
// unknown id (e.g. a `guest:telegram:<id>` session) yields no user prefs and
// falls through to the server defaults.
fn resolve_voice_for_user(
    state:    &TelegramState,
    user_id:  &str,
    channel:  &str,
) -> crate::voice::ResolvedVoice {
    let server_defaults = state.tts.as_ref()
        .map(|t| t.voice_prefs_defaults())
        .unwrap_or_default();
    let user_prefs = state.auth.as_ref()
        .and_then(|a| a.get_user(user_id).ok().flatten())
        .map(|u| parse_user_prefs(u.voice_prefs.as_deref()))
        .unwrap_or_default();
    resolve_voice(channel, Some(&user_prefs), &server_defaults)
}

// Synthesise `text` for `channel` when TTS is wired up. `voice_id_override`
// lets the resolver substitute the user's chosen voice; pass `None` to use
// the backend's default. Returns the buffer iff the backend produced
// OGG/Opus — voice-note containers don't accept WAV.
// // `tts.routing.<channel>` is a *pin* (overrides default backend), not a
// gate. When unpinned we fall through to `tts.default_backend` so a user
// who flips their channel policy to `Always` actually gets voice without
// having to also pin a routing in admin Settings.
pub(crate) async fn synth_voice_for_channel(
    tts:               Option<&TtsService>,
    channel:           &str,
    text:              &str,
    voice_id_override: Option<&str>,
) -> Option<AudioBuffer> {
    let Some(tts) = tts else {
        warn!(
            "tts: voice reply skipped on '{}' — TTS service unavailable, \
             falling back to text",
            channel,
        );
        return None;
    };
    if !tts.enabled() {
        warn!(
            "tts: voice reply skipped on '{}' — `tts.enabled = false`, \
             falling back to text",
            channel,
        );
        return None;
    }

    // Request WAV, not OggOpus: we always transcode to OGG/Opus locally below
    // (`ensure_ogg_opus`), and many OpenAI-compatible TTS servers (e.g. the
    // self-hosted Chatterbox/kokoro backends) only support wav/pcm — asking
    // them for `opus` returns HTTP 400 and silently demotes us to the robotic
    // piper fallback. WAV is universally supported, so the user's configured
    // voice is actually used.
    let buf = match tts.speak(text, voice_id_override, None, Some(OutputFormat::Wav), None, Some(channel)).await {
        Ok(buf) => buf,
        Err(e) => {
            warn!("tts: synth for channel '{}' failed: {} — falling back to text", channel, e);
            return None;
        }
    };
    if matches!(buf.codec, AudioCodec::OggOpus) {
        return Some(buf);
    }
    // Backend produced WAV/PCM (Piper, eSpeak). Transcode to OGG/Opus so
    // Telegram's `sendVoice` will accept the attachment.
    let original = buf.codec.clone();
    match crate::tts::encoder::ensure_ogg_opus(buf) {
        Ok(transcoded) => Some(transcoded),
        Err(e) => {
            warn!(
                "tts: voice reply skipped on '{}' — transcode {:?} → OGG/Opus failed: {}",
                channel, original, e,
            );
            None
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

async fn send_telegram_message(
    client:  &reqwest::Client,
    token:   &str,
    chat_id: i64,
    text:    &str,
) {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let payload = serde_json::json!({
        "chat_id":    chat_id,
        "text":       text,
        "parse_mode": "Markdown"
    });
    match client.post(&url).json(&payload).send().await {
        Ok(_)  => info!("Sent Telegram message to chat {}", chat_id),
        // `without_url()` keeps the bot token (carried in the request URL) out
        // of the log line.
        Err(e) => error!("Failed to send Telegram message to chat {}: {}", chat_id, e.without_url()),
    }
}

// Telegram caption length cap (Bot API doc — voice/audio/video etc.).
const TELEGRAM_CAPTION_MAX: usize = 1024;

// Send a voice note via `sendVoice`. Returns Err on transport / HTTP failure
// so the caller can fall back to plain text. The transcript travels in the
// `caption` field per the channel UX rule (audio + text together). Captions
// longer than the API cap are truncated with an ellipsis — the full text is
// already implicit in the audio, the caption is just a glanceable summary.
async fn send_telegram_voice(
    client:  &reqwest::Client,
    token:   &str,
    chat_id: i64,
    audio:   &AudioBuffer,
    caption: &str,
) -> Result<(), reqwest::Error> {
    let url = format!("https://api.telegram.org/bot{}/sendVoice", token);

    let cap = if caption.chars().count() > TELEGRAM_CAPTION_MAX {
        let mut truncated: String = caption.chars().take(TELEGRAM_CAPTION_MAX - 1).collect();
        truncated.push('…');
        truncated
    } else {
        caption.to_string()
    };

    let voice_part = reqwest::multipart::Part::bytes(audio.bytes.clone())
        .file_name("voice.ogg")
        .mime_str("audio/ogg")
        .expect("static mime is valid");

    let form = reqwest::multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .text("caption", cap)
        .part("voice", voice_part);

    let resp = client.post(&url).multipart(form).send().await?;
    match resp.error_for_status() {
        Ok(_)  => {
            info!("Sent Telegram voice note to chat {}", chat_id);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// ── Polling daemon ───────────────────────────────────────────────────────────
//
// Alternative transport to the webhook handler above. Lets users behind
// NAT / without a public HTTPS endpoint still receive Telegram messages —
// no tunnel needed. Long-polls Telegram's getUpdates with `timeout=N`,
// processes each batch through `process_message_for_account`, and tracks
// the offset locally so a restart resumes where we left off (Telegram
// retains undelivered updates for ~24h).

// One update envelope from `getUpdates`. We only deserialize the
// fields we use; Telegram sends many more (edited_message, callback_query,
// inline_query, …) that we ignore by default.
#[derive(Debug, Deserialize)]
struct PolledUpdate {
    update_id: i64,
    message:   Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ok:     bool,
    #[serde(default)]
    result: Vec<PolledUpdate>,
    #[serde(default)]
    description: Option<String>,
}

// Spawn a tokio task that long-polls Telegram for this account. Returns
// the JoinHandle so the channel manager can abort it on stop/restart.
// // `poll_timeout_secs` is the long-poll hold time passed to Telegram;
// 30s is the standard recommendation (Telegram itself caps at 50s).
// When no updates arrive within the window Telegram returns an empty
// result; we immediately re-issue the call so updates surface within
// 1s of arrival at Telegram's servers.
// // On startup we issue `deleteWebhook` so polling and webhook delivery
// don't compete — Telegram refuses to return updates via getUpdates if
// a webhook is registered.
pub fn spawn_telegram_poller(
    state:             TelegramState,
    ctx:               TelegramAccountCtx,
    poll_timeout_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let account = ctx.account_id.clone();
        // Best-effort webhook reset. If Telegram answers an error we log
        // it and proceed — getUpdates will fail loudly if a webhook
        // really is still set, and the operator will see it.
        let delete_webhook_url = format!(
            "https://api.telegram.org/bot{}/deleteWebhook?drop_pending_updates=false",
            ctx.bot_token,
        );
        match state.http_client.get(&delete_webhook_url).send().await {
            Ok(r) if r.status().is_success() => {
                info!("Telegram polling [acct={}]: deleteWebhook ok", account);
            }
            Ok(r) if r.status().as_u16() == 401 || r.status().as_u16() == 404 => {
                // Same root cause as the getUpdates 401 below — bad
                // token. Fail fast here too so we don't bother with a
                // pointless getUpdates that'll just hit the same wall.
                error!(
                    "Telegram polling [acct={}]: deleteWebhook {} — bot token \
                     is invalid or revoked. Fix the token in Channels and \
                     restart, or get a fresh one from @BotFather (/token in \
                     Telegram). Stopping poller.",
                    account, r.status(),
                );
                return;
            }
            Ok(r) => {
                warn!("Telegram polling [acct={}]: deleteWebhook {} (proceeding)",
                      account, r.status());
            }
            Err(e) => {
                warn!("Telegram polling [acct={}]: deleteWebhook failed: {} (proceeding)",
                      account, e.without_url());
            }
        }

        // Long-poll loop. `offset` is one past the highest update_id we
        // processed — Telegram acks deliveries by us advancing it.
        let mut offset: i64 = 0;
        let mut backoff_secs: u64 = 1;
        let max_backoff: u64 = 60;

        info!("Telegram polling [acct={}]: starting (timeout={}s)", account, poll_timeout_secs);

        loop {
            let url = format!(
                "https://api.telegram.org/bot{}/getUpdates?offset={}&timeout={}",
                ctx.bot_token, offset, poll_timeout_secs,
            );
            // Use a per-request timeout slightly longer than the long-poll
            // hold so a Telegram-side timeout doesn't trip our client.
            let req_timeout = std::time::Duration::from_secs(poll_timeout_secs + 15);
            // Belt-and-suspenders: wrap the whole call in an outer timeout a bit
            // longer than the per-request one. Even if reqwest somehow fails to
            // honour its own timeout (a wedged connection, a buggy route), this
            // guarantees the loop ticks and retries rather than going silent —
            // the failure mode we hit when WSL2 IPv6 egress died.
            let poll = state.http_client.get(&url).timeout(req_timeout).send();
            let resp = match tokio::time::timeout(req_timeout + std::time::Duration::from_secs(10), poll).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    // `without_url()` strips the request URL — which carries the
                    // bot token — out of the error's Display before we log it.
                    warn!(
                        "Telegram polling [acct={}]: HTTP error: {} — backing off {}s",
                        account, e.without_url(), backoff_secs,
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(max_backoff);
                    continue;
                }
                Err(_elapsed) => {
                    warn!(
                        "Telegram polling [acct={}]: request exceeded {}s with no response \
                         (network path stalled) — backing off {}s",
                        account, (req_timeout.as_secs() + 10), backoff_secs,
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(max_backoff);
                    continue;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                // 401/404 from Telegram = invalid bot token. Retrying
                // can't help; operator must fix the token. Exit the
                // loop so the warning doesn't flood the watchdog
                // dashboard every minute forever.
                if status.as_u16() == 401 || status.as_u16() == 404 {
                    error!(
                        "Telegram polling [acct={}]: HTTP {} from Telegram \
                         — bot token is invalid or revoked. Fix the token in \
                         Channels and restart, or get a fresh one from \
                         @BotFather (/token in Telegram). Stopping poller. ({})",
                        account, status, body,
                    );
                    return;
                }
                warn!(
                    "Telegram polling [acct={}]: HTTP {} — backing off {}s ({})",
                    account, status, backoff_secs, body,
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff);
                continue;
            }

            let parsed: GetUpdatesResponse = match resp.json().await {
                Ok(p) => p,
                Err(e) => {
                    warn!(
                        "Telegram polling [acct={}]: parse error: {} — backing off {}s",
                        account, e, backoff_secs,
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(max_backoff);
                    continue;
                }
            };

            if !parsed.ok {
                warn!(
                    "Telegram polling [acct={}]: API error: {:?} — backing off {}s",
                    account, parsed.description, backoff_secs,
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff);
                continue;
            }

            // Success — reset backoff and process each update.
            backoff_secs = 1;
            for update in parsed.result {
                offset = update.update_id + 1;
                let Some(msg) = update.message else {
                    debug!(
                        "Telegram polling [acct={}]: update {} has no message field, skipping",
                        account, update.update_id,
                    );
                    continue;
                };
                process_message_for_account(&state, &ctx, msg).await;
            }
        }
    })
}
