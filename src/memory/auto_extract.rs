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
    pub entity:     String,
    /// Coarse category slug for *topic-grouped retrieval* (`"plants"`,
    /// `"bike"`, `"road_trips"`). Unlike `entity` (a fine per-fact conflict
    /// key, deliberately distinct so facts don't supersede each other), the
    /// topic is shared across sibling facts so aggregation queries can pull
    /// the complete set. May be empty when no natural grouping applies.
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
    ) -> Vec<LlmCandidate> {
        let messages = vec![
            ChatMessage::system(build_extractor_system_prompt()),
            ChatMessage::user(build_extractor_user_prompt(user_msg, assistant_msg)),
        ];

        let opts = GenerationOptions {
            temperature: 0.0,
            // 4096 fits the capped 5 memories comfortably; `parse_raw_extraction`
            // salvages a response truncated mid-array as a backstop.
            max_tokens:  Some(4096),
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

            Some(LlmCandidate { content, category: cat, confidence: m.confidence, entity, topic })
        }).collect()
    }
}

impl Default for LlmMemoryExtractor {
    fn default() -> Self { Self::new() }
}

fn build_extractor_system_prompt() -> String {
    // Flat, strict-JSON prompt. The onboarding extractor taught us that
    // reasoning-distilled local models mirror visual hierarchy into their
    // output, so keep the shape flat and name the exact fields.
    r#"Extract facts about the USER from one conversation turn.

Output ONLY a JSON object, no prose, no reasoning, no code fences:
{"memories":[{"content":"<short factual sentence>","category":"<fact|preference|skill|relationship|project>","confidence":"<low|medium|high>","entity":"<short_conflict_key>","topic":"<coarse_group>"}]}

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
- `entity`: a short lowercase noun UNIQUELY naming THIS fact (e.g. "job",
  "location", "pet_name"). A new fact with the same entity REPLACES the old
  one, so make entities specific enough that distinct facts don't collide
  (e.g. "bike_chain_cost" not "bike" for one expense). snake_case.
- `topic`: a COARSE grouping shared by all facts of the same kind, used to
  retrieve the whole set together for counting/totalling. Many facts share
  one topic (e.g. every bike expense → "bike_expenses"; every plant you own
  → "plants"; every road trip → "road_trips"). Re-use the SAME topic slug
  across turns for the same kind of thing. snake_case; "" if none applies.
  Contrast with entity: entity is unique per fact, topic is shared.
- Return `{"memories":[]}` if nothing new was stated."#
        .to_owned()
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
    #[serde(default)]
    entity:     String,
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
        let got = LlmMemoryExtractor::new().extract(
            &provider, "u", "a",
            &["fact".to_owned(), "preference".to_owned()],
            ConfidenceTier::Medium,
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

        let got = LlmMemoryExtractor::new().extract(
            &provider, "u", "a",
            &["fact".to_owned(), "preference".to_owned()],
            ConfidenceTier::Medium,
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
