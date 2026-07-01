// SPDX-License-Identifier: AGPL-3.0-or-later

// src/image/backend/automatic1111.rs
//! Local Stable Diffusion via the **Automatic1111 / SD WebUI** API
//! (`POST /sdapi/v1/txt2img`). Synchronous: the call returns base64 PNG(s)
//! directly. No API key.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};

use super::super::{ImageBackend, ImageError, ImageOutput, ImageRequest};
use crate::config::Automatic1111Config;

pub struct Automatic1111Backend {
    base_url:        String,
    model:           String,
    steps:           u32,
    sampler:         String,
    cfg_scale:       f32,
    def_width:       u32,
    def_height:      u32,
    negative_prompt: String,
    http:            reqwest::Client,
}

impl Automatic1111Backend {
    pub fn new(cfg: &Automatic1111Config) -> Self {
        let http = reqwest::Client::builder()
            // txt2img on a busy local GPU can take a while.
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        Self {
            base_url:        cfg.base_url.trim().trim_end_matches('/').to_string(),
            model:           cfg.model.trim().to_string(),
            steps:           cfg.steps.max(1),
            sampler:         cfg.sampler.clone(),
            cfg_scale:       cfg.cfg_scale,
            def_width:       cfg.width.max(64),
            def_height:      cfg.height.max(64),
            negative_prompt: cfg.negative_prompt.clone(),
            http,
        }
    }
}

#[async_trait]
impl ImageBackend for Automatic1111Backend {
    fn name(&self) -> &'static str { "automatic1111" }
    fn enabled(&self) -> bool { !self.base_url.is_empty() }

    async fn generate(&self, req: &ImageRequest) -> Result<ImageOutput, ImageError> {
        let width  = if req.width  > 0 { req.width }  else { self.def_width };
        let height = if req.height > 0 { req.height } else { self.def_height };
        let negative = req.negative_prompt.clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.negative_prompt.clone());

        let mut body = json!({
            "prompt":          req.prompt,
            "negative_prompt": negative,
            "steps":           self.steps,
            "width":           width,
            "height":          height,
            "cfg_scale":       self.cfg_scale,
            "sampler_name":    self.sampler,
            "seed":            req.seed.unwrap_or(-1),
            "batch_size":      1,
            "n_iter":          1,
        });
        // Optionally switch the checkpoint for this call only.
        let model = req.model.as_deref().filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.model.clone());
        if !model.is_empty() {
            body["override_settings"] = json!({ "sd_model_checkpoint": model });
            body["override_settings_restore_afterwards"] = json!(true);
        }

        let resp = self.http
            .post(format!("{}/sdapi/v1/txt2img", self.base_url))
            .json(&body)
            .send().await
            .map_err(|e| ImageError::Backend(format!(
                "Automatic1111 unreachable at {}: {}", self.base_url, e.without_url()
            )))?;
        let status = resp.status();
        let payload: Value = resp.json().await
            .map_err(|e| ImageError::Backend(format!("bad txt2img response: {e}")))?;
        if !status.is_success() {
            let msg = payload.get("error").and_then(Value::as_str)
                .or_else(|| payload.get("detail").and_then(Value::as_str))
                .unwrap_or("unknown error");
            return Err(ImageError::Backend(format!("txt2img failed ({status}): {msg}")));
        }

        let b64 = payload.get("images").and_then(|i| i.get(0)).and_then(Value::as_str)
            .ok_or_else(|| ImageError::Backend("txt2img returned no images".into()))?;
        // A1111 may prefix a data URI; strip it if present.
        let b64 = b64.split(',').next_back().unwrap_or(b64);
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)
            .map_err(|e| ImageError::Backend(format!("bad image base64: {e}")))?;
        Ok(ImageOutput { bytes, ext: "png".into(), note: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real end-to-end against a local SD WebUI / Forge (needs `--api --listen`).
    // Ignored. Run with:
    //   A1111_URL=http://windows-host:7860 cargo test --lib \
    //     image::backend::automatic1111::tests::generates -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn generates_against_local_webui() {
        let base = std::env::var("A1111_URL").unwrap_or_else(|_| "http://windows-host:7860".into());
        let cfg = Automatic1111Config {
            enabled: true,
            base_url: base,
            model: String::new(), // use whatever checkpoint is loaded
            steps: 8,
            sampler: "Euler a".into(),
            width: 512,
            height: 512,
            cfg_scale: 7.0,
            negative_prompt: "blurry, low quality".into(),
        };
        let be = Automatic1111Backend::new(&cfg);
        let req = ImageRequest {
            prompt: "a serene mountain lake at sunrise, photorealistic".into(),
            negative_prompt: None, model: None, width: 512, height: 512, seed: Some(7),
        };
        let out = be.generate(&req).await.expect("a1111 generate");
        eprintln!("Automatic1111/Forge → {} bytes, ext={}", out.bytes.len(), out.ext);
        assert!(out.bytes.len() > 1000);
        assert_eq!(&out.bytes[..4], b"\x89PNG", "expected a PNG");
    }
}
