// SPDX-License-Identifier: AGPL-3.0-or-later

// src/external/api.rs
//
// CPP outbound (MIRA → provider) + inbound signature verification.
//
// Signature scheme (both directions, see the spec):
//   basestring = "v1:{timestamp}:{raw_body}"
//   value      = "v1=" + hex(HMAC_SHA256(secret, basestring))
// Inbound uses the account's `inbound_secret`; outbound uses
// `outbound_secret`. ±5-minute timestamp replay window. Same shape as the
// Slack signing scheme, versioned `v1=` so we can evolve it.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::types::{OutboundAudio, OutboundBody, CPP_VERSION};

/// ±window (seconds) for the inbound timestamp replay guard.
pub const MAX_TIMESTAMP_SKEW_SECS: i64 = 60 * 5;

/// Compute the CPP signature header value for `(timestamp, body)` under
/// `secret`. Used by MIRA to sign outbound calls; also the reference a
/// provider implements to sign inbound.
pub fn sign(secret: &str, timestamp: &str, raw_body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(b"v1:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(raw_body);
    format!("v1={}", hex::encode(mac.finalize().into_bytes()))
}

/// Verify an inbound CPP signature. `now_unix` is passed in for testability.
pub fn verify_signature(
    secret:    &str,
    timestamp: &str,
    raw_body:  &[u8],
    signature: &str,
    now_unix:  i64,
) -> bool {
    let Ok(ts) = timestamp.parse::<i64>() else { return false; };
    if (now_unix - ts).abs() > MAX_TIMESTAMP_SKEW_SECS {
        return false;
    }
    let Some(hex_sig) = signature.strip_prefix("v1=") else { return false; };
    let Ok(expected) = hex::decode(hex_sig) else { return false; };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(b"v1:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(raw_body);
    mac.verify_slice(&expected).is_ok()
}

/// Send an outbound message to the provider's `send_url`, signed with the
/// account's `outbound_secret`. `now_unix` is passed in for testability of
/// the caller; the live dispatcher uses the wall clock.
pub async fn send_message(
    client:          &reqwest::Client,
    send_url:        &str,
    outbound_secret: &str,
    account_id:      &str,
    conversation_id: &str,
    text:            &str,
    now_unix:        i64,
) -> Result<(), String> {
    send_message_inner(client, send_url, outbound_secret, account_id, conversation_id, text, None, now_unix).await
}

/// Like `send_message` but also attaches synthesized audio. The provider
/// plays it if it can; otherwise it falls back to the text (which is always
/// present). Used when the channel `supports_voice` and the user's voice
/// policy opts into spoken replies.
pub async fn send_message_with_audio(
    client:          &reqwest::Client,
    send_url:        &str,
    outbound_secret: &str,
    account_id:      &str,
    conversation_id: &str,
    text:            &str,
    audio:           OutboundAudio,
    now_unix:        i64,
) -> Result<(), String> {
    send_message_inner(client, send_url, outbound_secret, account_id, conversation_id, text, Some(audio), now_unix).await
}

async fn send_message_inner(
    client:          &reqwest::Client,
    send_url:        &str,
    outbound_secret: &str,
    account_id:      &str,
    conversation_id: &str,
    text:            &str,
    audio:           Option<OutboundAudio>,
    now_unix:        i64,
) -> Result<(), String> {
    let body = OutboundBody {
        cpp_version: CPP_VERSION,
        account_id,
        conversation_id,
        text,
        audio,
    };
    let raw = serde_json::to_vec(&body).map_err(|e| format!("serialise: {}", e))?;
    let ts  = now_unix.to_string();
    let sig = sign(outbound_secret, &ts, &raw);

    let resp = client
        .post(send_url)
        .header("Content-Type", "application/json")
        .header("X-MIRA-CPP-Timestamp", &ts)
        .header("X-MIRA-CPP-Signature", &sig)
        .body(raw)
        .send()
        .await
        .map_err(|e| format!("send: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        return Err(format!("provider {}: {}", status, preview(&txt, 200)));
    }
    Ok(())
}

fn preview(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..n]) }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_roundtrips() {
        let secret = "abc";
        let ts = "1700000000";
        let body = br#"{"text":"hi"}"#;
        let sig = sign(secret, ts, body);
        assert!(verify_signature(secret, ts, body, &sig, 1700000010));
    }

    #[test]
    fn verify_rejects_stale_timestamp() {
        let secret = "abc";
        let ts = "1700000000";
        let body = b"x";
        let sig = sign(secret, ts, body);
        assert!(!verify_signature(secret, ts, body, &sig, 1700000000 + 600));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let ts = "1700000000";
        let sig = sign("right", ts, b"x");
        assert!(!verify_signature("wrong", ts, b"x", &sig, 1700000010));
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let secret = "s";
        let ts = "1700000000";
        let sig = sign(secret, ts, b"orig");
        assert!(!verify_signature(secret, ts, b"tampered", &sig, 1700000010));
    }

    #[test]
    fn verify_rejects_malformed() {
        assert!(!verify_signature("s", "1700000000", b"x", "nope", 1700000000));
        assert!(!verify_signature("s", "notanum", b"x", "v1=ab", 1700000000));
        assert!(!verify_signature("s", "1700000000", b"x", "v1=zz", 1700000000));
    }

    #[test]
    fn distinct_secrets_produce_distinct_sigs() {
        // Inbound vs outbound secrets must not be interchangeable.
        let ts = "1700000000";
        let body = b"same body";
        assert_ne!(sign("inbound", ts, body), sign("outbound", ts, body));
    }
}
