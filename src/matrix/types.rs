// SPDX-License-Identifier: AGPL-3.0-or-later

// src/matrix/types.rs
//
// Matrix Client-Server API payloads — the subset we use for the
// long-poll sync loop + sending messages. We model only the fields we
// read; Matrix responses are large and deeply nested, so each struct is
// deliberately partial with `#[serde(default)]` to stay forward-compatible.
//
// Spec: https://spec.matrix.org/v1.11/client-server-api/

use serde::Deserialize;
use std::collections::HashMap;

// ── /sync response ─────────────────────────────────────────────────────

/// Top-level `GET /_matrix/client/v3/sync` response. We only pull the
/// `next_batch` token (to resume) and joined-room timeline events.
#[derive(Debug, Deserialize)]
pub struct SyncResponse {
    /// Opaque pagination token — pass back as `since` on the next sync.
    pub next_batch: String,
    #[serde(default)]
    pub rooms: Rooms,
}

#[derive(Debug, Default, Deserialize)]
pub struct Rooms {
    /// room_id → join state (timeline events live here).
    #[serde(default)]
    pub join: HashMap<String, JoinedRoom>,
    /// room_id → invite state. We use the presence of a key to auto-join
    /// rooms the bot is invited to (so a user can DM the bot by inviting
    /// it to a new room).
    #[serde(default)]
    pub invite: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct JoinedRoom {
    #[serde(default)]
    pub timeline: Timeline,
}

#[derive(Debug, Default, Deserialize)]
pub struct Timeline {
    #[serde(default)]
    pub events: Vec<RoomEvent>,
}

/// A single timeline event. We only act on `m.room.message` events with a
/// text body; everything else (state changes, reactions, redactions) is
/// ignored by the dispatcher.
#[derive(Debug, Deserialize)]
pub struct RoomEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    /// The Matrix user id of whoever sent the event, e.g. `@alice:hs.tld`.
    pub sender: String,
    #[serde(default)]
    pub content: EventContent,
    /// Server-assigned event id — used for idempotency / logging.
    #[serde(default)]
    pub event_id: Option<String>,
    /// Origin server timestamp (ms). Used to drop events older than the
    /// connect time so we don't replay history on first sync.
    #[serde(default)]
    pub origin_server_ts: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct EventContent {
    /// `m.text`, `m.notice`, `m.emote`, `m.image`, … We act on text/notice.
    #[serde(default)]
    pub msgtype: Option<String>,
    /// The human-readable message text for `m.text`/`m.notice`.
    #[serde(default)]
    pub body: Option<String>,
}

// ── /whoami response ───────────────────────────────────────────────────

/// `GET /_matrix/client/v3/account/whoami` — used to learn the bot's own
/// Matrix user id so we can skip our own echoed messages.
#[derive(Debug, Deserialize)]
pub struct WhoAmI {
    pub user_id: String,
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sync_with_one_text_message() {
        let raw = r#"{
            "next_batch": "s72595_4483_1934",
            "rooms": {
                "join": {
                    "!room:hs.tld": {
                        "timeline": {
                            "events": [
                                {
                                    "type": "m.room.message",
                                    "sender": "@alice:hs.tld",
                                    "event_id": "$evt1",
                                    "origin_server_ts": 1700000000000,
                                    "content": { "msgtype": "m.text", "body": "hello mira" }
                                }
                            ]
                        }
                    }
                }
            }
        }"#;
        let s: SyncResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(s.next_batch, "s72595_4483_1934");
        let room = s.rooms.join.get("!room:hs.tld").unwrap();
        assert_eq!(room.timeline.events.len(), 1);
        let e = &room.timeline.events[0];
        assert_eq!(e.event_type, "m.room.message");
        assert_eq!(e.sender, "@alice:hs.tld");
        assert_eq!(e.content.body.as_deref(), Some("hello mira"));
        assert_eq!(e.content.msgtype.as_deref(), Some("m.text"));
    }

    #[test]
    fn sync_tolerates_empty_rooms() {
        let s: SyncResponse = serde_json::from_str(
            r#"{"next_batch":"tok"}"#,
        ).unwrap();
        assert_eq!(s.next_batch, "tok");
        assert!(s.rooms.join.is_empty());
        assert!(s.rooms.invite.is_empty());
    }

    #[test]
    fn ignores_unknown_event_fields() {
        // Real events carry many fields (unsigned, age, etc.) — we must
        // not choke on them.
        let raw = r#"{
            "type": "m.room.message",
            "sender": "@bob:hs.tld",
            "event_id": "$x",
            "origin_server_ts": 1,
            "unsigned": { "age": 42 },
            "content": { "msgtype": "m.text", "body": "hi", "format": "org.matrix.custom.html" }
        }"#;
        let e: RoomEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(e.content.body.as_deref(), Some("hi"));
    }

    #[test]
    fn parses_invite_rooms() {
        let raw = r#"{
            "next_batch": "t",
            "rooms": { "invite": { "!new:hs.tld": {} } }
        }"#;
        let s: SyncResponse = serde_json::from_str(raw).unwrap();
        assert!(s.rooms.invite.contains_key("!new:hs.tld"));
    }
}
