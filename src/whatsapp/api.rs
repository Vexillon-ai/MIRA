// SPDX-License-Identifier: AGPL-3.0-or-later

// src/whatsapp/api.rs
//
// WhatsApp Business Cloud API outbound + webhook signature verification.
//
// Outbound: POST https://graph.facebook.com/v21.0/{phone_number_id}/messages
// with a Bearer access token. We send `type:text` free-form messages.
//
// IMPORTANT — the 24-hour customer-service window: Meta only allows a
// free-form text reply within 24h of the user's last inbound message.
// Outside that window you MUST send a pre-approved *template* message.
// Inbound-triggered replies (the dispatcher) are always inside the window
// so they use free-form text. Proactive sends (companion/automations) may
// fall outside it — `send_text` surfaces Meta's error (code 131047 /
// "re-engagement message") so the caller can log it clearly; a future
// `send_template` covers that case once the operator has approved
// templates. See design-docs/whatsapp-channel.md.

use serde_json::json;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const GRAPH_API: &str = "https://graph.facebook.com/v21.0";

/// Send a free-form text message to `to` (a WhatsApp phone in
/// international format, no `+`). Only valid inside the 24h window.
pub async fn send_text(
    client:          &reqwest::Client,
    phone_number_id: &str,
    access_token:    &str,
    to:              &str,
    body:            &str,
) -> Result<(), String> {
    let url = format!("{}/{}/messages", GRAPH_API, phone_number_id);
    for chunk in split_for_whatsapp(body) {
        let payload = json!({
            "messaging_product": "whatsapp",
            "recipient_type":    "individual",
            "to":                to,
            "type":              "text",
            "text":              { "preview_url": false, "body": chunk },
        });
        let resp = client
            .post(&url)
            .bearer_auth(access_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("send: {}", e))?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            // Meta's 131047 = "message outside 24h window, use a template".
            // Bubble the body up so the caller can log it intelligibly.
            return Err(format!("WhatsApp {}: {}", status, preview(&txt, 300)));
        }
    }
    Ok(())
}

/// Verify the `X-Hub-Signature-256` header Meta attaches to every webhook
/// POST. The signature is `sha256=<hex hmac>` of the **raw request body**
/// keyed by the app secret. Returns true iff valid. Constant-time compare.
pub fn verify_signature(app_secret: &str, raw_body: &[u8], header: &str) -> bool {
    let Some(hex_sig) = header.strip_prefix("sha256=") else { return false; };
    let Ok(expected) = hex::decode(hex_sig) else { return false; };

    let mut mac = match Hmac::<Sha256>::new_from_slice(app_secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(raw_body);
    // `verify_slice` is constant-time and checks length.
    mac.verify_slice(&expected).is_ok()
}

/// WhatsApp's text body cap is 4096 chars. Split on paragraph boundaries,
/// hard-slice an oversized paragraph. Returns at least one chunk.
pub fn split_for_whatsapp(text: &str) -> Vec<String> {
    const LIMIT: usize = 4000; // headroom under the 4096 hard cap
    if text.chars().count() <= LIMIT {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for para in text.split("\n\n") {
        let extra = if current.is_empty() { 0 } else { 2 };
        if current.chars().count() + para.chars().count() + extra > LIMIT {
            if !current.is_empty() { chunks.push(std::mem::take(&mut current)); }
        }
        if para.chars().count() > LIMIT {
            chunks.extend(chunk_by_chars(para, LIMIT));
            continue;
        }
        if !current.is_empty() { current.push_str("\n\n"); }
        current.push_str(para);
    }
    if !current.is_empty() { chunks.push(current); }
    if chunks.is_empty() { chunks.push(String::new()); }
    chunks
}

fn chunk_by_chars(s: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut n = 0;
    for ch in s.chars() {
        if n == limit { out.push(std::mem::take(&mut buf)); n = 0; }
        buf.push(ch);
        n += 1;
    }
    if !buf.is_empty() { out.push(buf); }
    out
}

fn preview(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..n]) }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signature_roundtrip_valid() {
        let secret = "app-secret";
        let body = br#"{"hello":"world"}"#;
        let header = sign(secret, body);
        assert!(verify_signature(secret, body, &header));
    }

    #[test]
    fn signature_rejects_wrong_secret() {
        let body = b"payload";
        let header = sign("right", body);
        assert!(!verify_signature("wrong", body, &header));
    }

    #[test]
    fn signature_rejects_tampered_body() {
        let secret = "s";
        let header = sign(secret, b"original");
        assert!(!verify_signature(secret, b"tampered", &header));
    }

    #[test]
    fn signature_rejects_malformed_header() {
        assert!(!verify_signature("s", b"x", "not-a-sig"));
        assert!(!verify_signature("s", b"x", "sha256=nothex"));
        assert!(!verify_signature("s", b"x", ""));
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(split_for_whatsapp("hi"), vec!["hi".to_string()]);
    }

    #[test]
    fn long_text_splits_lossless_under_cap() {
        let huge: String = std::iter::repeat('x').take(10000).collect();
        let chunks = split_for_whatsapp(&huge);
        assert!(chunks.len() >= 3);
        for c in &chunks { assert!(c.chars().count() <= 4000); }
        assert_eq!(chunks.concat().chars().count(), 10000);
    }
}
