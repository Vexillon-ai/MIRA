// SPDX-License-Identifier: AGPL-3.0-or-later

// src/discord/types.rs
//
// Discord gateway protocol payloads — the subset we use for D2 (inbound
// MESSAGE_CREATE + heartbeat/identify/resume). Keep this minimal: we only
// model the fields we actually read. Discord adds fields routinely; the
// `#[serde(default)]` + Value catch-all keep us forward-compatible.
//
// Gateway docs: https://discord.com/developers/docs/topics/gateway
// Opcode docs:  https://discord.com/developers/docs/topics/opcodes-and-status-codes

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Opcodes (gateway) ──────────────────────────────────────────────────

/// `op 0` — server-pushed event (followed by `t` event name + `d` payload).
pub const OP_DISPATCH:        u8 = 0;
/// `op 1` — client→server: send a heartbeat. Server may also push this
/// to request an immediate heartbeat (rare, treat the same as our timer).
pub const OP_HEARTBEAT:       u8 = 1;
/// `op 2` — client→server: IDENTIFY (first handshake on a fresh session).
pub const OP_IDENTIFY:        u8 = 2;
/// `op 6` — client→server: RESUME (replay missed events after disconnect).
pub const OP_RESUME:          u8 = 6;
/// `op 7` — server→client: reconnect immediately. Treat as "close + try
/// RESUME on the resume_gateway_url".
pub const OP_RECONNECT:       u8 = 7;
/// `op 9` — server→client: invalid session. `d` is `true` if resumable,
/// `false` if we must wait + fresh IDENTIFY.
pub const OP_INVALID_SESSION: u8 = 9;
/// `op 10` — server→client: HELLO with `heartbeat_interval` ms.
pub const OP_HELLO:           u8 = 10;
/// `op 11` — server→client: heartbeat ACK. Missed ACK = zombie connection.
pub const OP_HEARTBEAT_ACK:   u8 = 11;

// ── Gateway intents (bitfield) ─────────────────────────────────────────
//
// We need GUILDS (so READY ships guild_id mapping), GUILD_MESSAGES (text
// channel messages), DIRECT_MESSAGES (DMs to the bot), and MESSAGE_CONTENT
// (the actual `content` field — without this Discord strips message text
// for bots without a verification exception). MESSAGE_CONTENT is a
// privileged intent — the bot owner must enable it in the Developer
// Portal → Bot → Privileged Gateway Intents.

pub const INTENT_GUILDS:           u64 = 1 << 0;
pub const INTENT_GUILD_MESSAGES:   u64 = 1 << 9;
pub const INTENT_DIRECT_MESSAGES:  u64 = 1 << 12;
pub const INTENT_MESSAGE_CONTENT:  u64 = 1 << 15;

pub const REQUIRED_INTENTS: u64 =
    INTENT_GUILDS | INTENT_GUILD_MESSAGES | INTENT_DIRECT_MESSAGES | INTENT_MESSAGE_CONTENT;

// ── Wrapping envelope ──────────────────────────────────────────────────

/// Every gateway frame has this outer shape. `s` and `t` are only present
/// on `op 0 DISPATCH` frames; `d` is the per-op payload.
#[derive(Debug, Deserialize)]
pub struct GatewayFrame {
    pub op: u8,
    #[serde(default)]
    pub s:  Option<u64>,
    #[serde(default)]
    pub t:  Option<String>,
    #[serde(default)]
    pub d:  Value,
}

// ── `op 10` HELLO payload ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Hello {
    pub heartbeat_interval: u64, // milliseconds
}

// ── `op 2` IDENTIFY payload ────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Identify {
    pub token:      String,
    pub intents:    u64,
    pub properties: IdentifyProps,
}

#[derive(Debug, Serialize)]
pub struct IdentifyProps {
    #[serde(rename = "os")]
    pub os:      &'static str,
    #[serde(rename = "browser")]
    pub browser: &'static str,
    #[serde(rename = "device")]
    pub device:  &'static str,
}

impl Default for IdentifyProps {
    fn default() -> Self {
        // Discord asks clients to identify their library; "mira" is fine
        // for the device. OS/browser are advisory only.
        Self {
            os:      std::env::consts::OS,
            browser: concat!("mira/", env!("CARGO_PKG_VERSION")),
            device:  concat!("mira/", env!("CARGO_PKG_VERSION")),
        }
    }
}

// ── `op 6` RESUME payload ──────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Resume {
    pub token:      String,
    pub session_id: String,
    pub seq:        u64,
}

// ── `op 0 READY` event ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Ready {
    pub session_id:          String,
    /// New URL to RESUME on after disconnect. Use instead of the bootstrap
    /// gateway URL. Required since gateway v9.
    #[serde(default)]
    pub resume_gateway_url:  Option<String>,
    /// The bot user — used to skip our own MESSAGE_CREATE echoes if the
    /// caller didn't supply `application_id` ahead of time.
    pub user: ReadyUser,
}

#[derive(Debug, Deserialize)]
pub struct ReadyUser {
    pub id: String,
}

// ── `op 0 MESSAGE_CREATE` event ────────────────────────────────────────
//
// Trim shape — only the fields the dispatcher reads. Attachments and
// embeds are out of scope for D2 (text only); we'll extend in D3+.

#[derive(Debug, Deserialize)]
pub struct MessageCreate {
    pub id:         String,
    pub channel_id: String,
    /// None on certain system events; we ignore those.
    #[serde(default)]
    pub guild_id:   Option<String>,
    pub author:     MessageAuthor,
    /// Always present when MESSAGE_CONTENT intent is granted. Empty
    /// string when the bot has no content access — we treat that as
    /// "nothing to do" and skip.
    #[serde(default)]
    pub content:    String,
    /// Snowflakes the message text @-mentioned. We use this for the
    /// `mention_only` filter (cheaper + safer than regexing content).
    #[serde(default)]
    pub mentions:   Vec<MessageAuthor>,
    /// True when the message includes `@everyone` or `@here`. We do NOT
    /// treat these as bot mentions; only direct user-id mentions count.
    #[serde(default)]
    pub mention_everyone: bool,
}

#[derive(Debug, Deserialize)]
pub struct MessageAuthor {
    pub id:   String,
    /// `true` when the author is a bot account — we skip these to avoid
    /// loops with other bots and to suppress our own echoed messages.
    #[serde(default)]
    pub bot:  bool,
    /// Convenience for rendering / future TTS captions.
    #[serde(default)]
    pub username: Option<String>,
}

// ── `op 9` INVALID_SESSION payload ─────────────────────────────────────

/// Body of `op 9`: bare boolean. `true` = the session can still be
/// resumed (back off a few seconds then RESUME); `false` = drop the
/// session id + start a fresh IDENTIFY.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct InvalidSession(pub bool);

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hello_frame() {
        let frame: GatewayFrame = serde_json::from_str(
            r#"{"op":10,"d":{"heartbeat_interval":41250},"s":null,"t":null}"#,
        ).unwrap();
        assert_eq!(frame.op, OP_HELLO);
        let hello: Hello = serde_json::from_value(frame.d).unwrap();
        assert_eq!(hello.heartbeat_interval, 41250);
    }

    #[test]
    fn parses_message_create_with_content_intent() {
        let frame: GatewayFrame = serde_json::from_str(
            r#"{"op":0,"t":"MESSAGE_CREATE","s":5,"d":{
              "id":"123","channel_id":"456","guild_id":"789",
              "author":{"id":"u1","bot":false,"username":"alice"},
              "content":"hello mira","mentions":[],"mention_everyone":false
            }}"#,
        ).unwrap();
        assert_eq!(frame.op, OP_DISPATCH);
        assert_eq!(frame.t.as_deref(), Some("MESSAGE_CREATE"));
        let msg: MessageCreate = serde_json::from_value(frame.d).unwrap();
        assert_eq!(msg.content, "hello mira");
        assert!(!msg.author.bot);
    }

    #[test]
    fn message_create_tolerates_missing_optional_fields() {
        // No mentions / mention_everyone / guild_id / username.
        let msg: MessageCreate = serde_json::from_str(
            r#"{"id":"1","channel_id":"2",
              "author":{"id":"u","bot":true},
              "content":""}"#,
        ).unwrap();
        assert_eq!(msg.mentions.len(), 0);
        assert!(!msg.mention_everyone);
        assert!(msg.author.bot);
        assert_eq!(msg.guild_id, None);
        assert_eq!(msg.author.username, None);
    }

    #[test]
    fn invalid_session_resumable_vs_fresh() {
        let resumable: InvalidSession = serde_json::from_str("true").unwrap();
        let fresh:     InvalidSession = serde_json::from_str("false").unwrap();
        assert!(resumable.0);
        assert!(!fresh.0);
    }

    #[test]
    fn required_intents_includes_message_content() {
        // Guard against accidentally dropping MESSAGE_CONTENT — without it,
        // Discord strips message bodies for non-verified bots and the
        // dispatcher gets empty content on every event.
        assert!(REQUIRED_INTENTS & INTENT_MESSAGE_CONTENT != 0);
        assert!(REQUIRED_INTENTS & INTENT_GUILD_MESSAGES != 0);
        assert!(REQUIRED_INTENTS & INTENT_DIRECT_MESSAGES != 0);
    }
}
