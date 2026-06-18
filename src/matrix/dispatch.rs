// SPDX-License-Identifier: AGPL-3.0-or-later

// src/matrix/dispatch.rs
//
// Process one inbound Matrix `m.room.message` event end-to-end. Mirrors
// `discord::dispatch::process_discord_message` exactly — same trusted-
// identity / routing model (R1+R2), same MCP per-user filter, same
// history record + first-message auto-title, same shared-bot link-code
// flow. The only channel-specific differences: the external id is the
// `room_id` (postable directly), and the sender id is a Matrix MXID like
// `@alice:hs.tld`.

use std::sync::Arc;

use tracing::{debug, error, info, warn};

use crate::agent::{AgentCore, TurnContext};
use crate::auth::LocalAuthService;
use crate::history::HistoryStore;
use crate::mcp::McpServerRegistry;
use crate::web::LiveConfig;

use super::api::send_message;

/// Per-account immutable bits cloned into each dispatch. Cheap to clone.
#[derive(Clone)]
pub struct MatrixAccountCtx {
    pub account_id:    String,
    pub owner_user_id: String,
    pub homeserver:    String,
    pub access_token:  String,
    /// The bot's own MXID (`@mira:hs.tld`), resolved via /whoami at
    /// connect. Used to skip our own echoed timeline events.
    pub bot_mxid:      String,
    /// When true, only act on messages that mention the bot's MXID or
    /// localpart. Recommended for shared/group rooms.
    pub mention_only:  bool,
    pub routing_mode:  crate::channel_accounts::RoutingMode,
}

/// Long-lived deps cloned into each dispatch.
#[derive(Clone)]
pub struct MatrixDispatcherDeps {
    pub agent_core:  Arc<AgentCore>,
    pub history:     Option<Arc<HistoryStore>>,
    pub auth:        Option<Arc<LocalAuthService>>,
    pub live_config: Option<Arc<LiveConfig>>,
    pub mcp_servers: Option<Arc<McpServerRegistry>>,
    pub http_client: reqwest::Client,
    pub identity:    Option<Arc<crate::channel_identity::IdentityStore>>,
    pub link_codes:  Option<Arc<crate::channel_identity::LinkCodeStore>>,
}

/// A normalised inbound message the sync loop hands to the dispatcher.
pub struct InboundMatrix {
    pub room_id: String,
    pub sender:  String,
    pub body:    String,
}

/// Monotonic-ish transaction seed for outbound idempotency. Matrix wants
/// a unique txn id per send; we derive it from the inbound event so a
/// duplicated inbound (server retry) maps to the same outbound txn.
fn txn_seed(room_id: &str, sender: &str) -> u64 {
    // Cheap FNV-1a over room+sender — not for security, just a stable
    // spread so concurrent replies in different rooms don't collide.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in room_id.bytes().chain(sender.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub async fn process_matrix_message(
    deps: MatrixDispatcherDeps,
    ctx:  MatrixAccountCtx,
    msg:  InboundMatrix,
) {
    // ── Filter: self-echo + empty ─────────────────────────────────────
    if msg.sender == ctx.bot_mxid {
        debug!(account = %ctx.account_id, "skip: author is us");
        return;
    }
    let content = msg.body.trim();
    if content.is_empty() {
        debug!(account = %ctx.account_id, "skip: empty body");
        return;
    }

    // ── MIRA-wide kill switch ─────────────────────────────────────────
    if let Some(cfg) = deps.live_config.as_ref() {
        if !cfg.get().await.channels.matrix.enabled {
            debug!(account = %ctx.account_id, "skip: channels.matrix.enabled is false");
            return;
        }
    }

    // ── mention_only gate ─────────────────────────────────────────────
    //
    // Matrix has no structured per-message mention list in the base
    // event, so we check the body for the bot's MXID or its localpart
    // (`@mira:hs.tld` → also matches a bare `mira`). Cheap + good enough;
    // a future m.mentions parse can tighten this.
    if ctx.mention_only && !mentions_bot(content, &ctx.bot_mxid) {
        debug!(account = %ctx.account_id, "skip: mention_only set + bot not mentioned");
        return;
    }

    info!("Matrix [acct={}] room={} sender={}: {}",
          ctx.account_id, msg.room_id, msg.sender,
          &content[..content.len().min(80)]);

    let effective_text = strip_bot_mention(content, &ctx.bot_mxid);
    if effective_text.trim().is_empty() {
        debug!(account = %ctx.account_id, "skip: only a mention with no text");
        return;
    }

    // ── R1+R2: resolve which MIRA user this turn runs as ──────────────
    // Identical structure to the Discord dispatcher; the channel string
    // is "matrix" and the external id is the sender MXID.
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
            match idstore.lookup("matrix", &msg.sender) {
                Ok(Some(uid)) => uid,
                Ok(None) => {
                    if let Some(code) = crate::channel_identity::link_codes::looks_like_link_code(&effective_text) {
                        match deps.link_codes.as_ref().and_then(|cs| cs.consume(code, "matrix").ok().flatten()) {
                            Some(uid) => {
                                if let Err(e) = idstore.link(&uid, "matrix", &msg.sender) {
                                    warn!(account = %ctx.account_id,
                                          "link claim ok but persist failed: {}", e);
                                    reply(&deps, &ctx, &msg,
                                        "Sorry — link was accepted but I couldn't save it. Try again or ask the admin.").await;
                                    return;
                                }
                                info!(account = %ctx.account_id, user = %uid, external = %msg.sender,
                                      "matrix identity linked via code");
                                reply(&deps, &ctx, &msg,
                                    "✅ Linked! You can talk to me normally now.").await;
                                return;
                            }
                            None => {
                                reply(&deps, &ctx, &msg,
                                    "That link code didn't match — generate a fresh one in MIRA → Settings → My Channels and try again within 10 minutes.").await;
                                return;
                            }
                        }
                    }
                    if matches!(ctx.routing_mode, RoutingMode::GuestOk) {
                        format!("guest:matrix:{}", msg.sender)
                    } else {
                        reply(&deps, &ctx, &msg,
                            "Hi! I don't recognise you yet. Open MIRA → Settings → My Channels → Link Matrix, copy the LINK-XXXX-XXXX code, and send it to me here.").await;
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

    // Session id keys on (resolved user, room) — same shape as Discord.
    let session_id = format!("mx-{}-{}", resolved_user_id, msg.room_id);

    if let Some(bus) = deps.agent_core.event_bus() {
        bus.emit_named(
            crate::events::names::MESSAGE_RECEIVED,
            Some(msg.sender.clone()),
            serde_json::json!({
                "user_id":         msg.sender,
                "channel":         "matrix",
                "conversation_id": session_id,
                "text":            effective_text,
            }),
        );
    }

    let _ = deps.auth; // reserved for future per-account hooks
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
        let default_title = format!("Matrix — {}", msg.room_id);
        hist.find_or_create_external_conversation(
            &resolved_user_id, "matrix", &msg.room_id, Some(default_title.as_str()),
        ).map_err(|e| warn!("find_or_create_external_conversation failed: {}", e)).ok()
    });
    if let Some(ref conv) = history_conv {
        turn_ctx.conversation_id = Some(conv.id.clone());
    }

    let rx = match deps.agent_core
        .process_with_context(&session_id, &resolved_user_id, "matrix", &effective_text, None, turn_ctx)
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            error!("AgentCore failed for Matrix room {} (acct={}): {}", msg.room_id, ctx.account_id, e);
            return;
        }
    };
    let (response_text, _events) = AgentCore::collect_response(rx).await;

    // Record turn — external_user_id = room_id (one thread per room under
    // the resolved user), mirroring Discord's channel_id keying.
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
    reply(&deps, &ctx, &msg, &response_text).await;
}

async fn reply(deps: &MatrixDispatcherDeps, ctx: &MatrixAccountCtx, msg: &InboundMatrix, text: &str) {
    if let Err(e) = send_message(
        &deps.http_client, &ctx.homeserver, &ctx.access_token,
        &msg.room_id, text, txn_seed(&msg.room_id, &msg.sender),
    ).await {
        warn!(account = %ctx.account_id, "Matrix outbound failed: {}", e);
    }
}

/// True if `body` references the bot's MXID or its localpart. MXID form
/// is `@localpart:server`; we match the full id or a word-boundary'd
/// localpart so "mira" matches but "miracle" doesn't.
fn mentions_bot(body: &str, bot_mxid: &str) -> bool {
    if body.contains(bot_mxid) {
        return true;
    }
    if let Some(local) = mxid_localpart(bot_mxid) {
        return contains_word(body, local);
    }
    false
}

fn strip_bot_mention(body: &str, bot_mxid: &str) -> String {
    let mut out = body.replace(bot_mxid, "");
    if let Some(local) = mxid_localpart(bot_mxid) {
        // Strip a leading "localpart:" / "localpart " address prefix only;
        // leave mid-sentence uses intact so we don't mangle content.
        let trimmed = out.trim_start();
        for sep in [": ", ", ", " "] {
            let prefix = format!("{}{}", local, sep);
            if trimmed.starts_with(&prefix) {
                out = trimmed[prefix.len()..].to_string();
                break;
            }
        }
    }
    out.trim().to_string()
}

fn mxid_localpart(mxid: &str) -> Option<&str> {
    mxid.strip_prefix('@').and_then(|s| s.split(':').next())
}

fn contains_word(haystack: &str, word: &str) -> bool {
    haystack.split(|c: char| !c.is_alphanumeric())
        .any(|tok| tok.eq_ignore_ascii_case(word))
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localpart_extraction() {
        assert_eq!(mxid_localpart("@mira:hs.tld"), Some("mira"));
        assert_eq!(mxid_localpart("notanmxid"), None);
    }

    #[test]
    fn mentions_match_full_mxid_or_localpart_word() {
        let bot = "@mira:hs.tld";
        assert!(mentions_bot("hey @mira:hs.tld can you", bot));
        assert!(mentions_bot("mira help me", bot));
        assert!(mentions_bot("MIRA help", bot)); // case-insensitive
        assert!(!mentions_bot("a miracle happened", bot)); // word boundary
        assert!(!mentions_bot("nothing here", bot));
    }

    #[test]
    fn strip_removes_address_prefix_not_midsentence() {
        let bot = "@mira:hs.tld";
        assert_eq!(strip_bot_mention("mira: hello there", bot), "hello there");
        assert_eq!(strip_bot_mention("mira hello", bot), "hello");
        assert_eq!(strip_bot_mention("@mira:hs.tld do it", bot), "do it");
        // Mid-sentence localpart left intact.
        assert_eq!(strip_bot_mention("ask mira about it", bot), "ask mira about it");
    }

    #[test]
    fn txn_seed_is_stable_per_room_sender() {
        let a = txn_seed("!r:hs", "@u:hs");
        let b = txn_seed("!r:hs", "@u:hs");
        let c = txn_seed("!r:hs", "@v:hs");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
