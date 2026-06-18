// SPDX-License-Identifier: AGPL-3.0-or-later

// src/whatsapp/types.rs
//
// WhatsApp Business Cloud API webhook payloads — the subset we read for
// inbound text messages — plus the GET-verification query.
//
// Docs: https://developers.facebook.com/docs/whatsapp/cloud-api/webhooks
//
// The inbound webhook is a deeply-nested envelope:
//   entry[].changes[].value.messages[]   — the actual messages
//   entry[].changes[].value.contacts[]   — sender display names
// We model only what the dispatcher needs and lean on #[serde(default)]
// so status callbacks (delivered/read receipts), which share the envelope
// but carry `statuses` instead of `messages`, parse to an empty message
// list rather than erroring.

use serde::Deserialize;

// ── GET verification handshake ─────────────────────────────────────────

/// Query params Meta sends to `GET /webhook/whatsapp/{account_id}` when
/// you (re)subscribe the webhook. We echo `hub_challenge` back verbatim
/// iff `hub_verify_token` matches the account's configured verify token.
#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    #[serde(rename = "hub.mode")]
    pub mode:        Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub challenge:   Option<String>,
}

// ── POST webhook envelope ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct WebhookPayload {
    #[serde(default)]
    pub entry: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub changes: Vec<Change>,
}

#[derive(Debug, Deserialize)]
pub struct Change {
    #[serde(default)]
    pub value: ChangeValue,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChangeValue {
    /// Inbound messages. Absent on status callbacks (delivery receipts).
    #[serde(default)]
    pub messages: Vec<WaMessage>,
    /// Sender profile info, parallel to `messages` by `wa_id`.
    #[serde(default)]
    pub contacts: Vec<WaContact>,
}

#[derive(Debug, Deserialize)]
pub struct WaMessage {
    /// Sender phone in international format without `+`, e.g. "14155552671".
    pub from: String,
    /// Message id (`wamid....`) — for idempotency / logging.
    #[serde(default)]
    pub id:   Option<String>,
    /// "text", "image", "audio", "interactive", … We act on "text".
    #[serde(rename = "type", default)]
    pub msg_type: Option<String>,
    /// Present for `type=text`.
    #[serde(default)]
    pub text: Option<WaText>,
}

#[derive(Debug, Deserialize)]
pub struct WaText {
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct WaContact {
    /// The sender's WhatsApp id (same value as `WaMessage.from`).
    pub wa_id: String,
    #[serde(default)]
    pub profile: Option<WaProfile>,
}

#[derive(Debug, Deserialize)]
pub struct WaProfile {
    #[serde(default)]
    pub name: Option<String>,
}

impl WebhookPayload {
    /// Flatten the envelope to the inbound text messages we care about,
    /// pairing each with the sender's display name when present.
    /// Returns `(from, body, display_name)` tuples.
    pub fn text_messages(&self) -> Vec<(String, String, Option<String>)> {
        let mut out = Vec::new();
        for entry in &self.entry {
            for change in &entry.changes {
                let v = &change.value;
                // Build a wa_id → name map from contacts for this change.
                let name_of = |wa_id: &str| -> Option<String> {
                    v.contacts.iter()
                        .find(|c| c.wa_id == wa_id)
                        .and_then(|c| c.profile.as_ref())
                        .and_then(|p| p.name.clone())
                };
                for m in &v.messages {
                    let is_text = m.msg_type.as_deref() == Some("text");
                    if !is_text { continue; }
                    let Some(t) = m.text.as_ref() else { continue; };
                    let name = name_of(&m.from);
                    out.push((m.from.clone(), t.body.clone(), name));
                }
            }
        }
        out
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "object": "whatsapp_business_account",
      "entry": [{
        "id": "WABA_ID",
        "changes": [{
          "field": "messages",
          "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15551234567", "phone_number_id": "PNID" },
            "contacts": [{ "profile": { "name": "Alice" }, "wa_id": "14155552671" }],
            "messages": [{
              "from": "14155552671",
              "id": "wamid.ABC",
              "timestamp": "1700000000",
              "text": { "body": "hello mira" },
              "type": "text"
            }]
          }
        }]
      }]
    }"#;

    #[test]
    fn extracts_text_message_with_name() {
        let p: WebhookPayload = serde_json::from_str(SAMPLE).unwrap();
        let msgs = p.text_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0, "14155552671");
        assert_eq!(msgs[0].1, "hello mira");
        assert_eq!(msgs[0].2.as_deref(), Some("Alice"));
    }

    #[test]
    fn status_callback_has_no_text_messages() {
        // Delivery receipts share the envelope but carry `statuses`,
        // not `messages` — must parse cleanly to zero messages.
        let raw = r#"{
          "entry": [{
            "changes": [{
              "value": {
                "messaging_product": "whatsapp",
                "statuses": [{ "id": "wamid.X", "status": "delivered" }]
              }
            }]
          }]
        }"#;
        let p: WebhookPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(p.text_messages().len(), 0);
    }

    #[test]
    fn non_text_messages_are_skipped() {
        let raw = r#"{
          "entry": [{ "changes": [{ "value": {
            "messages": [{ "from": "1", "type": "image", "id": "wamid.I" }]
          }}]}]
        }"#;
        let p: WebhookPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(p.text_messages().len(), 0);
    }

    #[test]
    fn empty_payload_is_fine() {
        let p: WebhookPayload = serde_json::from_str("{}").unwrap();
        assert_eq!(p.text_messages().len(), 0);
    }

    #[test]
    fn verify_query_deserializes_from_json_shape() {
        // The handler uses axum's Query extractor (serde_urlencoded) over
        // the dotted hub.* params; here we just confirm the rename mapping
        // round-trips via JSON, which exercises the same serde attributes.
        let q: VerifyQuery = serde_json::from_str(
            r#"{"hub.mode":"subscribe","hub.verify_token":"secret","hub.challenge":"12345"}"#,
        ).unwrap();
        assert_eq!(q.mode.as_deref(), Some("subscribe"));
        assert_eq!(q.verify_token.as_deref(), Some("secret"));
        assert_eq!(q.challenge.as_deref(), Some("12345"));
    }
}
