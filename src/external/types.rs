// SPDX-License-Identifier: AGPL-3.0-or-later

// src/external/types.rs
//
// Channel Provider Protocol (CPP) wire types. See
// design-docs/channel-provider-protocol.md for the full spec. CPP is the
// "MCP for channels" plugin contract: an external provider process owns
// the transport to some messaging system and relays messages to/from MIRA
// over two signed HTTP calls.

use serde::{Deserialize, Serialize};

/// Inbound webhook body: provider → MIRA, POSTed to
/// `/webhook/external/{account_id}`.
#[derive(Debug, Deserialize)]
pub struct InboundBody {
    /// Protocol version. v1 MIRA accepts "1".
    #[serde(default)]
    pub cpp_version: Option<String>,
    /// Event type. We act on "message"; other types are accepted + ignored.
    #[serde(rename = "type", default)]
    pub event_type: Option<String>,
    /// Opaque provider room/DM id — the history key + outbound target.
    pub conversation_id: String,
    /// Opaque stable per-user id — drives shared-bot identity resolution.
    pub sender_id: String,
    /// Optional display name for the conversation title.
    #[serde(default)]
    pub sender_display_name: Option<String>,
    /// The message text. Optional (defaults to empty) so a provider can send
    /// a voice-only message carrying just `audio` — MIRA transcribes it.
    #[serde(default)]
    pub text: String,
    /// Optional inbound audio (a voice note the user sent). When present and
    /// `text` is empty, MIRA transcribes it server-side (if STT is wired) and
    /// uses the transcript as the message text. Providers that do their own
    /// speech-to-text should send the transcript as `text` and omit this.
    #[serde(default)]
    pub audio: Option<InboundAudio>,
}

/// Audio attached to an inbound CPP message (a user's voice note). Mirror of
/// `OutboundAudio` for the provider → MIRA direction.
#[derive(Debug, Deserialize)]
pub struct InboundAudio {
    /// MIME type, e.g. "audio/ogg". Used as the STT format hint.
    #[serde(default)]
    pub content_type: String,
    /// File extension hint, e.g. "ogg". Fallback when `content_type` is empty.
    #[serde(default)]
    pub extension:    String,
    /// Base64-encoded audio bytes.
    pub data_base64:  String,
}

impl InboundBody {
    pub fn is_message(&self) -> bool {
        // Default to treating a body with text as a message if `type` is
        // omitted (lenient — keeps minimal providers simple).
        match self.event_type.as_deref() {
            Some(t) => t == "message",
            None    => true,
        }
    }
}

/// Optional synthesized audio attached to an outbound CPP message. A
/// provider that can play audio (e.g. Nextcloud Talk voice messages) plays
/// `data_base64`; one that can't ignores this field and renders `text`.
/// Only present when the channel's account has `supports_voice` and the
/// user's voice policy opts into spoken replies.
#[derive(Debug, Serialize)]
pub struct OutboundAudio {
    /// MIME type, e.g. "audio/ogg" / "audio/mpeg" / "audio/wav".
    pub content_type: String,
    /// File extension hint, e.g. "ogg".
    pub extension:    String,
    /// Base64-encoded audio bytes.
    pub data_base64:  String,
}

/// Outbound body: MIRA → provider, POSTed to the account's `send_url`.
#[derive(Debug, Serialize)]
pub struct OutboundBody<'a> {
    pub cpp_version:     &'static str,
    pub account_id:      &'a str,
    pub conversation_id: &'a str,
    pub text:            &'a str,
    /// Present only when MIRA synthesized audio for this reply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio:           Option<OutboundAudio>,
}

pub const CPP_VERSION: &str = "1";

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_inbound() {
        let b: InboundBody = serde_json::from_str(
            r#"{"conversation_id":"room1","sender_id":"u1","text":"hi"}"#,
        ).unwrap();
        assert_eq!(b.conversation_id, "room1");
        assert_eq!(b.sender_id, "u1");
        assert_eq!(b.text, "hi");
        assert!(b.is_message()); // type omitted → treated as message
    }

    #[test]
    fn parses_full_inbound() {
        let b: InboundBody = serde_json::from_str(
            r#"{"cpp_version":"1","type":"message","conversation_id":"r",
                "sender_id":"u","sender_display_name":"Alice","text":"yo"}"#,
        ).unwrap();
        assert_eq!(b.sender_display_name.as_deref(), Some("Alice"));
        assert!(b.is_message());
    }

    #[test]
    fn non_message_type_is_not_a_message() {
        let b: InboundBody = serde_json::from_str(
            r#"{"type":"typing","conversation_id":"r","sender_id":"u","text":""}"#,
        ).unwrap();
        assert!(!b.is_message());
    }

    #[test]
    fn parses_voice_only_inbound_without_text() {
        // A voice-only message: no `text` key, just `audio`. `text` defaults
        // to "" so it still deserialises; MIRA transcribes the audio.
        let b: InboundBody = serde_json::from_str(
            r#"{"type":"message","conversation_id":"room","sender_id":"u",
                "audio":{"content_type":"audio/ogg","extension":"ogg","data_base64":"AAAA"}}"#,
        ).unwrap();
        assert!(b.text.is_empty());
        assert!(b.is_message());
        let a = b.audio.expect("audio present");
        assert_eq!(a.content_type, "audio/ogg");
        assert_eq!(a.data_base64, "AAAA");
    }

    #[test]
    fn inbound_audio_absent_by_default() {
        let b: InboundBody = serde_json::from_str(
            r#"{"conversation_id":"r","sender_id":"u","text":"hi"}"#,
        ).unwrap();
        assert!(b.audio.is_none());
    }

    #[test]
    fn outbound_serialises_with_version() {
        let o = OutboundBody {
            cpp_version: CPP_VERSION, account_id: "acc",
            conversation_id: "room", text: "reply", audio: None,
        };
        let s = serde_json::to_string(&o).unwrap();
        assert!(s.contains("\"cpp_version\":\"1\""));
        assert!(s.contains("\"conversation_id\":\"room\""));
        // audio omitted when None (skip_serializing_if).
        assert!(!s.contains("audio"));
    }

    #[test]
    fn outbound_includes_audio_when_present() {
        let o = OutboundBody {
            cpp_version: CPP_VERSION, account_id: "acc",
            conversation_id: "room", text: "spoken",
            audio: Some(OutboundAudio {
                content_type: "audio/ogg".into(), extension: "ogg".into(),
                data_base64: "AAAA".into(),
            }),
        };
        let s = serde_json::to_string(&o).unwrap();
        assert!(s.contains("\"audio\""));
        assert!(s.contains("\"content_type\":\"audio/ogg\""));
        assert!(s.contains("\"data_base64\":\"AAAA\""));
    }
}
