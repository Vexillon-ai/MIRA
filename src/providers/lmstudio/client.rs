// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/lmstudio/client.rs

//! LM Studio API client implementation
//! Uses OpenAI-compatible API format provided by LM Studio

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, warn};

use crate::providers::ModelProvider;
use crate::types::{
    ChatMessage, GenerationOptions, GenerationResponse, MessageRole, ProviderId,
    TokenUsage, ToolCall,
};

/// LM Studio provider implementation
pub struct LmStudioProvider {
    client: Client,
    model: String,
    base_url: String,
    /// Token cap for non-streaming tool-loop rounds.
    tool_round_max_tokens: u32,
    /// Token cap for the streaming final-answer path.
    response_max_tokens:   u32,
}

#[derive(Debug, Serialize)]
struct LmStudioRequest<'a> {
    model: &'a str,
    messages: Vec<LmStudioMessage<'a>>,
    stream: bool,
    /// Hard stop to prevent the reasoning-distilled Qwen template from
    /// emitting multiple back-to-back "turns" in one completion. That template
    /// inserts a literal `</function_calls>` separator between its internal
    /// turns, which is what lets it loop prose dozens of times in a single
    /// response. Stopping on that token caps the first turn where it belongs.
    /// (Also stop on `<|im_end|>` just in case the template leaks it.)
    stop: [&'static str; 2],
    #[serde(flatten)]
    options: &'a GenerationOptions,
}

const STOP_SEQUENCES: [&str; 2] = ["</function_calls>", "<|im_end|>"];

/// Fallback caps when no caller value and no per-instance config — only
/// reachable in the legacy two-arg `LmStudioProvider::new` path used by
/// unit tests / dev tooling. Production goes through `with_token_caps`
/// which threads the values from `agent.max_tool_round_tokens` /
/// `agent.max_response_tokens` in `mira_config.json`.
const FALLBACK_TOOL_ROUND_TOKENS: u32 = 2048;
const FALLBACK_RESPONSE_TOKENS:   u32 = 16384;

fn apply_cap(options: &GenerationOptions, cap: u32) -> GenerationOptions {
    let mut o = options.clone();
    if o.max_tokens.is_none() {
        o.max_tokens = Some(cap);
    }
    o
}

/// Outbound message. Mirrors the OpenAI chat-completions shape closely enough
/// that Qwen/Hermes fine-tunes recognize `tool_calls` on assistant turns and
/// `tool_call_id` on tool turns.
#[derive(Debug, Serialize)]
struct LmStudioMessage<'a> {
    role: &'a str,
    // String for text-only turns (keeps old LM Studio versions happy),
    // parts array when an attachment is present (vision-capable
    // models loaded in LM Studio expect the OpenAI-shaped parts).
    content: OutboundContent<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OutboundToolCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OutboundContent<'a> {
    Text(&'a str),
    Parts(Vec<OutboundPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum OutboundPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlInner },
}

#[derive(Debug, Serialize)]
struct ImageUrlInner { url: String }

#[derive(Debug, Serialize)]
struct OutboundToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OutboundToolFn<'a>,
}

#[derive(Debug, Serialize)]
struct OutboundToolFn<'a> {
    name: &'a str,
    // OpenAI serializes arguments as a JSON-encoded string.
    arguments: String,
}

fn role_str(r: &MessageRole) -> &'static str {
    match r {
        MessageRole::System    => "system",
        MessageRole::User      => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool      => "tool",
    }
}

fn to_outbound<'a>(messages: &'a [ChatMessage]) -> Vec<LmStudioMessage<'a>> {
    messages.iter().map(|m| {
        let tool_calls = m.tool_calls.as_ref().map(|calls| {
            calls.iter().map(|tc| OutboundToolCall {
                id: &tc.call_id,
                kind: "function",
                function: OutboundToolFn {
                    name: &tc.name,
                    arguments: tc.arguments.to_string(),
                },
            }).collect()
        });
        let content = match m.attachments.as_ref().filter(|v| !v.is_empty()) {
            None => OutboundContent::Text(m.content.as_str()),
            Some(att) => {
                let mut parts = Vec::with_capacity(att.len() + 1);
                if !m.content.is_empty() {
                    parts.push(OutboundPart::Text { text: m.content.clone() });
                }
                for a in att {
                    if matches!(a.kind, crate::types::AttachmentKind::Image) {
                        parts.push(OutboundPart::ImageUrl {
                            image_url: ImageUrlInner {
                                url: format!("data:{};base64,{}", a.mime_type, a.data_b64),
                            },
                        });
                    }
                }
                OutboundContent::Parts(parts)
            }
        };
        LmStudioMessage {
            role: role_str(&m.role),
            content,
            tool_calls,
            tool_call_id: m.tool_call_id.as_deref(),
        }
    }).collect()
}

// ─── Non-streaming response shape ────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LmStudioResponse {
    choices: Vec<LmStudioResponseChoice>,
    #[serde(default)]
    usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize)]
struct LmStudioResponseChoice {
    message: LmStudioResponseMessage,
}

#[derive(Debug, Deserialize)]
struct LmStudioResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<InboundToolCall>>,
    /// Reasoning-distilled Qwen models (and some others) route tool-call XML
    /// into this channel instead of `content`. When `content` and `tool_calls`
    /// both come back empty, we fall back to this so the Hermes parser in the
    /// tool loop can still recover the calls. Aliased to `reasoning` for parity
    /// with the streaming delta field name.
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InboundToolCall {
    #[serde(default)]
    id: Option<String>,
    function: InboundToolFn,
}

#[derive(Debug, Deserialize)]
struct InboundToolFn {
    name: String,
    // `arguments` arrives as a JSON-encoded string per OpenAI convention.
    // Some servers return a raw object instead; accept either.
    #[serde(default)]
    arguments: serde_json::Value,
}

// ─── Streaming response shape ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LmStudioStreamResponse {
    choices: Vec<LmStudioStreamChoice>,
    #[serde(default)]
    usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize)]
struct LmStudioStreamChoice {
    delta: LmStudioDelta,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LmStudioDelta {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning channel. LM Studio names this `reasoning_content` in
    /// non-streaming responses but **`reasoning`** in streaming deltas (gpt-oss,
    /// the qwen3 family, etc.) — accept both so live chain-of-thought is
    /// captured and wrapped in `<thinking>` for the collapsible UI block,
    /// instead of being silently dropped.
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
}

impl LmStudioProvider {
    /// Create a new LM Studio provider with the fallback token caps. Use
    /// [`Self::with_token_caps`] (or chain `.with_token_caps()` on the
    /// returned provider) to thread the user-configured values from
    /// `agent.max_tool_round_tokens` / `agent.max_response_tokens`.
    pub fn new(url: String, model: String) -> Self {
        let client = ClientBuilder::new()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            model,
            // LM Studio serves the OpenAI surface under `/v1`; tolerate a
            // host-only URL (the common case) by grafting it on.
            base_url: crate::providers::normalize_openai_base_url(&url, "/v1"),
            tool_round_max_tokens: FALLBACK_TOOL_ROUND_TOKENS,
            response_max_tokens:   FALLBACK_RESPONSE_TOKENS,
        }
    }

    /// Override the per-path token caps. Production construction sites
    /// pull these from `AgentConfig`.
    pub fn with_token_caps(mut self, tool_round: u32, response: u32) -> Self {
        self.tool_round_max_tokens = tool_round;
        self.response_max_tokens   = response;
        self
    }

    pub fn model_name(&self) -> &str { &self.model }

    /// Fetch the model list from LM Studio's `/v1/models`. LM Studio
    /// serves the OpenAI-shaped response — we return id-only
    /// entries since pricing/context for arbitrary local models
    /// isn't something we curate.
    pub async fn fetch_model_ids(&self) -> Result<Vec<crate::providers::catalog::ModelEntry>, crate::MiraError> {
        #[derive(serde::Deserialize)]
        struct ModelsResponse { data: Vec<ModelRow> }
        #[derive(serde::Deserialize)]
        struct ModelRow { id: String }
        let url = format!("{}/models", self.base_url);
        let resp = self.client.get(&url).send().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("lmstudio: catalog fetch connect failed: {e}")
            ))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body   = resp.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("lmstudio: catalog fetch {status}: {body}")
            ));
        }
        let parsed: ModelsResponse = resp.json().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("lmstudio: catalog parse failed: {e}")
            ))?;
        Ok(parsed.data.into_iter().map(|r| crate::providers::catalog::ModelEntry::id_only(r.id)).collect())
    }

    fn api_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Non-streaming generate. Used during tool rounds where the tool loop
    /// wants a complete response so it can inspect `tool_calls` before
    /// deciding whether this is the final answer or another round.
    async fn generate_non_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        debug!("Generating with LM Studio model '{}' (non-streaming)", self.model);

        let outbound = to_outbound(messages);
        let capped = apply_cap(options, self.tool_round_max_tokens);
        let request = LmStudioRequest {
            model: &self.model,
            messages: outbound,
            stream: false,
            stop: STOP_SEQUENCES,
            options: &capped,
        };

        let url = self.api_url();
        let response = self.client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("Failed to connect to LM Studio: {}", e)
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("LM Studio returned {}: {}", status, body)
            ));
        }

        let parsed: LmStudioResponse = response
            .json()
            .await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("Failed to parse LM Studio response: {}", e)
            ))?;

        let choice = parsed.choices.into_iter().next().ok_or_else(|| {
            crate::MiraError::ProviderError("LM Studio returned empty choices".to_string())
        })?;

        let raw_content = choice.message.content.unwrap_or_default();
        let tool_calls = choice.message.tool_calls.map(|calls| {
            calls.into_iter().map(|c| {
                // `arguments` is usually a JSON-encoded string — decode into a
                // real `Value` so downstream tools see `{"key": "..."}` and
                // not `"{\"key\":\"...\"}"`.
                let args = match c.function.arguments {
                    serde_json::Value::String(s) => {
                        serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
                    }
                    other => other,
                };
                ToolCall {
                    name: c.function.name,
                    arguments: args,
                    call_id: c.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                }
            }).collect::<Vec<_>>()
        });

        // Reasoning-distilled Qwen builds emit `<tool_call>` XML into the
        // `reasoning_content` channel instead of `content`/`tool_calls`. When
        // both real channels are empty, promote reasoning_content to content
        // so the Hermes parser in the tool loop can still recover the calls.
        // Wrap with `<thinking>…</thinking>` so the web UI collapses the
        // deliberation into a foldable details block — without the wrap,
        // the entire monologue dumps verbatim into the chat (the streaming
        // path already wraps; the non-streaming path used to skip it,
        // which made the reasoning leak whenever tool_loop probed via
        // non-streaming first to detect tool calls atomically).
        // The tool-call parsers in `tool_loop` look for `<tool_call>` /
        // `<think>` / bare-JSON shapes — none of those collide with our
        // `<thinking>` wrapper, so tool extraction still finds the calls
        // embedded inside the promoted content.
        let tool_calls_empty = tool_calls.as_ref().map_or(true, |v| v.is_empty());
        let content = if raw_content.trim().is_empty() && tool_calls_empty {
            match choice.message.reasoning_content {
                Some(r) if !r.trim().is_empty() => {
                    debug!("LM Studio returned empty content+tool_calls; promoting reasoning_content ({} chars), wrapped in <thinking>", r.len());
                    format!("<thinking>{r}</thinking>\n\n")
                }
                _ => raw_content,
            }
        } else {
            raw_content
        };

        Ok(GenerationResponse {
            content,
            tool_calls,
            reasoning:   None,
            usage: parsed.usage.unwrap_or_default(),
            provider_id: ProviderId::Local(format!("lmstudio/{}", self.model)),
            model_name: self.model.clone(),
            fallback: None,
            })
    }

    /// Streaming path — used for the final answer after tool rounds settle.
    /// Doesn't attempt to reconstruct tool_calls from streaming deltas; the
    /// tool loop already decides "final answer" based on the non-stream probe.
    async fn stream_impl<F>(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
        mut on_token: F,
    ) -> Result<GenerationResponse, crate::MiraError>
    where
        F: FnMut(String),
    {
        debug!("Generating with LM Studio model '{}' (streaming)", self.model);

        let outbound = to_outbound(messages);
        let capped = apply_cap(options, self.response_max_tokens);
        let request = LmStudioRequest {
            model: &self.model,
            messages: outbound,
            stream: true,
            stop: STOP_SEQUENCES,
            options: &capped,
        };

        let url = self.api_url();
        let response = self.client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("Failed to connect to LM Studio: {}", e)
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("LM Studio returned {}: {}", status, body)
            ));
        }

        let mut content = String::new();
        let mut usage = TokenUsage::default();
        // Track whether we're currently inside a reasoning_content run so
        // we can wrap it in `<thinking>...</thinking>` on transitions. The
        // web UI renders that block as a collapsed `<details>` so the
        // user can peek at the chain-of-thought without it dominating
        // the reply.
        let mut in_thinking = false;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| crate::MiraError::ProviderError(
                format!("Stream error: {}", e)
            ))?;

            let line = String::from_utf8_lossy(&chunk);
            for json_line in line.lines() {
                if json_line.is_empty() || json_line.starts_with("data: [DONE]") { continue; }

                if json_line.starts_with("data: ") {
                    let json_data = &json_line[6..];
                    if let Ok(stream_resp) = serde_json::from_str::<LmStudioStreamResponse>(json_data) {
                        for choice in stream_resp.choices {
                            let r_token = choice.delta.reasoning_content.unwrap_or_default();
                            let c_token = choice.delta.content.unwrap_or_default();

                            if !r_token.is_empty() {
                                if !in_thinking {
                                    content.push_str("<thinking>");
                                    on_token("<thinking>".to_string());
                                    in_thinking = true;
                                }
                                content.push_str(&r_token);
                                on_token(r_token);
                            }
                            if !c_token.is_empty() {
                                if in_thinking {
                                    content.push_str("</thinking>\n\n");
                                    on_token("</thinking>\n\n".to_string());
                                    in_thinking = false;
                                }
                                content.push_str(&c_token);
                                on_token(c_token);
                            }
                        }
                        if let Some(u) = stream_resp.usage {
                            usage = u;
                        }
                    }
                }
            }
        }
        // The model finished mid-reasoning (hit a stop, ran out of budget,
        // or just never produced a content delta). Close the tag so the
        // renderer doesn't see an open `<thinking>` and treat the rest of
        // the message as part of it.
        if in_thinking {
            content.push_str("</thinking>\n\n");
            on_token("</thinking>\n\n".to_string());
        }

        debug!("Streaming generation complete, {} tokens", usage.total_tokens);

        Ok(GenerationResponse {
            content,
            tool_calls: None,
            reasoning: None,
            usage,
            provider_id: ProviderId::Local(format!("lmstudio/{}", self.model)),
            model_name: self.model.clone(),
            fallback: None,
            })
    }
}

#[async_trait]
impl ModelProvider for LmStudioProvider {
    fn name(&self) -> &str { "lmstudio" }

    async fn generate(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        // Use non-streaming so structured tool_calls come back intact.
        self.generate_non_stream(messages, options).await
    }

    async fn generate_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
        on_token: &mut (dyn FnMut(String) + Send),
    ) -> Result<GenerationResponse, crate::MiraError> {
        self.stream_impl(messages, options, on_token).await
    }

    async fn health_check(&self) -> bool {
        let url = format!("{}/models", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                warn!("LM Studio health check failed: {}", e);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructed_provider_targets_v1_endpoints() {
        // Host-only URL (the setup default / common hand-typed form) gets `/v1`.
        let p = LmStudioProvider::new("http://localhost:1234".into(), "m".into());
        assert_eq!(p.api_url(), "http://localhost:1234/v1/chat/completions");
        // Already-correct URL is preserved.
        let p2 = LmStudioProvider::new("http://localhost:1234/v1".into(), "m".into());
        assert_eq!(p2.api_url(), "http://localhost:1234/v1/chat/completions");
    }
}
