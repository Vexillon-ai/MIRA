// SPDX-License-Identifier: AGPL-3.0-or-later

// src/matrix/api.rs
//
// Minimal Matrix Client-Server REST surface: send a message, learn our
// own user id, join an invited room, and run one /sync long-poll. All
// calls are authenticated with the account's access token as a Bearer.
//
// Homeserver base URL comes from the account config (e.g.
// "https://matrix.org" or a self-hosted Synapse/Dendrite). We append the
// versioned client API path. Tokens are long-lived (Element-style access
// tokens or ones minted via /login).

use serde_json::json;

use super::types::{SyncResponse, WhoAmI};

const CLIENT_API: &str = "/_matrix/client/v3";

/// Send a plain-text `m.text` message to `room_id`. Matrix has no hard
/// per-message size cap comparable to Discord's 2000, but very large
/// bodies are impolite + can be rejected by homeservers, so we chunk at
/// a generous 8000 chars on paragraph boundaries. `txn_id` must be unique
/// per message for idempotency; the caller supplies a monotonic value.
pub async fn send_message(
    client:    &reqwest::Client,
    homeserver: &str,
    token:     &str,
    room_id:   &str,
    body:      &str,
    txn_seed:  u64,
) -> Result<(), String> {
    let base = homeserver.trim_end_matches('/');
    for (i, chunk) in split_for_matrix(body).into_iter().enumerate() {
        // Transaction id: unique per chunk so a retry of the whole reply
        // doesn't collapse chunks. Homeservers dedupe on (token, txnId).
        let txn = format!("mira-{}-{}", txn_seed, i);
        let url = format!(
            "{}{}/rooms/{}/send/m.room.message/{}",
            base, CLIENT_API,
            urlencode(room_id),
            txn,
        );
        let resp = client
            .put(&url)
            .bearer_auth(token)
            .json(&json!({ "msgtype": "m.text", "body": chunk }))
            .send()
            .await
            .map_err(|e| format!("send: {}", e))?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(format!("Matrix {}: {}", status, preview(&txt, 256)));
        }
    }
    Ok(())
}

/// Resolve the bot's own Matrix user id (e.g. `@mira:hs.tld`) so the
/// dispatcher can skip our own echoed messages.
pub async fn whoami(
    client:     &reqwest::Client,
    homeserver: &str,
    token:      &str,
) -> Result<String, String> {
    let base = homeserver.trim_end_matches('/');
    let url = format!("{}{}/account/whoami", base, CLIENT_API);
    let resp = client.get(&url).bearer_auth(token).send().await
        .map_err(|e| format!("whoami send: {}", e))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(format!("whoami {}: {}", s, preview(&t, 200)));
    }
    let who: WhoAmI = resp.json().await.map_err(|e| format!("whoami parse: {}", e))?;
    Ok(who.user_id)
}

/// Join a room by id (used to auto-accept invites so a user can start a
/// conversation by inviting the bot to a fresh room). Idempotent — joining
/// an already-joined room succeeds.
pub async fn join_room(
    client:     &reqwest::Client,
    homeserver: &str,
    token:      &str,
    room_id:    &str,
) -> Result<(), String> {
    let base = homeserver.trim_end_matches('/');
    let url = format!("{}{}/rooms/{}/join", base, CLIENT_API, urlencode(room_id));
    let resp = client.post(&url).bearer_auth(token)
        .json(&json!({}))
        .send().await
        .map_err(|e| format!("join send: {}", e))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(format!("join {}: {}", s, preview(&t, 200)));
    }
    Ok(())
}

/// One `/sync` long-poll. `since` is the batch token from the previous
/// sync (None for the first call — we request a near-empty initial sync
/// with a `since` of the filter that limits backfill). `timeout_ms` is how
/// long the server holds the request open when there's nothing new.
pub async fn sync_once(
    client:     &reqwest::Client,
    homeserver: &str,
    token:      &str,
    since:      Option<&str>,
    timeout_ms: u64,
) -> Result<SyncResponse, String> {
    let base = homeserver.trim_end_matches('/');
    let mut url = format!("{}{}/sync?timeout={}", base, CLIENT_API, timeout_ms);
    if let Some(s) = since {
        url.push_str("&since=");
        url.push_str(&urlencode(s));
    } else {
        // First sync: ask for the smallest possible initial payload so we
        // don't pull the entire room history. A filter limiting the
        // timeline to 1 event per room keeps the connect cheap; we then
        // drop anything older than connect-time in the dispatcher.
        url.push_str("&filter=");
        url.push_str(&urlencode(r#"{"room":{"timeline":{"limit":1}}}"#));
    }
    let resp = client.get(&url).bearer_auth(token).send().await
        .map_err(|e| format!("sync send: {}", e))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(format!("sync {}: {}", s, preview(&t, 256)));
    }
    resp.json::<SyncResponse>().await.map_err(|e| format!("sync parse: {}", e))
}

/// Split a body for Matrix. Matrix tolerates large messages but we keep
/// them civil: paragraph-first split, then hard-slice an oversized
/// paragraph. Returns at least one chunk (possibly empty).
pub fn split_for_matrix(text: &str) -> Vec<String> {
    const LIMIT: usize = 8000;
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

/// Percent-encode a path segment. Matrix room ids (`!abc:hs.tld`) and
/// batch tokens contain `!`, `:`, `+`, `/` etc. that must be escaped.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn preview(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..n]) }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_matrix_ids() {
        assert_eq!(urlencode("!abc:hs.tld"), "%21abc%3Ahs.tld");
        assert_eq!(urlencode("plain-id_1.0~"), "plain-id_1.0~");
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(split_for_matrix("hello"), vec!["hello".to_string()]);
    }

    #[test]
    fn oversized_text_splits_and_is_lossless() {
        let huge: String = std::iter::repeat('x').take(20000).collect();
        let chunks = split_for_matrix(&huge);
        assert!(chunks.len() >= 3);
        for c in &chunks { assert!(c.chars().count() <= 8000); }
        assert_eq!(chunks.concat().chars().count(), 20000);
    }

    #[test]
    fn counts_chars_not_bytes() {
        let emoji: String = "🪐".repeat(9000);
        for c in split_for_matrix(&emoji) {
            assert!(c.chars().count() <= 8000);
        }
    }
}
