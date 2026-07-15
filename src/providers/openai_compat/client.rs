// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/openai_compat/client.rs

//! OpenAI-compatible HTTP client shared by every `/v1/chat/completions`
//! provider. See `mod.rs` for the supported gateway list.

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

// ─────────────────────────────────────────────────────────────────────────────
// Config knobs
// ─────────────────────────────────────────────────────────────────────────────

/// Which HTTP header carries the API key.
#[derive(Debug, Clone)]
pub enum AuthHeader {
    /// `Authorization: Bearer <key>` — every gateway except Azure.
    Bearer,
    /// `api-key: <key>` — Azure OpenAI.
    AzureApiKey,
    /// No auth header — anonymous gateway (rare; vLLM open install).
    None,
}

/// Static-name + dynamic-value header. Lets the OpenRouter-style attribution
/// hack (`HTTP-Referer`, `X-Title`) live in config without forcing string
/// allocations for the common no-header case.
#[derive(Debug, Clone)]
pub struct ExtraHeader {
    pub name:  &'static str,
    pub value: String,
}

/// All the knobs the shared client needs. Per-provider thin wrappers
/// pre-fill this with their defaults.
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// Slug used in `ProviderId::OpenAiCompat { provider, .. }`, log
    /// lines, and `ModelProvider::name()`. Should match the
    /// corresponding key in `providers.<slug>` in `mira_config.json`.
    pub provider_name: String,
    /// Base URL without trailing slash. `/chat/completions` is appended
    /// for inference; `/models` for the optional catalog probe.
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
    pub auth_header: AuthHeader,
    /// Extra request headers (e.g. OpenRouter's attribution pair).
    pub extra_headers: Vec<ExtraHeader>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Wire types — request
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<OutboundMessage<'a>>,
    stream: bool,
    #[serde(flatten)]
    options: &'a GenerationOptions,
}

#[derive(Debug, Serialize)]
struct OutboundMessage<'a> {
    role: &'a str,
    // OpenAI accepts either a string (text-only) or an array of parts
    // (mixed text + image). We pick at build time based on whether
    // the message has attachments; old/non-vision models like
    // gpt-3.5-turbo still get the simple string form.
    content: OutboundContent<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OutboundToolCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

/// Either a flat text string or a multi-part array. `serde(untagged)`
/// means it serializes directly as one or the other with no wrapper —
/// which is exactly what OpenAI expects.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OutboundContent<'a> {
    Text(&'a str),
    Parts(Vec<OutboundPart>),
}

/// One element of the multi-part array. Q1.3 ships text + image_url
/// only; `input_audio` and `file` can be added in later slices.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum OutboundPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlInner },
}

#[derive(Debug, Serialize)]
struct ImageUrlInner {
    /// `data:<mime>;base64,<data>` — OpenAI accepts both data URLs
    /// and remote https URLs; we use data URLs so we don't expose
    /// the user's images to a third-party host.
    url: String,
}

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

fn to_outbound<'a>(messages: &'a [ChatMessage]) -> Vec<OutboundMessage<'a>> {
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
        // Pick the simple string content path when there are no
        // attachments — keeps backwards compatibility with older
        // models / proxies that don't accept the parts array.
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
        OutboundMessage {
            role: role_str(&m.role),
            content,
            tool_calls,
            tool_call_id: m.tool_call_id.as_deref(),
        }
    }).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Wire types — non-streaming response
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatResponseChoice>,
    #[serde(default)]
    usage: Option<crate::providers::usage::WireUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatResponseChoice {
    message: ChatResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ChatResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<InboundToolCall>>,
    /// Private chain-of-thought emitted by reasoning models that wrap
    /// the OpenAI-compat shape:
    /// - DeepSeek R1 (`deepseek-reasoner`) → `reasoning_content`
    /// - xAI Grok-3-mini reasoning models → `reasoning_content`
    /// - several self-hosted reasoning forks adopt the same field
    ///
    /// OpenAI's own o-series uses the separate Responses API for
    /// reasoning output; the Chat Completions path covered here
    /// hides it. Surfaced to callers via
    /// `GenerationResponse.reasoning` rather than concatenated into
    /// `content` so the UI can present it separately.
    #[serde(default)]
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
    #[serde(default)]
    arguments: serde_json::Value,
}

// ─────────────────────────────────────────────────────────────────────────────
// Wire types — streaming response
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<crate::providers::usage::WireUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning tokens streamed by DeepSeek R1 / xAI Grok-3-mini /
    /// etc. arrive on this delta channel instead of `content` (see
    /// `ChatResponseMessage.reasoning_content` for the rationale).
    /// Accumulated separately so it doesn't pollute the visible
    /// answer text.
    #[serde(default)]
    reasoning_content: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Client
// ─────────────────────────────────────────────────────────────────────────────

pub struct OpenAiCompatClient {
    http:   Client,
    config: OpenAiCompatConfig,
}

impl OpenAiCompatClient {
    pub fn new(mut config: OpenAiCompatConfig) -> Self {
        let http = ClientBuilder::new()
            .timeout(Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .expect("openai_compat: failed to build HTTP client");
        // Be forgiving about the API version path: users often paste just the
        // host. Graft the provider's canonical path onto a bare host so
        // `{base}/chat/completions` resolves. Groq serves the OpenAI surface
        // under `/openai/v1`; everyone else uses `/v1`.
        let canonical = if config.provider_name.eq_ignore_ascii_case("groq") {
            "/openai/v1"
        } else {
            "/v1"
        };
        config.base_url = crate::providers::normalize_openai_base_url(&config.base_url, canonical);
        Self { http, config }
    }

    pub fn provider_name(&self) -> &str { &self.config.provider_name }
    pub fn model_name(&self) -> &str    { &self.config.model }

    fn chat_url(&self)   -> String { format!("{}/chat/completions", self.config.base_url) }
    fn models_url(&self) -> String { format!("{}/models",           self.config.base_url) }

    fn apply_headers(&self, mut rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.config.auth_header {
            AuthHeader::Bearer if !self.config.api_key.is_empty() => {
                rb = rb.header("Authorization", format!("Bearer {}", self.config.api_key));
            }
            AuthHeader::AzureApiKey if !self.config.api_key.is_empty() => {
                rb = rb.header("api-key", &self.config.api_key);
            }
            // Empty key under Bearer/Azure → don't send the header at all;
            // anonymous self-hosted servers happily accept this.
            _ => {}
        }
        for h in &self.config.extra_headers {
            rb = rb.header(h.name, &h.value);
        }
        rb
    }

    fn provider_id(&self) -> ProviderId {
        ProviderId::OpenAiCompat {
            provider: self.config.provider_name.clone(),
            model:    self.config.model.clone(),
        }
    }

    /// Fetch the model catalog from the upstream `/v1/models`
    /// endpoint. The response shape is OpenAI-standard:
    /// `{"data": [{"id": "..."}, ...]}`. Most OpenAI-compat
    /// gateways return id-only — pricing/context comes from
    /// hand-curated overlays applied by `catalog_with_overlay`.
    ///
    /// Returns a list of bare-id `ModelEntry`s, untouched by any
    /// pricing overlay. Callers wrap this in their own
    /// `ModelCatalog` and run [`catalog::apply_overlay`] over the
    /// entries with the right per-provider overlay table.
    pub async fn fetch_model_ids(&self) -> Result<Vec<crate::providers::catalog::ModelEntry>, crate::MiraError> {
        #[derive(serde::Deserialize)]
        struct ModelsResponse { data: Vec<ModelRow> }
        #[derive(serde::Deserialize)]
        struct ModelRow {
            id: String,
            // Some gateways add a friendlier label (Anthropic-style
            // does this; OpenAI does not). We accept either.
            #[serde(default)]
            display_name: Option<String>,
        }
        let url = self.models_url();
        let resp = self.apply_headers(self.http.get(&url)).send().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("{}: catalog fetch connect failed: {e}", self.config.provider_name)
            ))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body   = resp.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("{}: catalog fetch {status}: {body}", self.config.provider_name)
            ));
        }
        let body = resp.text().await.map_err(|e| crate::MiraError::ProviderError(
            format!("{}: catalog read body failed: {e}", self.config.provider_name)
        ))?;
        let parsed: ModelsResponse = serde_json::from_str(&body)
            .map_err(|e| crate::MiraError::ProviderError(
                format!("{}: catalog parse failed: {e}", self.config.provider_name)
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

    async fn generate_non_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        let url     = self.chat_url();
        let request = ChatRequest {
            model:    &self.config.model,
            messages: to_outbound(messages),
            stream:   false,
            options,
        };
        debug!("{}: POST {url} (non-streaming, model={})",
               self.config.provider_name, self.config.model);

        let response = self.apply_headers(self.http.post(&url).json(&request))
            .send().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("{}: connect failed: {e}", self.config.provider_name)
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body   = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("{}: {status} — {body}", self.config.provider_name)
            ));
        }

        let parsed: ChatResponse = response.json().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("{}: parse failed: {e}", self.config.provider_name)
            ))?;

        let choice = parsed.choices.into_iter().next().ok_or_else(|| {
            crate::MiraError::ProviderError(
                format!("{}: empty choices array", self.config.provider_name)
            )
        })?;

        let content = choice.message.content.unwrap_or_default();
        let reasoning = choice.message.reasoning_content.filter(|s| !s.is_empty());
        let tool_calls = choice.message.tool_calls.map(|calls| {
            calls.into_iter().map(|c| {
                // Some gateways serialise `function.arguments` as a JSON
                // string instead of an inline object — accept both.
                let args = match c.function.arguments {
                    serde_json::Value::String(s) =>
                        serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s)),
                    other => other,
                };
                ToolCall {
                    name:      c.function.name,
                    arguments: args,
                    call_id:   c.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                }
            }).collect::<Vec<_>>()
        });

        Ok(GenerationResponse {
            content,
            tool_calls,
            reasoning,
            usage:       parsed.usage.map(Into::into).unwrap_or_default(),
            provider_id: self.provider_id(),
            model_name:  self.config.model.clone(),
            fallback: None,
            })
    }

    async fn do_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
        on_token: &mut (dyn FnMut(String) + Send),
    ) -> Result<GenerationResponse, crate::MiraError> {
        let url     = self.chat_url();
        let request = ChatRequest {
            model:    &self.config.model,
            messages: to_outbound(messages),
            stream:   true,
            options,
        };
        debug!("{}: POST {url} (streaming, model={})",
               self.config.provider_name, self.config.model);

        let response = self.apply_headers(self.http.post(&url).json(&request))
            .send().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("{}: connect failed: {e}", self.config.provider_name)
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body   = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("{}: {status} — {body}", self.config.provider_name)
            ));
        }

        let mut content   = String::new();
        let mut reasoning = String::new();
        let mut usage     = TokenUsage::default();
        let mut line_buf  = String::new();
        let mut stream    = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| crate::MiraError::ProviderError(
                format!("{}: stream error: {e}", self.config.provider_name)
            ))?;

            line_buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = line_buf.find('\n') {
                let json_line = line_buf[..nl].trim_end_matches('\r').to_owned();
                line_buf = line_buf[nl + 1..].to_owned();
                if json_line.is_empty() || json_line == "data: [DONE]" { continue; }
                let Some(json_data) = json_line.strip_prefix("data: ") else { continue; };
                let Ok(stream_resp) = serde_json::from_str::<StreamChunk>(json_data) else {
                    continue;
                };
                for choice in stream_resp.choices {
                    if let Some(token_content) = choice.delta.content {
                        content.push_str(&token_content);
                        on_token(token_content);
                    }
                    // Reasoning tokens (DeepSeek R1 / xAI Grok-3-mini)
                    // arrive on a separate channel and are NOT forwarded
                    // to `on_token` so they don't leak into the
                    // streaming answer UI. They surface on the final
                    // `GenerationResponse.reasoning` for the agent
                    // detail page to render as collapsible context.
                    if let Some(rc) = choice.delta.reasoning_content {
                        reasoning.push_str(&rc);
                    }
                }
                if let Some(u) = stream_resp.usage {
                    usage = u.into();
                }
            }
        }

        let reasoning = if reasoning.is_empty() { None } else { Some(reasoning) };

        Ok(GenerationResponse {
            content,
            tool_calls:  None,
            reasoning,
            usage,
            provider_id: self.provider_id(),
            model_name:  self.config.model.clone(),
            fallback: None,
            })
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatClient {
    fn name(&self) -> &str { &self.config.provider_name }

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
        let url = self.models_url();
        let rb  = self.apply_headers(self.http.get(&url));
        match rb.send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                warn!("{}: health check failed: {e}", self.config.provider_name);
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

    fn cfg(provider: &str, base: &str) -> OpenAiCompatConfig {
        OpenAiCompatConfig {
            provider_name: provider.into(),
            base_url:      base.into(),
            api_key:       "test-key".into(),
            model:         "test-model".into(),
            timeout_secs:  30,
            auth_header:   AuthHeader::Bearer,
            extra_headers: vec![],
        }
    }

    #[test]
    fn client_constructs_from_config() {
        let c = OpenAiCompatClient::new(cfg("openai", "https://api.openai.com/v1"));
        assert_eq!(c.provider_name(), "openai");
        assert_eq!(c.model_name(),    "test-model");
        assert_eq!(c.chat_url(),      "https://api.openai.com/v1/chat/completions");
        assert_eq!(c.models_url(),    "https://api.openai.com/v1/models");
    }

    #[test]
    fn bare_host_gets_canonical_path() {
        // Most providers: append /v1 to a host-only URL.
        let c = OpenAiCompatClient::new(cfg("openai", "https://api.openai.com"));
        assert_eq!(c.chat_url(), "https://api.openai.com/v1/chat/completions");
        // Groq's OpenAI surface lives under /openai/v1.
        let g = OpenAiCompatClient::new(cfg("groq", "https://api.groq.com"));
        assert_eq!(g.chat_url(), "https://api.groq.com/openai/v1/chat/completions");
        // Already-correct URLs are preserved (idempotent).
        let g2 = OpenAiCompatClient::new(cfg("groq", "https://api.groq.com/openai/v1"));
        assert_eq!(g2.chat_url(), "https://api.groq.com/openai/v1/chat/completions");
    }

    #[test]
    fn provider_id_uses_config_slug() {
        let c = OpenAiCompatClient::new(cfg("deepseek", "https://api.deepseek.com/v1"));
        match c.provider_id() {
            ProviderId::OpenAiCompat { provider, model } => {
                assert_eq!(provider, "deepseek");
                assert_eq!(model,    "test-model");
            }
            other => panic!("expected OpenAiCompat, got {other:?}"),
        }
    }

    #[test]
    fn empty_key_with_bearer_skips_auth_header() {
        // Anonymous self-hosted setups (vLLM, LocalAI without auth)
        // must work with `api_key = ""`. apply_headers should not emit
        // an `Authorization: Bearer ` (with empty token).
        let mut c = cfg("vllm", "http://127.0.0.1:8000/v1");
        c.api_key = String::new();
        let client = OpenAiCompatClient::new(c);
        // Build a request, inspect the headers.
        let req = client.apply_headers(client.http.get("http://localhost/test"))
            .build().unwrap();
        assert!(req.headers().get("authorization").is_none(),
            "empty api_key under Bearer should not emit auth header");
    }

    #[test]
    fn azure_auth_uses_api_key_header() {
        let mut c = cfg("azure_openai", "https://x.openai.azure.com/openai/deployments/y");
        c.auth_header = AuthHeader::AzureApiKey;
        c.api_key = "azure-secret".into();
        let client = OpenAiCompatClient::new(c);
        let req = client.apply_headers(client.http.get("http://localhost/test"))
            .build().unwrap();
        assert_eq!(req.headers().get("api-key").and_then(|v| v.to_str().ok()),
                   Some("azure-secret"));
        assert!(req.headers().get("authorization").is_none(),
            "Azure path must not send Authorization");
    }

    #[test]
    fn extra_headers_are_attached() {
        let mut c = cfg("openrouter", "https://openrouter.ai/api/v1");
        c.extra_headers = vec![
            ExtraHeader { name: "HTTP-Referer", value: "https://mira.local".into() },
            ExtraHeader { name: "X-Title",      value: "MIRA".into() },
        ];
        let client = OpenAiCompatClient::new(c);
        let req = client.apply_headers(client.http.get("http://localhost/test"))
            .build().unwrap();
        assert_eq!(req.headers().get("http-referer").and_then(|v| v.to_str().ok()),
                   Some("https://mira.local"));
        assert_eq!(req.headers().get("x-title").and_then(|v| v.to_str().ok()),
                   Some("MIRA"));
    }

    #[tokio::test]
    async fn health_check_returns_false_when_unreachable() {
        // Bind 127.0.0.1:1 — guaranteed-closed port.
        let c = OpenAiCompatClient::new(cfg("test", "http://127.0.0.1:1"));
        assert!(!c.health_check().await);
    }
}
