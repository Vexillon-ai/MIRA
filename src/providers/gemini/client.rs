// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/gemini/client.rs

//! Gemini `:generateContent` / `:streamGenerateContent` HTTP client.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, ClientBuilder};
use std::time::Duration;
use tracing::{debug, warn};

use crate::providers::ModelProvider;
use crate::providers::gemini::wire::{
    GenerateContentRequest, GenerateContentResponse, GenerationConfig, ToolConfig,
    convert_messages, convert_response_parts, convert_tool_choice, convert_tool_specs,
};
use crate::types::{
    ChatMessage, GenerationOptions, GenerationResponse, ProviderId, TokenUsage,
    ToolCall,
};

/// Gemini API version. `v1beta` carries the current feature surface
/// (newer models, tool config, system instructions); `v1` exists but
/// is missing those, so it's not a viable target for MIRA.
const GEMINI_API_VERSION: &str = "v1beta";

pub struct GeminiProvider {
    http:     Client,
    api_key:  String,
    model:    String,
    /// Base URL without trailing slash, e.g.
    /// `https://generativelanguage.googleapis.com`.
    base_url: String,
}

impl GeminiProvider {
    pub fn new(api_key: String, model: String, base_url: String, timeout_secs: u64) -> Self {
        let http = ClientBuilder::new()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .build()
            .expect("gemini: failed to build HTTP client");
        Self { http, api_key, model, base_url }
    }

    pub fn model_name(&self) -> &str { &self.model }

    /// Fetch the model list from `/v1beta/models`. Gemini returns
    /// `{"models": [{"name", "displayName", "inputTokenLimit",
    /// "outputTokenLimit", "supportedGenerationMethods"}]}`. We
    /// only keep models that support `generateContent` (skip
    /// embedding-only and image-only entries) and capture the
    /// input-token limit as the context window. Pricing comes from
    /// the overlay.
    pub async fn fetch_model_ids(&self) -> Result<Vec<crate::providers::catalog::ModelEntry>, crate::MiraError> {
        #[derive(serde::Deserialize)]
        struct ListResponse { models: Vec<ModelRow> }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ModelRow {
            name: String,
            #[serde(default)]
            display_name: Option<String>,
            #[serde(default)]
            input_token_limit: Option<u32>,
            #[serde(default)]
            supported_generation_methods: Vec<String>,
        }
        let url = format!("{}/{}/models", self.base_url, GEMINI_API_VERSION);
        let mut rb = self.http.get(&url);
        if !self.api_key.is_empty() {
            rb = rb.header("x-goog-api-key", &self.api_key);
        }
        let resp = rb.send().await.map_err(|e| crate::MiraError::ProviderError(
            format!("gemini: catalog fetch connect failed: {e}")
        ))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body   = resp.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("gemini: catalog fetch {status}: {body}")
            ));
        }
        let body = resp.text().await.map_err(|e| crate::MiraError::ProviderError(
            format!("gemini: catalog read body failed: {e}")
        ))?;
        let parsed: ListResponse = serde_json::from_str(&body)
            .map_err(|e| crate::MiraError::ProviderError(
                format!("gemini: catalog parse failed: {e}")
            ))?;
        Ok(parsed.models.into_iter()
            .filter(|m| m.supported_generation_methods.iter().any(|s| s == "generateContent"))
            .map(|m| crate::providers::catalog::ModelEntry {
                id:                  m.name,
                display_name:        m.display_name,
                context_window:      m.input_token_limit,
                input_price_per_1m:  None,
                output_price_per_1m: None,
                notes:               None,
            }).collect())
    }

    /// Bare model id for request paths. Gemini's catalog (`/v1beta/models`)
    /// returns resource names as `models/<id>`, and users paste them that way,
    /// but our REST paths already include `/models/` — so a configured
    /// `models/<id>` would double up to `.../models/models/<id>` (404, provider
    /// shows red). Strip a leading `models/` so both forms work.
    fn model_path_id(&self) -> &str {
        self.model.strip_prefix("models/").unwrap_or(&self.model)
    }

    fn endpoint_url(&self, method: &str) -> String {
        // method is `generateContent` or `streamGenerateContent?alt=sse`.
        format!(
            "{}/{}/models/{}:{}",
            self.base_url, GEMINI_API_VERSION, self.model_path_id(), method,
        )
    }

    fn apply_headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb.header("content-type", "application/json");
        if !self.api_key.is_empty() {
            rb = rb.header("x-goog-api-key", &self.api_key);
        }
        rb
    }

    fn provider_id(&self) -> ProviderId { ProviderId::Gemini(self.model.clone()) }

    fn build_request(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> GenerateContentRequest {
        let (system_instruction, contents) = convert_messages(messages);

        // Tools + tool_config. `tool_choice = "none"` signals "omit
        // tools entirely" — Gemini's NONE mode still requires the
        // tools array, but omitting both is what users mean when they
        // pass "none" upstream.
        let mut tools = options.tools.as_ref()
            .filter(|s| !s.is_empty())
            .and_then(|s| convert_tool_specs(s));
        let mut tool_config = None;
        if let Some(tc) = options.tool_choice.as_ref() {
            let (cfg, omit) = convert_tool_choice(tc);
            if omit {
                tools = None;
            }
            tool_config = cfg.map(|c| ToolConfig { function_calling_config: c });
        }

        let temperature = if options.temperature > 0.0 {
            Some(options.temperature)
        } else { None };

        let mut generation_config = GenerationConfig::default();
        let mut has_gen_config = false;
        if let Some(t) = temperature      { generation_config.temperature       = Some(t);  has_gen_config = true; }
        if let Some(p) = options.top_p    { generation_config.top_p             = Some(p);  has_gen_config = true; }
        if let Some(m) = options.max_tokens { generation_config.max_output_tokens = Some(m); has_gen_config = true; }
        if let Some(s) = options.stop_sequences.as_ref() {
            if !s.is_empty() {
                generation_config.stop_sequences = Some(s.clone());
                has_gen_config = true;
            }
        }

        GenerateContentRequest {
            contents,
            system_instruction,
            tools,
            tool_config,
            generation_config: if has_gen_config { Some(generation_config) } else { None },
        }
    }

    async fn generate_non_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        let url     = self.endpoint_url("generateContent");
        let request = self.build_request(messages, options);
        debug!("gemini: POST {url} (non-streaming, model={})", self.model);

        let response = self.apply_headers(self.http.post(&url).json(&request))
            .send().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("gemini: connect failed: {e}")
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body   = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("gemini: {status} — {body}")
            ));
        }

        let parsed: GenerateContentResponse = response.json().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("gemini: parse failed: {e}")
            ))?;

        let (content, tool_calls) = parsed
            .candidates
            .into_iter()
            .next()
            .and_then(|c| c.content)
            .map(|c| convert_response_parts(c.parts))
            .unwrap_or_else(|| (String::new(), None));

        let usage = parsed.usage_metadata.map(|u| TokenUsage {
            prompt_tokens:     u.prompt_token_count,
            completion_tokens: u.candidates_token_count,
            total_tokens:      u.prompt_token_count + u.candidates_token_count,
        }).unwrap_or_default();

        Ok(GenerationResponse {
            content,
            tool_calls,
            reasoning:   None,
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
        // SSE form. The alternative `:streamGenerateContent` without
        // `?alt=sse` returns a JSON array which is awkward to parse
        // incrementally; SSE gives us one well-bounded JSON object
        // per frame.
        let url     = self.endpoint_url("streamGenerateContent?alt=sse");
        let request = self.build_request(messages, options);
        debug!("gemini: POST {url} (streaming, model={})", self.model);

        let response = self.apply_headers(self.http.post(&url).json(&request))
            .send().await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("gemini: connect failed: {e}")
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body   = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("gemini: {status} — {body}")
            ));
        }

        let mut content    = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage      = TokenUsage::default();
        let mut line_buf   = String::new();
        let mut stream     = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| crate::MiraError::ProviderError(
                format!("gemini: stream error: {e}")
            ))?;
            line_buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = line_buf.find('\n') {
                let line = line_buf[..nl].trim_end_matches('\r').to_owned();
                line_buf = line_buf[nl + 1..].to_owned();

                let Some(json_data) = line.strip_prefix("data: ") else { continue; };
                if json_data.trim().is_empty() { continue; }
                if json_data == "[DONE]" { continue; }

                let frame: GenerateContentResponse = match serde_json::from_str(json_data) {
                    Ok(f) => f,
                    Err(e) => {
                        debug!("gemini: skipping unparseable frame ({e}): {json_data}");
                        continue;
                    }
                };

                if let Some(c) = frame.candidates.into_iter().next() {
                    if let Some(ct) = c.content {
                        // Each frame's parts are an incremental
                        // continuation. Append text; collect any
                        // functionCall parts (they arrive complete in
                        // a single frame — Gemini doesn't split
                        // args mid-stream).
                        let (delta_text, delta_calls) = convert_response_parts(ct.parts);
                        if !delta_text.is_empty() {
                            content.push_str(&delta_text);
                            on_token(delta_text);
                        }
                        if let Some(mut calls) = delta_calls {
                            tool_calls.append(&mut calls);
                        }
                    }
                }

                // usageMetadata only populates on the last frame; we
                // overwrite each time it appears so the final value
                // wins regardless of ordering.
                if let Some(u) = frame.usage_metadata {
                    usage = TokenUsage {
                        prompt_tokens:     u.prompt_token_count,
                        completion_tokens: u.candidates_token_count,
                        total_tokens:      u.prompt_token_count + u.candidates_token_count,
                    };
                }
            }
        }

        let tool_calls = if tool_calls.is_empty() { None } else { Some(tool_calls) };

        Ok(GenerationResponse {
            content,
            tool_calls,
            reasoning:   None,
            usage,
            provider_id: self.provider_id(),
            model_name:  self.model.clone(),
            fallback: None,
            })
    }
}

#[async_trait]
impl ModelProvider for GeminiProvider {
    fn name(&self) -> &str { "gemini" }

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
        // GET /v1beta/models/{model} returns the model card if the
        // key is valid + the model is reachable. Cheaper than a full
        // generate call and doesn't burn output tokens.
        let url = format!("{}/{}/models/{}", self.base_url, GEMINI_API_VERSION, self.model_path_id());
        let mut rb = self.http.get(&url);
        if !self.api_key.is_empty() {
            rb = rb.header("x-goog-api-key", &self.api_key);
        }
        match rb.send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                warn!("gemini: health check failed: {e}");
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

    fn provider() -> GeminiProvider {
        GeminiProvider::new(
            "test-key".into(),
            "gemini-2.5-pro".into(),
            "https://generativelanguage.googleapis.com".into(),
            30,
        )
    }

    #[test]
    fn strips_leading_models_prefix_from_url() {
        // A model id pasted with Gemini's `models/` resource prefix (as the
        // catalog returns it) must NOT produce a doubled `/models/models/` path.
        let p = GeminiProvider::new(
            "k".into(), "models/gemini-flash-lite-latest".into(),
            "https://generativelanguage.googleapis.com".into(), 30,
        );
        let url = p.endpoint_url("generateContent");
        assert!(url.contains("/v1beta/models/gemini-flash-lite-latest:generateContent"), "got: {url}");
        assert!(!url.contains("models/models/"), "doubled prefix: {url}");
        // A bare id is unchanged.
        assert_eq!(provider().model_path_id(), "gemini-2.5-pro");
    }

    #[test]
    fn name_and_provider_id() {
        let p = provider();
        assert_eq!(p.name(),       "gemini");
        assert_eq!(p.model_name(), "gemini-2.5-pro");
        match p.provider_id() {
            ProviderId::Gemini(m) => assert_eq!(m, "gemini-2.5-pro"),
            other => panic!("expected Gemini, got {other:?}"),
        }
    }

    #[test]
    fn endpoint_url_embeds_model_and_method() {
        let p = provider();
        assert_eq!(
            p.endpoint_url("generateContent"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent",
        );
        assert_eq!(
            p.endpoint_url("streamGenerateContent?alt=sse"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
        );
    }

    #[test]
    fn headers_carry_api_key() {
        let p = provider();
        let req = p.apply_headers(p.http.post("http://localhost/test"))
            .build().unwrap();
        assert_eq!(
            req.headers().get("x-goog-api-key").and_then(|v| v.to_str().ok()),
            Some("test-key"),
        );
        // Gemini doesn't use Bearer auth in this code path.
        assert!(req.headers().get("authorization").is_none());
    }

    #[test]
    fn empty_api_key_omits_header() {
        let p = GeminiProvider::new(
            String::new(), "gemini-2.5-pro".into(),
            "http://x".into(), 30,
        );
        let req = p.apply_headers(p.http.post("http://localhost/test"))
            .build().unwrap();
        assert!(req.headers().get("x-goog-api-key").is_none());
    }

    #[test]
    fn build_request_lifts_system_and_strips_it() {
        let p = provider();
        let msgs = vec![
            ChatMessage::system("You are MIRA."),
            ChatMessage::user("hi"),
        ];
        let opts = GenerationOptions::default();
        let req  = p.build_request(&msgs, &opts);
        assert!(req.system_instruction.is_some());
        assert_eq!(req.contents.len(), 1);
        assert_eq!(req.contents[0].role, "user");
    }

    #[test]
    fn build_request_only_emits_generation_config_when_nonempty() {
        let p = provider();
        let msgs = [ChatMessage::user("hi")];
        let req  = p.build_request(&msgs, &GenerationOptions::default());
        // GenerationOptions::default has temperature = 0.7 which is
        // > 0 → generation_config is populated.
        assert!(req.generation_config.is_some(),
            "default temperature should be reflected in generation_config");
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
        let req  = p.build_request(&msgs, &opts);
        assert!(req.tools.is_none(),
            "tool_choice='none' must omit tools entirely on Gemini too");
    }

    #[test]
    fn build_request_named_tool_choice_sets_any_with_allowlist() {
        let p = provider();
        let mut opts = GenerationOptions::default();
        opts.tools = Some(vec![
            crate::types::ToolSpec::function(
                "search", "s", serde_json::json!({"type": "object"}),
            ),
        ]);
        opts.tool_choice = Some(serde_json::json!(
            {"type": "function", "function": {"name": "search"}}
        ));
        let msgs = [ChatMessage::user("hi")];
        let req  = p.build_request(&msgs, &opts);
        let cfg  = req.tool_config.expect("expected tool_config").function_calling_config;
        assert_eq!(cfg.mode, "ANY");
        assert_eq!(cfg.allowed_function_names.as_deref(), Some(&["search".to_string()][..]));
    }

    #[tokio::test]
    async fn health_check_returns_false_when_unreachable() {
        let p = GeminiProvider::new(
            "test-key".into(),
            "gemini-2.5-pro".into(),
            "http://127.0.0.1:1".into(),
            5,
        );
        assert!(!p.health_check().await);
    }
}
