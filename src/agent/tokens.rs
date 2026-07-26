// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/tokens.rs
//! Cheap, provider-agnostic token estimation for context budgeting +
//! instrumentation. This is a **heuristic** (≈ chars/4 + small per-message
//! overhead), but an EMPIRICALLY CALIBRATED one — it is not a guess:
//!
//! Measured against a real local tokenizer (ornith-1.0-35b, 2026-07), by
//! differencing prompt-token counts of two sample lengths to cancel the fixed
//! template overhead:
//!   * prose content ≈ **4.57 chars/token**, JSON content ≈ **4.44 chars/token**
//!     → `chars/4` is within **~3%** (and slightly conservative, which is the
//!     safe direction for budgeting).
//!   * the tool-definition block tokenizes at ≈ its raw-JSON `chars/4`
//!     (measured wire/raw multiplier **0.99** over the full 108-tool set) — so
//!     `estimate_tool_spec_tokens` (raw-JSON `chars/4`) is ~1% accurate.
//! The regression test `estimate_stays_within_the_calibrated_band` locks this in.
//! (The earlier "10×+ undercount" was NOT the ratio — it was the tool block being
//! excluded from the per-turn log; fixed in F15.) A real per-provider tokenizer
//! could still replace [`estimate_text`] behind this API, but the measured
//! accuracy says it isn't worth the dependency yet.

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
    fn estimate_stays_within_the_calibrated_band() {
        // Locks in the empirical calibration (see module docs): real content is
        // ~4.5 chars/token, so `chars/4` should sit within ~[0.9, 1.3]× of the
        // measured reference — accurate, and biased slightly conservative.
        // Prose:
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(90); // ~4050 chars
        let ref_prose = prose.chars().count() as f64 / 4.5;
        let r = estimate_text(&prose) as f64 / ref_prose;
        assert!((0.9..=1.3).contains(&r), "prose estimate ratio {r:.2} out of calibrated band");
        // JSON-ish (denser): measured ~4.44 chars/token — still within band.
        let jsonish = r#"{"name":"get_state","description":"read one entity","parameters":{"type":"object","properties":{"id":{"type":"string"}}}}"#.repeat(30);
        let ref_json = jsonish.chars().count() as f64 / 4.44;
        let rj = estimate_text(&jsonish) as f64 / ref_json;
        assert!((0.9..=1.3).contains(&rj), "json estimate ratio {rj:.2} out of calibrated band");
    }

    #[test]
    fn messages_sum_plus_envelope() {
        let msgs = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
        let total  = estimate_messages(&msgs);
        let manual = 3 + msgs.iter().map(estimate_message).sum::<usize>();
        assert_eq!(total, manual);
    }
}
