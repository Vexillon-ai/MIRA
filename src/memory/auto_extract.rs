// SPDX-License-Identifier: AGPL-3.0-or-later

// src/memory/auto_extract.rs

//! Auto-memory extraction from conversation turns.
//!
//! Three layers, in order of increasing cost and quality:
//!
//!   1. **Heuristic** (sync, instant): pattern-match user text for names,
//!      locations, etc. — cheap and deterministic, used as the historical
//!      default.
//!   2. **Legacy LLM** ([`LlmExtractor`]): the original prompt-building
//!      scaffolding for an LLM-based extractor. Kept for callers that still
//!      use it; new code should prefer layer 3.
//!   3. **Structured LLM** ([`LlmMemoryExtractor`]): mirrors
//!      `src/onboarding/extractor.rs`. Emits strict JSON with `category`,
//!      `confidence`, and an `entity` tag so the storage layer can
//!      conflict-detect and supersede instead of duplicate-inserting.
//!      Parser is lenient (tolerates code fences + prose wrappers).
//!
//! The active layer is selected by `config.memory.auto_extract.mode`.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, warn};

use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions};

/// Detect an explicit "don't record this" consent request in a user's message, so
/// the post-turn extractors SKIP it — honouring the family-privacy invariant that
/// a member can tell MIRA not to remember something. Deliberately
/// broad: a false positive only loses one auto-memory (harmless), whereas a false
/// negative violates stated consent, so we bias toward honouring the request.
/// Gates memory extraction, the knowledge graph, and wiki extraction.
pub fn is_no_store_request(text: &str) -> bool {
    let t = text.to_lowercase();
    const PATTERNS: &[&str] = &[
        "don't record", "dont record", "do not record",
        "don't save this", "dont save this", "do not save this",
        "don't remember this", "dont remember this", "do not remember this",
        "don't store this", "dont store this", "do not store this",
        "off the record", "don't keep this", "dont keep this",
        "forget what i just", "forget that i just", "don't log this",
    ];
    PATTERNS.iter().any(|p| t.contains(p))
}

/// The full set of memory categories — used to bypass the `allowed_categories`
/// privacy gate when the user has EXPLICITLY asked to remember something.
pub const ALL_CATEGORIES: &[&str] = &["fact", "preference", "skill", "relationship", "project"];

/// Detect an explicit "remember this" request in a user's message. When set, the
/// post-turn extractor stores the fact deterministically — bypassing the
/// category/confidence gates and recording it as `user_explicit` — so an explicit
/// request can't be silently dropped by (e.g.) the Relationship category being
/// off. A "don't record" request always wins (checked first), so a negated
/// "don't remember this" is never read as an explicit save.
pub fn is_explicit_remember(text: &str) -> bool {
    if is_no_store_request(text) {
        return false;
    }
    let t = text.trim().to_lowercase();
    // Imperative openers that begin a sentence/clause. Matching only at a clause
    // start avoids false positives where the words appear mid-sentence or inside a
    // question ("I don't remember I said that", "do you remember…"). Deliberately
    // excludes the ambiguous bare "note that …" (often informational, not a save),
    // and "remember to …"/"make a note to …" (a reminder/task, not a durable fact).
    const OPENERS: &[&str] = &[
        "remember that", "remember this", "remember my", "remember we ", "remember:",
        "please remember", "make a note", "take a note", "jot down",
        "don't forget", "dont forget", "do not forget",
        "keep in mind", "for the record", "for future reference",
        "save this to memory", "add this to memory", "add to memory", "store this in memory",
    ];
    // Clause-level (M1): split on sentence terminators, remembering which clause
    // was a QUESTION (ended with '?'). A question clause never counts as a save —
    // but a remember clause elsewhere in the same message still does ("Remember my
    // dog is Pixel. What's the weather?").
    let mut clause = String::new();
    let mut matched = false;
    let flush = |clause: &str, is_question: bool, matched: &mut bool| {
        if is_question { return; }
        let c = strip_remember_fillers(clause);
        // A matched opener whose remainder is a task ("remember to call mom",
        // "make a note to …") is a reminder, not a fact — don't fact-capture it.
        if let Some(p) = OPENERS.iter().find(|p| c.starts_with(**p)) {
            let rest = c[p.len()..].trim_start();
            if rest.starts_with("to ") { return; }
            *matched = true;
        }
    };
    for ch in t.chars() {
        match ch {
            '.' | ';' | '!' | '\n' => { flush(&clause, false, &mut matched); clause.clear(); }
            '?'                     => { flush(&clause, true,  &mut matched); clause.clear(); }
            _                       => clause.push(ch),
        }
        if matched { return true; }
    }
    flush(&clause, false, &mut matched); // trailing clause (no terminator)
    matched
}

/// Turn a raw explicit-remember request into the bare fact to store — strip the
/// leading filler + opener ("Please remember that …", "note: …", "don't forget
/// …") and the connective after it, so the verbatim fallback stores "my
/// favourite colour is teal", not the whole request (N3). Returns the trimmed
/// remainder, or the original (trimmed) text when no opener is found.
pub fn strip_remember_opener(text: &str) -> String {
    // Scan each sentence clause and return the first one that begins (after
    // fillers) with an opener, stripped — so "I had a long day. Remember my
    // sister's name is Leila." stores "my sister's name is Leila", not the whole
    // message (X11). No opener anywhere → the whole trimmed text.
    let mut start = 0usize;
    for (i, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | ';' | '\n' | '?') {
            if let Some(s) = strip_one_clause_opener(&text[start..i]) { return s; }
            start = i + ch.len_utf8();
        }
    }
    if let Some(s) = strip_one_clause_opener(&text[start..]) { return s; }
    text.trim().to_string()
}

/// If `clause` begins (after conversational fillers) with a remember opener,
/// return the bare fact after it (connective stripped); else None.
fn strip_one_clause_opener(clause: &str) -> Option<String> {
    const OPENERS: &[&str] = &[
        "please remember that", "please remember this", "please remember",
        "remember that", "remember this", "remember:", "remember",
        "don't forget that", "dont forget that", "don't forget", "dont forget", "do not forget",
        "make a note that", "make a note", "take a note that", "take a note", "jot down",
        "keep in mind that", "keep in mind", "for the record", "for future reference",
        "add this to memory", "add to memory", "save this to memory", "store this in memory",
        "note that", "note:",
    ];
    let lower = clause.to_lowercase();
    let start = strip_remember_fillers(&lower);
    let offset = clause.len() - start.len(); // fillers are ascii, length-preserving
    for p in OPENERS {
        if start.starts_with(p) {
            let idx = offset + p.len();
            if idx > clause.len() || !clause.is_char_boundary(idx) {
                return Some(clause.trim().to_string());
            }
            let mut rest = clause[idx..].trim_start();
            for conn in ["that ", "this ", ": ", ":", "to remember "] {
                if let Some(r) = rest.strip_prefix(conn) { rest = r.trim_start(); break; }
            }
            let rest = rest.trim();
            return Some(if rest.is_empty() { clause.trim().to_string() } else { rest.to_string() });
        }
    }
    None
}

/// Strip leading punctuation and conversational filler words (with optional
/// commas: "Actually, please remember…", "OK, so remember:") off a clause so the
/// opener match sees the real start.
fn strip_remember_fillers(clause: &str) -> &str {
    const FILLERS: &[&str] = &[
        "and", "also", "oh", "hey", "actually", "ok", "okay", "so", "well",
        "look", "right", "um", "yeah",
    ];
    let mut c = clause.trim_start_matches(|ch: char| !ch.is_alphanumeric());
    loop {
        let mut advanced = false;
        for f in FILLERS {
            if let Some(rest) = c.strip_prefix(f) {
                // Only a filler if followed by a word boundary (space/comma), not
                // a longer word ("sold" must not strip "so").
                let boundary = rest.chars().next().map(|ch| !ch.is_alphanumeric()).unwrap_or(true);
                if boundary {
                    c = rest.trim_start_matches(|ch: char| !ch.is_alphanumeric());
                    advanced = true;
                    break;
                }
            }
        }
        if !advanced { break; }
    }
    c
}

/// Heuristic: does `content` look like a credential (password / API key / token /
/// card number) that must never be stored in plain text? Used to refuse a
/// `memory_add` / verbatim write (N3).
pub fn looks_like_secret(content: &str) -> bool {
    // Tokenise on WORD BOUNDARIES but KEEP `:` and `=` as their own tokens, so a
    // connector can be detected. A cue fires only when a CONNECTOR (is/was/:/=)
    // sits between it and a value — so "my wifi password is sunshine" and
    // "password: hunter2" are caught, while a bare mention ("I reset my password
    // today", "rotate the API key", "my PIN changed in 2023") is NOT (W3/R3). A
    // password-family cue accepts ANY value; a generic cue (pin/token/secret/key)
    // needs a credential-LIKE value (digit-bearing or long/opaque).
    let mut toks: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in content.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            cur.push(ch);
        } else {
            if !cur.is_empty() { toks.push(std::mem::take(&mut cur)); }
            if ch == ':' || ch == '=' { toks.push(ch.to_string()); }
        }
    }
    if !cur.is_empty() { toks.push(cur); }

    // A lone opaque credential token (API key / access token) needs no cue.
    // Checked on the ORIGINAL-case whitespace words (so `sk-…`/`ghp_…` prefixes and
    // a mixed-case high-entropy run survive).
    let opaque_token = |raw: &str| {
        let w = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        let lw = w.to_ascii_lowercase();
        const PREFIXES: &[&str] = &["sk-", "sk_", "ghp_", "gho_", "ghs_", "ghu_", "github_pat_", "akia", "asia", "xoxb-", "xoxp-", "xapp-", "glpat-"];
        let has_digit = w.chars().any(|c| c.is_ascii_digit());
        if PREFIXES.iter().any(|p| lw.starts_with(p)) && w.len() >= 10 && has_digit {
            return true;
        }
        // A long high-entropy alnum run: ≥20 chars, has a digit, and mixed case.
        w.len() >= 20 && w.chars().all(|c| c.is_ascii_alphanumeric())
            && has_digit
            && w.chars().any(|c| c.is_ascii_uppercase()) && w.chars().any(|c| c.is_ascii_lowercase())
    };
    if content.split_whitespace().any(opaque_token) { return true; }

    // (cue words, strong?) — strong = password family (any value counts).
    const CUES: &[(&[&str], bool)] = &[
        (&["password"], true), (&["passwd"], true), (&["passcode"], true), (&["passphrase"], true),
        (&["wifi", "password"], true), (&["wifi", "passcode"], true), (&["wifi", "passphrase"], true),
        (&["pin"], false), (&["pin", "number"], false), (&["pin", "code"], false),
        (&["cvv"], false), (&["otp"], false), (&["wifi", "key"], false),
        (&["api", "key"], false), (&["apikey"], false), (&["secret", "key"], false),
        (&["access", "token"], false), (&["bearer", "token"], false), (&["auth", "token"], false),
        (&["private", "key"], false), (&["ssh", "key"], false),
        (&["card", "number"], false), (&["door", "code"], false), (&["security", "code"], false),
        (&["token"], false), (&["secret"], false),
    ];
    let is_connector = |w: &str| matches!(w, "is" | "was" | "are" | "be" | "equals" | ":" | "=");
    let credential_like = |w: &str| {
        let has_digit = w.chars().any(|c| c.is_ascii_digit());
        let all_digit = !w.is_empty() && w.chars().all(|c| c.is_ascii_digit());
        (has_digit && w.len() >= 4)
            || (w.len() >= 16 && w.chars().all(|c| c.is_ascii_alphanumeric()))
            || (all_digit && w.len() >= 3)
    };
    // After a password cue, these values mean it's NOT a credential disclosure
    // ("the password is in 1Password", "my password was changed / too weak").
    const NON_VALUE: &[&str] = &[
        "in", "too", "weak", "strong", "changed", "expired", "saved", "reset",
        "forgotten", "lost", "secure", "safe", "missing", "stored", "managed", "protected",
    ];
    let is_pin_cue = |cue: &[&str]| cue.first() == Some(&"pin");
    for i in 0..toks.len() {
        for (cue, strong) in CUES {
            if toks[i..].len() < cue.len()
                || !toks[i..i + cue.len()].iter().zip(cue.iter()).all(|(a, b)| a == b)
            { continue; }
            let after_cue = i + cue.len();
            // PIN family: an all-digit run right after the cue is a secret even
            // without a connector ("PIN 4821").
            if is_pin_cue(cue) {
                if toks[after_cue..].iter().take(2).any(|w| w.len() >= 3 && w.chars().all(|c| c.is_ascii_digit())) {
                    return true;
                }
            }
            let mut j = after_cue;
            let mut saw_conn = false;
            while j < toks.len() && is_connector(&toks[j]) { saw_conn = true; j += 1; }
            if !saw_conn { continue; } // no "is"/":" → a mention, not a value
            let vals: Vec<&str> = toks[j..].iter().take(5).map(|s| s.as_str()).collect();
            if *strong {
                // Any real value — unless it's a non-value predicate.
                if vals.first().is_some_and(|v| !v.is_empty() && !is_connector(v) && !NON_VALUE.contains(v)) {
                    return true;
                }
            } else if vals.iter().any(|v| credential_like(v)) {
                return true;
            }
        }
    }
    false
}

/// A candidate memory extracted from text before being stored
#[derive(Debug, Clone)]
pub struct MemoryCandidate {
    pub content: String,
    pub confidence: f32, // 0.0–1.0
}

/// Heuristic extractor using regex-like pattern matching
pub struct HeuristicExtractor;

impl HeuristicExtractor {
    pub fn new() -> Self { Self }

    /// Extract memorable facts from user text.
    /// Returns candidates with confidence scores.
    pub fn extract(&self, text: &str) -> Vec<MemoryCandidate> {
        let lower = text.to_lowercase();
        let mut candidates = Vec::new();

        // "My name is X"
        if let Some(name) = Self::capture_after(&lower, text, &["my name is ", "i'm called ", "call me "]) {
            candidates.push(MemoryCandidate { content: format!("User's name is {}", name), confidence: 0.95 });
        }

        // "I live in X" / "I'm from X" / "I'm based in X"
        if let Some(loc) = Self::capture_after(&lower, text, &["i live in ", "i'm from ", "i am from ", "i'm based in ", "i was born in "]) {
            candidates.push(MemoryCandidate { content: format!("User lives in/is from {}", loc), confidence: 0.90 });
        }

        // "I work at X" / "I work for X"
        if let Some(org) = Self::capture_after(&lower, text, &["i work at ", "i work for ", "i'm employed at ", "i work as "]) {
            candidates.push(MemoryCandidate { content: format!("User works at/as {}", org), confidence: 0.90 });
        }

        // "I'm X years old" / "My age is X"
        if let Some(age) = Self::capture_before(&lower, &[" years old", " year old"]) {
            if age.split_whitespace().last().map(|w| w.parse::<u8>().is_ok()).unwrap_or(false) {
                let num = age.split_whitespace().last().unwrap();
                candidates.push(MemoryCandidate { content: format!("User is {} years old", num), confidence: 0.95 });
            }
        }

        // "I prefer X" / "I like X" / "I love X" / "I hate X" / "I dislike X"
        for prefix in ["i prefer ", "i really like ", "i love ", "my favorite is ", "i hate ", "i dislike "] {
            if lower.contains(prefix) {
                let sentence = Self::containing_sentence(text, prefix);
                if let Some(s) = sentence {
                    if s.len() > 5 && s.len() < 200 {
                        candidates.push(MemoryCandidate { content: s, confidence: 0.75 });
                    }
                }
            }
        }

        // "I have a X" / "I own a X"  (pets, cars, etc.)
        if let Some(item) = Self::capture_after(&lower, text, &["i have a ", "i own a ", "i have an ", "i own an "]) {
            candidates.push(MemoryCandidate { content: format!("User has a/an {}", item), confidence: 0.70 });
        }

        debug!("Heuristic extractor found {} candidates in: {:.50}", candidates.len(), text);
        candidates
    }

    fn capture_after(lower: &str, original: &str, prefixes: &[&str]) -> Option<String> {
        for prefix in prefixes {
            if let Some(pos) = lower.find(prefix) {
                let start = pos + prefix.len();
                let rest = &original[start..];
                let end = rest.find(['.', '!', '?', '\n']).unwrap_or(rest.len());
                let candidate = rest[..end].trim().to_string();
                if !candidate.is_empty() && candidate.len() < 150 {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn capture_before(lower: &str, suffixes: &[&str]) -> Option<String> {
        for suffix in suffixes {
            if let Some(pos) = lower.find(suffix) {
                let before = &lower[..pos];
                let start = before.rfind([' ', '\n']).map(|p| p + 1).unwrap_or(0);
                let candidate = before[start..].trim().to_string();
                if !candidate.is_empty() {
                    return Some(before.to_string());
                }
            }
        }
        None
    }

    fn containing_sentence(text: &str, substr: &str) -> Option<String> {
        let lower = text.to_lowercase();
        let pos = lower.find(substr)?;
        let sentence_start = text[..pos]
            .rfind(['.', '!', '?', '\n'])
            .map(|p| p + 1)
            .unwrap_or(0);
        let sentence_end = text[pos..]
            .find(['.', '!', '?', '\n'])
            .map(|p| pos + p + 1)
            .unwrap_or(text.len());
        Some(text[sentence_start..sentence_end].trim().to_string())
    }
}

/// LLM-based extractor. Builds a prompt and parses JSON response.
/// Designed to run in a background task so it doesn't block the response.
pub struct LlmExtractor;

impl LlmExtractor {
    pub fn new() -> Self { Self }

    /// Build the extraction prompt from recent conversation turns.
    pub fn build_prompt(turns: &[(String, String)]) -> String {
        let history: String = turns
            .iter()
            .map(|(role, content)| format!("{}: {}", role, content))
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            r#"You are a memory extraction assistant. Read the following conversation excerpt and identify any personal facts, preferences, skills, relationships, or ongoing projects that the USER revealed about themselves. Do NOT extract things the assistant said.

Conversation:
{history}

Return a JSON array of strings. Each string is a single, concise memory to store. If nothing new was learned, return an empty array [].

Rules:
- Only facts about the user, not general knowledge
- Each memory is a single complete sentence
- Do not duplicate obvious things already extractable from the conversation
- Maximum 5 memories per extraction

Return ONLY the JSON array, no explanation:
"#
        )
    }

    /// Parse the LLM's JSON response into individual memory strings.
    pub fn parse_response(response: &str) -> Vec<String> {
        let start = response.find('[').unwrap_or(0);
        let end = response.rfind(']').map(|p| p + 1).unwrap_or(response.len());
        let json_slice = &response[start..end];
        serde_json::from_str::<Vec<String>>(json_slice)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.trim().is_empty() && s.len() < 300)
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Structured LLM extractor (post-turn, conflict-aware)
// ─────────────────────────────────────────────────────────────────────────────

/// Upper bound on the structured extractor call. Mirrors the onboarding
/// extractor timeout — reasoning-distilled local models burn 600–800
/// reasoning tokens before emitting the JSON, and the user isn't waiting on
/// this (the chat response has already been streamed).
const STRUCTURED_EXTRACTOR_TIMEOUT: Duration = Duration::from_secs(180);

/// One memory candidate produced by [`LlmMemoryExtractor`].
///
/// `entity` is the *conflict key*: a short noun describing what the memory
/// is about (`"job"`, `"location"`, `"pet_name"`). When a new candidate
/// arrives with an `entity` that already has a memory, storage calls
/// [`crate::memory::MemorySystem::supersede`] instead of inserting a
/// duplicate — this is what stops the memory table from accumulating
/// `"User works at Google"` / `"User works at Anthropic"` / `"User works at
/// OpenAI"` as three coexisting facts.
#[derive(Debug, Clone)]
pub struct LlmCandidate {
    pub content:    String,
    pub category:   String,
    pub confidence: String,
    /// The attribute this fact is about (snake_case): `favourite_colour`,
    /// `employer`, `location`, `language`, … Combined with `subject` it forms
    /// the conflict key used to decide a correction vs a distinct fact.
    pub entity:     String,
    /// Who the fact is about: `user`, or a specific named person (`leila`).
    /// A generic role (`sister`) is treated as unkeyed (never supersedes).
    pub subject:    String,
    /// `single` (one value — favourite colour, city, employer) or `multi`
    /// (many coexist — languages, pets, siblings). Only a `single` attribute
    /// with a changed value supersedes; a `multi` one always adds alongside.
    pub cardinality: String,
    /// Coarse category slug for *topic-grouped retrieval* (`"plants"`,
    /// `"bike"`, `"road_trips"`), shared across sibling facts so aggregation
    /// queries can pull the complete set. May be empty.
    pub topic:      String,
}

/// Confidence tier ordering used for the `min_confidence` gate. Higher =
/// more confident; `Low` is weak inference and usually filtered out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfidenceTier { Low, Medium, High }

impl ConfidenceTier {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "high"   => Self::High,
            "medium" => Self::Medium,
            _        => Self::Low, // unknown → treat as weakest
        }
    }
}

/// Post-turn structured extractor. Takes the provider, a user message, and
/// the assistant's reply, returns the memory candidates the model believes
/// are worth persisting.
///
/// Failure-tolerant by design: any parse/IO error returns an empty vec so a
/// bad extractor call never blocks a response that already streamed back
/// to the user.
pub struct LlmMemoryExtractor;

impl LlmMemoryExtractor {
    pub fn new() -> Self { Self }

    /// Run one extraction pass. Returns the filtered candidate list —
    /// callers don't need to re-apply `allowed_categories` / `min_confidence`.
    pub async fn extract(
        &self,
        provider:            &Arc<dyn ModelProvider>,
        user_msg:            &str,
        assistant_msg:       &str,
        allowed_categories:  &[String],
        min_confidence:      ConfidenceTier,
        // The user's existing `subject.attribute` keys, so the model REUSES one
        // for the same thing instead of inventing a drifting label (R4). Empty is
        // fine (a fresh user, or the heuristic/test paths).
        existing_keys:       &[String],
    ) -> Vec<LlmCandidate> {
        // A.3 — sanitise extractor INPUTS: strip reasoning blocks from both turns
        // before the prompt is built, so a `<thinking>` block (including an
        // unclosed one from a truncated response) never reaches the extractor.
        let user_clean = crate::memory::authorship::strip_reasoning_blocks(user_msg);
        let assistant_clean = crate::memory::authorship::strip_reasoning_blocks(assistant_msg);
        let messages = vec![
            ChatMessage::system(build_extractor_system_prompt(existing_keys)),
            ChatMessage::user(build_extractor_user_prompt(&user_clean, &assistant_clean)),
        ];

        let opts = GenerationOptions {
            temperature: 0.0,
            // Right-sized to the actual payload (the capped 5 memories emit a small
            // JSON object). 4096 was far too generous — only a runaway/degenerate
            // model ever reached it, burning the budget; `parse_raw_extraction`
            // salvages a response truncated mid-array as a backstop.
            max_tokens:  Some(768),
            ..Default::default()
        };

        let response = match tokio::time::timeout(
            STRUCTURED_EXTRACTOR_TIMEOUT,
            provider.generate(&messages, &opts),
        ).await {
            Ok(Ok(r))  => r.content,
            Ok(Err(e)) => {
                warn!("memory extractor: provider call failed, skipping: {}", e);
                return vec![];
            }
            Err(_) => {
                warn!("memory extractor: timed out after {:?}, skipping",
                    STRUCTURED_EXTRACTOR_TIMEOUT);
                return vec![];
            }
        };

        let raw = match parse_raw_extraction(&response) {
            Some(r) => r,
            None    => {
                let looks_json = response.trim_start().starts_with('{') || response.contains("memories");
                if looks_json {
                    debug!("memory extractor: response truncated with no complete memory (raise max_tokens or model too verbose). Head: {:?}",
                        truncate_for_log(&response, 200));
                } else {
                    warn!("memory extractor: no JSON in response. Raw: {:?}",
                        truncate_for_log(&response, 200));
                }
                return vec![];
            }
        };

        // Gate on allowed_categories and min_confidence. Also drop empty
        // content / empty entity — an unnamed candidate can't be
        // conflict-detected so storage would just duplicate-insert.
        let allowed: std::collections::HashSet<&str> =
            allowed_categories.iter().map(String::as_str).collect();

        raw.memories.into_iter().filter_map(|m| {
            let content = m.content.trim().to_owned();
            let entity  = m.entity.trim().to_ascii_lowercase();
            let subject = { let s = m.subject.trim().to_ascii_lowercase(); if s.is_empty() { "user".to_owned() } else { s } };
            let cardinality = m.cardinality.trim().to_ascii_lowercase();
            let cat     = m.category.trim().to_ascii_lowercase();
            let conf    = ConfidenceTier::parse(&m.confidence);
            // Normalise the topic to a stable snake_case slug so retrieval
            // matches what we store (`"Road Trips"` → `"road_trips"`). Empty
            // is fine — that memory just won't participate in topic expansion.
            let topic = slugify_topic(&m.topic);

            if content.is_empty() || entity.is_empty() {
                debug!("memory extractor: dropping candidate with empty content/entity");
                return None;
            }
            if !allowed.contains(cat.as_str()) {
                debug!("memory extractor: dropping category='{}' (not allowed)", cat);
                return None;
            }
            if conf < min_confidence {
                debug!("memory extractor: dropping '{}' (confidence {:?} < min {:?})",
                    entity, conf, min_confidence);
                return None;
            }
            // A.2 — code-enforced authorship/provenance. The prompt asks the
            // model to store only user-stated facts; this enforces it. Grade the
            // candidate against the user's own turn (reasoning-stripped inside)
            // and drop anything that is reasoning markup, assistant-/world-
            // sourced (no distinctive user support), or carries a multi-digit
            // number the user never said (MIRA's own state leaking as a fact).
            let verdict = crate::memory::authorship::grounding_verdict(&content, user_msg);
            if !verdict.is_grounded() {
                debug!("memory extractor: dropping '{}' (grounding={})", entity, verdict.name());
                return None;
            }

            Some(LlmCandidate { content, category: cat, confidence: m.confidence, entity, subject, cardinality, topic })
        }).collect()
    }
}

impl Default for LlmMemoryExtractor {
    fn default() -> Self { Self::new() }
}

/// Whether an attribute holds one value (a new value corrects the old) or many
/// (each coexists). The write path only supersedes a `Single` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality { Single, Multi }

/// Attributes that are inherently single-valued — a new value corrects the old.
/// Overrides a wrong model hint (so these supersede reliably).
const SINGLE_ATTRS: &[&str] = &[
    "favourite_colour", "favorite_color", "favourite_color", "favorite_colour", "favorite_colour",
    "employer", "company", "job", "job_title", "occupation", "role", "current_role", "title",
    "location", "city", "lives_in", "residence", "hometown", "home_city", "address", "home_address",
    "age", "birthday", "dob", "date_of_birth", "timezone", "tz", "marital_status", "relationship_status",
    "phone", "phone_number", "email", "email_address", "name", "full_name", "nickname",
    "height", "weight", "nationality", "gender", "pronouns",
];
/// Attributes where many values coexist — NEVER supersede, always add alongside.
/// Overrides a wrong model hint (so distinct facts are never overwritten — the
/// data-loss class the token heuristic used to hit).
const MULTI_ATTRS: &[&str] = &[
    "language", "languages", "pet", "pets", "pet_name", "sibling", "siblings",
    "child", "children", "kid", "kids", "son", "daughter", "car", "cars", "vehicle", "vehicles",
    "hobby", "hobbies", "skill", "skills", "allergy", "allergies", "like", "likes",
    "dislike", "dislikes", "interest", "interests", "project", "projects",
    "friend", "friends", "colleague", "colleagues", "meeting", "meetings",
    "appointment", "appointments", "event", "events", "expense", "expenses", "plant", "plants",
];
/// Role words that are NOT a stable subject identity — keying on them would let
/// two different people collide (two "sister"s). Such a subject is treated as
/// unkeyed → never supersedes.
const GENERIC_SUBJECTS: &[&str] = &[
    "sister", "brother", "mother", "father", "mom", "dad", "mum", "parent", "parents",
    "friend", "colleague", "boss", "manager", "partner", "spouse", "wife", "husband",
    "child", "son", "daughter", "kid", "pet", "cat", "dog", "neighbour", "neighbor",
    "cousin", "uncle", "aunt", "grandmother", "grandfather", "someone", "they", "person",
];

/// Fold a Latin diacritic to its base letter (`é`→`e`, `ë`→`e`, `ñ`→`n`, …) so
/// "Zoë" keys as "zoe". Non-Latin letters (Arabic, CJK, …) pass through so a
/// name like "ليلى" still produces a key (W2 — Unicode-aware, crate-free).
fn fold_diacritic(c: char) -> char {
    match c {
        'à'|'á'|'â'|'ã'|'ä'|'å'|'ā'|'ă'|'ą' => 'a',
        'ç'|'ć'|'č' => 'c',
        'è'|'é'|'ê'|'ë'|'ē'|'ė'|'ę' => 'e',
        'ì'|'í'|'î'|'ï'|'ī'|'į' => 'i',
        'ñ'|'ń' => 'n',
        'ò'|'ó'|'ô'|'õ'|'ö'|'ø'|'ō' => 'o',
        'ù'|'ú'|'û'|'ü'|'ū' => 'u',
        'ý'|'ÿ' => 'y',
        'ß' => 's',
        other => other,
    }
}

/// Normalise a subject/attribute token to a key segment: lowercase, Latin
/// diacritics folded, Unicode LETTERS/digits kept (so non-English names key),
/// everything else collapsed to a single `_`. `%`/`_`/punctuation can't leak a
/// LIKE wildcard because matching is exact (`json_each`), and this strips them
/// anyway.
fn normalize_key_token(s: &str) -> String {
    let mut out = String::new();
    let mut prev_us = false;
    for raw in s.trim().chars() {
        let c = fold_diacritic(raw);
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() { out.push(lc); }
            prev_us = false;
        } else if !out.is_empty() && !prev_us {
            out.push('_'); prev_us = true;
        }
    }
    while out.ends_with('_') { out.pop(); }
    out
}

/// Canonicalise a model-supplied attribute so a correction phrased differently
/// still lands on the SAME key (W2 attribute drift). First strips temporal
/// qualifier prefixes (`current_`, `new_`, `now_`, `former_`, `old_`,
/// `previous_`) that the model adds on a correction ("current_vehicle" vs
/// "vehicle"), then maps synonyms to one canonical form.
fn canonical_attr(attr: &str) -> String {
    // Strip ONLY forward-looking qualifiers — a "current/new" value IS the present
    // value and should land on the base key. A BACKWARD qualifier
    // (former/previous/old/ex) names a PAST value: route it to a distinct
    // `<attr>_past` key (resolved multi below) so it NEVER supersedes the current
    // one (R1 — "my previous employer was Google" must not replace Atlassian).
    let mut a = attr;
    for q in ["current_", "new_", "now_", "latest_", "present_"] {
        if let Some(stripped) = a.strip_prefix(q) { a = stripped; break; }
    }
    for q in ["former_", "old_", "previous_", "prev_", "ex_", "past_"] {
        if let Some(stripped) = a.strip_prefix(q) {
            return format!("{}_past", canonical_attr(stripped));
        }
    }
    // Polarity folds onto one key so a negation supersedes the positive:
    // `dislikes_coffee` / `not_coffee` → `likes_coffee` (the VALUE carries the
    // polarity; the key is the topic). So "I don't like coffee anymore"
    // supersedes "I like coffee".
    for q in ["dislikes_", "dislike_", "not_", "hates_", "hate_"] {
        if let Some(stripped) = a.strip_prefix(q) {
            return format!("likes_{}", canonical_attr(stripped));
        }
    }
    match a {
        "city" | "lives_in" | "residence" | "hometown" | "home_city" | "lives" | "located" | "based" | "location_city"
            | "address" | "home_address" | "home" => "location".into(),
        "favorite_color" | "favourite_color" | "favorite_colour" | "fav_colour" | "fav_color" | "colour" | "color" => "favourite_colour".into(),
        "company" | "workplace" | "work" | "work_place" | "employer_name" => "employer".into(),
        "job_title" | "occupation" | "role" | "title" | "profession" => "job".into(),
        "vehicle" | "cars" | "car" | "vehicles" | "car_i_drive" | "daily_driver" | "driving" => "vehicle".into(),
        other => other.to_string(),
    }
}

/// Build the conflict key `subject.attribute` + its resolved cardinality from a
/// candidate's subject/attribute and the model's cardinality hint. Returns None
/// (→ the write NEVER supersedes, just adds) when the key is unreliable: empty,
/// a generic-role subject, or an injection-y token. Cardinality resolution:
/// a known-multi attribute is ALWAYS multi (prevents data loss), a known-single
/// is always single, otherwise the model's hint, defaulting to multi (keep-both)
/// when unsure.
pub fn conflict_key_of(subject: &str, attribute: &str, model_cardinality: &str) -> Option<(String, Cardinality)> {
    let subj = normalize_key_token(subject);
    let attr = canonical_attr(&normalize_key_token(attribute));
    if subj.is_empty() || attr.is_empty() { return None; }
    if GENERIC_SUBJECTS.contains(&subj.as_str()) { return None; }
    // A "favourite <thing>", or a per-thing yes/no (`allergy_penicillin`,
    // `likes_coffee`), is SINGLE-valued even though the bare attribute
    // (`allergy`, `likes`) is multi — so a correction/negation on the SAME
    // specific thing supersedes (W2 negation + favourites).
    let per_thing_single =
        attr.starts_with("favourite_") || attr.starts_with("favorite_")
        || ((attr.starts_with("allergy_") || attr.starts_with("allergic_")
             || attr.starts_with("likes_") || attr.starts_with("like_")
             || attr.starts_with("dislikes_") || attr.starts_with("dislike_"))
            && attr.len() > "allergy_".len());
    let card = if attr.ends_with("_past") {
        // A past value is one of possibly many; it never replaces the current one.
        Cardinality::Multi
    } else if per_thing_single {
        Cardinality::Single
    } else if MULTI_ATTRS.contains(&attr.as_str()) {
        Cardinality::Multi
    } else if SINGLE_ATTRS.contains(&attr.as_str()) {
        Cardinality::Single
    } else if model_cardinality.trim().eq_ignore_ascii_case("single") {
        Cardinality::Single
    } else {
        Cardinality::Multi // unsure → keep both
    };
    Some((format!("{subj}.{attr}"), card))
}

/// The tag MIRA stores a fact's conflict key under (exact-matched, never LIKE).
pub fn conflict_key_tag(key: &str) -> String { format!("key:{key}") }

/// Does a conflict key's attribute PIN a specific thing (so the thing is in the
/// key, not the value)? `likes_coffee` / `allergy_penicillin` / `favourite_tea`
/// → yes; `employer` / `location` → no (the value is the object in the content).
pub fn is_per_thing_key(key: &str) -> bool {
    let attr = key.split_once('.').map(|(_, a)| a).unwrap_or(key);
    ["likes_", "like_", "dislikes_", "dislike_", "allergy_", "allergic_", "favourite_", "favorite_"]
        .iter().any(|p| attr.starts_with(p) && attr.len() > p.len())
}

/// Is a conflict key SINGLE-valued (holds exactly one current value, so a new
/// value supersedes) rather than MULTI (accumulates — languages, skills,
/// hobbies, past values)? Mirrors `conflict_key_of`'s cardinality decision from
/// the key string alone, for the tool paths that hold a stored key but not the
/// originating subject/attribute/cardinality. Unknown bare attribute → false
/// (treat as multi: keep both), the data-loss-averse default. A MULTI key must
/// never be overwritten through `memory_supersede` — that is the
/// "speaks French" → "speaks Arabic" data loss.
pub fn key_is_single_valued(key: &str) -> bool {
    let attr = key.split_once('.').map(|(_, a)| a).unwrap_or(key);
    if attr.ends_with("_past") { return false; }
    if is_per_thing_key(key) { return true; }
    if MULTI_ATTRS.contains(&attr) { return false; }
    SINGLE_ATTRS.contains(&attr)
}

/// Is `content` a NEGATIVE statement (no longer / not / never / stopped …)?
fn is_negated_statement(content: &str) -> bool {
    let l = format!(" {} ", content.to_ascii_lowercase().replace('\'', ""));
    const NEG: &[&str] = &[
        " no longer ", " not ", " never ", " dont ", " doesnt ", " didnt ",
        " isnt ", " arent ", " wasnt ", " nt ", " anymore ", " stopped ",
        " quit ", " left ", " no more ",
    ];
    NEG.iter().any(|m| l.contains(m)) || l.contains("n't")
}

/// A "transition" phrasing — `from X to Y`, `X → Y`, `switched/changed/moved/
/// swapped/upgraded/migrated … from …`, `after leaving X`, `instead of X`,
/// `rather than X` — asserts a NEW value and discards the old. Even though the
/// OLD value's token (X) still appears in the sentence, this is a correction,
/// never a duplicate — so it must read as Contradicts, not Same.
fn is_transition_statement(content: &str) -> bool {
    let l = format!(" {} ", content.to_ascii_lowercase().replace('\'', ""));
    if l.contains(" from ") && l.contains(" to ") { return true; }
    if l.contains(" instead of ") || l.contains(" after leaving ") || l.contains(" rather than ") {
        return true;
    }
    if content.contains('→') || content.contains("->") { return true; }
    (l.contains(" switched ") || l.contains(" changed ") || l.contains(" moved ")
        || l.contains(" swapped ") || l.contains(" upgraded ") || l.contains(" migrated "))
        && l.contains(" from ")
}

/// The VALUE tokens of a fact — distinctive nouns, excluding structural verbs,
/// negation words, stopwords and `user`. Lightly stemmed (drop trailing s/es/ed/
/// ing) so "lives"/"live"/"lived" match. This is what a conflict's value compares
/// on.
fn value_tokens(content: &str) -> std::collections::HashSet<String> {
    const SKIP: &[&str] = &[
        "user", "users", "work", "works", "working", "worked", "live", "lives",
        "living", "lived", "like", "likes", "liked", "drive", "drives", "driving",
        "drove", "move", "moves", "moved", "moving", "have", "has", "had", "been",
        "being", "now", "longer", "anymore", "currently", "still", "also", "prefer",
        "prefers", "enjoy", "enjoys", "name", "named", "call", "called", "the",
        "and", "that", "with", "from", "into", "this", "there", "their", "they",
        "previously", "previous", "former", "formerly", "used", "about",
    ];
    let stem = |w: &str| {
        let w = w.to_string();
        for suf in ["ing", "ed", "es", "s"] {
            if w.len() > suf.len() + 2 && w.ends_with(suf) {
                return w[..w.len() - suf.len()].to_string();
            }
        }
        w
    };
    content.split(|c: char| !c.is_alphanumeric())
        .map(|t| t.to_lowercase())
        .filter(|t| t.chars().count() >= 4 && !SKIP.contains(&t.as_str()))
        .map(|t| stem(&t))
        .collect()
}

/// How a NEW fact relates to an EXISTING one that shares its conflict key — the
/// value check. Supersede ONLY on `Contradicts`; `Same` is a
/// duplicate; `Unrelated` keeps both (the new fact is about a DIFFERENT value, so
/// it must not replace the current one — e.g. "no longer works at Google" must
/// not replace "works at Atlassian").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueVerdict { Contradicts, Same, Unrelated }

pub fn value_verdict(old_content: &str, new_content: &str, per_thing: bool) -> ValueVerdict {
    let o = value_tokens(old_content);
    let n = value_tokens(new_content);
    if per_thing {
        // The key pins the thing; a POLARITY flip ("allergic" → "not allergic",
        // "likes" → "no longer likes") is a contradiction even though the value
        // tokens are the same (the negation word is too short to be a token).
        if is_negated_statement(old_content) != is_negated_statement(new_content) {
            return ValueVerdict::Contradicts;
        }
        return if o == n { ValueVerdict::Same } else { ValueVerdict::Contradicts };
    }
    if is_negated_statement(new_content) {
        // A negated B contradicts A only if it negates A's OWN value: A's value
        // tokens must all appear in B ("no longer live in Perth" negates "lives in
        // Perth"; "no longer works at Google" does NOT negate "works at Atlassian").
        if !o.is_empty() && o.is_subset(&n) { ValueVerdict::Contradicts } else { ValueVerdict::Unrelated }
    } else {
        // Positive B. A transition phrasing ("moved from Brisbane to Hobart",
        // "switched from teal to orange", "after leaving Atlassian") asserts a
        // NEW value and negates the old — a correction, not a duplicate, even
        // though the old value's token still appears in B.
        if is_transition_statement(new_content) { return ValueVerdict::Contradicts; }
        // Otherwise: IDENTICAL value tokens → duplicate; any difference → a real
        // change. Containment is NOT a duplicate — that was the bug: {brisbane}
        // ⊂ {brisbane, hobart} wrongly read "moved from Brisbane to Hobart" as a
        // restatement of "lives in Brisbane" and dropped Hobart.
        if o == n { ValueVerdict::Same } else { ValueVerdict::Contradicts }
    }
}

/// Is the user's message actually ABOUT THE USER — a first-person subject and no
/// third-party signal — so a `user.*` fact may supersede their own data? A
/// mis-attributed third party (especially a dropped non-Latin name, or "she
/// moved…") must NOT overwrite the user's facts. Biased safe: any doubt →
/// false → keep-both.
pub fn message_is_about_user(user_msg: &str) -> bool {
    let toks: std::collections::HashSet<String> = user_msg
        .split(|c: char| !c.is_alphanumeric()) // Unicode-aware
        .filter(|s| !s.is_empty()).map(|s| s.to_lowercase()).collect();
    const FIRST_PERSON: &[&str] = &["i", "im", "ive", "ill", "me", "my", "mine", "myself", "we", "our", "us"];
    if !FIRST_PERSON.iter().any(|w| toks.contains(*w)) { return false; }
    !toks.iter().any(|t| THIRD_PARTY_SUBJECT.contains(&t.as_str()))
}

/// Third-party subject signals (pronouns + relationship roles). A clause with any
/// of these is about someone else, not the user.
const THIRD_PARTY_SUBJECT: &[&str] = &[
    "she", "he", "they", "her", "hers", "his", "him", "their", "theirs", "them",
    "cousin", "sister", "brother", "mother", "father", "mom", "dad", "mum", "mama", "papa",
    "friend", "colleague", "coworker", "boss", "manager", "partner", "spouse",
    "wife", "husband", "son", "daughter", "child", "kid", "pet", "cat", "dog",
    "neighbour", "neighbor", "aunt", "uncle", "grandmother", "grandfather",
    "grandma", "grandpa", "nephew", "niece", "parents", "parent", "fiance", "fiancee",
];

/// Grounding for a `user.*` supersede: is the fact's VALUE asserted by the
/// user, in their own voice, about themselves? Three gates, each biased toward
/// keep-both on doubt:
///   1. the message is first-person at all (an "I/we" somewhere);
///   2. REPORTED SPEECH — a reporting verb ("I heard …", "Dad told me …") hands
///      the subject to someone else for the rest of that span, so a value that
///      appears only inside a reported span isn't the user's ("I heard Sara
///      moved to Doha");
///   3. THIRD-PARTY LOCAL SUBJECT — the value's own clause names no other person
///      ("…, my sister moved to Perth").
/// Crucially it does NOT require a first-person pronoun in the value's clause: a
/// plain conjunction keeps the same subject, so "I quit Atlassian and joined
/// Canva" grounds "joined Canva" to the user. A value the model
/// paraphrased out of the message falls back to the whole-message check (still
/// blocked by a reporting verb).
pub fn user_fact_grounded(user_msg: &str, content: &str) -> bool {
    use std::collections::HashSet;
    let value = value_tokens(content);
    if value.is_empty() { return message_is_about_user(user_msg); }
    let lower = user_msg.to_lowercase();
    let words: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric()) // Unicode-aware
        .filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();

    const FP: &[&str] = &["i", "im", "ive", "ill", "me", "my", "mine", "myself", "we", "our", "us", "weve"];
    // Gate 1: a message with no first-person marker ("سارة moved to Doha", "she
    // moved") is never about the user.
    if !words.iter().any(|w| FP.contains(&w.as_str())) { return false; }

    // Reporting verbs hand the subject to someone else; a soft reset ("but",
    // "however") returns the voice to the speaker.
    const REPORTING: &[&str] = &[
        "heard", "told", "said", "mentioned", "thinks", "think", "believes",
        "believe", "saw", "sees", "says", "guess", "guesses", "read", "learned",
        "learnt", "gather", "reckon",
    ];
    const SOFT_RESET: &[&str] = &["but", "however", "though", "although"];
    let value_hit = |t: &str| value.iter().any(|v| t == v.as_str() || t.starts_with(v.as_str()));

    // Gate 2: mark each word reported-or-not, then require at least one
    // occurrence of the value in the user's own (non-reported) voice.
    let mut non_reported_value = false;
    let mut value_present = false;
    let mut in_reported = false;
    for w in &words {
        if REPORTING.contains(&w.as_str()) { in_reported = true; continue; }
        if SOFT_RESET.contains(&w.as_str()) { in_reported = false; }
        if value_hit(w) {
            value_present = true;
            if !in_reported { non_reported_value = true; }
        }
    }
    if !value_present {
        // The model paraphrased the value out of the message: fall back to a
        // whole-message check, but a reporting verb anywhere still blocks it.
        if words.iter().any(|w| REPORTING.contains(&w.as_str())) { return false; }
        return message_is_about_user(user_msg);
    }
    if !non_reported_value { return false; } // value only appears in reported speech

    // Gate 3: walk clauses in order, tracking the governing SUBJECT. A clause
    // naming a third party is third-person; one with a first-person token (and
    // no third party) is first-person; one with NEITHER inherits the subject of
    // the clause before it — a plain conjunction keeps the same speaker. So
    // "I quit Atlassian and joined Canva" → "joined Canva" is first-person, while
    // "my cousin moved to Doha and joined Qatar Airways" → "joined Qatar Airways"
    // is third-person. The value's clause must be first-person.
    // (A bare name as a clause's own subject — "Leila moved…" — is not yet a
    // third-party signal; that refinement is tracked separately.)
    #[derive(PartialEq, Clone, Copy)]
    enum Subj { First, Third, Unknown }
    const CLAUSE_SEP: &[&str] = &[
        ",", ";", ":", ".", "?", "!", " and ", " but ", " so ", " then ", " while ", " whereas ",
    ];
    let mut clauses = vec![lower.as_str()];
    for sep in CLAUSE_SEP { clauses = clauses.iter().flat_map(|c| c.split(sep)).collect(); }
    let mut current = Subj::Unknown;
    for c in &clauses {
        let ct: HashSet<String> = c.split(|ch: char| !ch.is_alphanumeric())
            .filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
        let third = ct.iter().any(|t| THIRD_PARTY_SUBJECT.contains(&t.as_str()));
        let first = ct.iter().any(|t| FP.contains(&t.as_str()));
        // A third party in the clause wins even when a first-person possessive
        // is present ("my cousin" is about the cousin, not the user).
        let subj = if third { Subj::Third } else if first { Subj::First } else { current };
        current = subj;
        if ct.iter().any(|t| value_hit(t)) && subj != Subj::First { return false; }
    }
    true
}

/// Is a conflict key's SUBJECT grounded in the user's message, so a supersede on
/// that key is safe? `user` → the message is about the user; a named subject
/// → that name appears as a token in the message. Not grounded ⇒ the caller drops
/// the key and keeps both rather than risk overwriting the wrong person's fact.
pub fn subject_grounded(subject: &str, user_msg: &str) -> bool {
    if subject == "user" { return message_is_about_user(user_msg); }
    let want = normalize_key_token(subject);
    if want.is_empty() { return false; }
    user_msg.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .any(|t| normalize_key_token(t) == want)
}

/// Does the fact describe a PAST/former value rather than the current one? The
/// model often keeps the base attribute ("employer") and only marks the past in
/// the CONTENT ("previously worked at…"), so content detection — not just the
/// attribute prefix — is what stops a past value overwriting the present one
/// (R1). Biased toward catching it: a false positive only means keep-both (safe),
/// never a delete.
pub fn is_backward_looking(content: &str) -> bool {
    let l = format!(" {} ", content.to_ascii_lowercase());
    // A PRESENT/now signal means the fact states the CURRENT value (a correction),
    // NOT an archived past one — so "no longer likes coffee" / "used to live in
    // Perth but now in Sydney" must supersede, not route to `_past`. These
    // win over any past marker.
    const PRESENT: &[&str] = &[
        " now ", " currently ", " nowadays ", " these days ", " anymore ",
        " no longer ", " not anymore ", " right now ", " today ", " at the moment ",
    ];
    if PRESENT.iter().any(|m| l.contains(m)) { return false; }
    // Genuinely-archival markers (a value that USED to hold).
    const MARKERS: &[&str] = &[
        " previous ", " previously ", " former ", " formerly ", " ex ",
        " used to ", " in the past ", " once worked ", " once lived ",
        " past employer ", " past job ", " old job ", " old address ",
    ];
    MARKERS.iter().any(|m| l.contains(m))
}

fn build_extractor_system_prompt(existing_keys: &[String]) -> String {
    // Show the model the keys it has already used for this user, so a correction
    // reuses the SAME subject.attribute instead of inventing a drifting one (R4).
    let existing = if existing_keys.is_empty() {
        String::new()
    } else {
        let mut keys: Vec<&str> = existing_keys.iter().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        keys.truncate(60); // keep the prompt bounded
        format!(
            "\n\nExisting subject.attribute keys for THIS user (reuse the SAME \
             subject and attribute when a new fact is about the same thing, so an \
             update lands on it):\n{}\n",
            keys.join(", "),
        )
    };
    // Flat, strict-JSON prompt. The onboarding extractor taught us that
    // reasoning-distilled local models mirror visual hierarchy into their
    // output, so keep the shape flat and name the exact fields.
    let body = r#"Extract facts about the USER from one conversation turn.

Output ONLY a JSON object, no prose, no reasoning, no code fences:
{"memories":[{"content":"<short factual sentence>","category":"<fact|preference|skill|relationship|project>","confidence":"<low|medium|high>","subject":"<who>","attribute":"<what>","cardinality":"<single|multi>","topic":"<coarse_group>"}]}

Rules:
- Extract ONLY facts the USER stated or clearly implied about themselves.
  Do NOT extract things the assistant said, general world knowledge, or
  content from quoted third parties.
- One memory per fact. Keep `content` to a single sentence starting with
  "User ..." (e.g. "User works at Anthropic as a software engineer").
- `category`: pick ONE of fact, preference, skill, relationship, project.
- `confidence`: "high" = user directly stated it, "medium" = clear
  implication from context, "low" = weak inference (use sparingly — these
  are usually filtered out).
- `subject`: WHO the fact is about — "user" for the user themselves, or the
  specific person's lowercase first name for someone else (e.g. "leila").
  Use the NAME, never a role word like "sister"/"friend".
- `attribute`: WHAT the fact is about, a short stable snake_case word reused
  for the same kind of fact (e.g. "favourite_colour", "employer", "location",
  "language"). Keep it the SAME across turns and across a correction so an
  update lands on the same attribute — e.g. "Leila lives in Perth" and "Leila
  moved to Sydney" are BOTH subject "leila", attribute "location". Do NOT add
  qualifiers like "current_"/"new_" (write "vehicle", not "current_vehicle").
  For an allergy, like or dislike about a SPECIFIC thing, name the thing in the
  attribute: "allergy_penicillin", "likes_coffee", "dislikes_cilantro" — so
  "not allergic to penicillin" corrects "allergic to penicillin".
- `cardinality`: "single" if the subject has exactly ONE such value (favourite
  colour, city, employer, age, and each specific allergy/like like
  "allergy_penicillin") so a new value REPLACES the old; "multi" if many coexist
  (languages, pets, siblings, children, cars owned, hobbies) so each is kept
  ALONGSIDE the others. When unsure, use "multi".
- `topic`: a COARSE grouping shared by all facts of the same kind, used to
  retrieve the whole set together for counting/totalling. Many facts share
  one topic (e.g. every bike expense → "bike_expenses"; every plant you own
  → "plants"; every road trip → "road_trips"). Re-use the SAME topic slug
  across turns for the same kind of thing. snake_case; "" if none applies.
  Contrast with entity: entity is unique per fact, topic is shared.
- Return `{"memories":[]}` if nothing new was stated."#;
    format!("{body}{existing}")
}

fn build_extractor_user_prompt(user_msg: &str, assistant_msg: &str) -> String {
    format!(
        "USER TURN:\n{}\n\nASSISTANT TURN:\n{}\n\nEmit the JSON object now.",
        user_msg.trim(),
        assistant_msg.trim(),
    )
}

#[derive(Debug, Default, Deserialize)]
struct RawExtraction {
    #[serde(default)]
    memories: Vec<RawMemory>,
}

#[derive(Debug, Deserialize)]
struct RawMemory {
    #[serde(default)]
    content:    String,
    #[serde(default)]
    category:   String,
    #[serde(default)]
    confidence: String,
    #[serde(default, alias = "attribute")]
    entity:     String,
    #[serde(default)]
    subject:    String,
    #[serde(default)]
    cardinality: String,
    #[serde(default)]
    topic:      String,
}

/// Normalise a free-text topic into a stable `snake_case` slug: lowercase,
/// non-alphanumerics collapsed to single underscores, trimmed. Returns empty
/// for empty/garbage input so the caller can skip tagging.
fn slugify_topic(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_us = true; // suppress leading underscore
    for ch in raw.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    while out.ends_with('_') { out.pop(); }
    out
}

fn parse_raw_extraction(raw: &str) -> Option<RawExtraction> {
    // Fast path: a complete, balanced object.
    if let Some(payload) = extract_json_object(raw) {
        if let Ok(r) = serde_json::from_str::<RawExtraction>(&payload) {
            return Some(r);
        }
    }
    // Salvage: the response was truncated mid-array (a verbose model blew past
    // max_tokens). Recover the `memories` that closed cleanly by re-closing the
    // array + root after the last complete element.
    let start = raw.find('{')?;
    salvage_truncated_memories(&raw[start..])
}

/// Reconstruct a valid `{"memories":[...]}` from a response truncated mid-array
/// by keeping only the elements that closed cleanly. `body` must start at `{`.
fn salvage_truncated_memories(body: &str) -> Option<RawExtraction> {
    let bytes = body.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    let mut last_elem_end: Option<usize> = None; // byte index past a completed array element
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc { esc = false; } else if b == b'\\' { esc = true; } else if b == b'"' { in_str = false; }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' => { depth -= 1; if depth == 2 { last_elem_end = Some(i + 1); } }
            b']' => depth -= 1,
            _ => {}
        }
    }
    let end = last_elem_end?;
    let rebuilt = format!("{}]}}", &body[..end]);
    serde_json::from_str::<RawExtraction>(&rebuilt).ok()
}

/// Find the first balanced `{ ... }` in `s`, skipping over string contents.
/// Small/local models love to wrap JSON in ```json fences or add prose
/// around it — be liberal about recovery.
pub(crate) fn extract_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;

    let mut depth  = 0i32;
    let mut in_str = false;
    let mut escape = false;

    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escape { escape = false; continue; }
            match b {
                b'\\' => { escape = true; }
                b'"'  => { in_str = false; }
                _     => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..=i].to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_owned() } else {
        format!("{}…({} more chars)", &s[..max], s.len() - max)
    }
}

#[cfg(test)]
mod structured_extractor_tests {
    use super::*;

    #[test]
    fn no_store_request_detection() {
        // Honoured phrasings.
        for s in [
            "Don't record this, but my PIN is 1234",
            "do not save this: I'm feeling low today",
            "off the record — I applied for another job",
            "please don't remember this",
            "dont store this password",
            "forget what I just told you",
        ] {
            assert!(is_no_store_request(s), "should honour: {s:?}");
        }
        // Normal messages are NOT flagged (no over-triggering).
        for s in [
            "What's the weather today?",
            "Remember to buy milk tomorrow",
            "Record a note that the meeting moved to 3pm",
            "Save this recipe to my collection",
        ] {
            assert!(!is_no_store_request(s), "should NOT flag: {s:?}");
        }
    }

    #[test]
    fn parse_well_formed_json() {
        let raw = r#"{"memories":[
            {"content":"User works at Anthropic","category":"fact","confidence":"high","entity":"job"},
            {"content":"User prefers brief responses","category":"preference","confidence":"medium","entity":"verbosity"}
        ]}"#;
        let parsed = parse_raw_extraction(raw).unwrap();
        assert_eq!(parsed.memories.len(), 2);
        assert_eq!(parsed.memories[0].entity, "job");
    }

    #[test]
    fn parse_captures_topic_when_present() {
        let raw = r#"{"memories":[
            {"content":"User spent $25 on a bike chain","category":"fact","confidence":"high","entity":"bike_chain_cost","topic":"bike_expenses"},
            {"content":"User spent $120 on a bike repair","category":"fact","confidence":"high","entity":"bike_repair_cost","topic":"bike_expenses"}
        ]}"#;
        let parsed = parse_raw_extraction(raw).unwrap();
        // Distinct entities (so they don't supersede) but the SAME topic (so
        // they retrieve together for "total bike expenses").
        assert_ne!(parsed.memories[0].entity, parsed.memories[1].entity);
        assert_eq!(parsed.memories[0].topic, "bike_expenses");
        assert_eq!(parsed.memories[1].topic, "bike_expenses");
    }

    #[test]
    fn topic_slug_is_normalised() {
        assert_eq!(slugify_topic("Road Trips"), "road_trips");
        assert_eq!(slugify_topic("  Bike-Expenses!! "), "bike_expenses");
        assert_eq!(slugify_topic("plants"), "plants");
        assert_eq!(slugify_topic(""), "");
        assert_eq!(slugify_topic("   "), "");
    }

    #[test]
    fn parse_salvages_truncated_array() {
        // One complete memory, then a second cut off mid-string (verbose model
        // > max_tokens). Recover the completed one rather than dropping all.
        let raw = r#"{"memories":[
            {"content":"User graduated in Business Administration","category":"fact","confidence":"high","entity":"degree"},
            {"content":"User commutes 45 minutes each way to a very long workplace named"#;
        let parsed = parse_raw_extraction(raw).expect("should salvage the complete memory");
        assert_eq!(parsed.memories.len(), 1);
        assert_eq!(parsed.memories[0].entity, "degree");
    }

    #[test]
    fn parse_tolerates_code_fence_wrapping() {
        let raw = "```json\n{\"memories\":[{\"content\":\"User lives in Cairo\",\"category\":\"fact\",\"confidence\":\"high\",\"entity\":\"location\"}]}\n```";
        let parsed = parse_raw_extraction(raw).unwrap();
        assert_eq!(parsed.memories.len(), 1);
    }

    #[test]
    fn parse_tolerates_prose_wrapping() {
        let raw = "Here's what I extracted:\n{\"memories\":[{\"content\":\"User\",\"category\":\"fact\",\"confidence\":\"high\",\"entity\":\"x\"}]}\nHope that helps!";
        let parsed = parse_raw_extraction(raw).unwrap();
        assert_eq!(parsed.memories.len(), 1);
    }

    #[test]
    fn parse_returns_none_without_json() {
        assert!(parse_raw_extraction("nothing to extract").is_none());
    }

    #[test]
    fn conflict_key_cardinality_rules() {
        use Cardinality::*;
        // Known-single attribute → Single regardless of a wrong model hint.
        assert_eq!(conflict_key_of("user", "favourite_colour", "multi"), Some(("user.favourite_colour".into(), Single)));
        // Known-multi attribute → Multi even if the model says single (no data loss).
        assert_eq!(conflict_key_of("user", "language", "single"), Some(("user.language".into(), Multi)));
        assert_eq!(conflict_key_of("user", "pet", "single"), Some(("user.pet".into(), Multi)));
        // Unknown attribute → model hint, default Multi when unsure.
        assert_eq!(conflict_key_of("user", "mood", "single").unwrap().1, Single);
        assert_eq!(conflict_key_of("user", "mood", "").unwrap().1, Multi);
        // Synonyms canonicalise so a correction lands on the same key.
        assert_eq!(conflict_key_of("leila", "city", "single").unwrap().0, "leila.location");
        assert_eq!(conflict_key_of("leila", "lives_in", "single").unwrap().0, "leila.location");
        // R1: forward qualifiers strip to the base key (current employer).
        assert_eq!(conflict_key_of("user", "current_employer", "single").unwrap().0, "user.employer");
        // R1: a BACKWARD qualifier routes to a distinct, MULTI `_past` key, so a
        // past value never supersedes the current one.
        let (pk, pc) = conflict_key_of("user", "previous_employer", "single").unwrap();
        assert_eq!(pk, "user.employer_past");
        assert_eq!(pc, Multi);
        assert_eq!(conflict_key_of("user", "former_employer", "single").unwrap().0, "user.employer_past");
        assert_eq!(conflict_key_of("user", "old_address", "single").unwrap().0, "user.location_past");
        // Generic-role subject → unkeyed (two sisters can't collide).
        assert_eq!(conflict_key_of("sister", "location", "single"), None);
        // Injection / empties → unkeyed (never supersedes).
        assert_eq!(conflict_key_of("user", "%", "single"), None);
        assert_eq!(conflict_key_of("user", "", "single"), None);
        assert_eq!(conflict_key_of("", "colour", "single"), None);
    }

    #[test]
    fn value_verdict_transition_and_equality() {
        use ValueVerdict::*;
        // "from X to Y" is a correction, NOT a duplicate — even though X (the
        // old value) still appears in the new sentence.
        assert_eq!(value_verdict("User lives in Brisbane", "User moved from Brisbane to Hobart", false), Contradicts);
        assert_eq!(value_verdict("User works at Atlassian", "User changed jobs from Atlassian to Canva", false), Contradicts);
        assert_eq!(value_verdict("User works at Atlassian", "User now works at Canva after leaving Atlassian", false), Contradicts);
        assert_eq!(value_verdict("User's favourite colour is teal", "User switched from teal to orange", true), Contradicts);
        // Only an IDENTICAL value is a duplicate now (containment is a change).
        assert_eq!(value_verdict("User lives in Sydney", "User lives in Sydney", false), Same);
        assert_eq!(value_verdict("User lives in Sydney", "User lives in Melbourne", false), Contradicts);
        // Negated-unrelated still keeps both (must not regress).
        assert_eq!(value_verdict("User works at Atlassian", "User no longer works at Google", false), Unrelated);
    }

    #[test]
    fn user_fact_grounding_rules() {
        // Compound first-person sentence: the value clause ("joined Canva") has
        // no pronoun of its own but inherits the user as subject.
        assert!(user_fact_grounded("I quit Atlassian and joined Canva", "User now works at Canva"));
        assert!(user_fact_grounded("I live in Sydney now", "User lives in Sydney"));
        assert!(user_fact_grounded("I moved from Brisbane to Hobart", "User lives in Hobart"));
        // Reported speech hands the subject away: not about the user.
        assert!(!user_fact_grounded("I heard Sara moved to Doha", "User lives in Doha"));
        assert!(!user_fact_grounded("My dad told me he moved to Doha", "User lives in Doha"));
        // No first-person marker at all → never about the user.
        assert!(!user_fact_grounded("سارة has moved to Doha", "User has moved to Doha"));
        assert!(!user_fact_grounded("she moved to Perth", "User lives in Perth"));
        // Third-party subject in the value's own clause → not the user.
        assert!(!user_fact_grounded("I live in Sydney, my sister moved to Perth", "User lives in Perth"));
        // …but the user's own clause in that same sentence still grounds.
        assert!(user_fact_grounded("I live in Sydney, my sister moved to Perth", "User lives in Sydney"));
        // A conjunction clause inherits the previous clause's subject. The
        // part after "and" is about the cousin, not the user — even though "my"
        // appears earlier in the message.
        assert!(!user_fact_grounded("my cousin moved to Doha and joined Qatar Airways", "User works at Qatar Airways"));
        assert!(!user_fact_grounded("Sara joined Canva and I'm happy for her", "User works at Canva"));
        // …while a genuinely first-person compound still grounds both halves.
        assert!(user_fact_grounded("I quit Atlassian and joined Canva", "User works at Canva"));
    }

    #[test]
    fn key_single_vs_multi_valued() {
        // Single-valued: one current value, a new one supersedes.
        assert!(key_is_single_valued("user.location"));
        assert!(key_is_single_valued("user.employer"));
        assert!(key_is_single_valued("user.favourite_colour"));
        assert!(key_is_single_valued("user.likes_coffee"));
        // Multi-valued / past: accumulates — never overwrite through a tool.
        assert!(!key_is_single_valued("user.language"));
        assert!(!key_is_single_valued("user.pet"));
        assert!(!key_is_single_valued("user.employer_past"));
        // Unknown bare attribute → treat as multi (keep both), the safe default.
        assert!(!key_is_single_valued("user.mood"));
    }

    #[test]
    fn looks_like_secret_flags_credentials_only() {
        for s in [
            "My password is hunter2",
            "my wifi password is hunter2",
            "my wifi password is sunshine",                  // plain-word value (R3)
            "my password is correct horse battery staple",   // passphrase (R3)
            "my wifi passcode is 4821",
            "password: hunter2",                             // colon form
            "my PIN: 4821",                                  // colon form
            "my pin number is 4821",                         // pin number
            "PIN 4821",                                      // pin + digits, no connector
            "api key: sk ABCDEF1234567890abcdef",
            "the bearer token is a1b2c3d4e5f6g7h8",
            "sk-ABCDEF1234567890abcdef",                     // opaque token, no cue
        ] {
            assert!(looks_like_secret(s), "should flag: {s:?}");
        }
        for s in [
            "User's favourite colour is teal",
            "User works at Anthropic",
            "User lives at 10 Downing Street",
            "My spin is fun",                                // must not match "pin"
            "I reset my password today",                     // mention, no connector
            "rotate the API key for the weather service",    // mention, no connector (R3)
            "my private key is on a YubiKey",                // value isn't credential-like (R3)
            "save my SSH key",                               // mention, no value
            "my PIN changed in 2023",                        // "changed" not a connector (R3)
            "the secret santa budget is 100",                // "santa" not a connector (R3)
            "the secret to good bread is 500g flour",        // "to" not a connector (R3)
            "the password is in 1Password",                  // "in" = non-value
            "my password was changed",                       // "changed" = non-value
            "my password is too weak",                       // "too" = non-value
        ] {
            assert!(!looks_like_secret(s), "should NOT flag: {s:?}");
        }
    }

    #[test]
    fn explicit_remember_detection() {
        for yes in [
            "Please remember this about me: my dog is Biscuit, a kelpie.",
            "remember that I prefer window seats",
            "don't forget I'm allergic to penicillin",
            "keep in mind I work nights",
            "make a note that I switched to decaf",
            "I had a long day. Remember my sister's name is Leila.", // clause-start opener
            "Actually, please remember that my favourite colour is orange.", // filler + comma
            "OK, so remember: my gym is on Fifth Ave.",                      // fillers + "remember:"
            "Remember my dog is Pixel. What's the weather?",                 // remember clause + trailing question
        ] {
            assert!(is_explicit_remember(yes), "should be explicit: {yes:?}");
        }
        // M1 false positives that must NOT be treated as a save:
        for no in [
            "do you remember my dog's name?",            // question
            "I don't remember I said that",              // mid-sentence, not a clause opener
            "note that the build failed",                // ambiguous "note that" — excluded
            "what do you remember about my trip?",       // question
            "please don't remember this",                // consent (no_store)
            "off the record, I quit my job",             // consent
            "what's the weather today?",                 // no intent
            "remember to call mom tomorrow",             // reminder/task, not a durable fact
            "make a note to buy milk",                   // task, not a fact
            "do you remember my favourite colour?",      // question (even with 'remember my')
        ] {
            assert!(!is_explicit_remember(no), "should NOT be explicit: {no:?}");
        }
        // no_store always wins even if a remember word appears.
        assert!(is_no_store_request("don't remember this"));
        assert!(!is_explicit_remember("don't remember this"));
    }

    #[test]
    fn confidence_tier_parses_and_orders() {
        assert_eq!(ConfidenceTier::parse("high"),    ConfidenceTier::High);
        assert_eq!(ConfidenceTier::parse("MEDIUM"),  ConfidenceTier::Medium);
        assert_eq!(ConfidenceTier::parse("low"),     ConfidenceTier::Low);
        assert_eq!(ConfidenceTier::parse("unknown"), ConfidenceTier::Low);

        assert!(ConfidenceTier::High > ConfidenceTier::Medium);
        assert!(ConfidenceTier::Medium > ConfidenceTier::Low);
    }

    use async_trait::async_trait;
    use std::sync::Mutex;
    use crate::providers::ModelProvider;
    use crate::types::{GenerationResponse, ProviderId, TokenUsage};
    use crate::MiraError;

    /// Mock provider with canned responses, identical pattern to the
    /// onboarding extractor tests.
    struct CannedProvider(Mutex<Vec<String>>);
    #[async_trait]
    impl ModelProvider for CannedProvider {
        fn name(&self) -> &str { "canned" }
        async fn generate(&self, _m: &[ChatMessage], _o: &GenerationOptions)
            -> Result<GenerationResponse, MiraError>
        {
            let next = self.0.lock().unwrap().remove(0);
            Ok(GenerationResponse {
                content:     next,
                tool_calls:  None,
                reasoning:   None,
                usage:       TokenUsage::default(),
                provider_id: ProviderId::Local("canned".into()),
                model_name:  "canned".into(),
                fallback: None,
            })
        }
        async fn health_check(&self) -> bool { true }
    }

    #[tokio::test]
    async fn extract_respects_allowed_categories() {
        let canned = r#"{"memories":[
            {"content":"User works at Anthropic","category":"fact","confidence":"high","entity":"job"},
            {"content":"User's sister is a doctor","category":"relationship","confidence":"high","entity":"sister"}
        ]}"#;
        let provider: Arc<dyn ModelProvider> = Arc::new(
            CannedProvider(Mutex::new(vec![canned.to_owned()]))
        );

        // `relationship` excluded — must be dropped even at high confidence.
        // User turn supports the kept fact (A.2 grounding needs distinctive
        // overlap); the relationship candidate is dropped by the category gate
        // before grounding regardless.
        let got = LlmMemoryExtractor::new().extract(
            &provider, "I work at Anthropic and my sister is a doctor.", "a",
            &["fact".to_owned(), "preference".to_owned()],
            ConfidenceTier::Medium,
            &[],
        ).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].category, "fact");
    }

    #[tokio::test]
    async fn extract_respects_min_confidence() {
        let canned = r#"{"memories":[
            {"content":"User definitely likes Rust","category":"preference","confidence":"high","entity":"lang_pref"},
            {"content":"User might be based in Dubai","category":"fact","confidence":"low","entity":"location"}
        ]}"#;
        let provider: Arc<dyn ModelProvider> = Arc::new(
            CannedProvider(Mutex::new(vec![canned.to_owned()]))
        );

        // User turn supports the kept preference ('rust'); the low-confidence
        // 'Dubai' fact is dropped by the confidence gate before grounding.
        let got = LlmMemoryExtractor::new().extract(
            &provider, "I really like Rust, and I might move to Dubai.", "a",
            &["fact".to_owned(), "preference".to_owned()],
            ConfidenceTier::Medium,
            &[],
        ).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].entity, "lang_pref");
    }

    #[tokio::test]
    async fn extract_drops_empty_entity() {
        // Entity is required for conflict detection — a candidate with no
        // entity can't be deduped so it must be dropped rather than
        // accumulate as a new row on every turn.
        let canned = r#"{"memories":[
            {"content":"User said something","category":"fact","confidence":"high","entity":""}
        ]}"#;
        let provider: Arc<dyn ModelProvider> = Arc::new(
            CannedProvider(Mutex::new(vec![canned.to_owned()]))
        );
        let got = LlmMemoryExtractor::new().extract(
            &provider, "u", "a",
            &["fact".to_owned()], ConfidenceTier::Low,
            &[],
        ).await;
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn extract_returns_empty_on_provider_failure() {
        struct Failing;
        #[async_trait]
        impl ModelProvider for Failing {
            fn name(&self) -> &str { "failing" }
            async fn generate(&self, _: &[ChatMessage], _: &GenerationOptions)
                -> Result<GenerationResponse, MiraError>
            {
                Err(MiraError::ProviderError("boom".into()))
            }
            async fn health_check(&self) -> bool { false }
        }
        let provider: Arc<dyn ModelProvider> = Arc::new(Failing);
        let got = LlmMemoryExtractor::new().extract(
            &provider, "u", "a",
            &["fact".to_owned()], ConfidenceTier::Low,
            &[],
        ).await;
        assert!(got.is_empty(), "provider failure must not panic, must return empty");
    }
}

#[cfg(test)]
mod llm_extractor_tests {
    use super::*;

    #[test]
    fn test_parse_response_valid_json() {
        let resp = r#"["User is learning Rust", "User has 6 months of experience"]"#;
        let memories = LlmExtractor::parse_response(resp);
        assert_eq!(memories.len(), 2);
        assert!(memories[0].contains("Rust"));
    }

    #[test]
    fn test_parse_response_empty() {
        assert!(LlmExtractor::parse_response("[]").is_empty());
    }

    #[test]
    fn test_parse_response_prose_wrapped() {
        let resp = r#"Here are the memories: ["User likes coffee", "User is based in Cairo"]"#;
        let memories = LlmExtractor::parse_response(resp);
        assert_eq!(memories.len(), 2);
    }

    #[test]
    fn test_llm_extraction_prompt_format() {
        let turns = vec![
            ("user".to_string(), "I've been learning Rust for 6 months now.".to_string()),
            ("assistant".to_string(), "That's great!".to_string()),
        ];
        let prompt = LlmExtractor::build_prompt(&turns);
        assert!(prompt.contains("Rust"));
        assert!(prompt.contains("JSON"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_name() {
        let e = HeuristicExtractor::new();
        let result = e.extract("My name is Tarek");
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("Tarek"));
        assert!(result[0].confidence > 0.9);
    }

    #[test]
    fn test_extract_location() {
        let e = HeuristicExtractor::new();
        let result = e.extract("I live in Cairo, Egypt.");
        assert!(result.iter().any(|c| c.content.contains("Cairo")));
    }

    #[test]
    fn test_extract_work() {
        let e = HeuristicExtractor::new();
        let result = e.extract("I work at Google as an engineer.");
        assert!(result.iter().any(|c| c.content.contains("Google")));
    }

    #[test]
    fn test_extract_preference() {
        let e = HeuristicExtractor::new();
        let result = e.extract("I love hiking in the mountains!");
        assert!(!result.is_empty());
    }

    #[test]
    fn test_extract_age() {
        let e = HeuristicExtractor::new();
        let result = e.extract("I'm 32 years old.");
        assert!(result.iter().any(|c| c.content.contains("32")));
    }

    #[test]
    fn test_no_false_positives_on_questions() {
        let e = HeuristicExtractor::new();
        let result = e.extract("What time is it?");
        assert!(result.is_empty());
    }

    #[test]
    fn test_low_noise_text() {
        let e = HeuristicExtractor::new();
        let result = e.extract("The capital of France is Paris.");
        assert!(result.is_empty());
    }
}
