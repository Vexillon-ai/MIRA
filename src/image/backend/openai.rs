// SPDX-License-Identifier: AGPL-3.0-or-later

// src/image/backend/openai.rs
//! OpenAI Images (or an OpenAI-compatible endpoint) backend — the cloud
//! default. Key/endpoint come from `providers.openai`. This is the original
//! `image_generate` logic, lifted behind the [`ImageBackend`] trait.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};

use super::super::{ImageBackend, ImageError, ImageOutput, ImageRequest};
use crate::config::OpenAiConfig;

pub struct OpenAiImageBackend {
    api_key:       String,
    base_url:      String,
    default_model: String,
    http:          reqwest::Client,
}

impl OpenAiImageBackend {
    pub fn new(oa: &OpenAiConfig, default_model: &str) -> Self {
        let api_key = oa.api_key.as_deref().map(str::trim).filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| std::env::var("OPENAI_API_KEY").unwrap_or_default());
        let base_url = {
            let b = oa.base_url.trim().trim_end_matches('/');
            if b.is_empty() { "https://api.openai.com/v1".to_string() } else { b.to_string() }
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        let default_model = if default_model.trim().is_empty() { "dall-e-3" } else { default_model.trim() };
        Self { api_key, base_url, default_model: default_model.to_string(), http }
    }

    fn size_string(req: &ImageRequest) -> String {
        // OpenAI takes a "WxH" string from a fixed set; pass through what we have.
        format!("{}x{}", req.width, req.height)
    }
}

#[async_trait]
impl ImageBackend for OpenAiImageBackend {
    fn name(&self) -> &'static str { "openai" }
    fn enabled(&self) -> bool { !self.api_key.is_empty() }

    async fn generate(&self, req: &ImageRequest) -> Result<ImageOutput, ImageError> {
        let model = req.model.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.default_model);
        let mut body = json!({
            "model": model, "prompt": req.prompt, "n": 1, "size": Self::size_string(req),
        });
        // gpt-image-1 always returns b64_json and rejects response_format;
        // dall-e-* needs it set to get bytes rather than a URL.
        if !model.starts_with("gpt-image") {
            body["response_format"] = json!("b64_json");
        }

        let resp = self.http
            .post(format!("{}/images/generations", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send().await
            .map_err(|e| ImageError::Backend(format!("request failed: {}", e.without_url())))?;
        let status = resp.status();
        let payload: Value = resp.json().await
            .map_err(|e| ImageError::Backend(format!("bad response: {e}")))?;
        if !status.is_success() {
            let msg = payload.get("error").and_then(|e| e.get("message"))
                .and_then(Value::as_str).unwrap_or("unknown error");
            return Err(ImageError::Backend(format!("API error ({status}): {msg}")));
        }

        let first = payload.get("data").and_then(|d| d.get(0));
        let bytes: Vec<u8> = if let Some(b64) = first.and_then(|d| d.get("b64_json")).and_then(Value::as_str) {
            base64::engine::general_purpose::STANDARD.decode(b64)
                .map_err(|e| ImageError::Backend(format!("bad base64: {e}")))?
        } else if let Some(url) = first.and_then(|d| d.get("url")).and_then(Value::as_str) {
            self.http.get(url).send().await
                .map_err(|e| ImageError::Backend(format!("fetch image: {}", e.without_url())))?
                .bytes().await
                .map_err(|e| ImageError::Backend(format!("read image: {e}")))?
                .to_vec()
        } else {
            return Err(ImageError::Backend("response carried no image data".into()));
        };

        let note = first.and_then(|d| d.get("revised_prompt")).and_then(Value::as_str)
            .map(|p| format!("Prompt used: {p}"));
        Ok(ImageOutput { bytes, ext: "png".into(), note })
    }
}
