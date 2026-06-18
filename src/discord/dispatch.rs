// SPDX-License-Identifier: AGPL-3.0-or-later

// src/discord/dispatch.rs
//
// Process one Discord MESSAGE_CREATE event end-to-end. Mirrors
// `server::handlers::telegram::process_message_for_account` — same shape,
// same trusted-identity injection model, same MCP per-user filter,
// same history record + first-message auto-title pattern. Read the
// matching Telegram code if anything here is unclear; the comments here
// only cover the Discord-specific bits.

use std::sync::Arc;

use tracing::{debug, error, info, warn};

use crate::agent::{AgentCore, TurnContext};
use crate::auth::LocalAuthService;
use crate::history::HistoryStore;
use crate::mcp::McpServerRegistry;
use crate::web::LiveConfig;

use super::api::post_message;
use super::gateway::DiscordAccountCtx;
use super::types::MessageCreate;

/// Long-lived deps every gateway connection clones into its dispatcher
/// spawns. All fields are Arcs or Options-of-Arcs — cheap to clone.
#[derive(Clone)]
pub struct DiscordDispatcherDeps {
    pub agent_core:  Arc<AgentCore>,
    pub history:     Option<Arc<HistoryStore>>,
    pub auth:        Option<Arc<LocalAuthService>>,
    pub live_config: Option<Arc<LiveConfig>>,
    pub mcp_servers: Option<Arc<McpServerRegistry>>,
    pub http_client: reqwest::Client,
    /// R1+R2 — sender-id → MIRA user lookup. Read on the hot path for
    /// `Shared` / `GuestOk` bots; `None` forces every bot to fall back
    /// to `Personal` semantics (matches the pre-R1+R2 behaviour).
    pub identity:    Option<Arc<crate::channel_identity::IdentityStore>>,
    /// R1+R2 — pending one-time link codes. Read when an unmapped sender
    /// posts a `LINK-XXXX-XXXX` string so we can claim the identity.
    pub link_codes:  Option<Arc<crate::channel_identity::LinkCodeStore>>,
}

pub async fn process_discord_message(
    deps:       DiscordDispatcherDeps,
    ctx:        DiscordAccountCtx,
    msg:        MessageCreate,
    bot_user_id_seen: Option<String>,
) {
    // ── Filter: self-echo, bots, empty content ───────────────────────
    //
    // Discord's gateway echoes our own outbound messages right back to
    // us as MESSAGE_CREATE events; without this guard we'd loop forever.
    // We skip on three signals (any one is sufficient):
    //   1. The bot's own user id (known after READY at the latest).
    //   2. The Application snowflake if the operator pre-filled it.
    //   3. The catch-all `author.bot` flag for other bots in the room.
    if msg.author.bot {
        debug!(account = %ctx.account_id, "skip: author is a bot");
        return;
    }
    if let Some(ref bot_id) = bot_user_id_seen {
        if &msg.author.id == bot_id {
            debug!(account = %ctx.account_id, "skip: author is us (bot_user_id match)");
            return;
        }
    }

    let content = msg.content.trim();
    if content.is_empty() {
        // Empty content typically means MESSAGE_CONTENT intent was not
        // granted in the Developer Portal — every text body is stripped.
        debug!(account = %ctx.account_id,
               "skip: empty content (privileged MESSAGE_CONTENT intent likely missing)");
        return;
    }

    // ── MIRA-wide kill switch ─────────────────────────────────────────
    if let Some(cfg) = deps.live_config.as_ref() {
        if !cfg.get().await.channels.discord.enabled {
            debug!(account = %ctx.account_id, "skip: channels.discord.enabled is false");
            return;
        }
    }

    // ── mention_only gate ────────────────────────────────────────────
    //
    // For shared/public servers we don't want MIRA to answer every
    // unrelated message. When `mention_only` is set, we require the
    // bot user id to appear in the `mentions` array. (Discord's
    // `mentions` is the parsed structured list — cheaper + safer than
    // regexing `content` for `<@SNOWFLAKE>` syntax.)
    if ctx.mention_only {
        let we_were_mentioned = bot_user_id_seen.as_deref()
            .map(|bot_id| msg.mentions.iter().any(|m| m.id == bot_id))
            .unwrap_or(false);
        if !we_were_mentioned {
            debug!(account = %ctx.account_id, "skip: mention_only set + we weren't mentioned");
            return;
        }
    }

    info!("Discord [acct={}] guild={:?} channel={} user={}: {}",
          ctx.account_id, msg.guild_id, msg.channel_id, msg.author.id,
          &content[..content.len().min(80)]);

    // Strip our own @-mention from the content so the LLM doesn't see
    // a literal `<@123>` token at the front of every prompt. We only
    // strip exact-id mentions, not nicknames (which would require an
    // extra Discord round-trip per message).
    let effective_text = if let Some(ref bot_id) = bot_user_id_seen {
        strip_bot_mentions(content, bot_id)
    } else {
        content.to_string()
    };
    if effective_text.trim().is_empty() {
        debug!(account = %ctx.account_id, "skip: content was only an @-mention with no text");
        return;
    }

    // ── R1+R2: resolve which MIRA user this turn runs as ───────────────
    //
    // The default `Personal` path runs everything as `ctx.owner_user_id`
    // (the bot owner) and is exactly what shipped in D1+D2 — that branch
    // returns immediately and stays as the hot path for the common
    // single-user case.
    //
    // `Shared` and `GuestOk` look the sender's Discord snowflake up in
    // the identity table. On a miss:
    //   * `Shared`  — if the message body is a `LINK-XXXX-XXXX` code,
    //                 consume it (creating the link + replying); if not,
    //                 send a one-line "you need to link first" hint and
    //                 stop. Either way we don't invoke the agent.
    //   * `GuestOk` — fall through to a deterministic "guest:<snowflake>"
    //                 identity. Tools that gate on user identity will
    //                 see this string and can refuse or limit accordingly.
    use crate::channel_accounts::RoutingMode;
    let resolved_user_id: String = match ctx.routing_mode {
        RoutingMode::Personal => ctx.owner_user_id.clone(),
        RoutingMode::Shared | RoutingMode::GuestOk => 'resolve: {
            // Identity store is required for any non-Personal routing.
            // When missing (failed to open auth.db at startup) we fall
            // closed for Shared and back to owner for GuestOk.
            let idstore = match deps.identity.as_ref() {
                Some(s) => s,
                None => {
                    warn!(account = %ctx.account_id,
                          "shared/guest_ok bot but no IdentityStore wired — degrading");
                    if matches!(ctx.routing_mode, RoutingMode::GuestOk) {
                        break 'resolve ctx.owner_user_id.clone();
                    }
                    return;
                }
            };
            match idstore.lookup("discord", &msg.author.id) {
                Ok(Some(uid)) => uid,
                Ok(None) => {
                    // Maybe a link code? Codes look like LINK-XXXX-XXXX
                    // and are accepted in any Discord channel the bot
                    // can see (DM or guild).
                                        if let Some(code) = crate::channel_identity::link_codes::looks_like_link_code(&effective_text) {
                        match deps.link_codes.as_ref().and_then(|cs| cs.consume(code, "discord").ok().flatten()) {
                            Some(uid) => {
                                // Link the identity.
                                if let Err(e) = idstore.link(&uid, "discord", &msg.author.id) {
                                    warn!(account = %ctx.account_id,
                                          "link claim succeeded but persist failed: {}", e);
                                    let _ = post_message(&deps.http_client, &ctx.bot_token,
                                        &msg.channel_id,
                                        "Sorry — link was accepted but I couldn't save it. Try again or ask the admin.",
                                    ).await;
                                    return;
                                }
                                info!(account = %ctx.account_id, user = %uid,
                                      external = %msg.author.id,
                                      "discord identity linked via code");
                                let _ = post_message(&deps.http_client, &ctx.bot_token,
                                    &msg.channel_id,
                                    "✅ Linked! You can talk to me normally now.",
                                ).await;
                                return;
                            }
                            None => {
                                let _ = post_message(&deps.http_client, &ctx.bot_token,
                                    &msg.channel_id,
                                    "That link code didn't match — generate a fresh one in MIRA → Settings → My Channels and try again within 10 minutes.",
                                ).await;
                                return;
                            }
                        }
                    }
                    // Not a code, not a linked sender.
                    if matches!(ctx.routing_mode, RoutingMode::GuestOk) {
                        // Guest path — synthesise a stable per-sender id.
                        format!("guest:discord:{}", msg.author.id)
                    } else {
                        // Shared — refuse and prompt for linking.
                        let _ = post_message(&deps.http_client, &ctx.bot_token,
                            &msg.channel_id,
                            "Hi! I don't recognise you yet. Open MIRA → Settings → My Channels → Link Discord, copy the LINK-XXXX-XXXX code, and DM it to me.",
                        ).await;
                        return;
                    }
                }
                Err(e) => {
                    warn!(account = %ctx.account_id,
                          "identity lookup failed: {}", e);
                    return;
                }
            }
        }
    };

    // Session id bakes in the resolved MIRA user so two users sharing
    // a server (Shared mode) — or one user across personal bots — never
    // cross sessions even if remote channel ids collide.
    let session_id = format!("dc-{}-{}", resolved_user_id, msg.channel_id);

    if let Some(bus) = deps.agent_core.event_bus() {
        bus.emit_named(
            crate::events::names::MESSAGE_RECEIVED,
            Some(msg.author.id.clone()),
            serde_json::json!({
                "user_id":         msg.author.id,
                "channel":         "discord",
                "conversation_id": session_id,
                "text":            effective_text,
            }),
        );
    }

    // Trusted identity injection. In Personal mode resolved_user_id is
    // ctx.owner_user_id (every inbound runs as bot owner — the v1
    // single-user model). In Shared/GuestOk it's the per-sender MIRA
    // user the identity table mapped to.
    let _ = deps.auth; // reserved for future per-channel-account hooks
    let mut inject = serde_json::Map::new();
    inject.insert(
        "_user_id".to_string(),
        serde_json::Value::String(resolved_user_id.clone()),
    );
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
        let default_title = discord_default_title(&msg);
        hist.find_or_create_external_conversation(
            &resolved_user_id, "discord", &msg.channel_id, Some(default_title.as_str()),
        ).map_err(|e| warn!("find_or_create_external_conversation failed: {}", e)).ok()
    });
    if let Some(ref conv) = history_conv {
        turn_ctx.conversation_id = Some(conv.id.clone());
    }

    let rx = match deps.agent_core
        .process_with_context(
            &session_id,
            &resolved_user_id,
            "discord",
            &effective_text,
            None,
            turn_ctx,
        )
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            error!("AgentCore failed for Discord channel {} (acct={}): {}",
                   msg.channel_id, ctx.account_id, e);
            return;
        }
    };

    let (response_text, _events) = AgentCore::collect_response(rx).await;

    // Record turn — external_user_id keys on the Discord channel id so
    // each channel/DM gets its own thread under the owning user. Mirrors
    // the Telegram pattern (chat_id). We pick channel_id rather than
    // author.id so a server-channel conversation is one thread even
    // when multiple people post in it; DMs naturally have a 1:1
    // channel↔user mapping so this also works for those.
    if let (Some(ref hist), Some(ref conv)) = (deps.history.as_ref(), history_conv.as_ref()) {
        let _ = hist.record_turn(&conv.id, &effective_text, &response_text, None, None);

        // First-message auto-title — same pattern as Telegram.
        let first_turn = hist
            .get_messages(&conv.id, 5, None)
            .map(|m| m.len())
            .unwrap_or(99) <= 2;
        if first_turn {
            let agent2  = Arc::clone(&deps.agent_core);
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

    // ── Outbound reply ───────────────────────────────────────────────
    //
    // D2 ships a minimal text reply (no embeds, no attachments yet —
    // those land in D3+ once the companion/automations dispatchers
    // route through a `DiscordChannel` trait impl). Failures are warn-
    // logged but not retried: Discord's "the user can scroll back and
    // ask again" UX means this is a survivable error.
    if response_text.is_empty() {
        debug!(account = %ctx.account_id,
               "agent returned empty response — sending nothing");
        return;
    }
    if let Err(e) = post_message(
        &deps.http_client,
        &ctx.bot_token,
        &msg.channel_id,
        &response_text,
    ).await {
        warn!(account = %ctx.account_id, "Discord outbound failed: {}", e);
    }
}

fn discord_default_title(msg: &MessageCreate) -> String {
    if msg.guild_id.is_some() {
        // Server message — channel-id is the most stable label until
        // we resolve channel names (separate REST call, D3 polish).
        format!("Discord — #{}", msg.channel_id)
    } else if let Some(name) = msg.author.username.as_deref() {
        format!("Discord — {}", name)
    } else {
        format!("Discord — DM {}", msg.author.id)
    }
}

fn strip_bot_mentions(content: &str, bot_user_id: &str) -> String {
    // Discord mention syntax: <@USER_ID> or <@!USER_ID> (the ! variant
    // is the legacy nickname form, still emitted by some clients).
    let with_id    = format!("<@{}>", bot_user_id);
    let with_nick  = format!("<@!{}>", bot_user_id);
    content
        .replace(&with_id, "")
        .replace(&with_nick, "")
        .trim()
        .to_string()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_mention_handles_both_variants_anywhere_in_text() {
        assert_eq!(strip_bot_mentions("<@123> hello",  "123"), "hello");
        assert_eq!(strip_bot_mentions("<@!123> hello", "123"), "hello");
        assert_eq!(strip_bot_mentions("hi <@123> there", "123"), "hi  there".trim());
        // Leave other users' mentions alone.
        assert_eq!(strip_bot_mentions("<@123> ping <@456>", "123"), "ping <@456>");
    }

    #[test]
    fn default_title_guild_uses_channel_id() {
        let msg = MessageCreate {
            id: "1".into(), channel_id: "C123".into(),
            guild_id: Some("G456".into()),
            author: super::super::types::MessageAuthor {
                id: "u".into(), bot: false, username: Some("alice".into()),
            },
            content: "hi".into(), mentions: vec![], mention_everyone: false,
        };
        assert_eq!(discord_default_title(&msg), "Discord — #C123");
    }

    #[test]
    fn default_title_dm_uses_username_or_id() {
        let msg = MessageCreate {
            id: "1".into(), channel_id: "C".into(), guild_id: None,
            author: super::super::types::MessageAuthor {
                id: "u".into(), bot: false, username: Some("alice".into()),
            },
            content: "hi".into(), mentions: vec![], mention_everyone: false,
        };
        assert_eq!(discord_default_title(&msg), "Discord — alice");

        let mut no_name = msg;
        no_name.author.username = None;
        assert_eq!(discord_default_title(&no_name), "Discord — DM u");
    }
}
