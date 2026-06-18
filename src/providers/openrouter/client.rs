// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/openrouter/client.rs

//! OpenRouter API client implementation

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::providers::ModelProvider;
use crate::providers::openrouter::catalog::{self, Catalog};
use crate::types::{
    ChatMessage, GenerationOptions, GenerationResponse, MessageRole, ProviderId,
    TokenUsage, ToolCall,
};

/// OpenRouter provider implementation
pub struct OpenRouterProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
}

#[derive(Debug, Serialize)]
struct OpenRouterRequest<'a> {
    model: &'a str,
    messages: Vec<OpenRouterMessage<'a>>,
    stream: bool,
    #[serde(flatten)]
    options: &'a GenerationOptions,
}

#[derive(Debug, Serialize)]
struct OpenRouterMessage<'a> {
    role: &'a str,
    // OpenRouter is an OpenAI-shaped proxy in front of many backends.
    // Vision-capable models (gemini-*-vision, gpt-4o, claude-3*, etc.)
    // expect a parts array; text-only models accept either form. We
    // pick at build time based on whether the message has attachments
    // so non-vision endpoints don't see an unexpected shape.
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
struct ImageUrlInner {
    /// `data:<mime>;base64,<data>` — same form OpenAI accepts so the
    /// proxy doesn't have to translate.
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

fn to_outbound<'a>(messages: &'a [ChatMessage]) -> Vec<OpenRouterMessage<'a>> {
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
        // Q1.3 — use the parts array only when there are attachments,
        // so text-only turns keep the simple string content shape.
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
        OpenRouterMessage {
            role: role_str(&m.role),
            content,
            tool_calls,
            tool_call_id: m.tool_call_id.as_deref(),
        }
    }).collect()
}

// ─── Non-streaming response shape ────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OpenRouterResponse {
    choices: Vec<OpenRouterResponseChoice>,
    #[serde(default)]
    usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterResponseChoice {
    message: OpenRouterResponseMessage,
}

#[derive(Debug, Deserialize)]
struct OpenRouterResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<InboundToolCall>>,
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

// ─── Streaming response shape ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OpenRouterStreamResponse {
    choices: Vec<OpenRouterChoice>,
    #[serde(default)]
    usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterChoice {
    delta: OpenRouterDelta,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterDelta {
    #[serde(default)]
    content: Option<String>,
}

impl OpenRouterProvider {
    pub fn new(api_key: String, model: String) -> Self {
        let client = ClientBuilder::new()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            api_key,
            model,
            base_url: "https://openrouter.ai/api/v1".to_string(),
        }
    }

    pub fn from_env(model: String) -> Option<Self> {
        std::env::var("OPENROUTER_API_KEY")
            .ok()
            .map(|api_key| Self::new(api_key, model))
    }

    pub fn model_name(&self) -> &str { &self.model }

    fn api_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Return the model catalog, served from `<data_dir>/cache/openrouter-models.json`
    /// when fresh and re-fetched from `/models` otherwise.
    ///
    /// * `force = true` skips the cache freshness check and always re-fetches.
    /// * `max_age_hours` matches `[providers.openrouter] catalog_refresh_hours`;
    ///   `0` means always re-fetch.
    ///
    /// On a fetch failure we fall back to a stale cache when one exists, so a
    /// flaky upstream never leaves callers without a catalog.
    pub async fn catalog(
        &self,
        data_dir:      &Path,
        force:         bool,
        max_age_hours: u64,
    ) -> Result<Catalog, crate::MiraError> {
        if !force {
            if let Some(fresh) = catalog::load_if_fresh(data_dir, max_age_hours) {
                debug!("openrouter: using cached catalog ({} models)", fresh.models.len());
                return Ok(fresh);
            }
        }

        match self.fetch_catalog().await {
            Ok(cat) => {
                if let Err(e) = cat.save(data_dir) {
                    warn!("openrouter: failed to write catalog cache: {e}");
                }
                info!("openrouter: refreshed catalog ({} models)", cat.models.len());
                Ok(cat)
            }
            Err(e) => {
                // Fetch failed — fall back to whatever is on disk, even if stale.
                if let Ok(Some(stale)) = Catalog::load(data_dir) {
                    warn!("openrouter: catalog fetch failed ({e}); serving stale cache");
                    Ok(stale)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn fetch_catalog(&self) -> Result<Catalog, crate::MiraError> {
        let url = format!("{}/models", self.base_url);
        debug!("openrouter: fetching catalog from {url}");

        let mut req = self.client
            .get(&url)
            .header("HTTP-Referer", "https://mira.local")
            .header("X-Title", "MIRA - Multi-tasking Intelligent Responsive Assistant");
        if !self.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.api_key));
        }

        let resp = req.send().await.map_err(|e| crate::MiraError::ProviderError(
            format!("Failed to connect to OpenRouter: {e}")
        ))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("OpenRouter /models returned {status}: {body}")
            ));
        }

        let body = resp.text().await.map_err(|e| crate::MiraError::ProviderError(
            format!("Failed to read OpenRouter /models body: {e}")
        ))?;
        catalog::parse_upstream_json(&body)
    }

    async fn generate_non_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        debug!("Generating with OpenRouter model '{}' (non-streaming)", self.model);

        let outbound = to_outbound(messages);
        let request = OpenRouterRequest {
            model: &self.model,
            messages: outbound,
            stream: false,
            options,
        };

        let url = self.api_url();
        let response = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", "https://mira.local")
            .header("X-Title", "MIRA - Multi-tasking Intelligent Responsive Assistant")
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("Failed to connect to OpenRouter: {}", e)
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("OpenRouter returned {}: {}", status, body)
            ));
        }

        let parsed: OpenRouterResponse = response
            .json()
            .await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("Failed to parse OpenRouter response: {}", e)
            ))?;

        let choice = parsed.choices.into_iter().next().ok_or_else(|| {
            crate::MiraError::ProviderError("OpenRouter returned empty choices".to_string())
        })?;

        let content = choice.message.content.unwrap_or_default();
        let tool_calls = choice.message.tool_calls.map(|calls| {
            calls.into_iter().map(|c| {
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

        Ok(GenerationResponse {
            content,
            tool_calls,
            reasoning:   None,
            usage: parsed.usage.unwrap_or_default(),
            provider_id: ProviderId::OpenRouter(self.model.clone()),
            model_name: self.model.clone(),
            fallback: None,
            })
    }

    async fn do_stream(
        &self,
        messages: &[ChatMessage],
        options: &GenerationOptions,
        on_token: &mut (dyn FnMut(String) + Send),
    ) -> Result<GenerationResponse, crate::MiraError> {
        debug!("Generating with OpenRouter model '{}' (streaming)", self.model);

        let outbound = to_outbound(messages);
        let request = OpenRouterRequest {
            model: &self.model,
            messages: outbound,
            stream: true,
            options,
        };

        let url = self.api_url();
        debug!("Sending streaming request to {}", url);

        let response = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", "https://mira.local")
            .header("X-Title", "MIRA - Multi-tasking Intelligent Responsive Assistant")
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("Failed to connect to OpenRouter: {}", e)
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("OpenRouter returned {}: {}", status, body)
            ));
        }

        let mut content = String::new();
        let mut usage = TokenUsage::default();
        let mut line_buf = String::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| crate::MiraError::ProviderError(
                format!("Stream error: {}", e)
            ))?;

            line_buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = line_buf.find('\n') {
                let json_line = line_buf[..nl].trim_end_matches('\r').to_owned();
                line_buf = line_buf[nl + 1..].to_owned();

                if json_line.is_empty() || json_line == "data: [DONE]" { continue; }

                if json_line.starts_with("data: ") {
                    let json_data = &json_line[6..];
                    if let Ok(stream_resp) = serde_json::from_str::<OpenRouterStreamResponse>(json_data) {
                        for choice in stream_resp.choices {
                            if let Some(ref token_content) = choice.delta.content {
                                content.push_str(token_content);
                                on_token(token_content.clone());
                            }
                        }
                        if let Some(u) = stream_resp.usage {
                            usage = u;
                        }
                    }
                }
            }
        }

        debug!("Streaming generation complete, {} tokens", usage.total_tokens);

        Ok(GenerationResponse {
            content,
            tool_calls: None,
            reasoning: None,
            usage,
            provider_id: ProviderId::OpenRouter(self.model.clone()),
            model_name: self.model.clone(),
            fallback: None,
            })
    }
}

#[async_trait]
impl ModelProvider for OpenRouterProvider {
    fn name(&self) -> &str { "openrouter" }

    async fn generate(
        &self,
        messages: &[ChatMessage],
        options: &GenerationOptions,
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
        let url = format!("{}/models", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                warn!("OpenRouter health check failed: {}", e);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openrouter_has_do_stream_signature() {
        fn _assert_has_stream(
            p: &OpenRouterProvider,
            msgs: &[ChatMessage],
            opts: &GenerationOptions,
        ) {
            let mut cb = |_: String| {};
            let _ = p.do_stream(msgs, opts, &mut cb);
        }
    }

    #[test]
    fn test_from_env_returns_none_without_key() {
        unsafe { std::env::remove_var("OPENROUTER_API_KEY"); }
        assert!(OpenRouterProvider::from_env("gpt-4".to_string()).is_none());
    }
}
