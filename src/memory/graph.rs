// SPDX-License-Identifier: AGPL-3.0-or-later

// src/memory/graph.rs
//! Temporal knowledge-graph memory (see `design-docs/graph-memory.md`).
//!
//! the data structs, entity-name normalisation, and the LLM **triple
//! extractor**. Storage (entity resolution + edge insert) lives on
//! [`crate::memory::storage::MemoryStorage`]; the per-turn orchestration that
//! ties extraction → resolution → store lives on
//! [`crate::memory::MemorySystem`]. Retrieval is 
//!
//! The graph models a fact as a timestamped, typed edge:
//! `subject —predicate→ object | value+unit @ event_at`. This is what lets
//! aggregation become exact (`COUNT`/`SUM` over edges) instead of fuzzy
//! semantic top-k, which is the LongMemEval multi-session ceiling flat memory
//! could not pass.

use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions};

const TRIPLE_EXTRACTOR_TIMEOUT: Duration = Duration::from_secs(180);

// A resolved graph entity row.
#[derive(Debug, Clone)]
pub struct GraphEntity {
    pub id:          i64,
    pub name:        String,
    pub entity_type: String,
}

// A graph edge (fact) row.
#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub id:         i64,
    pub subject_id: i64,
    pub predicate:  String,
    pub object_id:  Option<i64>,
    pub value_num:  Option<f64>,
    pub value_unit: Option<String>,
    pub fact_text:  String,
    pub event_at:   Option<i64>,
    pub valid_from: i64,
    pub valid_to:   Option<i64>,
}

// One extracted triple, before entity resolution. The subject (and optional
// object) are still free-text names; resolution maps them to entity ids.
#[derive(Debug, Clone)]
pub struct Triple {
    pub subject:      String,
    pub subject_type: String,
    pub predicate:    String,
    // A named object entity, when the fact relates two things.
    pub object:       Option<String>,
    // Numeric payload for COUNT/SUM/AVG (e.g. 25.0 for "$25").
    pub value_num:    Option<f64>,
    // Unit for `value_num` (e.g. "usd", "hours", "days").
    pub value_unit:   Option<String>,
    // Human-readable restatement of the fact.
    pub fact_text:    String,
    // When the fact happened, unix-ms, if the turn made it explicit.
    pub event_at:     Option<i64>,
}

// Normalise an entity name for resolution: lowercase, trim, drop a leading
// article, collapse internal whitespace. `"The Art Cube"` → `"art cube"`,
// `"a peace lily"` → `"peace lily"`. Deliberately conservative — it only
// folds away formatting/article noise, not semantics.
pub fn normalize_name(name: &str) -> String {
    let lc = name.trim().to_ascii_lowercase();
    let stripped = lc
        .strip_prefix("the ")
        .or_else(|| lc.strip_prefix("an "))
        .or_else(|| lc.strip_prefix("a "))
        .unwrap_or(&lc);
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Crude singular stem for query↔graph matching: lowercased, common plural
// suffix stripped. Consistency matters more than linguistic correctness —
// query and entity name/type pass through the same fn, so equal stems match.
pub(crate) fn stem_word(w: &str) -> String {
    let w: String = w.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    if w.len() < 3 { return String::new(); }
    if w.ends_with("ies") && w.len() > 4 { return format!("{}y", &w[..w.len() - 3]); }
    if w.ends_with('s') && !w.ends_with("ss") && w.len() > 3 { return w[..w.len() - 1].to_string(); }
    w
}

// Content-word stems from a query, dropping stopwords and short tokens.
pub(crate) fn query_stems(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the","and","for","you","your","have","has","had","did","does","how","many",
        "much","what","when","where","which","who","whom","that","this","these","those",
        "with","from","about","into","over","been","were","was","are","they","them",
        "there","here","total","count","number","list","all","any","some","last","past",
        "recent","recently","ago","since","during","between","time","times","tell","give",
    ];
    let mut out: Vec<String> = Vec::new();
    for tok in query.split(|c: char| !c.is_ascii_alphanumeric()) {
        let lc = tok.to_ascii_lowercase();
        if lc.len() < 4 || STOP.contains(&lc.as_str()) { continue; }
        let s = stem_word(tok);
        if !s.is_empty() && !out.contains(&s) { out.push(s); }
    }
    out
}

// Does any stem of `text` (split on non-alphanumerics and on '_') match any of
// `qstems`? Used to test an entity name/type against the query.
pub(crate) fn text_matches_query(text: &str, qstems: &[String]) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .flat_map(|w| w.split('_'))
        .any(|w| { let s = stem_word(w); !s.is_empty() && qstems.iter().any(|q| q == &s) })
}

// ── Triple extraction ──────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Deserialize)]
struct RawTriples {
    #[serde(default)]
    triples: Vec<RawTriple>,
}

#[derive(Debug, serde::Deserialize)]
struct RawTriple {
    #[serde(default)]
    subject:      String,
    #[serde(default)]
    subject_type: String,
    #[serde(default)]
    predicate:    String,
    #[serde(default)]
    object:       String,
    // Accept number or string ("25", 25, "$25") — coerced below.
    #[serde(default)]
    value:        serde_json::Value,
    #[serde(default)]
    unit:         String,
    #[serde(default)]
    fact:         String,
}

// Coerce a JSON value or "$1,234.5"-style string into a number.
fn coerce_num(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => {
            let cleaned: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
            if cleaned.is_empty() { None } else { cleaned.parse::<f64>().ok() }
        }
        _ => None,
    }
}

// Extract typed triples from one conversation turn. Failure-tolerant: any
// parse/IO error yields an empty vec so a bad call never blocks a response.
pub async fn extract_triples(
    provider:      &Arc<dyn ModelProvider>,
    user_msg:      &str,
    assistant_msg: &str,
    turn_event_at: Option<i64>,
) -> Vec<Triple> {
    let messages = vec![
        ChatMessage::system(triple_extractor_system_prompt()),
        ChatMessage::user(format!(
            "USER TURN:\n{}\n\nASSISTANT TURN:\n{}\n\nEmit the JSON object now.",
            user_msg.trim(), assistant_msg.trim(),
        )),
    ];
    let opts = GenerationOptions { temperature: 0.0, max_tokens: Some(4096), ..Default::default() };

    let response = match tokio::time::timeout(TRIPLE_EXTRACTOR_TIMEOUT, provider.generate(&messages, &opts)).await {
        Ok(Ok(r))  => r.content,
        Ok(Err(e)) => { warn!("triple extractor: provider call failed: {}", e); return vec![]; }
        Err(_)     => { warn!("triple extractor: timed out"); return vec![]; }
    };

    let Some(payload) = crate::memory::auto_extract::extract_json_object(&response) else {
        debug!("triple extractor: no JSON object in response");
        return vec![];
    };
    let raw: RawTriples = match serde_json::from_str(&payload) {
        Ok(r)  => r,
        Err(e) => { debug!("triple extractor: parse failed: {}", e); return vec![]; }
    };

    raw.triples.into_iter().filter_map(|t| {
        let subject = t.subject.trim().to_string();
        let predicate = t.predicate.trim().to_ascii_lowercase();
        let fact = t.fact.trim().to_string();
        // A triple is useless without a subject, a predicate, and a
        // human-readable fact to fall back on for display/retrieval.
        if subject.is_empty() || predicate.is_empty() || fact.is_empty() {
            return None;
        }
        let object = { let o = t.object.trim(); if o.is_empty() { None } else { Some(o.to_string()) } };
        let value_unit = { let u = t.unit.trim().to_ascii_lowercase(); if u.is_empty() { None } else { Some(u) } };
        let subject_type = {
            let st = t.subject_type.trim().to_ascii_lowercase();
            if st.is_empty() { "thing".to_string() } else { st }
        };
        Some(Triple {
            subject,
            subject_type,
            predicate,
            object,
            value_num: coerce_num(&t.value),
            value_unit,
            fact_text: fact,
            event_at: turn_event_at,
        })
    }).collect()
}

fn triple_extractor_system_prompt() -> String {
    r#"Extract factual TRIPLES about the USER from one conversation turn, for a knowledge graph used to COUNT and TOTAL things.

Output ONLY a JSON object, no prose, no reasoning, no code fences:
{"triples":[{"subject":"<the THING the fact is about>","subject_type":"<grouping category>","predicate":"<short relation>","object":"<related named thing, or empty>","value":"<number if any, else empty>","unit":"<usd|hours|days|count|… else empty>","fact":"<one-sentence restatement>"}]}

Rules:
- Extract ONLY facts the USER stated or implied about themselves. Ignore the
  assistant's words, general knowledge, and quoted third parties.
- ONE triple per fact. Decompose "I spent $25 on a chain and $40 on tires" into
  TWO triples.
- `subject` is the SPECIFIC THING the fact is about — the item owned, the place
  visited, the expense, the activity — **NOT the word "user"**. "I own a peace
  lily" → subject "peace lily" (not "user"). "I wore a navy blazer" → subject
  "navy blazer".
- `subject_type` is the GROUP that thing belongs to, so siblings can be counted
  together. Use the SAME type string for things of the same kind: every plant →
  "plant"; every clothing item → "clothing"; every bike cost → "bike_expense";
  every road trip → "road_trip"; every movie watched → "movie". This is what
  makes "how many plants?" or "total bike spend?" answerable exactly.
- `predicate`: the relation to the user ("owned","worn","visited","cost",
  "lasted","watched","lent").
- `value` + `unit`: fill whenever the fact has a NUMBER. "$25" → value 25, unit
  "usd". "3 hours" → value 3, unit "hours". This is what lets totals be exact.
- `object`: another named thing the fact links to (e.g. who you lent it to),
  else empty.
- `fact`: a plain one-sentence restatement.
- For a plain attribute of the user with nothing to count (job, hometown), it is
  fine to use subject "user" (e.g. subject "user", predicate "works at", object
  "Google").

Examples:
USER: "I bought a peace lily and a snake plant this week, and spent $25 on a bike chain."
{"triples":[
{"subject":"peace lily","subject_type":"plant","predicate":"acquired","object":"","value":"","unit":"","fact":"User acquired a peace lily."},
{"subject":"snake plant","subject_type":"plant","predicate":"acquired","object":"","value":"","unit":"","fact":"User acquired a snake plant."},
{"subject":"bike chain","subject_type":"bike_expense","predicate":"cost","object":"","value":"25","unit":"usd","fact":"User spent $25 on a bike chain."}
]}

Return {"triples":[]} if nothing factual about the user was stated."#
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_name_folds_articles_and_case() {
        assert_eq!(normalize_name("The Art Cube"), "art cube");
        assert_eq!(normalize_name("a peace lily"), "peace lily");
        assert_eq!(normalize_name("  An   Old   Bike "), "old bike");
        assert_eq!(normalize_name("Anthropic"), "anthropic"); // 'an' not stripped mid-word
        assert_eq!(normalize_name("Peace Lily"), "peace lily");
    }

    #[test]
    fn query_matches_entity_type_and_name() {
        let qs = query_stems("How many plants do I have?");
        assert!(qs.contains(&"plant".to_string()), "got {qs:?}");
        // type "plant" matches "plants" query
        assert!(text_matches_query("plant", &qs));
        // grouping type with underscore: "bike_expense" matches "bike"
        let qs2 = query_stems("what have I spent on the bike");
        assert!(text_matches_query("bike_expense", &qs2), "got {qs2:?}");
        // entity name match: "road_trip" type matches "trips"
        let qs3 = query_stems("total hours across my road trips");
        assert!(text_matches_query("road_trip", &qs3), "got {qs3:?}");
        // no false match
        assert!(!text_matches_query("clothing", &qs));
    }

    #[test]
    fn coerce_num_handles_money_and_numbers() {
        assert_eq!(coerce_num(&serde_json::json!(25)), Some(25.0));
        assert_eq!(coerce_num(&serde_json::json!("$1,234.50")), Some(1234.50));
        assert_eq!(coerce_num(&serde_json::json!("3")), Some(3.0));
        assert_eq!(coerce_num(&serde_json::json!("")), None);
        assert_eq!(coerce_num(&serde_json::json!("none")), None);
    }
}
