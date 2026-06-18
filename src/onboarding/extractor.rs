// SPDX-License-Identifier: AGPL-3.0-or-later

// src/onboarding/extractor.rs
//! Post-turn profile extractor.
//!
//! Reasoning-distilled local models (Qwen, Gemma, Hermes, many others)
//! routinely *narrate* tool calls instead of firing them:
//!
//! > "I've updated your timezone to Australia/Melbourne"
//! > "I've tucked away those details about your family"
//!
//! …without ever emitting a structured `record_profile` / `skip_topic`
//! call. Every guard or finalization backstop we added downstream assumed
//! some tool activity would land server-side, so the flow stalls at
//! whichever group the model went quiet on.
//!
//! This module decouples data capture from the primary model's tool-calling
//! discipline. After each onboarding turn, the chat handler runs
//! [`extract_updates_from_transcript`], which re-reads the full conversation
//! and asks the provider to emit a **strict JSON object** enumerating what
//! the user has shared so far. The server then replays those findings
//! through the existing `record_profile` / `skip_topic` / `mark_group_complete`
//! / `complete_onboarding` tools, idempotently.
//!
//! Idempotency matters because the extractor runs every turn: once a key is
//! answered, subsequent extractions will keep reporting it, and the tool
//! handlers must treat that as a no-op.
//!
//! ## What it is NOT
//!
//! This is not a retry loop or a second conversation. The extractor is a
//! passive read-only scan of existing turns that produces a structured
//! summary. The primary model still drives the conversation; the extractor
//! is purely about catching what its narration didn't commit.
//!
//! ## Failure mode
//!
//! Any parse/IO error is logged and swallowed — the extractor is a safety
//! net, not a gate. A missing extraction turn just means the user stalls
//! the same way they would have stalled without it, so we never let
//! extractor failure block the primary response from streaming.
//!
//! ## Cost
//!
//! One extra non-streaming model call per onboarding turn. Onboarding is
//! short (≤ ~15 turns) and only runs once per user, so the total overhead
//! is small — and it's the price of supporting models that can't be
//! trusted to call tools reliably.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::onboarding::{OnboardingSchema, WriteTarget};
use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions, MessageRole};
use crate::MiraError;

/// Upper bound on how long the extractor call is allowed to take. Past
/// this we give up.
///
/// Reasoning-distilled local models (gemma-4-26b-a4b, qwen3.5-27b-*) burn
/// 600–800 reasoning tokens before emitting JSON for the full schema, so 45s
/// wasn't enough — the connection would drop mid-reasoning and the model
/// never got to the JSON. 180s is generous enough to cover reasoning +
/// output even on slow local GPUs. The user isn't waiting on this (we
/// stream `done` before firing the extractor), so a longer wait is cheap.
const EXTRACTOR_TIMEOUT: Duration = Duration::from_secs(180);

// ── Public API ────────────────────────────────────────────────────────────────

/// A single op the extractor wants the server to apply.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Treat as if `record_profile` was called with these args.
    Record { key: String, value: Value },
    /// Treat as if `skip_topic` was called with this key.
    Skip   { key: String },
    /// Treat as if `mark_group_complete` was called with this group id.
    MarkGroupComplete { group_id: String },
    /// Treat as if `complete_onboarding` was called. The primary model said
    /// we're done and the user agreed — hand off to the finalization guard
    /// to validate (activity coverage is still required).
    Finalize,
}

/// Result of extracting updates from an onboarding transcript.
#[derive(Debug, Default, Clone)]
pub struct ExtractedUpdates {
    pub ops: Vec<Op>,
}

/// Read the onboarding transcript and return the ops that should land
/// server-side, independent of whether the primary model fired tools.
///
/// Returns `Ok(Default::default())` on any recoverable error (provider
/// failure, malformed JSON, empty response). This is a safety net — we
/// never want to abort a chat turn because the extractor had a bad day.
pub async fn extract_updates_from_transcript(
    provider:          &Arc<dyn ModelProvider>,
    schema:            &OnboardingSchema,
    transcript:        &[ChatMessage],
    current_progress:  &Value,
) -> ExtractedUpdates {
    if transcript.iter().all(|m| !matches!(m.role, MessageRole::User | MessageRole::Assistant)) {
        // Nothing to extract from — all system messages.
        return ExtractedUpdates::default();
    }

    let system_prompt = build_extractor_system_prompt(schema, current_progress);
    let user_prompt   = build_extractor_user_prompt(transcript);

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ];
    // Temperature 0 for deterministic structured output. Max tokens is
    // intentionally generous — reasoning-distilled models (gemma, qwen) use
    // 600–800 tokens of internal reasoning *before* emitting the JSON, and
    // 1024 would clip the output mid-object. 4096 covers reasoning + the
    // largest plausible JSON payload for the full schema with comfortable
    // headroom.
    let opts = GenerationOptions {
        temperature: 0.0,
        max_tokens:  Some(4096),
        ..Default::default()
    };

    let response = match tokio::time::timeout(
        EXTRACTOR_TIMEOUT,
        provider.generate(&messages, &opts),
    ).await {
        Ok(Ok(r))  => r.content,
        Ok(Err(e)) => {
            warn!("onboarding extractor: provider call failed, skipping: {}", e);
            return ExtractedUpdates::default();
        }
        Err(_) => {
            warn!("onboarding extractor: timed out after {:?}, skipping", EXTRACTOR_TIMEOUT);
            return ExtractedUpdates::default();
        }
    };

    match parse_extractor_response(&response, schema) {
        Ok(u)  => {
            debug!("onboarding extractor produced {} ops", u.ops.len());
            u
        }
        Err(e) => {
            warn!("onboarding extractor: could not parse response as JSON: {}. Raw: {:?}",
                e, truncate_for_log(&response, 500));
            ExtractedUpdates::default()
        }
    }
}

// ── Prompt construction ──────────────────────────────────────────────────────

fn build_extractor_system_prompt(schema: &OnboardingSchema, current_progress: &Value) -> String {
    // Scope the schema slice we send the model to *only* the questions still
    // outstanding — keys not yet in `answered_keys` / `skipped_keys` and
    // belonging to groups not yet in `completed_groups`. Reasoning-distilled
    // models (gemma-4, qwen3.5) spiral when given the full schema + a long
    // "already done" exclusion list: they re-read every rule, cross-check
    // every key against the exclusion list, and burn thousands of reasoning
    // tokens before emitting JSON. Sending only the relevant slice cuts that
    // cognitive load by an order of magnitude.
    let answered_set: std::collections::HashSet<&str> = current_progress
        .get("answered_keys").and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    let skipped_set: std::collections::HashSet<&str> = current_progress
        .get("skipped_keys").and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    let done_set: std::collections::HashSet<&str> = current_progress
        .get("completed_groups").and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();

    // Flatten remaining questions into a single list — one key per line with
    // its group as metadata, rather than nesting questions under group
    // headers. Reasoning-distilled models (gemma-4, qwen3.5) mirror the
    // visual hierarchy of the prompt into their output: when questions were
    // nested under `work_hobbies:\n  work_summary: ...\n  hobbies: ...`,
    // gemma emitted `{"key":"work_hobbies","value":{"work_summary":"…","hobbies":"…"}}`
    // — a nested record with the group id as the key. The parser silently
    // dropped those (`work_hobbies` isn't a question key). A flat list
    // `work_summary: paraphrase (group=work_hobbies)` removes that cue.
    let mut lines = String::new();
    for g in &schema.groups {
        if done_set.contains(g.id.as_str()) { continue; }
        for q in &g.questions {
            if answered_set.contains(q.key.as_str()) || skipped_set.contains(q.key.as_str()) {
                continue;
            }
            let target = match &q.writes_to {
                Some(WriteTarget::User(_))        => "string",
                Some(WriteTarget::UserProfile(c)) => match c.as_str() {
                    "height_cm" | "weight_kg"     => "integer",
                    "contact_hours_start_end"     => "'HH:MM-HH:MM' 24h",
                    "birth_date"                  => "'YYYY-MM-DD'",
                    _                             => "string",
                },
                Some(WriteTarget::ProfileMd(_))   => "paraphrase",
                Some(WriteTarget::MemorySeed)     => "paraphrase",
                None                              => continue,
            };
            let opts = q.options.as_ref()
                .map(|o| format!(" [{}]", o.join("|")))
                .unwrap_or_default();
            let optional = if g.optional { " optional" } else { "" };
            lines.push_str(&format!(
                "{}: {}{} (group={}{})\n",
                q.key, target, opts, g.id, optional,
            ));
        }
    }

    if lines.is_empty() {
        // Everything's already been captured — only finalize decision remains.
        return
            "Extract end-of-onboarding signal from this transcript.\n\
             Output ONLY this JSON, no prose, no reasoning:\n\
             {\"records\":[],\"skips\":[],\"completed_groups\":[],\"finalize\":<true|false>}\n\
             Set finalize=true if the user said they're done (\"let's get started\", \"we're set\", \"move on\"). \
             Otherwise finalize=false.".to_string();
    }

    format!(
        "Extract onboarding answers from the transcript. Output ONLY a JSON object, no prose, no reasoning.\n\
         \n\
         Schema:\n\
         {{\"records\":[{{\"key\":\"<question_key>\",\"value\":<string|number>}}],\"skips\":[\"<question_key>\"],\"completed_groups\":[\"<group_id>\"],\"finalize\":false}}\n\
         \n\
         Example:\n\
         {{\"records\":[{{\"key\":\"hobbies\",\"value\":\"photography, trail running\"}},{{\"key\":\"top_goals\",\"value\":\"ship onboarding by April\"}},{{\"key\":\"verbosity\",\"value\":\"brief\"}}],\"skips\":[\"off_limits_topics\"],\"completed_groups\":[\"personal\"],\"finalize\":false}}\n\
         \n\
         Outstanding questions (one record per line, format `key: type [options] (group=...)`):\n{lines}\n\
         Rules:\n\
         - One record per question. `key` MUST be one of the keys listed above.\n\
         - NEVER use a group id as a record key — groups are metadata only.\n\
         - `value` must be a JSON string or number, NEVER a nested object.\n\
         - Paraphrase prose fields briefly.\n\
         - Use `skips` only if the user explicitly declined a question.\n\
         - Use `completed_groups` only if the user clearly finished a group.\n\
         - Set `finalize`:true if the user said they're done (\"let's go\", \"we're set\", \"move on\")."
    )
}

fn build_extractor_user_prompt(transcript: &[ChatMessage]) -> String {
    let mut out = String::from("ONBOARDING TRANSCRIPT:\n\n");
    for m in transcript {
        let label = match m.role {
            MessageRole::User      => "USER",
            MessageRole::Assistant => "ASSISTANT",
            MessageRole::System | MessageRole::Tool => continue,
        };
        out.push_str(&format!("--- {} ---\n{}\n\n", label, m.content.trim()));
    }
    out.push_str("--- END TRANSCRIPT ---\n\nEmit the JSON object now.");
    out
}

// ── Response parsing ─────────────────────────────────────────────────────────

/// Shape of the JSON object the extractor model returns. We parse leniently:
/// missing fields default to empty, unknown fields are ignored.
#[derive(Debug, Default, Deserialize)]
struct RawExtraction {
    #[serde(default)]
    records: Vec<RawRecord>,
    #[serde(default)]
    skips: Vec<String>,
    #[serde(default)]
    completed_groups: Vec<String>,
    #[serde(default)]
    finalize: bool,
}

#[derive(Debug, Deserialize)]
struct RawRecord {
    key:   String,
    value: Value,
}

fn parse_extractor_response(raw: &str, schema: &OnboardingSchema) -> Result<ExtractedUpdates, MiraError> {
    // Small/local models love to wrap JSON in ```json fences or add prose
    // around it even when told not to. Be liberal: find the first `{` and
    // the matching `}` span, parse that.
    let payload = extract_json_object(raw)
        .ok_or_else(|| MiraError::ToolError("extractor: no JSON object found in response".to_string()))?;

    let parsed: RawExtraction = serde_json::from_str(&payload)
        .map_err(|e| MiraError::ToolError(format!("extractor: JSON parse failed: {}", e)))?;

    let mut ops = Vec::new();

    for r in parsed.records {
        // Forgiveness: reasoning-distilled models sometimes confuse group ids
        // with question keys and emit `{"key":"<group_id>","value":{"<sub>":V,…}}`.
        // Fan that out to per-sub-key records rather than dropping the whole
        // record as unknown. Salvaging a 150s extractor call is worth the
        // small amount of extra code.
        if schema.question(&r.key).is_none() && schema.group(&r.key).is_some() {
            if let Value::Object(map) = &r.value {
                for (k, v) in map {
                    if schema.question(k).is_none() {
                        warn!("extractor: dropping nested sub-key '{}' under group '{}' (not a question)", k, r.key);
                        continue;
                    }
                    match v {
                        Value::Null => continue,
                        Value::String(s) if s.trim().is_empty() => continue,
                        _ => {}
                    }
                    ops.push(Op::Record { key: k.clone(), value: v.clone() });
                }
                continue;
            }
        }

        // Drop records with unknown keys — protects tool handlers from
        // hallucinated question names.
        if schema.question(&r.key).is_none() {
            warn!("extractor: dropping record with unknown key '{}'", r.key);
            continue;
        }
        // Null/empty string values would fail the tool's non-null check.
        // Skip silently — if the user actually declined, the extractor
        // would have used `skips`.
        match &r.value {
            Value::Null => continue,
            Value::String(s) if s.trim().is_empty() => continue,
            _ => {}
        }
        ops.push(Op::Record { key: r.key, value: r.value });
    }

    for k in parsed.skips {
        if schema.question(&k).is_none() {
            warn!("extractor: dropping skip with unknown key '{}'", k);
            continue;
        }
        ops.push(Op::Skip { key: k });
    }

    for g in parsed.completed_groups {
        if schema.group(&g).is_none() {
            warn!("extractor: dropping mark-complete for unknown group '{}'", g);
            continue;
        }
        ops.push(Op::MarkGroupComplete { group_id: g });
    }

    if parsed.finalize {
        ops.push(Op::Finalize);
    }

    Ok(ExtractedUpdates { ops })
}

/// Find the first balanced `{ ... }` in `s`, skipping over string contents
/// (so `{ "text": "}" }` parses correctly). Returns `None` if no balanced
/// object exists.
fn extract_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;

    let mut depth   = 0i32;
    let mut in_str  = false;
    let mut escape  = false;

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
    if s.len() <= max { s.to_owned() } else { format!("{}…({} more chars)", &s[..max], s.len() - max) }
}

// ── Applying extracted ops ───────────────────────────────────────────────────

/// Apply the extracted ops by dispatching each through the tool registry,
/// re-using the server-side plumbing that injects `_user_id`, jump-ahead,
/// and auto-finalize. Ops are idempotent — the record/skip/mark tools all
/// tolerate repeated calls for the same key.
///
/// Errors from individual ops are logged and swallowed: if one record
/// can't be written (bad format, DB hiccup), we still want the rest of the
/// extraction to land.
pub async fn apply_ops(
    tools:   &crate::tools::ToolRegistry,
    user_id: &str,
    conv_id: &str,
    ops:     &[Op],
) {
    for op in ops {
        let (tool_name, mut args) = match op {
            Op::Record { key, value } => (
                "record_profile",
                json!({ "key": key, "value": value }),
            ),
            Op::Skip { key } => (
                "skip_topic",
                json!({ "key": key }),
            ),
            Op::MarkGroupComplete { group_id } => (
                "mark_group_complete",
                json!({ "group_id": group_id }),
            ),
            Op::Finalize => (
                "complete_onboarding",
                json!({}),
            ),
        };

        // Mirror the chat-handler injection so the tools see a trusted
        // user_id. Without this, `require_user_id` rejects the call.
        if let Some(obj) = args.as_object_mut() {
            obj.insert("_user_id".to_string(),         Value::String(user_id.to_owned()));
            obj.insert("_conversation_id".to_string(), Value::String(conv_id.to_owned()));
        }

        match tools.execute(tool_name, args).await {
            Ok(r) if r.success => {
                debug!("extractor applied {}: {:?}", tool_name, op);
            }
            Ok(r) => {
                // `complete_onboarding` returning a soft failure (guard
                // rejected) is expected mid-flow — we still want the records
                // and skips to land even if the primary model jumped the gun
                // on finalize. Downgrade to debug for that case.
                let level_debug = matches!(op, Op::Finalize);
                if level_debug {
                    debug!("extractor {} soft-failed (ok, guard still protects): {:?}", tool_name, r.error);
                } else {
                    warn!("extractor {} soft-failed: {:?}", tool_name, r.error);
                }
            }
            Err(e) => {
                warn!("extractor {} errored: {}", tool_name, e);
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> OnboardingSchema {
        OnboardingSchema::bundled().unwrap()
    }

    #[test]
    fn extract_json_finds_first_balanced_object() {
        let s = r#"sure, here it is: ```json
{"a": 1, "b": {"c": 2}}
``` end"#;
        let got = extract_json_object(s).unwrap();
        assert_eq!(got, r#"{"a": 1, "b": {"c": 2}}"#);
    }

    #[test]
    fn extract_json_ignores_braces_inside_strings() {
        let s = r#"prefix {"text": "has } brace", "x": 1} suffix"#;
        let got = extract_json_object(s).unwrap();
        assert_eq!(got, r#"{"text": "has } brace", "x": 1}"#);
    }

    #[test]
    fn extract_json_returns_none_without_object() {
        assert!(extract_json_object("no braces here at all").is_none());
    }

    #[test]
    fn parse_drops_unknown_keys_and_groups() {
        let raw = r#"{
            "records": [
                {"key": "preferred_name", "value": "Tarek"},
                {"key": "does_not_exist", "value": "x"}
            ],
            "skips": ["weight_kg", "not_a_key"],
            "completed_groups": ["name", "bogus_group"],
            "finalize": false
        }"#;
        let u = parse_extractor_response(raw, &schema()).unwrap();
        let ops = u.ops;

        assert!(ops.contains(&Op::Record { key: "preferred_name".into(), value: json!("Tarek") }));
        assert!(!ops.iter().any(|o| matches!(o, Op::Record { key, .. } if key == "does_not_exist")));

        assert!(ops.contains(&Op::Skip { key: "weight_kg".into() }));
        assert!(!ops.iter().any(|o| matches!(o, Op::Skip { key } if key == "not_a_key")));

        assert!(ops.contains(&Op::MarkGroupComplete { group_id: "name".into() }));
        assert!(!ops.iter().any(|o| matches!(o, Op::MarkGroupComplete { group_id } if group_id == "bogus_group")));
    }

    #[test]
    fn parse_drops_null_and_empty_record_values() {
        let raw = r#"{
            "records": [
                {"key": "preferred_name", "value": null},
                {"key": "full_name",      "value": "  "},
                {"key": "pronouns",       "value": "he/him"}
            ]
        }"#;
        let u = parse_extractor_response(raw, &schema()).unwrap();
        assert_eq!(u.ops.len(), 1);
        assert!(matches!(&u.ops[0], Op::Record { key, .. } if key == "pronouns"));
    }

    #[test]
    fn parse_handles_finalize_flag() {
        let raw = r#"{"records":[],"skips":[],"completed_groups":[],"finalize":true}"#;
        let u = parse_extractor_response(raw, &schema()).unwrap();
        assert!(u.ops.contains(&Op::Finalize));
    }

    #[test]
    fn parse_tolerates_code_fence_wrapping() {
        let raw = "```json\n{\"records\":[{\"key\":\"timezone\",\"value\":\"Australia/Melbourne\"}],\"skips\":[],\"completed_groups\":[],\"finalize\":false}\n```";
        let u = parse_extractor_response(raw, &schema()).unwrap();
        assert_eq!(u.ops.len(), 1);
    }

    #[test]
    fn parse_tolerates_extra_prose() {
        let raw = "Sure! Here is the extracted data:\n{\"records\":[{\"key\":\"preferred_name\",\"value\":\"Alex\"}]}\nLet me know if you need anything else.";
        let u = parse_extractor_response(raw, &schema()).unwrap();
        assert_eq!(u.ops.len(), 1);
    }

    #[test]
    fn parse_errors_on_no_object() {
        let err = parse_extractor_response("I have nothing to extract", &schema()).unwrap_err();
        assert!(err.to_string().contains("no JSON object"));
    }

    #[test]
    fn build_system_prompt_mentions_every_group_and_question() {
        let progress = json!({});
        let p = build_extractor_system_prompt(&schema(), &progress);
        for g in &schema().groups {
            assert!(p.contains(&g.id), "system prompt missing group id '{}'", g.id);
            for q in &g.questions {
                assert!(p.contains(&q.key), "system prompt missing key '{}'", q.key);
            }
        }
    }

    #[test]
    fn build_system_prompt_excludes_already_answered_keys_and_done_groups() {
        // The prompt slices the schema: already-answered/skipped keys and
        // already-complete groups are *omitted* rather than listed in a
        // "do not repeat" section. Reasoning models spiral on exclusion
        // lists — omission is cheaper and less error-prone.
        let progress = json!({
            "answered_keys": ["preferred_name", "timezone"],
            "skipped_keys":  ["weight_kg"],
            "completed_groups": ["name"],
        });
        let p = build_extractor_system_prompt(&schema(), &progress);
        // Questions are rendered as `key: type (group=...)` — anchor on
        // `\nkey:` to avoid collisions with substrings in the example JSON
        // or in longer key names.
        assert!(!p.contains("\npreferred_name:"), "already-answered key should not be listed as outstanding");
        assert!(!p.contains("\ntimezone:"),       "already-answered key should not be listed as outstanding");
        assert!(!p.contains("\nweight_kg:"),      "already-skipped key should not be listed as outstanding");
        // Other keys in the completed `name` group must also be omitted.
        assert!(!p.contains("\nfull_name:"), "key in done group should not appear");
        assert!(!p.contains("\npronouns:"),  "key in done group should not appear");
        // Remaining questions (those still outstanding) should still be
        // present so the model has something to extract.
        assert!(p.contains("\ncontact_hours:"), "outstanding key should appear");
    }

    #[test]
    fn build_system_prompt_uses_flat_key_listing() {
        // Questions must be presented flat (`key: type (group=...)`), never
        // nested under group headers. Reasoning-distilled models mirror the
        // visual hierarchy of the prompt into their output — a nested schema
        // produces nested records with the group id as key.
        let p = build_extractor_system_prompt(&schema(), &json!({}));
        for g in &schema().groups {
            // The flat renderer uses the form `(group=<id>)`; it never emits
            // a bare `\n<group_id>:` header line.
            let header = format!("\n{}:\n", g.id);
            assert!(!p.contains(&header),
                "prompt must not contain nested group header '{}'", g.id);
            let group_tag = format!("(group={}", g.id);
            assert!(p.contains(&group_tag),
                "every outstanding group should appear as metadata tag '{}'", group_tag);
        }
    }

    #[test]
    fn parse_flattens_nested_group_record() {
        // Reasoning-distilled models sometimes confuse group ids with
        // question keys and emit `{"key":"work_hobbies","value":{"work_summary":"…","hobbies":"…"}}`.
        // The parser must fan that out into per-sub-key records rather than
        // silently dropping the whole thing.
        let raw = r#"{
            "records": [
                {"key": "work_hobbies", "value": {
                    "work_summary": "software engineer at a fintech",
                    "hobbies":      "photography, trail running"
                }}
            ],
            "skips": [], "completed_groups": [], "finalize": false
        }"#;
        let u = parse_extractor_response(raw, &schema()).unwrap();
        assert_eq!(u.ops.len(), 2, "nested record should fan out to two flat records, got {:?}", u.ops);
        assert!(u.ops.contains(&Op::Record {
            key: "work_summary".into(),
            value: json!("software engineer at a fintech"),
        }));
        assert!(u.ops.contains(&Op::Record {
            key: "hobbies".into(),
            value: json!("photography, trail running"),
        }));
    }

    #[test]
    fn parse_nested_record_drops_unknown_sub_keys() {
        let raw = r#"{
            "records": [
                {"key": "name", "value": {
                    "preferred_name": "Alex",
                    "bogus_sub_key":  "x"
                }}
            ]
        }"#;
        let u = parse_extractor_response(raw, &schema()).unwrap();
        assert_eq!(u.ops.len(), 1);
        assert!(matches!(&u.ops[0], Op::Record { key, .. } if key == "preferred_name"));
    }

    // ── End-to-end: transcript → extractor → apply_ops → progress ─────────

    use async_trait::async_trait;
    use std::sync::Mutex;

    use crate::auth::{LocalAuthService, NewUser, Role};
    use crate::history::HistoryStore;
    use crate::memory::MemorySystem;
    use crate::providers::ModelProvider;
    use crate::tools::ToolRegistry;
    use crate::tools::onboarding::{
        CompleteOnboardingTool, MarkGroupCompleteTool, OnboardingServices,
        RecordProfileTool, SkipTopicTool,
    };
    use crate::types::{GenerationResponse, ProviderId, TokenUsage};

    /// Mock provider that returns a canned JSON response. Used to simulate
    /// what a well-behaved extractor model would emit, so we can prove the
    /// apply_ops wiring writes to the DB end-to-end.
    struct CannedProvider(Mutex<Vec<String>>);

    impl CannedProvider {
        fn new(responses: Vec<String>) -> Self {
            Self(Mutex::new(responses))
        }
    }

    #[async_trait]
    impl ModelProvider for CannedProvider {
        fn name(&self) -> &str { "canned" }
        async fn generate(
            &self,
            _msgs: &[ChatMessage],
            _opts: &GenerationOptions,
        ) -> Result<GenerationResponse, MiraError> {
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

    async fn setup_end_to_end() -> (tempfile::TempDir, String, Arc<ToolRegistry>, Arc<LocalAuthService>) {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();
        let auth = Arc::new(LocalAuthService::new(
            &data_dir.join("auth.db"), "test-secret".into(), 7,
        ).unwrap());
        let user = auth.create_user(NewUser {
            username: "tester".into(), display_name: None, email: None,
            password: "hunter22".into(), role: Role::User,
        }).unwrap();
        let history = Arc::new(HistoryStore::open(&data_dir.join("history.db")).unwrap());
        let memory  = Arc::new(MemorySystem::new_keyword_only(data_dir.join("memory.db")).unwrap());
        let schema  = Arc::new(OnboardingSchema::bundled().unwrap());

        let services = Arc::new(OnboardingServices {
            auth:     Arc::clone(&auth),
            history,
            memory,
            schema,
            data_dir,
            wiki:     None,
        });
        let mut registry = ToolRegistry::new();
        registry.register(RecordProfileTool::new(Arc::clone(&services)));
        registry.register(SkipTopicTool::new(Arc::clone(&services)));
        registry.register(MarkGroupCompleteTool::new(Arc::clone(&services)));
        registry.register(CompleteOnboardingTool::new(Arc::clone(&services)));

        (dir, user.id, Arc::new(registry), auth)
    }

    #[tokio::test]
    async fn end_to_end_narrated_turn_captures_answers_via_extractor() {
        // Simulates the exact failure mode from production: the "assistant"
        // message is pure narration ("I've noted that") with no tool calls.
        // The extractor provider returns structured JSON for what the user
        // said, apply_ops dispatches through the real tool registry, and
        // the DB ends up correctly populated.
        let (_dir, uid, tools, auth) = setup_end_to_end().await;

        let transcript = vec![
            ChatMessage::system("you are MIRA"),
            ChatMessage::user("Hi, I'm Tarek El Diab, you can call me Tarek. Pronouns he/him."),
            ChatMessage::assistant("Nice to meet you, Tarek! I've noted your details."),
        ];

        // Canned extractor response matching what a well-behaved model would
        // emit given the above transcript.
        let provider: Arc<dyn ModelProvider> = Arc::new(CannedProvider::new(vec![
            json!({
                "records": [
                    {"key": "full_name",      "value": "Tarek El Diab"},
                    {"key": "preferred_name", "value": "Tarek"},
                    {"key": "pronouns",       "value": "he/him"}
                ],
                "skips": [],
                "completed_groups": ["name"],
                "finalize": false
            }).to_string(),
        ]));

        let schema = OnboardingSchema::bundled().unwrap();
        let progress = json!({});

        let updates = extract_updates_from_transcript(&provider, &schema, &transcript, &progress).await;
        assert_eq!(updates.ops.len(), 4, "expected 3 records + 1 group complete, got {:?}", updates.ops);

        apply_ops(&tools, &uid, "conv-1", &updates.ops).await;

        let p = auth.get_profile(&uid).unwrap().unwrap();
        assert_eq!(p.full_name.as_deref(),      Some("Tarek El Diab"));
        assert_eq!(p.preferred_name.as_deref(), Some("Tarek"));
        assert_eq!(p.pronouns.as_deref(),       Some("he/him"));

        let progress: Value = serde_json::from_str(p.onboarding_progress.as_deref().unwrap()).unwrap();
        let completed: Vec<&str> = progress["completed_groups"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(completed.contains(&"name"));
    }

    #[tokio::test]
    async fn end_to_end_extractor_is_idempotent_on_repeat() {
        // Running the extractor twice with the same transcript must not
        // corrupt state or trigger duplicate side effects.
        let (_dir, uid, tools, auth) = setup_end_to_end().await;

        let transcript = vec![
            ChatMessage::system("..."),
            ChatMessage::user("I'm Alex."),
            ChatMessage::assistant("Hi Alex!"),
        ];
        let canned = json!({
            "records": [{"key": "preferred_name", "value": "Alex"}],
            "skips": [], "completed_groups": [], "finalize": false
        }).to_string();
        let provider: Arc<dyn ModelProvider> = Arc::new(CannedProvider::new(vec![canned.clone(), canned]));

        let schema = OnboardingSchema::bundled().unwrap();

        for _ in 0..2 {
            let progress = auth.get_profile(&uid).unwrap()
                .and_then(|p| p.onboarding_progress)
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(json!({}));
            let u = extract_updates_from_transcript(&provider, &schema, &transcript, &progress).await;
            apply_ops(&tools, &uid, "conv-1", &u.ops).await;
        }

        let p = auth.get_profile(&uid).unwrap().unwrap();
        assert_eq!(p.preferred_name.as_deref(), Some("Alex"));
    }

    #[tokio::test]
    async fn end_to_end_extractor_finalizes_full_coverage() {
        // Full-coverage case: every required group has answered or skipped
        // keys. Extractor emits `finalize: true`. complete_onboarding's
        // guard accepts → onboarded_at gets stamped.
        let (_dir, uid, tools, auth) = setup_end_to_end().await;

        let canned = json!({
            "records": [
                {"key": "preferred_name",     "value": "Alex"},
                {"key": "timezone",           "value": "Australia/Melbourne"},
                {"key": "work_summary",       "value": "software engineer"},
                {"key": "top_goals",          "value": "- ship onboarding"},
                {"key": "verbosity",          "value": "brief"},
                {"key": "autonomy_preference","value": "ask_first"}
            ],
            "skips": ["off_limits_topics"],
            "completed_groups": [],
            "finalize": true
        }).to_string();
        let provider: Arc<dyn ModelProvider> = Arc::new(CannedProvider::new(vec![canned]));

        let transcript = vec![
            ChatMessage::system("..."),
            ChatMessage::user("Alex, Melbourne, engineer, ship onboarding, brief, ask first, nothing off-limits. We're done."),
            ChatMessage::assistant("Great — all set."),
        ];

        let schema = OnboardingSchema::bundled().unwrap();
        let updates = extract_updates_from_transcript(&provider, &schema, &transcript, &json!({})).await;
        apply_ops(&tools, &uid, "conv-1", &updates.ops).await;

        let p = auth.get_profile(&uid).unwrap().unwrap();
        assert!(p.onboarded_at.is_some(),
            "expected extractor finalize + guard pass to stamp onboarded_at");
    }

    #[tokio::test]
    async fn end_to_end_extractor_finalize_soft_fails_on_incomplete_coverage() {
        // Extractor claims `finalize: true` but several required groups
        // have no activity. complete_onboarding's guard must still refuse;
        // the soft failure is logged but doesn't abort the other ops.
        let (_dir, uid, tools, auth) = setup_end_to_end().await;

        let canned = json!({
            "records": [{"key": "preferred_name", "value": "Alex"}],
            "skips": [], "completed_groups": [], "finalize": true
        }).to_string();
        let provider: Arc<dyn ModelProvider> = Arc::new(CannedProvider::new(vec![canned]));

        let transcript = vec![
            ChatMessage::system("..."),
            ChatMessage::user("Alex"),
            ChatMessage::assistant("Got it."),
        ];

        let schema = OnboardingSchema::bundled().unwrap();
        let updates = extract_updates_from_transcript(&provider, &schema, &transcript, &json!({})).await;
        apply_ops(&tools, &uid, "conv-1", &updates.ops).await;

        let p = auth.get_profile(&uid).unwrap().unwrap();
        assert_eq!(p.preferred_name.as_deref(), Some("Alex"));
        assert!(p.onboarded_at.is_none(),
            "guard must refuse finalize when required groups untouched");
    }

    #[test]
    fn user_prompt_strips_system_and_tool_messages() {
        let msgs = vec![
            ChatMessage::system("you are MIRA"),
            ChatMessage::user("hi I'm Alex"),
            ChatMessage::assistant("nice to meet you Alex"),
        ];
        let p = build_extractor_user_prompt(&msgs);
        assert!( p.contains("hi I'm Alex"));
        assert!( p.contains("nice to meet you Alex"));
        assert!(!p.contains("you are MIRA"));
    }
}
