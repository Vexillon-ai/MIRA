// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/tokens.rs
//! Cheap, provider-agnostic token estimation for context budgeting +
//! instrumentation. This is a **heuristic** (≈ chars/4 + small per-message
//! overhead) — good enough to pack a context budget and to measure
//! turn-over-turn deltas in the `bench context` harness. A real per-provider
//! tokenizer (tiktoken for the OpenAI family, etc.) can replace [`estimate_text`]
//! behind the same API later; the harness will tell us whether the estimate is
//! accurate enough in practice before we pay for that complexity.

use crate::types::ChatMessage;

/// Rough tokens for a plain string: ~4 chars/token. Counts `chars`, not bytes,
/// so multi-byte / non-ASCII text isn't over-counted. Rounds up.
pub fn estimate_text(s: &str) -> usize {
    (s.chars().count() + 3) / 4
}

/// Per-message structural overhead (role tag + delimiters) — a small constant
/// most chat-format tokenizers add around each message.
const MSG_OVERHEAD: usize = 4;

/// Very rough per-image cost. Real providers vary widely (~85 to 1500+ by
/// resolution/detail); a conservative constant keeps image-heavy turns from
/// being wildly under-counted until a per-provider estimate lands.
const IMAGE_TOKENS: usize = 1024;

/// Estimated tokens for one message (content + tool calls + image attachments).
pub fn estimate_message(m: &ChatMessage) -> usize {
    let mut t = MSG_OVERHEAD + estimate_text(&m.content);
    if let Some(calls) = &m.tool_calls {
        if let Ok(js) = serde_json::to_string(calls) {
            t += estimate_text(&js);
        }
    }
    if let Some(atts) = &m.attachments {
        t += atts.len() * IMAGE_TOKENS;
    }
    t
}

/// Estimated tokens for an assembled prompt (message list) — the number we
/// budget against and instrument per turn.
pub fn estimate_messages(msgs: &[ChatMessage]) -> usize {
    // Small priming overhead for the request envelope itself.
    3 + msgs.iter().map(estimate_message).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_estimate_scales_with_length() {
        assert_eq!(estimate_text(""), 0);
        assert_eq!(estimate_text("abcd"), 1);      // 4/4
        assert_eq!(estimate_text("abcde"), 2);     // ceil(5/4)
        assert!(estimate_text(&"x".repeat(400)) >= 100);
        // char-based, not byte-based: a 4-char multibyte string ≈ 1 token.
        assert_eq!(estimate_text("café"), 1);
    }

    #[test]
    fn message_includes_overhead_and_content() {
        let t = estimate_message(&ChatMessage::user("hello world"));
        assert!(t >= MSG_OVERHEAD + 3, "got {t}");
    }

    #[test]
    fn messages_sum_plus_envelope() {
        let msgs = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
        let total  = estimate_messages(&msgs);
        let manual = 3 + msgs.iter().map(estimate_message).sum::<usize>();
        assert_eq!(total, manual);
    }
}
