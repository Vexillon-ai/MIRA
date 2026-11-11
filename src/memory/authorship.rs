// SPDX-License-Identifier: AGPL-3.0-or-later

//! Code-enforced authorship + provenance gates for memory extraction (Part A).
//!
//! The LLM extractor's system prompt *asks* the model to store only facts the
//! USER stated and to ignore the assistant's own text — but a small local model
//! ignores that instruction often enough to matter (assistant `<thinking>` text
//! and MIRA's own internal state have leaked into memory as "facts about the
//! user"). These gates enforce the rule in code rather than trusting the prompt:
//!
//! - [`strip_reasoning_blocks`] sanitises extractor *inputs* (both turns) so a
//!   reasoning block never reaches the extractor prompt. An **unclosed** opening
//!   tag truncates to end-of-input — a response cut off mid-thought is exactly
//!   how the worst leak formed.
//! - [`contains_reasoning_markup`] flags a *candidate* that still carries any
//!   reasoning markup (bare closers and attributes included).
//! - [`grounding_verdict`] checks a candidate against the user's own turn:
//!   reasoning markup, no distinctive-token support (catches assistant-authored
//!   and world-knowledge facts), and multi-digit numbers the user never said
//!   (the exact shape of MIRA's own state becoming a user fact).
//!
//! Design stance: **permissive on wording, strict on provenance.** The extractor
//! is told to rewrite into the third person, so a *ratio* threshold would reject
//! good paraphrases; instead a single shared distinctive token grounds a
//! candidate. Single digits are excluded from the number gate as too
//! collision-prone.

/// Reasoning-style tags whose contents are the model's private scratch work,
/// never user-stated fact. Matched case-insensitively, as openers (with or
/// without attributes), closers, and bare/unclosed forms.
const REASONING_TAGS: &[&str] = &["thinking", "think", "reasoning", "scratchpad", "reflection"];

/// Remove reasoning blocks from `text`. A matched `<tag>…</tag>` pair is dropped
/// (keeping the surrounding text); an **unclosed** `<tag>` truncates everything
/// from the tag to the end of input. Case-insensitive across every tag.
pub fn strip_reasoning_blocks(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        // Find the next reasoning opener at or after i.
        if let Some((open_start, tag_len)) = next_opener(&lower, i) {
            // Keep everything up to the opener.
            out.push_str(&text[i..open_start]);
            // Where does the opening tag end (the `>`)?
            let after_open = match lower[open_start..].find('>') {
                Some(gt) => open_start + gt + 1,
                None => return out, // malformed, unterminated tag → drop to end
            };
            let _ = tag_len;
            // Find the matching closer `</tag>` after the opener.
            if let Some(close_rel) = find_any_closer(&lower, after_open) {
                // Skip the whole block including the closer.
                let close_end = after_open + close_rel;
                i = close_end;
            } else {
                // Unclosed opener → truncate to end of input.
                return out;
            }
        } else {
            out.push_str(&text[i..]);
            break;
        }
    }
    out
}

/// Does `text` contain ANY reasoning markup — an opener (with/without
/// attributes) or a closer — for any reasoning tag? Case-insensitive.
pub fn contains_reasoning_markup(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    next_opener(&lower, 0).is_some() || first_closer(&lower, 0).is_some()
}

/// Find the earliest reasoning opener `<tag` (optionally with attributes) at or
/// after `from`. Returns (byte index of `<`, tag length). The tag must be
/// followed by whitespace, `>`, or `/` so `<thinker>` ≠ `<think>`.
fn next_opener(lower: &str, from: usize) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for tag in REASONING_TAGS {
        let needle = format!("<{tag}");
        let mut start = from;
        while let Some(rel) = lower[start..].find(&needle) {
            let idx = start + rel;
            let after = idx + needle.len();
            let next = lower[after..].chars().next();
            let boundary = matches!(next, None | Some('>') | Some('/') | Some(' ')
                | Some('\t') | Some('\n') | Some('\r'));
            if boundary {
                if best.map(|(b, _)| idx < b).unwrap_or(true) {
                    best = Some((idx, needle.len()));
                }
                break;
            }
            start = after;
        }
    }
    best
}

/// Find the earliest reasoning closer `</tag>` at or after `from`, returning its
/// end offset relative to `from` (i.e. one past the `>`).
fn find_any_closer(lower: &str, from: usize) -> Option<usize> {
    let mut best: Option<usize> = None;
    for tag in REASONING_TAGS {
        let needle = format!("</{tag}>");
        if let Some(rel) = lower[from..].find(&needle) {
            let end = rel + needle.len();
            if best.map(|b| end < b).unwrap_or(true) {
                best = Some(end);
            }
        }
    }
    best
}

fn first_closer(lower: &str, from: usize) -> Option<usize> {
    find_any_closer(lower, from).map(|rel| from + rel)
}

/// The outcome of grounding a candidate fact against the user's own turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroundingVerdict {
    /// Shares distinctive support with the user turn and carries no unsupported
    /// number or reasoning markup — safe to store.
    Grounded,
    /// No distinctive token overlaps the user's turn — likely assistant-authored
    /// or world knowledge the user never stated.
    NoUserSupport,
    /// Carries a multi-digit number the user never said — the shape of MIRA's own
    /// internal state (counts, queue sizes) being recorded as a user fact.
    UnsupportedNumber,
    /// The candidate itself still contains reasoning markup.
    ReasoningMarkup,
}

impl GroundingVerdict {
    pub fn name(&self) -> &'static str {
        match self {
            GroundingVerdict::Grounded => "grounded",
            GroundingVerdict::NoUserSupport => "no_user_support",
            GroundingVerdict::UnsupportedNumber => "unsupported_number",
            GroundingVerdict::ReasoningMarkup => "reasoning_markup",
        }
    }
    pub fn is_grounded(&self) -> bool {
        matches!(self, GroundingVerdict::Grounded)
    }
}

/// Minimal stopword set for distinctive-token support. Tokens shorter than 4
/// chars are already excluded, so this only needs the ≥4-char filler words.
const STOPWORDS: &[&str] = &[
    "that", "this", "with", "from", "have", "your", "been", "into", "than", "then",
    "there", "here", "over", "under", "also", "just", "like", "some", "more", "most",
    "such", "only", "very", "much", "many", "each", "other", "these", "those", "what",
    "when", "where", "which", "their", "they", "them", "about", "would", "could", "should",
    "will", "them", "does", "done", "said", "says", "tell", "told", "want", "need",
];

/// Lowercase alphanumeric tokens of length ≥ 4 that aren't stopwords and aren't
/// the literal word "user" (which every third-person paraphrase contains).
fn distinctive_tokens(text: &str) -> Vec<String> {
    tokens(text)
        .into_iter()
        .filter(|t| t.chars().count() >= 4 && t != "user" && !STOPWORDS.contains(&t.as_str()))
        .collect()
}

/// All lowercase alphanumeric word tokens.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Multi-digit (≥2) number tokens in `text`.
fn multi_digit_numbers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let push = |cur: &mut String, out: &mut Vec<String>| {
        if cur.chars().count() >= 2 {
            out.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for c in text.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else {
            push(&mut cur, &mut out);
        }
    }
    push(&mut cur, &mut out);
    out
}

/// Grade a candidate fact against the user's turn. The user turn is
/// reasoning-stripped first, so a leak inside the user's own message cannot
/// ground a fact.
pub fn grounding_verdict(content: &str, user_msg: &str) -> GroundingVerdict {
    // 1. Candidate must carry no reasoning markup of its own.
    if contains_reasoning_markup(content) {
        return GroundingVerdict::ReasoningMarkup;
    }
    let user_clean = strip_reasoning_blocks(user_msg);

    // 2. A multi-digit number the user never said → reject (even if the topic
    //    otherwise overlaps). Single digits are excluded as too collision-prone.
    let user_nums: std::collections::HashSet<String> =
        multi_digit_numbers(&user_clean).into_iter().collect();
    for n in multi_digit_numbers(content) {
        if !user_nums.contains(&n) {
            return GroundingVerdict::UnsupportedNumber;
        }
    }

    // 3. At least one distinctive token must appear in a clause the user
    //    ASSERTED — not merely asked (M2). "what's my favourite tea?" must not
    //    ground "User's favourite tea is X"; only "my favourite tea is genmaicha"
    //    does. Questions are stripped before the support check.
    let user_toks: std::collections::HashSet<String> =
        tokens(&asserted_text(&user_clean)).into_iter().collect();
    let has_support = distinctive_tokens(content).iter().any(|t| user_toks.contains(t));
    if !has_support {
        return GroundingVerdict::NoUserSupport;
    }
    GroundingVerdict::Grounded
}

/// The user's ASSERTED text — interrogative clauses removed. A clause ending in
/// `?` is a question and contributes no grounding support; every other clause
/// (including a trailing fragment with no terminator) is treated as asserted.
fn asserted_text(text: &str) -> String {
    let mut out = String::new();
    let mut clause = String::new();
    for c in text.chars() {
        match c {
            '?' => clause.clear(), // the clause was a question — drop it
            '.' | '!' | '\n' => { out.push_str(&clause); out.push(' '); clause.clear(); }
            _ => clause.push(c),
        }
    }
    out.push_str(&clause); // trailing fragment with no terminator = asserted
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_reasoning_blocks ───────────────────────────────────────────────
    #[test]
    fn strips_a_closed_thinking_block_and_keeps_the_rest() {
        let out = strip_reasoning_blocks("before <thinking>secret plan</thinking> after");
        assert!(!out.contains("secret"), "block content must be gone: {out:?}");
        assert!(out.contains("before"), "surrounding text kept: {out:?}");
        assert!(out.contains("after"), "surrounding text kept: {out:?}");
    }

    #[test]
    fn strips_an_unclosed_block_to_end_of_input() {
        // A response cut off mid-thought (row #3760's shape): no closer → drop to end.
        let out = strip_reasoning_blocks("keep this <thinking>runaway thought that never closes");
        assert_eq!(out.trim(), "keep this");
        assert!(!out.contains("runaway"));
    }

    #[test]
    fn stripping_is_case_insensitive_and_covers_each_tag() {
        for (open, close) in [
            ("<THINKING>", "</THINKING>"),
            ("<Think>", "</Think>"),
            ("<Reasoning>", "</Reasoning>"),
            ("<scratchpad>", "</scratchpad>"),
            ("<REFLECTION>", "</reflection>"),
        ] {
            let input = format!("a {open}x{close} b");
            let out = strip_reasoning_blocks(&input);
            assert!(!out.contains('x'), "tag {open} not stripped: {out:?}");
            assert!(out.contains('a') && out.contains('b'), "kept surrounds for {open}: {out:?}");
        }
    }

    // ── contains_reasoning_markup ────────────────────────────────────────────
    #[test]
    fn markup_detector_catches_bare_closers_and_attributes() {
        assert!(contains_reasoning_markup("</think>"));
        assert!(contains_reasoning_markup("<reasoning step=\"3\">"));
        assert!(contains_reasoning_markup("text <thinking attr=1> more"));
        assert!(!contains_reasoning_markup("a perfectly normal sentence"));
        assert!(!contains_reasoning_markup("the thinker statue is in Paris"), "word boundary: <thinker> ≠ <think>");
    }

    // ── grounding_verdict ────────────────────────────────────────────────────
    #[test]
    fn rejects_the_live_leaked_memory_3760() {
        // Verbatim shape of the live leak: assistant reasoning markup as a "fact".
        let v = grounding_verdict(
            "<thinking>The user probably wants me to remember their trip</thinking>",
            "any user message",
        );
        assert_eq!(v, GroundingVerdict::ReasoningMarkup);
    }

    #[test]
    fn keeps_a_fact_the_user_actually_stated() {
        let v = grounding_verdict(
            "User enjoys hiking in the mountains.",
            "I really enjoy hiking in the mountains on weekends.",
        );
        assert_eq!(v, GroundingVerdict::Grounded);
    }

    #[test]
    fn keeps_a_third_person_paraphrase_of_the_users_own_words() {
        // Only `anthropic`/`software` overlap — a ratio gate would wrongly drop this.
        let v = grounding_verdict(
            "User works at Anthropic as a software engineer.",
            "I'm employed at Anthropic, doing software engineering.",
        );
        assert_eq!(v, GroundingVerdict::Grounded);
    }

    #[test]
    fn rejects_a_planted_assistant_sourced_fact() {
        // The assistant offered a recipe; nothing the user said supports it.
        let v = grounding_verdict(
            "User prefers risotto cooked with saffron.",
            "What should I make for dinner tonight?",
        );
        assert_eq!(v, GroundingVerdict::NoUserSupport);
    }

    #[test]
    fn rejects_support_from_a_question_only(/* M2 */) {
        // The user ASKED about their favourite tea; the only token overlap comes
        // from a question, so the fact is not grounded.
        let v = grounding_verdict(
            "User's favourite tea is genmaicha.",
            "what's my favourite tea again?",
        );
        assert_eq!(v, GroundingVerdict::NoUserSupport);
    }

    #[test]
    fn keeps_support_from_an_asserted_clause_beside_a_question() {
        // A statement + a trailing question: the statement still grounds it.
        let v = grounding_verdict(
            "User's favourite tea is genmaicha.",
            "My favourite tea is genmaicha. what else do you remember?",
        );
        assert_eq!(v, GroundingVerdict::Grounded);
    }

    #[test]
    fn rejects_world_knowledge_with_no_user_support() {
        let v = grounding_verdict(
            "The capital of France is Paris.",
            "Can you help me plan my week?",
        );
        assert_eq!(v, GroundingVerdict::NoUserSupport);
    }

    #[test]
    fn rejects_mira_internal_state_recorded_as_a_user_fact() {
        // Live rows 3759/3741/318 shape: MIRA's own queue size as a user fact.
        let v = grounding_verdict(
            "User has 19 pending wiki edits waiting for approval.",
            "Can you summarise what we discussed about the wiki?",
        );
        assert_eq!(v, GroundingVerdict::UnsupportedNumber);
    }

    #[test]
    fn rejects_a_number_the_user_never_said_even_when_topic_matches() {
        // Topic (`books`) overlaps, but the count 4000 is invented.
        let v = grounding_verdict(
            "User owns 4000 books.",
            "I love books, my shelves are overflowing.",
        );
        assert_eq!(v, GroundingVerdict::UnsupportedNumber);
    }

    #[test]
    fn keeps_a_number_the_user_did_say() {
        let v = grounding_verdict(
            "User owns 3000 books.",
            "I have about 3000 books at home.",
        );
        assert_eq!(v, GroundingVerdict::Grounded);
    }

    #[test]
    fn single_digits_are_not_treated_as_evidence() {
        // A lone single digit the user didn't say must NOT trip the number gate
        // (only multi-digit numbers are gated); support comes from `children`.
        let v = grounding_verdict(
            "User has 3 children.",
            "My children keep me busy — being a parent is a full-time job.",
        );
        assert_eq!(v, GroundingVerdict::Grounded);
    }

    #[test]
    fn grounding_ignores_reasoning_text_in_the_user_turn() {
        // A leak inside the USER turn must not ground a fact: the only overlap
        // (`saffron`) lives in a reasoning block that gets stripped first.
        let v = grounding_verdict(
            "User prefers saffron risotto.",
            "hello <thinking>they mentioned saffron risotto earlier</thinking> how are you?",
        );
        assert_eq!(v, GroundingVerdict::NoUserSupport);
    }
}
