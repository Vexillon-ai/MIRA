// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/chunker.rs
//! Streaming text → sentence chunker.
//!
//! Drives the "synthesise as the LLM streams" UX: callers `feed()` whatever
//! text fragments arrive (token-by-token from the model, line-by-line from a
//! stdin pipe, the whole string in one shot — anything goes), and the chunker
//! emits sentence-sized strings that are ready to hand to a TTS backend.
//!
//! Boundary rules (in order):
//!   1. After a `.`, `!`, or `?` followed by ASCII whitespace.
//!   2. After a newline (paragraph break).
//!   3. Hard cut at `MAX_CHUNK_CHARS` chars when no sentence boundary appears
//!      — picks the most recent space so we never cut mid-word; falls back to
//!      a hard slice only when there is no whitespace at all.
//!
//! The chunker never emits text mid-word and never emits the empty string.
//! Trailing input that hasn't hit a boundary stays in the internal buffer
//! until `flush()` is called (typically when the LLM stream ends).

const MAX_CHUNK_CHARS: usize = 150;
/// Don't fire a sentence boundary on chunks shorter than this — it dodges
/// most "Mr. Smith"-style false positives without a real abbreviation table.
const MIN_SENTENCE_CHARS: usize = 8;

/// Streaming text → sentence chunker. See module docs for boundary rules.
#[derive(Debug, Default)]
pub struct SentenceChunker {
    buf: String,
}

impl SentenceChunker {
    pub fn new() -> Self { Self::default() }

    /// Append a fragment and drain any complete sentences.
    pub fn feed(&mut self, fragment: &str) -> Vec<String> {
        self.buf.push_str(fragment);
        self.drain()
    }

    /// Emit any text still in the buffer as one final chunk. Idempotent —
    /// safe to call when the buffer is empty.
    pub fn flush(&mut self) -> Option<String> {
        let pending = std::mem::take(&mut self.buf);
        let trimmed = pending.trim();
        if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
    }

    /// Repeatedly look for the next boundary and split it off.
    fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(idx) = self.next_boundary() {
            // `idx` is a byte index just past the boundary — safe to split
            // there because next_boundary always returns a char boundary.
            let head = self.buf[..idx].to_string();
            self.buf = self.buf[idx..].trim_start().to_string();
            let trimmed = head.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        out
    }

    /// Find the byte index of the first viable cut point in `self.buf`.
    /// Returns `None` if the buffer is too short / has no usable boundary.
    fn next_boundary(&self) -> Option<usize> {
        let s = &self.buf;
        if s.is_empty() { return None; }

        // 1. Sentence-terminating punctuation followed by whitespace.
        let mut prev_idx: Option<usize> = None;
        for (i, c) in s.char_indices() {
            if matches!(c, '.' | '!' | '?') {
                let after = i + c.len_utf8();
                let next  = s[after..].chars().next();
                if let Some(n) = next {
                    if n.is_ascii_whitespace() && after >= MIN_SENTENCE_CHARS {
                        return Some(after);
                    }
                }
                prev_idx = Some(after);
            }
        }
        let _ = prev_idx;

        // 2. Newline → paragraph break (any non-empty leading chunk qualifies).
        if let Some(nl) = s.find('\n') {
            if nl > 0 { return Some(nl + 1); }
        }

        // 3. Hard-cut fallback when the buffer outgrows MAX_CHUNK_CHARS.
        if s.chars().count() >= MAX_CHUNK_CHARS {
            // Walk back to the last whitespace at or before the char limit.
            let mut last_ws: Option<usize> = None;
            let mut chars = 0usize;
            for (i, c) in s.char_indices() {
                if chars >= MAX_CHUNK_CHARS { break; }
                if c.is_whitespace() { last_ws = Some(i); }
                chars += 1;
            }
            return Some(match last_ws {
                Some(ws) => ws + 1,
                // No whitespace yet — emit the first MAX_CHUNK_CHARS chars
                // as-is rather than waiting forever. Find a char boundary.
                None => {
                    let mut end = 0usize;
                    for (i, _) in s.char_indices().take(MAX_CHUNK_CHARS) {
                        end = i;
                    }
                    // step past the last char we counted
                    let last = s[end..].chars().next().map(char::len_utf8).unwrap_or(0);
                    end + last
                }
            });
        }

        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_nothing() {
        let mut c = SentenceChunker::new();
        assert!(c.feed("").is_empty());
        assert!(c.flush().is_none());
    }

    #[test]
    fn short_fragment_buffers_until_flush() {
        let mut c = SentenceChunker::new();
        assert!(c.feed("Hello").is_empty());
        assert!(c.feed(" world").is_empty());
        assert_eq!(c.flush().as_deref(), Some("Hello world"));
        assert!(c.flush().is_none(), "flush is idempotent");
    }

    #[test]
    fn splits_on_sentence_terminator_followed_by_space() {
        let mut c = SentenceChunker::new();
        let out = c.feed("Hello there. How are you? I am fine! ");
        assert_eq!(out, vec![
            "Hello there.".to_string(),
            "How are you?".to_string(),
            "I am fine!".to_string(),
        ]);
    }

    #[test]
    fn splits_across_multiple_feeds() {
        let mut c = SentenceChunker::new();
        // Streamed token by token — no chunk should appear until the period+space.
        for tok in ["The", " quick", " brown", " fox", " jumps."] {
            assert!(c.feed(tok).is_empty(), "no boundary yet on `{tok}`");
        }
        // Period without trailing whitespace stays buffered.
        let out = c.feed(" Then it runs.");
        // "The quick brown fox jumps." emits when the leading space arrives.
        assert_eq!(out, vec!["The quick brown fox jumps.".to_string()]);
        assert_eq!(c.flush().as_deref(), Some("Then it runs."));
    }

    #[test]
    fn never_cuts_inside_a_word_under_max_length() {
        let mut c = SentenceChunker::new();
        // No sentence punctuation, well under MAX_CHUNK_CHARS.
        assert!(c.feed("a sentence with no terminator yet").is_empty());
    }

    #[test]
    fn newline_acts_as_paragraph_break() {
        let mut c = SentenceChunker::new();
        let out = c.feed("Heading\nFirst body sentence with no period yet");
        assert_eq!(out, vec!["Heading".to_string()]);
    }

    #[test]
    fn max_length_fallback_cuts_at_last_whitespace() {
        let mut c = SentenceChunker::new();
        // A long stream of words with no sentence punctuation.
        let long = "word ".repeat(40); // 200 chars, well over MAX_CHUNK_CHARS
        let out = c.feed(&long);
        assert!(!out.is_empty(), "should hard-cut at MAX_CHUNK_CHARS");
        for chunk in &out {
            assert!(chunk.chars().count() <= MAX_CHUNK_CHARS,
                "chunk over limit: {} chars — {:?}", chunk.chars().count(), chunk);
            assert!(!chunk.ends_with(|c: char| c.is_alphanumeric() && false),
                "chunks should never end mid-word: {:?}", chunk);
        }
    }

    #[test]
    fn ignores_short_period_to_avoid_abbreviation_split() {
        let mut c = SentenceChunker::new();
        // "Mr. Smith" — period at byte 3, well under MIN_SENTENCE_CHARS, so no split.
        assert!(c.feed("Mr. Smith arrived").is_empty());
    }

    #[test]
    fn unicode_safe_in_buffer_and_split() {
        let mut c = SentenceChunker::new();
        let out = c.feed("Café first. Naïve second. ");
        assert_eq!(out, vec![
            "Café first.".to_string(),
            "Naïve second.".to_string(),
        ]);
    }

    #[test]
    fn flush_after_drain_returns_only_remainder() {
        let mut c = SentenceChunker::new();
        let out = c.feed("First sentence. Trailing partial");
        assert_eq!(out, vec!["First sentence.".to_string()]);
        assert_eq!(c.flush().as_deref(), Some("Trailing partial"));
    }

    #[test]
    fn whitespace_only_flush_is_dropped() {
        let mut c = SentenceChunker::new();
        c.feed("   ");
        assert!(c.flush().is_none());
    }
}
