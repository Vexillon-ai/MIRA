// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/anthropic/wire.rs

//! Wire types for Anthropic's `/v1/messages` API + conversions to/from
//! MIRA's internal `ChatMessage` / `ToolCall` / `ToolSpec` shapes.
//!
//! Keeping the wire types in their own module lets the client logic in
//! `client.rs` stay focused on HTTP + streaming, and makes the conversion
//! helpers easy to unit-test in isolation.

use serde::{Deserialize, Serialize};

use crate::types::{ChatMessage, MessageRole, ToolCall, ToolSpec};

// ─────────────────────────────────────────────────────────────────────────────
// Request types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct MessagesRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    pub messages: Vec<OutboundMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OutboundToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<OutboundToolChoice>,
    /// Extended thinking (roadmap #13). When set, Anthropic spends up to
    /// `budget_tokens` on internal reasoning before answering. Requires
    /// `max_tokens > budget_tokens` and that `temperature` is unset (the
    /// builder enforces both).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
}

/// Top-level `system` field. Anthropic accepts either a bare string or an
/// array of content blocks; the block form is required to attach a
/// `cache_control` breakpoint. We serialize as a plain string in the common
/// (no-cache) case so the wire shape is unchanged, and as a one-block array
/// when prompt caching is on.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

impl SystemPrompt {
    /// The system text regardless of variant — handy for tests/logging.
    #[cfg(test)]
    pub(super) fn text(&self) -> &str {
        match self {
            SystemPrompt::Text(s) => s,
            SystemPrompt::Blocks(b) => b.first().map(|x| x.text.as_str()).unwrap_or(""),
        }
    }
}

/// One `system` content block. Only `text` blocks are used; a trailing
/// `cache_control` marks the end of the cacheable prefix (tools + system).
#[derive(Debug, Serialize)]
pub(super) struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "text"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Anthropic cache breakpoint. `{"type":"ephemeral"}` = the default ~5-minute
/// TTL cache. A breakpoint on the last system block caches everything before
/// it (tools → system), so identical prefixes on later turns are read from
/// cache at ~10% of the input cost.
#[derive(Debug, Serialize)]
pub(super) struct CacheControl {
    #[serde(rename = "type")]
    pub kind: &'static str, // "ephemeral"
}

/// Anthropic `thinking` request block. `kind` is always `"enabled"`.
#[derive(Debug, Serialize)]
pub(super) struct Thinking {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub budget_tokens: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(super) struct OutboundMessage {
    pub role: &'static str, // "user" | "assistant"
    pub content: Vec<OutboundContentBlock>,
}

/// Outbound content blocks. The shape is symmetric across user and
/// assistant messages, but only certain block types are valid in each:
/// user messages carry `text` / `tool_result`; assistant messages carry
/// `text` / `tool_use`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum OutboundContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
    /// Q1.3 — inline base64 image. Anthropic shape:
    /// `{ type: "image", source: { type: "base64", media_type, data } }`.
    Image {
        source: ImageSource,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(super) struct ImageSource {
    #[serde(rename = "type")]
    pub kind:       &'static str, // always "base64" for now
    pub media_type: String,
    pub data:       String,
}

#[derive(Debug, Serialize)]
pub(super) struct OutboundToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Anthropic's `tool_choice` is a tagged enum: `{type:"auto"}`,
/// `{type:"any"}`, or `{type:"tool", name:"..."}`. None of those map
/// exactly to OpenAI's `"none"`; for `"none"` we just omit the tools
/// list entirely upstream of this enum.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum OutboundToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

// ─────────────────────────────────────────────────────────────────────────────
// Non-streaming response
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct MessagesResponse {
    #[allow(dead_code)]
    pub id: String,
    pub content: Vec<InboundContentBlock>,
    #[allow(dead_code)]
    #[serde(default)]
    pub stop_reason: Option<String>,
    pub usage: ResponseUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum InboundContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Extended-thinking output (Claude 3.7+ / Opus 4 when the request
    /// opted in). Surfaced via `GenerationResponse.reasoning` so it
    /// doesn't pollute the visible answer text in the UI.
    Thinking {
        thinking: String,
    },
}

#[derive(Debug, Deserialize, Default)]
pub(super) struct ResponseUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    /// Prompt-cache accounting (Phase 0). Present only when prompt caching is
    /// in play; absent → 0.
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Streaming SSE events
// ─────────────────────────────────────────────────────────────────────────────

/// Top-level streaming event. Anthropic emits these as `event:` /
/// `data:` SSE frames; the `data:` payload is one of the variants
/// below tagged by `type`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum StreamEvent {
    MessageStart {
        #[allow(dead_code)]
        message: serde_json::Value,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockHeader,
    },
    ContentBlockDelta {
        index: usize,
        delta: StreamDelta,
    },
    ContentBlockStop {
        #[allow(dead_code)]
        index: usize,
    },
    MessageDelta {
        #[allow(dead_code)]
        #[serde(default)]
        delta: serde_json::Value,
        #[serde(default)]
        usage: Option<ResponseUsage>,
    },
    MessageStop,
    /// `ping` and other auxiliary events Anthropic emits — we don't act
    /// on them. Catch-all via `#[serde(other)]` would be cleaner but
    /// requires unit variants; an explicit name is fine since the only
    /// other event we'd see today is `ping`.
    Ping,
    /// Server-side error mid-stream (rare; e.g. quota exceeded after
    /// some tokens were already flushed).
    Error {
        #[serde(default)]
        error: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ContentBlockHeader {
    Text {
        #[allow(dead_code)]
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[allow(dead_code)]
        #[serde(default)]
        input: serde_json::Value,
    },
    /// Same as the non-streaming Thinking variant; silently consumed.
    Thinking {
        #[allow(dead_code)]
        #[serde(default)]
        thinking: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum StreamDelta {
    TextDelta {
        text: String,
    },
    /// Tool-use input arrives chunk-by-chunk; the client accumulates
    /// these into one buffer per `index` until `content_block_stop`
    /// closes the block, then parses the buffer as JSON.
    InputJsonDelta {
        partial_json: String,
    },
    /// Extended-thinking incremental output. Accumulated into
    /// `GenerationResponse.reasoning` so the agent detail page can
    /// render it separately from the visible answer.
    ThinkingDelta {
        #[serde(default)]
        thinking: String,
    },
    /// Other delta types Anthropic might add. Treated as no-op.
    #[serde(other)]
    Unknown,
}

// ─────────────────────────────────────────────────────────────────────────────
// Conversion: MIRA ChatMessage[] → Anthropic request
// ─────────────────────────────────────────────────────────────────────────────

/// Strip system messages off the front (concatenated into one
/// top-level `system` string) and convert the rest into Anthropic's
/// strict-alternating user/assistant blocks. Adjacent same-role
/// messages get merged into one message with multiple content blocks.
pub(super) fn convert_messages(
    messages: &[ChatMessage],
) -> (Option<String>, Vec<OutboundMessage>) {
    // 1. System content — concatenate all `MessageRole::System` entries
    //    in order. Anthropic accepts a single string here.
    let mut system_parts: Vec<&str> = Vec::new();
    let mut rest:        Vec<&ChatMessage> = Vec::new();
    for m in messages {
        match m.role {
            MessageRole::System => {
                if !m.content.is_empty() {
                    system_parts.push(&m.content);
                }
            }
            _ => rest.push(m),
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    // 2. Walk the remaining messages, converting each to its Anthropic
    //    shape, then merge adjacent same-role messages.
    let mut out: Vec<OutboundMessage> = Vec::new();
    for m in rest {
        let (role, mut blocks) = chat_message_to_blocks(m);
        match out.last_mut() {
            Some(prev) if prev.role == role => {
                prev.content.append(&mut blocks);
            }
            _ => {
                if blocks.is_empty() {
                    // Skip empty messages — Anthropic rejects requests
                    // with empty content arrays.
                    continue;
                }
                out.push(OutboundMessage { role, content: blocks });
            }
        }
    }

    (system, out)
}

fn chat_message_to_blocks(
    m: &ChatMessage,
) -> (&'static str, Vec<OutboundContentBlock>) {
    match m.role {
        MessageRole::User => {
            let mut blocks = Vec::new();
            if !m.content.is_empty() {
                blocks.push(OutboundContentBlock::Text { text: m.content.clone() });
            }
            // Q1.3 — append image blocks for any inline attachments.
            // Order matters: Anthropic recommends image-then-text or
            // text-then-image, but always with the question last so the
            // model sees the prompt after the image. Our convention:
            // text first, images appended.
            if let Some(att) = &m.attachments {
                for a in att {
                    if matches!(a.kind, crate::types::AttachmentKind::Image) {
                        blocks.push(OutboundContentBlock::Image {
                            source: ImageSource {
                                kind:       "base64",
                                media_type: a.mime_type.clone(),
                                data:       a.data_b64.clone(),
                            },
                        });
                    }
                }
            }
            ("user", blocks)
        }
        MessageRole::Assistant => {
            let mut blocks = Vec::new();
            if !m.content.is_empty() {
                blocks.push(OutboundContentBlock::Text { text: m.content.clone() });
            }
            if let Some(calls) = &m.tool_calls {
                for c in calls {
                    blocks.push(OutboundContentBlock::ToolUse {
                        id:    c.call_id.clone(),
                        name:  c.name.clone(),
                        input: c.arguments.clone(),
                    });
                }
            }
            ("assistant", blocks)
        }
        MessageRole::Tool => {
            // OpenAI's tool role doesn't exist in Anthropic. A tool
            // result is a user message containing a `tool_result`
            // content block referencing the prior assistant's
            // `tool_use.id`. The tool_call_id from the OpenAI shape
            // becomes the tool_use_id here.
            let id = m.tool_call_id.clone().unwrap_or_default();
            let block = OutboundContentBlock::ToolResult {
                tool_use_id: id,
                content:     m.content.clone(),
            };
            ("user", vec![block])
        }
        // System should have been stripped before reaching this
        // function; fall through to user as a defensive non-crash path.
        MessageRole::System => {
            ("user", vec![OutboundContentBlock::Text { text: m.content.clone() }])
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Conversion: MIRA ToolSpec[] / tool_choice → Anthropic
// ─────────────────────────────────────────────────────────────────────────────

pub(super) fn convert_tool_specs(specs: &[ToolSpec]) -> Vec<OutboundToolSpec> {
    specs.iter().map(|s| OutboundToolSpec {
        name:         s.function.name.clone(),
        description:  s.function.description.clone(),
        input_schema: sanitize_input_schema(&s.function.parameters),
    }).collect()
}

/// Anthropic rejects `oneOf` / `anyOf` / `allOf` at the top level of a tool's
/// `input_schema` (`400 invalid_request_error: input_schema does not support
/// oneOf, allOf, or anyOf at the top level`) — which makes the whole
/// tool-enabled request fail, taking every tool down with one offending schema.
/// OpenAI-shaped schemas (which MIRA authors) freely use these combinators for
/// "at least one of" constraints.
///
/// Sanitize by flattening: union any `properties` from the top level and from
/// inside the combinator branches into one `type: object` schema, then drop the
/// combinators. The dropped constraint is only mutual-exclusivity / conditional
/// `required`, which the tools re-validate at call time anyway (e.g.
/// `companion_briefing_set` returns an error if neither field is passed), so
/// nothing is lost functionally — the model still sees every parameter.
/// Non-object schemas and schemas without top-level combinators pass through
/// untouched.
fn sanitize_input_schema(schema: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    let Some(obj) = schema.as_object() else { return schema.clone() };
    if !obj.contains_key("oneOf") && !obj.contains_key("anyOf") && !obj.contains_key("allOf") {
        return schema.clone();
    }
    let mut out: Map<String, Value> = obj.clone();
    let mut props: Map<String, Value> = out
        .get("properties")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(Value::Array(branches)) = out.remove(key) {
            for b in branches {
                if let Some(bp) = b.get("properties").and_then(|v| v.as_object()) {
                    for (k, v) in bp {
                        props.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        }
    }
    if !props.is_empty() {
        out.insert("properties".into(), Value::Object(props));
    }
    out.entry("type").or_insert_with(|| Value::String("object".into()));
    Value::Object(out)
}

/// Translate OpenAI's `tool_choice` shape to Anthropic's `OutboundToolChoice`.
/// Returns `None` when the OpenAI value is `"none"` — Anthropic's
/// equivalent is to omit the tools array entirely, which the caller
/// handles separately.
pub(super) fn convert_tool_choice(v: &serde_json::Value) -> Option<OutboundToolChoice> {
    match v {
        serde_json::Value::String(s) => match s.as_str() {
            "auto"     => Some(OutboundToolChoice::Auto),
            "required" => Some(OutboundToolChoice::Any),
            "none"     => None,
            _          => Some(OutboundToolChoice::Auto), // unknown → safest default
        },
        serde_json::Value::Object(o) => {
            // OpenAI: {"type":"function","function":{"name":"..."}}
            if let Some(name) = o.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
            {
                return Some(OutboundToolChoice::Tool { name: name.to_string() });
            }
            Some(OutboundToolChoice::Auto)
        }
        _ => Some(OutboundToolChoice::Auto),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Conversion: Anthropic response → MIRA
// ─────────────────────────────────────────────────────────────────────────────

/// Walks the inbound content blocks, joining text into the response
/// body, collecting tool_use blocks into ToolCalls, and concatenating
/// extended-thinking content into the third return slot so callers
/// can surface it via `GenerationResponse.reasoning`. Returns
/// `(content, tool_calls, reasoning)`.
pub(super) fn convert_response_content(
    blocks: Vec<InboundContentBlock>,
) -> (String, Option<Vec<ToolCall>>, Option<String>) {
    let mut text      = String::new();
    let mut reasoning = String::new();
    let mut calls     = Vec::new();
    for b in blocks {
        match b {
            InboundContentBlock::Text { text: t } => text.push_str(&t),
            InboundContentBlock::ToolUse { id, name, input } => {
                calls.push(ToolCall {
                    name,
                    arguments: input,
                    call_id:   id,
                });
            }
            InboundContentBlock::Thinking { thinking } => {
                reasoning.push_str(&thinking);
            }
        }
    }
    let calls_opt     = if calls.is_empty()     { None } else { Some(calls) };
    let reasoning_opt = if reasoning.is_empty() { None } else { Some(reasoning) };
    (text, calls_opt, reasoning_opt)
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
    fn sanitizer_output_is_anthropic_clean_conformance() {
        use crate::tools::schema_lint::residual_for_anthropic;
        // Real shapes that 400'd Anthropic before the sanitizer. After
        // sanitizing, none may retain a top-level combinator.
        let schemas = [
            json!({"type":"object","properties":{"enabled":{"type":"boolean"}},"anyOf":[{"required":["enabled"]},{"required":["hour"]}]}),
            json!({"type":"object","oneOf":[{"properties":{"a":{"type":"string"}}}]}),
            json!({"allOf":[{"properties":{"b":{"type":"integer"}}}]}),
        ];
        for s in schemas {
            let cleaned = sanitize_input_schema(&s);
            let residual = residual_for_anthropic(&cleaned);
            assert!(residual.is_empty(), "residual {residual:?} after sanitizing {s}");
        }
    }

    #[test]
    fn sanitize_strips_top_level_anyof_and_keeps_properties() {
        // The real companion_briefing_set shape that made Anthropic 400.
        let schema = json!({
            "type": "object",
            "properties": {
                "enabled": { "type": "boolean" },
                "hour": { "type": "integer" }
            },
            "anyOf": [ { "required": ["enabled"] }, { "required": ["hour"] } ]
        });
        let out = sanitize_input_schema(&schema);
        assert!(out.get("anyOf").is_none(), "top-level anyOf must be dropped");
        assert_eq!(out["type"], "object");
        assert!(out["properties"].get("enabled").is_some());
        assert!(out["properties"].get("hour").is_some());
    }

    #[test]
    fn sanitize_merges_branch_properties_when_top_level_has_none() {
        // A schema that is purely a top-level oneOf — branch props must be
        // lifted so the model still sees the parameter names.
        let schema = json!({
            "oneOf": [
                { "properties": { "a": { "type": "string" } } },
                { "properties": { "b": { "type": "number" } } }
            ]
        });
        let out = sanitize_input_schema(&schema);
        assert!(out.get("oneOf").is_none());
        assert_eq!(out["type"], "object");
        assert!(out["properties"].get("a").is_some());
        assert!(out["properties"].get("b").is_some());
    }

    #[test]
    fn sanitize_passes_through_plain_object_schema() {
        let schema = json!({ "type": "object", "properties": { "x": { "type": "string" } } });
        assert_eq!(sanitize_input_schema(&schema), schema);
    }

    #[test]
    fn system_messages_are_lifted_to_top_level() {
        let msgs = vec![
            ChatMessage::system("You are MIRA."),
            user("hi"),
        ];
        let (system, out) = convert_messages(&msgs);
        assert_eq!(system.as_deref(), Some("You are MIRA."));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
    }

    #[test]
    fn multiple_system_messages_concatenate() {
        let msgs = vec![
            ChatMessage::system("a"),
            ChatMessage::system("b"),
            user("hi"),
        ];
        let (system, _) = convert_messages(&msgs);
        assert_eq!(system.as_deref(), Some("a\n\nb"));
    }

    #[test]
    fn empty_system_message_is_dropped() {
        let msgs = vec![ChatMessage::system(""), user("hi")];
        let (system, _) = convert_messages(&msgs);
        assert!(system.is_none());
    }

    #[test]
    fn adjacent_user_messages_merge_into_one() {
        let msgs = vec![user("hello"), user("again")];
        let (_, out) = convert_messages(&msgs);
        assert_eq!(out.len(), 1, "two user messages → one merged");
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content.len(), 2);
    }

    #[test]
    fn user_assistant_alternation_preserves_separation() {
        let msgs = vec![user("hi"), assistant("hello"), user("again")];
        let (_, out) = convert_messages(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[1].role, "assistant");
        assert_eq!(out[2].role, "user");
    }

    #[test]
    fn assistant_with_tool_calls_emits_text_plus_tool_use_blocks() {
        let mut msg = assistant("Let me check.");
        msg.tool_calls = Some(vec![ToolCall {
            name:      "get_weather".into(),
            arguments: json!({"city": "Paris"}),
            call_id:   "toolu_abc".into(),
        }]);
        let msgs = vec![msg];
        let (_, out) = convert_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content.len(), 2);
        match &out[0].content[0] {
            OutboundContentBlock::Text { text } => assert_eq!(text, "Let me check."),
            other => panic!("expected text block, got {other:?}"),
        }
        match &out[0].content[1] {
            OutboundContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_abc");
                assert_eq!(name, "get_weather");
                assert_eq!(input, &json!({"city": "Paris"}));
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn tool_role_becomes_user_message_with_tool_result_block() {
        let msgs = vec![ChatMessage::tool("72°F", "toolu_abc")];
        let (_, out) = convert_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        match &out[0].content[0] {
            OutboundContentBlock::ToolResult { tool_use_id, content } => {
                assert_eq!(tool_use_id, "toolu_abc");
                assert_eq!(content, "72°F");
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn multiple_tool_results_merge_into_one_user_message() {
        let msgs = vec![
            ChatMessage::tool("r1", "tool_a"),
            ChatMessage::tool("r2", "tool_b"),
        ];
        let (_, out) = convert_messages(&msgs);
        assert_eq!(out.len(), 1, "Anthropic requires merged user messages");
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content.len(), 2);
    }

    #[test]
    fn empty_user_message_is_skipped() {
        // Producing an empty content array on a user message would
        // trip Anthropic's request validation. The conversion drops
        // such messages so the call still goes through.
        let msgs = vec![user(""), assistant("hi")];
        let (_, out) = convert_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "assistant");
    }

    #[test]
    fn tool_spec_converts_field_names() {
        let spec = ToolSpec::function(
            "get_weather",
            "Look up weather",
            json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        );
        let out = convert_tool_specs(&[spec]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "get_weather");
        assert_eq!(out[0].description, "Look up weather");
        assert_eq!(out[0].input_schema["type"], "object");
    }

    #[test]
    fn tool_choice_string_variants_map_correctly() {
        assert!(matches!(
            convert_tool_choice(&json!("auto")),
            Some(OutboundToolChoice::Auto)
        ));
        assert!(matches!(
            convert_tool_choice(&json!("required")),
            Some(OutboundToolChoice::Any)
        ));
        assert!(convert_tool_choice(&json!("none")).is_none(),
            "'none' is signalled by omitting tools, not a choice variant");
    }

    #[test]
    fn tool_choice_function_object_maps_to_tool_name() {
        let v = json!({"type": "function", "function": {"name": "search"}});
        match convert_tool_choice(&v) {
            Some(OutboundToolChoice::Tool { name }) => assert_eq!(name, "search"),
            other => panic!("expected Tool variant, got {other:?}"),
        }
    }

    #[test]
    fn response_text_blocks_concatenate_into_content() {
        let blocks = vec![
            InboundContentBlock::Text { text: "Hello ".into() },
            InboundContentBlock::Text { text: "world".into() },
        ];
        let (text, calls, reasoning) = convert_response_content(blocks);
        assert_eq!(text, "Hello world");
        assert!(calls.is_none());
        assert!(reasoning.is_none());
    }

    #[test]
    fn response_tool_use_blocks_become_tool_calls() {
        let blocks = vec![
            InboundContentBlock::Text { text: "checking…".into() },
            InboundContentBlock::ToolUse {
                id:    "toolu_xyz".into(),
                name:  "search".into(),
                input: json!({"query": "rust"}),
            },
        ];
        let (text, calls, reasoning) = convert_response_content(blocks);
        assert_eq!(text, "checking…");
        let calls = calls.expect("expected tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "toolu_xyz");
        assert_eq!(calls[0].name,    "search");
        assert_eq!(calls[0].arguments, json!({"query": "rust"}));
        assert!(reasoning.is_none());
    }

    #[test]
    fn thinking_blocks_are_separated_from_visible_content() {
        let blocks = vec![
            InboundContentBlock::Thinking { thinking: "private chain-of-thought".into() },
            InboundContentBlock::Text     { text:     "answer".into() },
        ];
        let (text, _, reasoning) = convert_response_content(blocks);
        assert_eq!(text, "answer",
            "thinking blocks must not leak into the visible content");
        assert_eq!(reasoning.as_deref(), Some("private chain-of-thought"),
            "thinking content surfaces via the reasoning field");
    }

    #[test]
    fn thinking_blocks_concatenate_when_multiple() {
        let blocks = vec![
            InboundContentBlock::Thinking { thinking: "step 1. ".into() },
            InboundContentBlock::Thinking { thinking: "step 2.".into() },
            InboundContentBlock::Text     { text:     "answer".into() },
        ];
        let (_, _, reasoning) = convert_response_content(blocks);
        assert_eq!(reasoning.as_deref(), Some("step 1. step 2."));
    }
}
