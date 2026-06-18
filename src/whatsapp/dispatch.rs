// SPDX-License-Identifier: AGPL-3.0-or-later

// src/whatsapp/dispatch.rs
//
// Process one inbound WhatsApp text message end-to-end. Mirrors
// `discord::dispatch::process_discord_message` — same R1+R2 routing,
// MCP per-user filter, history record + auto-title, link-code flow.
// Channel-specific bits: the external id is the sender's phone (`wa_id`),
// outbound goes to the same phone via the Cloud API, and replies are
// always inside the 24h window (the user just messaged us).

use std::sync::Arc;

use tracing::{debug, error, info, warn};

use crate::agent::{AgentCore, TurnContext};
use crate::auth::LocalAuthService;
use crate::history::HistoryStore;
use crate::mcp::McpServerRegistry;
use crate::web::LiveConfig;

use super::api::send_text;

/// Per-account context consumed by the shared webhook handler. Cheap to
/// clone — the lookup map stores one per account id.
#[derive(Clone, Debug)]
pub struct WhatsAppAccountCtx {
    pub account_id:      String,
    pub owner_user_id:   String,
    /// The Cloud API phone-number id messages are sent *from*.
    pub phone_number_id: String,
    pub access_token:    String,
    /// App secret for X-Hub-Signature-256 verification. None = unverified
    /// (operator opted out; we warn at startup).
    pub app_secret:      Option<String>,
    /// Token echoed back during the GET subscription handshake.
    pub verify_token:    String,
    pub mention_only:    bool,
    pub routing_mode:    crate::channel_accounts::RoutingMode,
}

/// Long-lived deps the handler clones into each dispatch.
#[derive(Clone)]
pub struct WhatsAppDispatcherDeps {
    pub agent_core:  Arc<AgentCore>,
    pub history:     Option<Arc<HistoryStore>>,
    pub auth:        Option<Arc<LocalAuthService>>,
    pub live_config: Option<Arc<LiveConfig>>,
    pub mcp_servers: Option<Arc<McpServerRegistry>>,
    pub http_client: reqwest::Client,
    pub identity:    Option<Arc<crate::channel_identity::IdentityStore>>,
    pub link_codes:  Option<Arc<crate::channel_identity::LinkCodeStore>>,
}

/// One normalised inbound text message.
pub struct InboundWhatsApp {
    /// Sender phone (international, no `+`) — both the conversation key and
    /// the outbound recipient.
    pub from:         String,
    pub body:         String,
    pub display_name: Option<String>,
}

pub async fn process_whatsapp_message(
    deps: WhatsAppDispatcherDeps,
    ctx:  WhatsAppAccountCtx,
    msg:  InboundWhatsApp,
) {
    let content = msg.body.trim();
    if content.is_empty() {
        debug!(account = %ctx.account_id, "skip: empty body");
        return;
    }

    // ── MIRA-wide kill switch ─────────────────────────────────────────
    if let Some(cfg) = deps.live_config.as_ref() {
        if !cfg.get().await.channels.whatsapp.enabled {
            debug!(account = %ctx.account_id, "skip: channels.whatsapp.enabled is false");
            return;
        }
    }

    // ── mention_only gate ─────────────────────────────────────────────
    // WhatsApp has no @-mention concept in 1:1 chats; in group contexts a
    // mention is just the bot's number/name in the text. We check for a
    // bare "mira" word when mention_only is set (cheap, good enough).
    if ctx.mention_only && !contains_word(content, "mira") {
        debug!(account = %ctx.account_id, "skip: mention_only set + 'mira' not in text");
        return;
    }

    info!("WhatsApp [acct={}] from={}: {}",
          ctx.account_id, msg.from, &content[..content.len().min(80)]);

    let effective_text = content.to_string();

    // ── R1+R2: resolve which MIRA user this turn runs as ──────────────
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
            match idstore.lookup("whatsapp", &msg.from) {
                Ok(Some(uid)) => uid,
                Ok(None) => {
                    if let Some(code) = crate::channel_identity::link_codes::looks_like_link_code(&effective_text) {
                        match deps.link_codes.as_ref().and_then(|cs| cs.consume(code, "whatsapp").ok().flatten()) {
                            Some(uid) => {
                                if let Err(e) = idstore.link(&uid, "whatsapp", &msg.from) {
                                    warn!(account = %ctx.account_id, "link claim ok but persist failed: {}", e);
                                    reply(&deps, &ctx, &msg.from,
                                        "Sorry — link was accepted but I couldn't save it. Try again or ask the admin.").await;
                                    return;
                                }
                                info!(account = %ctx.account_id, user = %uid, external = %msg.from,
                                      "whatsapp identity linked via code");
                                reply(&deps, &ctx, &msg.from,
                                    "✅ Linked! You can talk to me normally now.").await;
                                return;
                            }
                            None => {
                                reply(&deps, &ctx, &msg.from,
                                    "That link code didn't match — generate a fresh one in MIRA → Settings → My Channels and try again within 10 minutes.").await;
                                return;
                            }
                        }
                    }
                    if matches!(ctx.routing_mode, RoutingMode::GuestOk) {
                        format!("guest:whatsapp:{}", msg.from)
                    } else {
                        reply(&deps, &ctx, &msg.from,
                            "Hi! I don't recognise you yet. Open MIRA → Settings → My Channels → Link WhatsApp, copy the LINK-XXXX-XXXX code, and send it to me here.").await;
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

    let session_id = format!("wa-{}-{}", resolved_user_id, msg.from);

    if let Some(bus) = deps.agent_core.event_bus() {
        bus.emit_named(
            crate::events::names::MESSAGE_RECEIVED,
            Some(msg.from.clone()),
            serde_json::json!({
                "user_id":         msg.from,
                "channel":         "whatsapp",
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
            Some(n) => format!("WhatsApp — {}", n),
            None    => format!("WhatsApp — {}", msg.from),
        };
        hist.find_or_create_external_conversation(
            &resolved_user_id, "whatsapp", &msg.from, Some(default_title.as_str()),
        ).map_err(|e| warn!("find_or_create_external_conversation failed: {}", e)).ok()
    });
    if let Some(ref conv) = history_conv {
        turn_ctx.conversation_id = Some(conv.id.clone());
    }

    let rx = match deps.agent_core
        .process_with_context(&session_id, &resolved_user_id, "whatsapp", &effective_text, None, turn_ctx)
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            error!("AgentCore failed for WhatsApp {} (acct={}): {}", msg.from, ctx.account_id, e);
            return;
        }
    };
    let (response_text, _events) = AgentCore::collect_response(rx).await;

    // Record turn — external_user_id = sender phone (one thread per
    // contact under the resolved user).
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
    reply(&deps, &ctx, &msg.from, &response_text).await;
}

async fn reply(deps: &WhatsAppDispatcherDeps, ctx: &WhatsAppAccountCtx, to: &str, text: &str) {
    if let Err(e) = send_text(
        &deps.http_client, &ctx.phone_number_id, &ctx.access_token, to, text,
    ).await {
        warn!(account = %ctx.account_id, "WhatsApp outbound failed: {}", e);
    }
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
    fn contains_word_is_boundary_aware() {
        assert!(contains_word("hey mira help", "mira"));
        assert!(contains_word("MIRA", "mira"));
        assert!(!contains_word("a miracle", "mira"));
        assert!(!contains_word("nothing", "mira"));
    }
}
