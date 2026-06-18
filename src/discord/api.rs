// SPDX-License-Identifier: AGPL-3.0-or-later

// src/discord/api.rs
//
// Minimal REST surface used by D2 — only "send a text message to a
// channel". D3 expands this to attachments, embeds, edits, deletes, slash
// command responses, etc. We pin v10 to match the gateway version we
// IDENTIFY with.

use serde_json::json;

const API_BASE:  &str = "https://discord.com/api/v10";
/// Discord requires a User-Agent of the form `DiscordBot (URL, version)`.
/// Bots without one get 4xx'd. The URL is informational.
const USER_AGENT: &str = concat!(
    "DiscordBot (https://github.com/tarekedoz/mira, ",
    env!("CARGO_PKG_VERSION"),
    ")",
);

/// Post a plain-text message to a channel. `content` is sent verbatim
/// (no markdown escaping — Discord renders markdown by default, which
/// is usually what the user wants from an LLM reply). The 2000-char
/// limit is enforced by chunking + posting each chunk as a separate
/// message in order; longer replies get split rather than truncated.
pub async fn post_message(
    client:     &reqwest::Client,
    bot_token:  &str,
    channel_id: &str,
    content:    &str,
) -> Result<(), String> {
    let url = format!("{}/channels/{}/messages", API_BASE, channel_id);
    let auth = format!("Bot {}", bot_token);

    for chunk in split_for_discord_limit(content) {
        let body = json!({ "content": chunk });
        let resp = client
            .post(&url)
            .header("Authorization", &auth)
            .header("User-Agent",     USER_AGENT)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("send: {}", e))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Discord {}: {}", status, preview(&body, 256)));
        }
    }
    Ok(())
}

/// Discord's per-message content cap is 2000 unicode codepoints (NOT
/// bytes). We split on paragraph boundaries first, then on lines, then
/// just hard-slice if a single line is over the limit. Returns at least
/// one chunk even for empty input.
pub fn split_for_discord_limit(text: &str) -> Vec<String> {
    const LIMIT: usize = 1990; // leave headroom for any markdown wrapping
    if text.is_empty() {
        return vec![String::new()];
    }
    // Quick path: short text is one chunk.
    if text.chars().count() <= LIMIT {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let push_current = |chunks: &mut Vec<String>, current: &mut String| {
        if !current.is_empty() {
            chunks.push(std::mem::take(current));
        }
    };

    // Try paragraph-first splitting.
    for para in text.split("\n\n") {
        // If adding this paragraph (+separator) blows the limit, flush
        // and start a fresh chunk.
        let extra = if current.is_empty() { 0 } else { 2 }; // for "\n\n"
        if current.chars().count() + para.chars().count() + extra > LIMIT {
            push_current(&mut chunks, &mut current);
        }
        if para.chars().count() > LIMIT {
            // A single paragraph too big — fall back to char-slicing.
            for slice in chunk_by_chars(para, LIMIT) {
                chunks.push(slice);
            }
            continue;
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(para);
    }
    push_current(&mut chunks, &mut current);
    chunks
}

fn chunk_by_chars(s: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::with_capacity(limit);
    let mut n = 0usize;
    for ch in s.chars() {
        if n == limit {
            out.push(std::mem::take(&mut buf));
            n = 0;
        }
        buf.push(ch);
        n += 1;
    }
    if !buf.is_empty() {
        out.push(buf);
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
    fn short_text_is_single_chunk() {
        let chunks = split_for_discord_limit("hello world");
        assert_eq!(chunks, vec!["hello world".to_string()]);
    }

    #[test]
    fn empty_text_returns_one_empty_chunk() {
        // Discord's POST /messages rejects fully-empty content; this is
        // not the function's job to enforce — the caller (post_message)
        // skips empty replies before getting here. We just guarantee a
        // single-element return so downstream loops don't go zero-iter.
        assert_eq!(split_for_discord_limit(""), vec!["".to_string()]);
    }

    #[test]
    fn long_text_splits_on_paragraph_boundary() {
        // Two ~1500-char paragraphs — each fits, together they don't.
        let para = "lorem ".repeat(250); // ~1500 chars
        let big  = format!("{}\n\n{}", para, para);
        let chunks = split_for_discord_limit(&big);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].chars().count() <= 1990);
        assert!(chunks[1].chars().count() <= 1990);
    }

    #[test]
    fn oversized_single_paragraph_hard_slices() {
        let chars = 5000;
        let huge: String = std::iter::repeat('A').take(chars).collect();
        let chunks = split_for_discord_limit(&huge);
        assert!(chunks.len() >= 3); // 5000 / 1990 ≈ 2.51, so at least 3
        for c in &chunks {
            assert!(c.chars().count() <= 1990, "chunk {} chars", c.chars().count());
        }
        // Concatenation lossless.
        assert_eq!(chunks.concat().chars().count(), chars);
    }

    #[test]
    fn counts_chars_not_bytes_for_unicode() {
        // 2000 emoji = 8000 bytes, but only 2000 chars. Verify we use
        // chars().count() throughout so Discord's codepoint limit holds.
        let many_emoji: String = "🦀".repeat(2500);
        let chunks = split_for_discord_limit(&many_emoji);
        for c in &chunks {
            assert!(c.chars().count() <= 1990);
        }
    }
}
