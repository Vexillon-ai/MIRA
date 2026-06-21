// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/anthropic/client.rs

//! Anthropic `/v1/messages` HTTP client + streaming SSE parser.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, ClientBuilder};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::{debug, warn};

use crate::providers::ModelProvider;
use crate::providers::anthropic::wire::{
    ContentBlockHeader, MessagesRequest, MessagesResponse, Thinking,
    StreamDelta, StreamEvent, convert_messages, convert_response_content,
    convert_tool_choice, convert_tool_specs,
};
use crate::types::{
    ChatMessage, GenerationOptions, GenerationResponse, ProviderId, TokenUsage,
    ToolCall,
};

/// API version pinned in the `anthropic-version` header. `2023-06-01`
/// is Anthropic's most current stable date — every later feature opts
/// in via additional headers (beta flags), so this is safe long-term.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic requires `max_tokens` on every request. When the caller
/// doesn't set it on `GenerationOptions`, fall back to this. 4096 is
/// generous enough for any sensible tool-loop turn and well below the
/// per-model output caps Anthropic enforces.
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicProvider {
    http:     Client,
    api_key:  String,
    model:    String,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String, base_url: String, timeout_secs: u64) -> Self {
        let http = ClientBuilder::new()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .build()
            .expect("anthropic: failed to build HTTP client");
        // This client appends `/v1/...` itself, so the base must be host-only.
        // Forgive a user who included `/v1` (default is host-only) — otherwise
        // requests go to `…/v1/v1/messages` and 404.
        let base_url = base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .trim_end_matches('/')
            .to_string();
        Self {
            http,
            api_key,
            model,
            base_url,
        }
    }

    pub fn model_name(&self) -> &str { &self.model }

    fn messages_url(&self) -> String { format!("{}/v1/messages", self.base_url) }
    fn models_url(&self)   -> String { format!("{}/v1/models",   self.base_url) }

    /// Fetch the model list from `/v1/models`. Anthropic returns
    /// `{"data": [{"id", "display_name", "created_at", "type"}]}` —
    /// `display_name` is friendly and worth surfacing; the rest we
    /// drop. Returns id-only `ModelEntry`s; the caller overlays
    /// pricing from `providers::overlays::ANTHROPIC`.
    pub async fn fetch_model_ids(&self) -> Result<Vec<crate::providers::catalog::ModelEntry>, crate::MiraError> {
        #[derive(serde::Deserialize)]
        struct ModelsResponse { data: Vec<ModelRow> }
        #[derive(serde::Deserialize)]
        struct ModelRow {
            id: String,
            #[serde(default)]
            display_name: Option<String>,
        }
        let url = self.models_url();
        let resp = self.apply_headers(self.http.get(&url)).send().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("anthropic: catalog fetch connect failed: {e}")
            ))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body   = resp.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("anthropic: catalog fetch {status}: {body}")
            ));
        }
        let body = resp.text().await.map_err(|e| crate::MiraError::ProviderError(
            format!("anthropic: catalog read body failed: {e}")
        ))?;
        let parsed: ModelsResponse = serde_json::from_str(&body)
            .map_err(|e| crate::MiraError::ProviderError(
                format!("anthropic: catalog parse failed: {e}")
            ))?;
        Ok(parsed.data.into_iter().map(|r| crate::providers::catalog::ModelEntry {
            id:                  r.id,
            display_name:        r.display_name,
            context_window:      None,
            input_price_per_1m:  None,
            output_price_per_1m: None,
            notes:               None,
        }).collect())
    }

    fn apply_headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        if !self.api_key.is_empty() {
            rb = rb.header("x-api-key", &self.api_key);
        }
        rb
    }

    fn provider_id(&self) -> ProviderId { ProviderId::Anthropic(self.model.clone()) }

    fn build_request<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        options:  &'a GenerationOptions,
        stream:   bool,
    ) -> MessagesRequest<'a> {
        let (system, outbound) = convert_messages(messages);
        let (tools, tool_choice) = match &options.tools {
            Some(specs) if !specs.is_empty() => {
                // OpenAI's `"none"` is signalled by omitting tools, not
                // by a choice variant; mirror that here.
                let choice = options.tool_choice.as_ref()
                    .and_then(convert_tool_choice);
                let want_none = matches!(
                    options.tool_choice.as_ref().and_then(|v| v.as_str()),
                    Some("none"),
                );
                if want_none {
                    (None, None)
                } else {
                    (Some(convert_tool_specs(specs)), choice)
                }
            }
            _ => (None, None),
        };
        // Anthropic temperature is 0..1; OpenAI's is 0..2. Clamp at the
        // boundary rather than pass-through so requests don't 400.
        let base_temperature = if options.temperature > 0.0 {
            Some(options.temperature.clamp(0.0, 1.0))
        } else { None };
        let base_max = options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

        // Extended thinking (roadmap #13). On a reasoning-effort hint, enable
        // Anthropic thinking with a budget mapped from the effort, ensure
        // max_tokens exceeds the budget, and drop temperature (Anthropic
        // rejects a set temperature when thinking is enabled).
        let (thinking, max_tokens, temperature) = match options.reasoning_effort.as_deref() {
            Some(effort) => {
                let budget = match effort {
                    "high" => 8192,
                    "low"  => 1024,
                    _      => 4096, // "medium" / unknown
                };
                (
                    Some(Thinking { kind: "enabled", budget_tokens: budget }),
                    base_max.max(budget + 1024),
                    None,
                )
            }
            None => (None, base_max, base_temperature),
        };

        MessagesRequest {
            model:          &self.model,
            max_tokens,
            system,
            messages:       outbound,
            stream,
            temperature,
            top_p:          options.top_p,
            stop_sequences: options.stop_sequences.clone(),
            tools,
            tool_choice,
            thinking,
        }
    }

    async fn generate_non_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        let url     = self.messages_url();
        let request = self.build_request(messages, options, false);
        debug!("anthropic: POST {url} (non-streaming, model={})", self.model);

        let response = self.apply_headers(self.http.post(&url).json(&request))
            .send().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("anthropic: connect failed: {e}")
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body   = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("anthropic: {status} — {body}")
            ));
        }

        let parsed: MessagesResponse = response.json().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("anthropic: parse failed: {e}")
            ))?;

        let (content, tool_calls, reasoning) = convert_response_content(parsed.content);
        let usage = TokenUsage {
            prompt_tokens:     parsed.usage.input_tokens,
            completion_tokens: parsed.usage.output_tokens,
            total_tokens:      parsed.usage.input_tokens + parsed.usage.output_tokens,
        };

        Ok(GenerationResponse {
            content,
            tool_calls,
            reasoning,
            usage,
            provider_id: self.provider_id(),
            model_name:  self.model.clone(),
            fallback: None,
            })
    }

    async fn do_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
        on_token: &mut (dyn FnMut(String) + Send),
    ) -> Result<GenerationResponse, crate::MiraError> {
        let url     = self.messages_url();
        let request = self.build_request(messages, options, true);
        debug!("anthropic: POST {url} (streaming, model={})", self.model);

        let response = self.apply_headers(self.http.post(&url).json(&request))
            .send().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("anthropic: connect failed: {e}")
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body   = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("anthropic: {status} — {body}")
            ));
        }

        // Per-index accumulators. Each content_block_start sets up an
        // entry keyed by `index`; deltas append into it; content_block_stop
        // finalises the entry (parses accumulated JSON for tool_use,
        // drops the entry for text since text was streamed). BTreeMap so
        // any straggler entries we finalise at message_stop come out in
        // the order the model emitted them.
        struct PendingBlock {
            kind:       PendingKind,
            tool_id:    Option<String>,
            tool_name:  Option<String>,
            json_buf:   String,
        }
        enum PendingKind { Text, ToolUse, Thinking }

        // Helper closure: parse accumulated JSON, push to tool_calls.
        let finalise_tool_use = |pending: PendingBlock, tool_calls: &mut Vec<ToolCall>| {
            let id   = pending.tool_id.unwrap_or_default();
            let name = pending.tool_name.unwrap_or_default();
            let args: serde_json::Value = if pending.json_buf.trim().is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                serde_json::from_str(&pending.json_buf).unwrap_or_else(|e| {
                    warn!("anthropic: tool_use input was not valid JSON ({e}); \
                           keeping raw string: {}", pending.json_buf);
                    serde_json::Value::String(pending.json_buf.clone())
                })
            };
            tool_calls.push(ToolCall { call_id: id, name, arguments: args });
        };

        let mut blocks: BTreeMap<usize, PendingBlock> = BTreeMap::new();
        let mut content   = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage   = TokenUsage::default();

        let mut line_buf = String::new();
        let mut stream   = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| crate::MiraError::ProviderError(
                format!("anthropic: stream error: {e}")
            ))?;
            line_buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = line_buf.find('\n') {
                let line = line_buf[..nl].trim_end_matches('\r').to_owned();
                line_buf = line_buf[nl + 1..].to_owned();

                // Anthropic SSE frames look like:
                //   event: content_block_delta
                //   data: { ...json... }
                //   <blank>
                // We only care about the `data:` lines; the `event:`
                // line is redundant with the JSON's `type` field.
                let Some(json_data) = line.strip_prefix("data: ") else { continue; };
                if json_data.trim().is_empty() { continue; }

                let evt: StreamEvent = match serde_json::from_str(json_data) {
                    Ok(e) => e,
                    Err(e) => {
                        debug!("anthropic: skipping unparseable event ({e}): {json_data}");
                        continue;
                    }
                };

                match evt {
                    StreamEvent::MessageStart { .. } | StreamEvent::Ping => {}

                    StreamEvent::ContentBlockStart { index, content_block } => {
                        match content_block {
                            ContentBlockHeader::Text { .. } => {
                                blocks.insert(index, PendingBlock {
                                    kind:      PendingKind::Text,
                                    tool_id:   None,
                                    tool_name: None,
                                    json_buf:  String::new(),
                                });
                            }
                            ContentBlockHeader::ToolUse { id, name, .. } => {
                                blocks.insert(index, PendingBlock {
                                    kind:      PendingKind::ToolUse,
                                    tool_id:   Some(id),
                                    tool_name: Some(name),
                                    json_buf:  String::new(),
                                });
                            }
                            ContentBlockHeader::Thinking { .. } => {
                                blocks.insert(index, PendingBlock {
                                    kind:      PendingKind::Thinking,
                                    tool_id:   None,
                                    tool_name: None,
                                    json_buf:  String::new(),
                                });
                            }
                        }
                    }

                    StreamEvent::ContentBlockDelta { index, delta } => {
                        let Some(pending) = blocks.get_mut(&index) else { continue; };
                        match (delta, &pending.kind) {
                            (StreamDelta::TextDelta { text }, PendingKind::Text) => {
                                content.push_str(&text);
                                on_token(text);
                            }
                            (StreamDelta::InputJsonDelta { partial_json }, PendingKind::ToolUse) => {
                                pending.json_buf.push_str(&partial_json);
                            }
                            (StreamDelta::ThinkingDelta { thinking }, PendingKind::Thinking) => {
                                // Capture extended-thinking content but
                                // don't forward to on_token — keeps
                                // chain-of-thought out of the live
                                // answer stream; surfaces on the
                                // final GenerationResponse.reasoning.
                                reasoning.push_str(&thinking);
                            }
                            // Other delta combinations we don't
                            // recognise: silently consumed.
                            _ => {}
                        }
                    }

                    StreamEvent::ContentBlockStop { index } => {
                        if let Some(pending) = blocks.remove(&index) {
                            match pending.kind {
                                PendingKind::ToolUse =>
                                    finalise_tool_use(pending, &mut tool_calls),
                                PendingKind::Text | PendingKind::Thinking => {}
                            }
                        }
                    }

                    StreamEvent::MessageDelta { usage: maybe_usage, .. } => {
                        if let Some(u) = maybe_usage {
                            usage.completion_tokens += u.output_tokens;
                            usage.prompt_tokens     += u.input_tokens;
                        }
                    }

                    StreamEvent::MessageStop => {
                        // Drain all still-open tool_use blocks. (In
                        // practice every block has been closed by a
                        // ContentBlockStop before MessageStop.) Stop
                        // here either way; the next bytes_stream poll
                        // will return None.
                    }

                    StreamEvent::Error { error } => {
                        return Err(crate::MiraError::ProviderError(
                            format!("anthropic: server error mid-stream: {error}")
                        ));
                    }
                }
            }
        }

        // Defensive cleanup — finalise any block that was opened but
        // never explicitly closed (shouldn't happen in practice; the
        // server emits content_block_stop for every block). Iteration
        // order is by index ascending thanks to the BTreeMap.
        for (_idx, pending) in std::mem::take(&mut blocks) {
            if let PendingKind::ToolUse = pending.kind {
                finalise_tool_use(pending, &mut tool_calls);
            }
        }

        usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;

        let tool_calls = if tool_calls.is_empty() { None } else { Some(tool_calls) };
        let reasoning  = if reasoning.is_empty()  { None } else { Some(reasoning)  };

        Ok(GenerationResponse {
            content,
            tool_calls,
            reasoning,
            usage,
            provider_id: self.provider_id(),
            model_name:  self.model.clone(),
            fallback: None,
            })
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str { "anthropic" }

    async fn generate(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        self.generate_non_stream(messages, options).await
    }

    async fn generate_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
        on_token: &mut (dyn FnMut(String) + Send),
    ) -> Result<GenerationResponse, crate::MiraError> {
        self.do_stream(messages, options, on_token).await
    }

    async fn health_check(&self) -> bool {
        // Anthropic doesn't expose a public health endpoint. Probe by
        // sending the smallest legal request — one user message, one
        // token max — and counting any 2xx as healthy. We accept the
        // cost (a few input tokens) in exchange for a real signal that
        // both connectivity AND auth are working. If the operator
        // doesn't want to spend tokens on health-checks, they can flip
        // `enabled = false` on the provider entry (future work).
        let probe = MessagesRequest {
            model:          &self.model,
            max_tokens:     1,
            system:         None,
            messages:       vec![crate::providers::anthropic::wire::OutboundMessage {
                role:    "user",
                content: vec![crate::providers::anthropic::wire::OutboundContentBlock::Text {
                    text: "ping".into(),
                }],
            }],
            stream:         false,
            temperature:    None,
            top_p:          None,
            stop_sequences: None,
            tools:          None,
            tool_choice:    None,
            thinking:       None,
        };
        let url = self.messages_url();
        match self.apply_headers(self.http.post(&url).json(&probe)).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                warn!("anthropic: health check failed: {e}");
                false
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> AnthropicProvider {
        AnthropicProvider::new(
            "test-key".into(),
            "claude-sonnet-4-5".into(),
            "https://api.anthropic.com".into(),
            30,
        )
    }

    #[test]
    fn name_and_provider_id() {
        let p = provider();
        assert_eq!(p.name(),       "anthropic");
        assert_eq!(p.model_name(), "claude-sonnet-4-5");
        match p.provider_id() {
            ProviderId::Anthropic(m) => assert_eq!(m, "claude-sonnet-4-5"),
            other => panic!("expected Anthropic, got {other:?}"),
        }
    }

    #[test]
    fn url_construction() {
        let p = provider();
        assert_eq!(p.messages_url(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn headers_carry_api_key_and_version() {
        let p = provider();
        let req = p.apply_headers(p.http.post("http://localhost/test"))
            .build().unwrap();
        assert_eq!(
            req.headers().get("anthropic-version").and_then(|v| v.to_str().ok()),
            Some(ANTHROPIC_VERSION),
        );
        assert_eq!(
            req.headers().get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("test-key"),
        );
        // Anthropic does NOT use Authorization: Bearer.
        assert!(req.headers().get("authorization").is_none(),
            "Anthropic uses x-api-key, not Authorization");
    }

    #[test]
    fn empty_api_key_omits_header() {
        let p = AnthropicProvider::new(String::new(), "x".into(), "http://x".into(), 30);
        let req = p.apply_headers(p.http.post("http://localhost/test"))
            .build().unwrap();
        assert!(req.headers().get("x-api-key").is_none());
    }

    #[test]
    fn build_request_sets_required_max_tokens() {
        // GenerationOptions::default has max_tokens = None; the client
        // must substitute DEFAULT_MAX_TOKENS so the request is legal.
        let p    = provider();
        let opts = GenerationOptions::default();
        let msgs = [ChatMessage::user("hi")];
        let req  = p.build_request(&msgs, &opts, false);
        assert_eq!(req.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(req.thinking.is_none());
    }

    #[test]
    fn build_request_enables_thinking_on_effort() {
        // reasoning_effort → a thinking block with a budget, max_tokens bumped
        // above the budget, and temperature dropped (Anthropic rejects a set
        // temperature with thinking enabled).
        let p = provider();
        let opts = GenerationOptions {
            reasoning_effort: Some("high".into()),
            temperature: 0.7,
            max_tokens: Some(256), // below budget+1024 → must be raised
            ..Default::default()
        };
        let msgs = [ChatMessage::user("prove it")];
        let req = p.build_request(&msgs, &opts, false);
        let thinking = req.thinking.expect("thinking block present");
        assert_eq!(thinking.kind, "enabled");
        assert_eq!(thinking.budget_tokens, 8192);
        assert!(req.max_tokens > thinking.budget_tokens, "max_tokens must exceed budget");
        assert_eq!(req.temperature, None, "temperature must be unset with thinking");
    }

    #[test]
    fn build_request_honours_explicit_max_tokens() {
        let p    = provider();
        let mut opts = GenerationOptions::default();
        opts.max_tokens = Some(128);
        let msgs = [ChatMessage::user("hi")];
        let req  = p.build_request(&msgs, &opts, false);
        assert_eq!(req.max_tokens, 128);
    }

    #[test]
    fn build_request_clamps_temperature_to_anthropic_range() {
        // OpenAI accepts up to 2.0; Anthropic caps at 1.0. Without
        // clamping, a temperature passed through from a multi-provider
        // tool chain would 400 on Anthropic.
        let p    = provider();
        let mut opts = GenerationOptions::default();
        opts.temperature = 1.7;
        let msgs = [ChatMessage::user("hi")];
        let req  = p.build_request(&msgs, &opts, false);
        assert_eq!(req.temperature, Some(1.0));
    }

    #[test]
    fn build_request_strips_system_from_messages() {
        let p = provider();
        let msgs = vec![
            ChatMessage::system("You are MIRA."),
            ChatMessage::user("hi"),
        ];
        let opts = GenerationOptions::default();
        let req  = p.build_request(&msgs, &opts, false);
        assert_eq!(req.system.as_deref(), Some("You are MIRA."));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
    }

    #[test]
    fn build_request_tool_choice_none_omits_tools() {
        let p = provider();
        let mut opts = GenerationOptions::default();
        opts.tools = Some(vec![
            crate::types::ToolSpec::function("x", "y", serde_json::json!({})),
        ]);
        opts.tool_choice = Some(serde_json::json!("none"));
        let msgs = [ChatMessage::user("hi")];
        let req = p.build_request(&msgs, &opts, false);
        assert!(req.tools.is_none(), "tool_choice='none' must omit tools entirely");
        assert!(req.tool_choice.is_none());
    }

    #[tokio::test]
    async fn health_check_returns_false_when_unreachable() {
        let p = AnthropicProvider::new(
            "test-key".into(),
            "claude-sonnet-4-5".into(),
            "http://127.0.0.1:1".into(),
            5,
        );
        assert!(!p.health_check().await);
    }
}
