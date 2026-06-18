// SPDX-License-Identifier: AGPL-3.0-or-later

// src/slack/api.rs
//
// Slack Web API outbound (chat.postMessage) + Events API request
// signature verification.
//
// Signature scheme (https://api.slack.com/authentication/verifying-requests-from-slack):
//   basestring = "v0:{timestamp}:{raw_body}"
//   expected   = "v0=" + hex(HMAC_SHA256(signing_secret, basestring))
//   compare to the `X-Slack-Signature` header, constant-time.
// The `X-Slack-Request-Timestamp` header must also be within 5 minutes of
// now to defeat replay attacks.

use serde_json::json;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const POST_MESSAGE_URL: &str = "https://slack.com/api/chat.postMessage";

/// Max age (seconds) of a request timestamp before we treat it as a
/// replay and reject it. Slack's own recommendation is 5 minutes.
pub const MAX_TIMESTAMP_SKEW_SECS: i64 = 60 * 5;

/// Post a plain-text message to a Slack channel/DM id via the Web API.
/// Slack's per-message text limit is ~40000 chars but blocks above ~4000
/// render poorly, so we chunk at 3500 on paragraph boundaries.
///
/// NOTE: chat.postMessage returns HTTP 200 even on logical failure, with
/// `{"ok": false, "error": "..."}` in the body — so we parse `ok` rather
/// than trusting the status code.
pub async fn post_message(
    client:     &reqwest::Client,
    bot_token:  &str,
    channel:    &str,
    text:       &str,
) -> Result<(), String> {
    for chunk in split_for_slack(text) {
        let resp = client
            .post(POST_MESSAGE_URL)
            .bearer_auth(bot_token)
            .json(&json!({ "channel": channel, "text": chunk }))
            .send()
            .await
            .map_err(|e| format!("send: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("Slack HTTP {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await
            .map_err(|e| format!("parse: {}", e))?;
        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            return Err(format!("Slack API error: {}", err));
        }
    }
    Ok(())
}

/// Verify a Slack Events API request signature. `timestamp` is the
/// `X-Slack-Request-Timestamp` header, `signature` the `X-Slack-Signature`
/// header, `raw_body` the exact bytes received. `now_unix` is passed in
/// (rather than read from the clock) so the replay window is testable.
pub fn verify_signature(
    signing_secret: &str,
    timestamp:      &str,
    raw_body:       &[u8],
    signature:      &str,
    now_unix:       i64,
) -> bool {
    // Replay guard: reject stale (or absurdly future) timestamps.
    let Ok(ts) = timestamp.parse::<i64>() else { return false; };
    if (now_unix - ts).abs() > MAX_TIMESTAMP_SKEW_SECS {
        return false;
    }
    let Some(hex_sig) = signature.strip_prefix("v0=") else { return false; };
    let Ok(expected) = hex::decode(hex_sig) else { return false; };

    // basestring = "v0:{timestamp}:{body}"
    let mut mac = match Hmac::<Sha256>::new_from_slice(signing_secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(raw_body);
    mac.verify_slice(&expected).is_ok()
}

/// Slack text chunker. Paragraph-first, hard-slice oversized paragraphs.
pub fn split_for_slack(text: &str) -> Vec<String> {
    const LIMIT: usize = 3500;
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

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    fn sign(secret: &str, ts: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(b"v0:");
        mac.update(ts.as_bytes());
        mac.update(b":");
        mac.update(body);
        format!("v0={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signature_roundtrip_valid() {
        let secret = "shhh";
        let ts = "1700000000";
        let body = br#"{"type":"event_callback"}"#;
        let sig = sign(secret, ts, body);
        // now within the window of ts.
        assert!(verify_signature(secret, ts, body, &sig, 1700000010));
    }

    #[test]
    fn signature_rejects_stale_timestamp() {
        let secret = "shhh";
        let ts = "1700000000";
        let body = b"x";
        let sig = sign(secret, ts, body);
        // now is 10 minutes after ts → outside the 5-min window.
        assert!(!verify_signature(secret, ts, body, &sig, 1700000000 + 600));
    }

    #[test]
    fn signature_rejects_wrong_secret() {
        let ts = "1700000000";
        let body = b"x";
        let sig = sign("right", ts, body);
        assert!(!verify_signature("wrong", ts, body, &sig, 1700000010));
    }

    #[test]
    fn signature_rejects_tampered_body() {
        let secret = "s";
        let ts = "1700000000";
        let sig = sign(secret, ts, b"original");
        assert!(!verify_signature(secret, ts, b"tampered", &sig, 1700000010));
    }

    #[test]
    fn signature_rejects_malformed() {
        assert!(!verify_signature("s", "1700000000", b"x", "nope", 1700000000));
        assert!(!verify_signature("s", "notanumber", b"x", "v0=ab", 1700000000));
        assert!(!verify_signature("s", "1700000000", b"x", "v0=nothex", 1700000000));
    }

    #[test]
    fn chunker_is_lossless_under_cap() {
        let huge: String = std::iter::repeat('z').take(9000).collect();
        let chunks = split_for_slack(&huge);
        assert!(chunks.len() >= 3);
        for c in &chunks { assert!(c.chars().count() <= 3500); }
        assert_eq!(chunks.concat().chars().count(), 9000);
    }
}
