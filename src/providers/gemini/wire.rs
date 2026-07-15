// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/gemini/wire.rs

//! Wire types for Gemini's `:generateContent` / `:streamGenerateContent`
//! endpoints, plus conversions to/from MIRA's `ChatMessage` /
//! `ToolCall` / `ToolSpec` shapes.
//!
//! Keeping the wire types here lets the client logic in `client.rs` stay
//! focused on HTTP and SSE handling, and makes the conversions easy to
//! unit-test in isolation.

use serde::{Deserialize, Serialize};

use crate::types::{ChatMessage, MessageRole, ToolCall, ToolSpec};

// ─────────────────────────────────────────────────────────────────────────────
// Request types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GenerateContentRequest {
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<SystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDeclaration>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<ToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Content {
    /// `"user"` or `"model"`. There is no `system` role here — see
    /// `system_instruction` at the top level instead.
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct SystemInstruction {
    pub parts: Vec<Part>,
}

/// A part is exactly one of `text`, `functionCall`, or
/// `functionResponse` (we don't yet emit images or inlineData).
/// Modelled as a struct with all-optional fields so a deserialised
/// Part is always parseable regardless of which kind it is — Gemini's
/// wire format doesn't tag the variant.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct Part {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_response: Option<FunctionResponse>,
    /// Q1.3 — inline base64 image. Gemini's shape is
    /// `inlineData: { mimeType, data }`. Skip-serialise when None so
    /// text-only turns produce the same wire as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<InlineData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct InlineData {
    pub mime_type: String,
    /// Standard base64 (Gemini wants no `data:` prefix and no
    /// url-safe alphabet — same RFC 4648 form we already store).
    pub data:      String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct FunctionResponse {
    pub name: String,
    /// Gemini expects this as an object — wrap arbitrary string results
    /// in `{"result": "..."}` so the schema is well-formed.
    pub response: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolDeclaration {
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Serialize)]
pub(super) struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    /// Gemini accepts an OpenAPI-Schema-like object here. JSON Schema
    /// is mostly compatible — common limitations: no `$ref`, no
    /// `additionalProperties` in some versions, no `oneOf` (use
    /// `anyOf`). We pass through unchanged; the model errors back if
    /// the schema isn't supported.
    pub parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolConfig {
    pub function_calling_config: FunctionCallingConfig,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FunctionCallingConfig {
    /// `"AUTO"` (default, model decides), `"ANY"` (force at least one
    /// function call, optionally narrowed by `allowed_function_names`),
    /// or `"NONE"` (forbid tool calls).
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_function_names: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Response types
// ─────────────────────────────────────────────────────────────────────────────

/// One `data:` frame in the streaming case carries one of these, with
/// possibly-partial `candidates[].content.parts` content. The
/// non-streaming endpoint also returns the same shape.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub usage_metadata: Option<UsageMetadata>,
    /// Surfaced when the request itself fails (rare; usually errors
    /// come back as a non-200 HTTP response).
    #[serde(default)]
    #[allow(dead_code)]
    pub prompt_feedback: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct Candidate {
    #[serde(default)]
    pub content: Option<Content>,
    #[serde(default)]
    #[allow(dead_code)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub index: Option<u32>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    #[serde(default)]
    #[allow(dead_code)]
    pub total_token_count: u32,
    /// Prompt tokens served from Gemini's implicit context cache (Gemini 2.5
    /// caches automatically and reports the hit here). Folded into
    /// `TokenUsage.cache_read_tokens` so the per-turn accounting matches the
    /// other providers (Phase-0 #3). 0 when nothing hit / model doesn't cache.
    #[serde(default)]
    pub cached_content_token_count: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// MIRA -> Gemini conversions
// ─────────────────────────────────────────────────────────────────────────────

/// Strip system messages off the front (concatenated into one top-level
/// `systemInstruction`) and convert the rest to Gemini's contents
/// array. The conversion has to walk the messages with context (a tool
/// result needs the function name from the prior assistant's
/// tool_calls) so we use a single forward pass and remember the most
/// recent tool_call_id -> name mapping.
pub(super) fn convert_messages(
    messages: &[ChatMessage],
) -> (Option<SystemInstruction>, Vec<Content>) {
    // 1. Collect system parts.
    let mut system_text: Vec<&str> = Vec::new();
    for m in messages {
        if let MessageRole::System = m.role {
            if !m.content.is_empty() {
                system_text.push(&m.content);
            }
        }
    }
    let system = if system_text.is_empty() {
        None
    } else {
        Some(SystemInstruction {
            parts: vec![Part { text: Some(system_text.join("\n\n")), ..Default::default() }],
        })
    };

    // 2. Build a tool_call_id -> function_name index by walking
    //    assistant messages' tool_calls. Gemini's functionResponse
    //    parts identify their call by function name, not call_id, so
    //    when we encounter a Tool message we need to resolve which
    //    name it answers. The map covers every assistant tool_call
    //    seen so far in the conversation.
    let mut id_to_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in messages {
        if let MessageRole::Assistant = m.role {
            if let Some(calls) = &m.tool_calls {
                for c in calls {
                    id_to_name.insert(c.call_id.clone(), c.name.clone());
                }
            }
        }
    }

    // 3. Convert the remaining messages.
    let mut out: Vec<Content> = Vec::new();
    for m in messages {
        match m.role {
            MessageRole::System => continue, // already lifted
            MessageRole::User => {
                let has_attachments = m.attachments.as_ref().is_some_and(|a| !a.is_empty());
                if m.content.is_empty() && !has_attachments { continue; }
                let mut parts: Vec<Part> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(Part { text: Some(m.content.clone()), ..Default::default() });
                }
                // Q1.3 — append inline_data for any image attachments.
                if let Some(att) = &m.attachments {
                    for a in att {
                        if matches!(a.kind, crate::types::AttachmentKind::Image) {
                            parts.push(Part {
                                inline_data: Some(InlineData {
                                    mime_type: a.mime_type.clone(),
                                    data:      a.data_b64.clone(),
                                }),
                                ..Default::default()
                            });
                        }
                    }
                }
                push_or_merge(&mut out, "user", parts);
            }
            MessageRole::Assistant => {
                let mut parts: Vec<Part> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(Part { text: Some(m.content.clone()), ..Default::default() });
                }
                if let Some(calls) = &m.tool_calls {
                    for c in calls {
                        parts.push(Part {
                            function_call: Some(FunctionCall {
                                name: c.name.clone(),
                                args: c.arguments.clone(),
                            }),
                            ..Default::default()
                        });
                    }
                }
                if parts.is_empty() { continue; }
                push_or_merge(&mut out, "model", parts);
            }
            MessageRole::Tool => {
                let id = m.tool_call_id.clone().unwrap_or_default();
                // Look up the function name from the conversation
                // history. Fall back to using the call_id itself as
                // the name — Gemini will likely error, but at least
                // the request is well-formed.
                let name = id_to_name.get(&id).cloned().unwrap_or_else(|| id.clone());
                // Wrap the textual result in an object so Gemini's
                // schema (which expects `response` to be an object)
                // accepts it.
                let response = serde_json::json!({ "result": m.content });
                let parts = vec![Part {
                    function_response: Some(FunctionResponse { name, response }),
                    ..Default::default()
                }];
                push_or_merge(&mut out, "user", parts);
            }
        }
    }

    (system, out)
}

/// Merge adjacent same-role Contents. Gemini's API doesn't strictly
/// require this (it accepts repeated roles), but consolidating cleans
/// up the wire shape and matches the conversion we did for Anthropic.
fn push_or_merge(out: &mut Vec<Content>, role: &str, mut parts: Vec<Part>) {
    match out.last_mut() {
        Some(prev) if prev.role == role => prev.parts.append(&mut parts),
        _ => out.push(Content { role: role.to_string(), parts }),
    }
}

/// Translate an OpenAI-style `tools` array into Gemini's single
/// `functionDeclarations` block. Each tool's `parameters` is run through
/// [`sanitize_schema_for_gemini`] first — Gemini's function-declaration schema
/// is a strict OpenAPI 3.0 subset that hard-rejects JSON-Schema keywords MIRA
/// uses (notably `additionalProperties`, but also `$schema`, `oneOf`, etc.)
/// with `400 Unknown name "…": Cannot find field`, taking every tool down.
pub(super) fn convert_tool_specs(specs: &[ToolSpec]) -> Option<Vec<ToolDeclaration>> {
    if specs.is_empty() { return None; }
    let declarations = specs.iter().map(|s| FunctionDeclaration {
        name:        s.function.name.clone(),
        description: s.function.description.clone(),
        parameters:  sanitize_schema_for_gemini(&s.function.parameters),
    }).collect();
    Some(vec![ToolDeclaration { function_declarations: declarations }])
}

/// Recursively strip JSON-Schema keywords Gemini's schema proto doesn't
/// understand, and flatten `oneOf`/`allOf` (whose branch `properties` are
/// merged up, since Gemini's subset can't express them). Recurses through
/// `properties`, `items`, and `anyOf` (which Gemini *does* support). Everything
/// not in the drop-set is passed through, so supported constraints (`enum`,
/// `format`, `minimum`, `required`, `nullable`, …) survive.
fn sanitize_schema_for_gemini(schema: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    let Some(map) = schema.as_object() else { return schema.clone() };

    // Keywords Gemini's function-declaration Schema proto rejects outright.
    // `anyOf` is technically supported for type-unions, but MIRA only uses it
    // for "at least one of" constraints whose branches are bare `{required:[…]}`
    // objects — and Gemini rejects `required` on a non-OBJECT branch. So we
    // flatten anyOf too (merge any branch properties up, drop the constraint,
    // which the tool re-validates at call time).
    const DROP: &[&str] = &[
        "additionalProperties", "$schema", "$ref", "$defs", "definitions",
        "oneOf", "allOf", "anyOf", "not", "patternProperties", "const", "contains",
        "if", "then", "else", "dependentRequired", "dependencies",
        "propertyNames", "unevaluatedProperties", "title", "default", "examples",
    ];

    // Merge oneOf/allOf/anyOf branch properties up into the object's own props.
    let mut props: Map<String, Value> = map
        .get("properties").and_then(|p| p.as_object()).cloned().unwrap_or_default();
    for combinator in ["oneOf", "allOf", "anyOf"] {
        if let Some(Value::Array(branches)) = map.get(combinator) {
            for b in branches {
                if let Some(bp) = b.get("properties").and_then(|p| p.as_object()) {
                    for (k, v) in bp { props.entry(k.clone()).or_insert_with(|| v.clone()); }
                }
            }
        }
    }

    let mut out: Map<String, Value> = Map::new();
    for (k, v) in map {
        if DROP.contains(&k.as_str()) || k == "properties" { continue; }
        match k.as_str() {
            "items" => { out.insert(k.clone(), sanitize_schema_for_gemini(v)); }
            // Gemini's schema `type` is a scalar, not a JSON-Schema union array
            // (`{"type":["string","null"]}` → 400 "Proto field is not
            // repeating"). Collapse: keep the first non-"null" member as the
            // type and, if "null" was present, mark the field nullable.
            "type" if v.is_array() => {
                let arr = v.as_array().unwrap();
                let mut nullable = false;
                let mut chosen: Option<Value> = None;
                for t in arr {
                    if t.as_str() == Some("null") { nullable = true; }
                    else if chosen.is_none() { chosen = Some(t.clone()); }
                }
                out.insert("type".into(),
                    chosen.unwrap_or_else(|| Value::String("string".into())));
                if nullable { out.insert("nullable".into(), Value::Bool(true)); }
            }
            _ => { out.insert(k.clone(), v.clone()); }
        }
    }
    if !props.is_empty() {
        let cleaned: Map<String, Value> = props
            .iter().map(|(k, v)| (k.clone(), sanitize_schema_for_gemini(v))).collect();
        out.insert("properties".into(), Value::Object(cleaned));
        out.entry("type").or_insert_with(|| Value::String("object".into()));
    }
    // Gemini: `required` is "only allowed for OBJECT type". Any schema that
    // kept a `required` list must therefore declare type object.
    if out.contains_key("required") {
        out.entry("type").or_insert_with(|| Value::String("object".into()));
    }
    Value::Object(out)
}

/// Translate OpenAI's `tool_choice` to Gemini's `functionCallingConfig`.
/// Returns `(config, omit_tools)`:
/// - `omit_tools = true` signals the caller to also drop the `tools`
///   array entirely — i.e. the OpenAI `"none"` form, which has no
///   exact analogue in Gemini (NONE mode still requires the tools
///   array; safer to omit).
pub(super) fn convert_tool_choice(
    v: &serde_json::Value,
) -> (Option<FunctionCallingConfig>, bool /* omit_tools */) {
    match v {
        serde_json::Value::String(s) => match s.as_str() {
            "auto"     => (Some(FunctionCallingConfig { mode: "AUTO", allowed_function_names: None }), false),
            "required" => (Some(FunctionCallingConfig { mode: "ANY",  allowed_function_names: None }), false),
            "none"     => (None, true),
            _          => (Some(FunctionCallingConfig { mode: "AUTO", allowed_function_names: None }), false),
        },
        serde_json::Value::Object(o) => {
            // OpenAI: {"type":"function","function":{"name":"..."}}
            if let Some(name) = o.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
            {
                return (Some(FunctionCallingConfig {
                    mode: "ANY",
                    allowed_function_names: Some(vec![name.to_string()]),
                }), false);
            }
            (Some(FunctionCallingConfig { mode: "AUTO", allowed_function_names: None }), false)
        }
        _ => (Some(FunctionCallingConfig { mode: "AUTO", allowed_function_names: None }), false),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Gemini -> MIRA conversions
// ─────────────────────────────────────────────────────────────────────────────

/// Walk a non-streaming response's candidate parts, joining text and
/// extracting functionCall parts as ToolCalls. Mirrors the Anthropic
/// equivalent — call_id is synthesised since Gemini doesn't supply one.
pub(super) fn convert_response_parts(
    parts: Vec<Part>,
) -> (String, Option<Vec<ToolCall>>) {
    let mut text  = String::new();
    let mut calls = Vec::new();
    for p in parts {
        if let Some(t) = p.text {
            text.push_str(&t);
        }
        if let Some(fc) = p.function_call {
            calls.push(ToolCall {
                name:      fc.name,
                arguments: fc.args,
                // Gemini doesn't expose a call_id. Synthesise one so
                // downstream tool_call_id round-trips through MIRA's
                // OpenAI-style ChatMessage; convert_messages above
                // looks up the name from the assistant message's
                // tool_calls, so the synthetic id is only used to
                // distinguish concurrent calls within one turn.
                call_id:   uuid::Uuid::new_v4().to_string(),
            });
        }
        // function_response parts in a model output would be a bug —
        // ignore.
    }
    let calls_opt = if calls.is_empty() { None } else { Some(calls) };
    (text, calls_opt)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(s: &str)      -> ChatMessage { ChatMessage::user(s) }
    fn assistant(s: &str) -> ChatMessage { ChatMessage::assistant(s) }

    #[test]
    fn gemini_sanitize_drops_additionalproperties_recursively() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" },
                "nested": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "x": { "type": "integer" } }
                }
            },
            "required": ["name"]
        });
        let out = sanitize_schema_for_gemini(&schema);
        assert!(out.get("additionalProperties").is_none(), "top-level dropped");
        assert!(out["properties"]["nested"].get("additionalProperties").is_none(), "nested dropped");
        // Supported keys survive.
        assert_eq!(out["type"], "object");
        assert_eq!(out["required"], json!(["name"]));
        assert!(out["properties"]["name"].get("type").is_some());
        assert!(out["properties"]["nested"]["properties"]["x"].get("type").is_some());
    }

    #[test]
    fn gemini_sanitize_flattens_all_combinators_and_keeps_enum() {
        // The real companion_briefing_set shape: top-level anyOf whose branches
        // are bare {required:[…]} — Gemini rejects `required` on a non-object
        // branch, so anyOf/oneOf/allOf are all flattened away.
        let schema = json!({
            "type": "object",
            "properties": {
                "enabled": { "type": "boolean" },
                "mode": { "type": "string", "enum": ["a", "b"] }
            },
            "oneOf": [ { "properties": { "hour": { "type": "integer" } } } ],
            "anyOf": [ { "required": ["enabled"] }, { "required": ["mode"] } ]
        });
        let out = sanitize_schema_for_gemini(&schema);
        assert!(out.get("oneOf").is_none(), "oneOf flattened");
        assert!(out.get("anyOf").is_none(), "anyOf flattened (constraint re-validated at call time)");
        assert!(out["properties"].get("hour").is_some(), "oneOf branch property merged up");
        assert_eq!(out["properties"]["mode"]["enum"], json!(["a", "b"]), "enum survives");
        assert_eq!(out["type"], "object");
    }

    #[test]
    fn gemini_sanitize_adds_object_type_when_required_present() {
        let schema = json!({ "required": ["x"], "properties": { "x": { "type": "string" } } });
        let out = sanitize_schema_for_gemini(&schema);
        assert_eq!(out["type"], "object", "required implies OBJECT type for Gemini");
    }

    #[test]
    fn gemini_sanitize_collapses_type_arrays() {
        // ["string","integer"] → "string"; ["string","null"] → string + nullable.
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "type": ["string", "integer"] },
                "b": { "type": ["string", "null"] }
            }
        });
        let out = sanitize_schema_for_gemini(&schema);
        assert_eq!(out["properties"]["a"]["type"], "string");
        assert!(out["properties"]["a"].get("nullable").is_none());
        assert_eq!(out["properties"]["b"]["type"], "string");
        assert_eq!(out["properties"]["b"]["nullable"], true);
    }

    #[test]
    fn gemini_sanitize_cleans_array_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tags": { "type": "array", "items": { "type": "string", "additionalProperties": false } }
            }
        });
        let out = sanitize_schema_for_gemini(&schema);
        assert!(out["properties"]["tags"]["items"].get("additionalProperties").is_none());
        assert_eq!(out["properties"]["tags"]["items"]["type"], "string");
    }

    #[test]
    fn system_messages_become_system_instruction() {
        let msgs = vec![
            ChatMessage::system("You are MIRA."),
            user("hi"),
        ];
        let (system, contents) = convert_messages(&msgs);
        let sys = system.expect("expected systemInstruction");
        assert_eq!(sys.parts.len(), 1);
        assert_eq!(sys.parts[0].text.as_deref(), Some("You are MIRA."));
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
    }

    #[test]
    fn assistant_role_maps_to_model() {
        let msgs = vec![user("hi"), assistant("hello")];
        let (_, contents) = convert_messages(&msgs);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[1].role, "model",
            "Gemini's role is 'model', not 'assistant'");
    }

    #[test]
    fn adjacent_user_messages_merge_parts() {
        let msgs = vec![user("hello"), user("again")];
        let (_, contents) = convert_messages(&msgs);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].parts.len(), 2);
    }

    #[test]
    fn assistant_with_tool_calls_produces_text_plus_functioncall_parts() {
        let mut a = assistant("Let me check.");
        a.tool_calls = Some(vec![ToolCall {
            name:      "get_weather".into(),
            arguments: json!({"city": "Paris"}),
            call_id:   "abc".into(),
        }]);
        let (_, contents) = convert_messages(&[a]);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "model");
        assert_eq!(contents[0].parts.len(), 2);
        assert_eq!(contents[0].parts[0].text.as_deref(), Some("Let me check."));
        let fc = contents[0].parts[1].function_call.as_ref().expect("expected functionCall");
        assert_eq!(fc.name, "get_weather");
        assert_eq!(fc.args, json!({"city": "Paris"}));
    }

    #[test]
    fn tool_response_uses_function_name_from_preceding_assistant() {
        let mut a = assistant("Checking…");
        a.tool_calls = Some(vec![ToolCall {
            name:      "get_weather".into(),
            arguments: json!({}),
            call_id:   "abc".into(),
        }]);
        let tool = ChatMessage::tool("72°F", "abc");
        let msgs = vec![user("Paris weather?"), a, tool];
        let (_, contents) = convert_messages(&msgs);
        // user -> model -> user (functionResponse)
        assert_eq!(contents.len(), 3);
        let last = &contents[2];
        assert_eq!(last.role, "user");
        let fr = last.parts[0].function_response.as_ref().expect("expected functionResponse");
        assert_eq!(fr.name, "get_weather",
            "function name must be looked up from the assistant's tool_calls");
        assert_eq!(fr.response, json!({"result": "72°F"}));
    }

    #[test]
    fn unknown_tool_call_id_falls_back_to_using_id_as_name() {
        let tool = ChatMessage::tool("result", "stray-id");
        let (_, contents) = convert_messages(&[tool]);
        let fr = contents[0].parts[0].function_response.as_ref().unwrap();
        assert_eq!(fr.name, "stray-id",
            "fallback uses the call_id when no assistant call matched");
    }

    #[test]
    fn empty_user_message_is_skipped() {
        let msgs = vec![user(""), assistant("hi")];
        let (_, contents) = convert_messages(&msgs);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "model");
    }

    #[test]
    fn tool_specs_become_one_declarations_block() {
        let spec = ToolSpec::function(
            "get_weather",
            "weather",
            json!({"type": "object", "properties": {}}),
        );
        let tools = convert_tool_specs(&[spec]).expect("expected tools");
        assert_eq!(tools.len(), 1, "Gemini bundles all functions in one tool");
        assert_eq!(tools[0].function_declarations.len(), 1);
        assert_eq!(tools[0].function_declarations[0].name, "get_weather");
    }

    #[test]
    fn empty_tool_specs_returns_none() {
        assert!(convert_tool_specs(&[]).is_none());
    }

    #[test]
    fn tool_choice_auto_required_none_map_correctly() {
        let (cfg, omit) = convert_tool_choice(&json!("auto"));
        assert!(matches!(cfg, Some(FunctionCallingConfig { mode: "AUTO", .. })));
        assert!(!omit);

        let (cfg, omit) = convert_tool_choice(&json!("required"));
        assert!(matches!(cfg, Some(FunctionCallingConfig { mode: "ANY", .. })));
        assert!(!omit);

        let (cfg, omit) = convert_tool_choice(&json!("none"));
        assert!(cfg.is_none());
        assert!(omit, "'none' must omit the tools array entirely");
    }

    #[test]
    fn tool_choice_named_function_maps_to_any_with_allowlist() {
        let v = json!({"type": "function", "function": {"name": "search"}});
        let (cfg, omit) = convert_tool_choice(&v);
        assert!(!omit);
        let cfg = cfg.expect("expected config");
        assert_eq!(cfg.mode, "ANY");
        assert_eq!(cfg.allowed_function_names.as_deref(), Some(&["search".to_string()][..]));
    }

    #[test]
    fn response_text_parts_concatenate() {
        let parts = vec![
            Part { text: Some("Hello ".into()), ..Default::default() },
            Part { text: Some("world".into()),  ..Default::default() },
        ];
        let (text, calls) = convert_response_parts(parts);
        assert_eq!(text, "Hello world");
        assert!(calls.is_none());
    }

    #[test]
    fn response_function_call_becomes_tool_call() {
        let parts = vec![
            Part {
                function_call: Some(FunctionCall {
                    name: "search".into(),
                    args: json!({"query": "rust"}),
                }),
                ..Default::default()
            },
        ];
        let (text, calls) = convert_response_parts(parts);
        assert!(text.is_empty());
        let calls = calls.expect("expected tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments, json!({"query": "rust"}));
        // Gemini doesn't provide a call_id; the conversion synthesises one.
        assert!(!calls[0].call_id.is_empty());
    }
}
