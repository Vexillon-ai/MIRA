// SPDX-License-Identifier: AGPL-3.0-or-later

// src/slack/types.rs
//
// Slack Events API payloads — the subset we read for inbound messages —
// plus the one-time url_verification handshake.
//
// Docs: https://api.slack.com/apis/connections/events-api
//
// The webhook delivers two relevant envelope shapes to the same endpoint:
//   * { "type": "url_verification", "challenge": "..." }  — one-time setup
//   * { "type": "event_callback", "event": { ... } }      — actual events
// We model both with one tagged-ish struct (untagged on `type`) and lean
// on #[serde(default)] so unrelated event types parse without erroring.

use serde::Deserialize;

/// Top-level webhook envelope. `event_type` distinguishes the handshake
/// from real events; `challenge` is only present on the handshake.
#[derive(Debug, Deserialize)]
pub struct WebhookEnvelope {
    #[serde(rename = "type", default)]
    pub event_type: Option<String>,
    /// Present iff `type == "url_verification"` — echo it back verbatim.
    #[serde(default)]
    pub challenge: Option<String>,
    /// Present iff `type == "event_callback"`.
    #[serde(default)]
    pub event: Option<SlackEvent>,
}

/// A single event inside an `event_callback`. We act on `message` events
/// with a text body and no bot author.
#[derive(Debug, Default, Deserialize)]
pub struct SlackEvent {
    #[serde(rename = "type", default)]
    pub event_type: Option<String>,
    /// Sub-type qualifies a message event: "bot_message", "message_changed",
    /// "channel_join", … We only act when this is absent (a plain user
    /// message) to avoid loops + noise.
    #[serde(default)]
    pub subtype: Option<String>,
    /// The Slack user id of the sender, e.g. "U12345".
    #[serde(default)]
    pub user: Option<String>,
    /// Set when the message was posted by a bot (including ourselves) —
    /// we skip these to avoid echo loops.
    #[serde(default)]
    pub bot_id: Option<String>,
    /// The message text.
    #[serde(default)]
    pub text: Option<String>,
    /// The channel id the message was posted in, e.g. "C12345" (channel)
    /// or "D12345" (DM). Used as both the conversation key and the
    /// outbound target.
    #[serde(default)]
    pub channel: Option<String>,
}

impl WebhookEnvelope {
    /// True when this is the one-time URL-verification handshake.
    pub fn is_url_verification(&self) -> bool {
        self.event_type.as_deref() == Some("url_verification")
    }

    /// Extract a dispatchable inbound message: `(channel, user, text)`.
    /// Returns None unless this is a plain user `message` event (no
    /// subtype, no bot author) carrying text + a channel.
    pub fn inbound_message(&self) -> Option<(String, String, String)> {
        let ev = self.event.as_ref()?;
        if ev.event_type.as_deref() != Some("message") { return None; }
        // Skip bot messages + any sub-typed message (edits, joins, etc).
        if ev.bot_id.is_some() || ev.subtype.is_some() { return None; }
        let channel = ev.channel.clone()?;
        let user    = ev.user.clone()?;
        let text    = ev.text.clone().filter(|t| !t.trim().is_empty())?;
        Some((channel, user, text))
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_verification() {
        let e: WebhookEnvelope = serde_json::from_str(
            r#"{"type":"url_verification","challenge":"abc123","token":"x"}"#,
        ).unwrap();
        assert!(e.is_url_verification());
        assert_eq!(e.challenge.as_deref(), Some("abc123"));
        assert!(e.inbound_message().is_none());
    }

    #[test]
    fn extracts_plain_user_message() {
        let raw = r#"{
            "type": "event_callback",
            "event": {
                "type": "message",
                "user": "U123",
                "text": "hello mira",
                "channel": "C999",
                "ts": "1700000000.000100"
            }
        }"#;
        let e: WebhookEnvelope = serde_json::from_str(raw).unwrap();
        assert!(!e.is_url_verification());
        let (chan, user, text) = e.inbound_message().unwrap();
        assert_eq!(chan, "C999");
        assert_eq!(user, "U123");
        assert_eq!(text, "hello mira");
    }

    #[test]
    fn skips_bot_messages() {
        let raw = r#"{
            "type": "event_callback",
            "event": {
                "type": "message",
                "bot_id": "B123",
                "text": "i am a bot",
                "channel": "C1"
            }
        }"#;
        let e: WebhookEnvelope = serde_json::from_str(raw).unwrap();
        assert!(e.inbound_message().is_none());
    }

    #[test]
    fn skips_subtyped_messages() {
        // message_changed / channel_join / etc carry a subtype.
        let raw = r#"{
            "type": "event_callback",
            "event": { "type": "message", "subtype": "message_changed",
                       "user": "U1", "text": "edited", "channel": "C1" }
        }"#;
        let e: WebhookEnvelope = serde_json::from_str(raw).unwrap();
        assert!(e.inbound_message().is_none());
    }

    #[test]
    fn ignores_non_message_events() {
        let raw = r#"{
            "type": "event_callback",
            "event": { "type": "reaction_added", "user": "U1", "channel": "C1" }
        }"#;
        let e: WebhookEnvelope = serde_json::from_str(raw).unwrap();
        assert!(e.inbound_message().is_none());
    }

    #[test]
    fn empty_text_is_skipped() {
        let raw = r#"{
            "type": "event_callback",
            "event": { "type": "message", "user": "U1", "text": "   ", "channel": "C1" }
        }"#;
        let e: WebhookEnvelope = serde_json::from_str(raw).unwrap();
        assert!(e.inbound_message().is_none());
    }
}
