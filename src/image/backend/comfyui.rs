// SPDX-License-Identifier: AGPL-3.0-or-later

// src/image/backend/comfyui.rs
//! Local **ComfyUI** backend. ComfyUI runs a node-graph "workflow"; generation
//! is asynchronous: `POST /prompt` enqueues a workflow (API format) and returns
//! a `prompt_id`, then we poll `GET /history/{prompt_id}` until the output node
//! reports an image and fetch it from `GET /view`.
//!
//! The workflow is either the operator's own (config `workflow_json`, an
//! API-format graph with placeholder tokens) or a built-in default SD txt2img
//! graph. Tokens: `{{prompt}}` `{{negative}}` `{{seed}}` `{{width}}`
//! `{{height}}` `{{steps}}` `{{cfg}}` `{{ckpt}}`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::super::{ImageBackend, ImageError, ImageOutput, ImageRequest};
use crate::config::ComfyUiConfig;

/// Max wall-clock for one generation (enqueue → image on disk).
const GEN_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

pub struct ComfyUiBackend {
    base_url:        String,
    workflow_json:   String,
    model:           String,
    steps:           u32,
    cfg_scale:       f32,
    def_width:       u32,
    def_height:      u32,
    negative_prompt: String,
    http:            reqwest::Client,
}

impl ComfyUiBackend {
    pub fn new(cfg: &ComfyUiConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60)) // per-request; generation uses polling
            .build()
            .unwrap_or_default();
        Self {
            base_url:        cfg.base_url.trim().trim_end_matches('/').to_string(),
            workflow_json:   cfg.workflow_json.clone(),
            model:           cfg.model.trim().to_string(),
            steps:           cfg.steps.max(1),
            cfg_scale:       cfg.cfg_scale,
            def_width:       cfg.width.max(64),
            def_height:      cfg.height.max(64),
            negative_prompt: cfg.negative_prompt.clone(),
            http,
        }
    }

    /// Resolve a checkpoint name: the configured one, else the first the server
    /// reports under `CheckpointLoaderSimple`.
    async fn resolve_ckpt(&self) -> Result<String, ImageError> {
        if !self.model.is_empty() {
            return Ok(self.model.clone());
        }
        let url = format!("{}/object_info/CheckpointLoaderSimple", self.base_url);
        let info: Value = self.http.get(&url).send().await
            .map_err(|e| ImageError::Backend(format!("ComfyUI unreachable at {}: {}", self.base_url, e.without_url())))?
            .json().await
            .map_err(|e| ImageError::Backend(format!("object_info parse: {e}")))?;
        // …["CheckpointLoaderSimple"]["input"]["required"]["ckpt_name"][0] = [names…]
        info.get("CheckpointLoaderSimple")
            .and_then(|n| n.pointer("/input/required/ckpt_name/0"))
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ImageError::Backend(
                "no checkpoint found — set image.comfyui.model or install a checkpoint".into()))
    }

    /// Build the workflow graph (API format) for this request.
    async fn build_workflow(&self, req: &ImageRequest) -> Result<Value, ImageError> {
        let width  = if req.width  > 0 { req.width }  else { self.def_width };
        let height = if req.height > 0 { req.height } else { self.def_height };
        let negative = req.negative_prompt.clone().filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.negative_prompt.clone());
        let seed = req.seed.filter(|s| *s >= 0).unwrap_or_else(|| (rand::random::<u32>()) as i64);
        let ckpt = self.resolve_ckpt().await?;

        if self.workflow_json.trim().is_empty() {
            // Built-in default SD txt2img graph — values inlined (no escaping risk).
            return Ok(json!({
                "3": { "class_type": "KSampler", "inputs": {
                    "seed": seed, "steps": self.steps, "cfg": self.cfg_scale,
                    "sampler_name": "euler", "scheduler": "normal", "denoise": 1.0,
                    "model": ["4", 0], "positive": ["6", 0], "negative": ["7", 0], "latent_image": ["5", 0] }},
                "4": { "class_type": "CheckpointLoaderSimple", "inputs": { "ckpt_name": ckpt }},
                "5": { "class_type": "EmptyLatentImage", "inputs": { "width": width, "height": height, "batch_size": 1 }},
                "6": { "class_type": "CLIPTextEncode", "inputs": { "text": req.prompt, "clip": ["4", 1] }},
                "7": { "class_type": "CLIPTextEncode", "inputs": { "text": negative, "clip": ["4", 1] }},
                "8": { "class_type": "VAEDecode", "inputs": { "samples": ["3", 0], "vae": ["4", 2] }},
                "9": { "class_type": "SaveImage", "inputs": { "filename_prefix": "MIRA", "images": ["8", 0] }},
            }));
        }

        // Custom template — substitute tokens. String tokens are replaced *with
        // their JSON quotes* by a fully JSON-escaped value; numeric tokens by a
        // bare number, so the result stays valid JSON regardless of prompt text.
        let jstr = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
        let out = self.workflow_json
            .replace("\"{{prompt}}\"",   &jstr(&req.prompt))
            .replace("\"{{negative}}\"", &jstr(&negative))
            .replace("\"{{ckpt}}\"",     &jstr(&ckpt))
            .replace("{{seed}}",   &seed.to_string())
            .replace("{{steps}}",  &self.steps.to_string())
            .replace("{{cfg}}",    &self.cfg_scale.to_string())
            .replace("{{width}}",  &width.to_string())
            .replace("{{height}}", &height.to_string());
        serde_json::from_str(&out)
            .map_err(|e| ImageError::Backend(format!("workflow_json invalid after substitution: {e}")))
    }
}

#[async_trait]
impl ImageBackend for ComfyUiBackend {
    fn name(&self) -> &'static str { "comfyui" }
    fn enabled(&self) -> bool { !self.base_url.is_empty() }

    async fn generate(&self, req: &ImageRequest) -> Result<ImageOutput, ImageError> {
        let workflow = self.build_workflow(req).await?;
        let client_id = uuid::Uuid::new_v4().to_string();

        // 1) Enqueue.
        let enqueue: Value = self.http
            .post(format!("{}/prompt", self.base_url))
            .json(&json!({ "prompt": workflow, "client_id": client_id }))
            .send().await
            .map_err(|e| ImageError::Backend(format!("ComfyUI /prompt failed: {}", e.without_url())))?
            .error_for_status()
            .map_err(|e| ImageError::Backend(format!("ComfyUI /prompt rejected the workflow: {}", e.without_url())))?
            .json().await
            .map_err(|e| ImageError::Backend(format!("/prompt response parse: {e}")))?;
        let prompt_id = enqueue.get("prompt_id").and_then(Value::as_str)
            .ok_or_else(|| ImageError::Backend("/prompt returned no prompt_id".into()))?
            .to_string();

        // 2) Poll history until an output image appears (or error / timeout).
        let start = Instant::now();
        let (filename, subfolder, ftype) = loop {
            if start.elapsed() > GEN_TIMEOUT {
                return Err(ImageError::Timeout(GEN_TIMEOUT.as_secs()));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            let hist: Value = match self.http
                .get(format!("{}/history/{prompt_id}", self.base_url))
                .send().await.and_then(|r| r.error_for_status())
            {
                Ok(r) => r.json().await.map_err(|e| ImageError::Backend(format!("history parse: {e}")))?,
                Err(_) => continue, // transient — keep polling
            };
            let Some(entry) = hist.get(&prompt_id) else { continue };
            // Surface a workflow execution error rather than spinning to timeout.
            if entry.pointer("/status/status_str").and_then(Value::as_str) == Some("error") {
                let msg = entry.pointer("/status/messages")
                    .map(|m| m.to_string()).unwrap_or_else(|| "workflow execution error".into());
                return Err(ImageError::Backend(format!("ComfyUI workflow error: {}", truncate(&msg, 300))));
            }
            // Find the first output node carrying images.
            if let Some(outputs) = entry.get("outputs").and_then(Value::as_object) {
                let img = outputs.values()
                    .filter_map(|n| n.get("images").and_then(Value::as_array))
                    .flatten()
                    .find(|i| i.get("filename").is_some());
                if let Some(img) = img {
                    break (
                        img.get("filename").and_then(Value::as_str).unwrap_or_default().to_string(),
                        img.get("subfolder").and_then(Value::as_str).unwrap_or_default().to_string(),
                        img.get("type").and_then(Value::as_str).unwrap_or("output").to_string(),
                    );
                }
            }
        };
        if filename.is_empty() {
            return Err(ImageError::Backend("ComfyUI produced no image filename".into()));
        }

        // 3) Fetch the rendered image.
        let bytes = self.http
            .get(format!("{}/view", self.base_url))
            .query(&[("filename", filename.as_str()), ("subfolder", subfolder.as_str()), ("type", ftype.as_str())])
            .send().await
            .map_err(|e| ImageError::Backend(format!("/view failed: {}", e.without_url())))?
            .error_for_status()
            .map_err(|e| ImageError::Backend(format!("/view error: {}", e.without_url())))?
            .bytes().await
            .map_err(|e| ImageError::Backend(format!("/view read: {e}")))?
            .to_vec();

        let ext = filename.rsplit('.').next()
            .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("png").to_lowercase();
        Ok(ImageOutput { bytes, ext, note: None })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max).collect::<String>() + "…" }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real end-to-end against a local ComfyUI. Ignored (needs a running server
    // + GPU). Run with:
    //   COMFYUI_URL=http://windows-host:8188 cargo test --lib \
    //     image::backend::comfyui::tests::generates -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn generates_against_local_comfyui() {
        let base = std::env::var("COMFYUI_URL").unwrap_or_else(|_| "http://windows-host:8188".into());
        let model = std::env::var("COMFY_MODEL").unwrap_or_else(|_| "DreamShaper_8_pruned.safetensors".into());
        let dim: u32 = std::env::var("COMFY_DIM").ok().and_then(|s| s.parse().ok()).unwrap_or(512);
        let steps: u32 = std::env::var("COMFY_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
        let cfg = ComfyUiConfig {
            enabled: true,
            base_url: base,
            workflow_json: String::new(),
            model,
            steps,
            width: dim,
            height: dim,
            cfg_scale: 7.0,
            negative_prompt: "blurry, low quality, watermark".into(),
        };
        let be = ComfyUiBackend::new(&cfg);
        let req = ImageRequest {
            prompt: "a neon cyberpunk cat, highly detailed, vibrant".into(),
            negative_prompt: None, model: None, width: dim, height: dim, seed: Some(42),
        };
        let out = be.generate(&req).await.expect("comfyui generate");
        eprintln!("ComfyUI → {} bytes, ext={}", out.bytes.len(), out.ext);
        assert!(out.bytes.len() > 1000, "image should be non-trivial");
        assert_eq!(&out.bytes[..4], b"\x89PNG", "expected a PNG");
    }
}
