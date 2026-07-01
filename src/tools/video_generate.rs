// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/video_generate.rs

//! `video_generate` — turn a text prompt into a short video via whichever
//! backend is configured: OpenAI Videos (Sora), local ComfyUI (a video
//! workflow), or local WAN2GP. The tool is backend-agnostic — it dispatches
//! through [`crate::video::VideoService`].
//!
//! The rendered bytes land in the content-addressed [`ArtifactStore`] and we
//! return a markdown ref (`![alt](/api/artifacts/<sha>.mp4)`) — the chat UI's
//! renderer switches on the video extension and shows a real `<video controls>`
//! player, so no extra web plumbing is needed (same path as audio/MCP media).
//!
//! Network tier; enabled when at least one video backend is configured.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::artifacts::ArtifactStore;
use crate::config::MiraConfig;
use crate::video::{VideoRequest, VideoService};
use crate::MiraError;

pub struct VideoGenerateTool {
    service:   Arc<VideoService>,
    artifacts: Arc<ArtifactStore>,
}

impl VideoGenerateTool {
    pub fn new(config: &MiraConfig, artifacts: Arc<ArtifactStore>) -> Self {
        let service = Arc::new(VideoService::from_config(&config.video, &config.providers.openai));
        Self { service, artifacts }
    }

    pub fn from_service(service: Arc<VideoService>, artifacts: Arc<ArtifactStore>) -> Self {
        Self { service, artifacts }
    }
}

#[async_trait]
impl Tool for VideoGenerateTool {
    fn name(&self) -> &str {
        "video_generate"
    }

    fn description(&self) -> &str {
        "Generate a short video clip from a text prompt and return it inline. \
         Use for animations, b-roll, motion mockups, etc. Rendering is \
         asynchronous and can take up to a few minutes — call this once and \
         wait for the result; it returns a markdown video the user sees as a \
         player (with controls/download). Optional `backend` (openai | comfyui \
         | wan2gp) and `negative_prompt` are honoured by local backends. Not \
         for editing an existing video."
    }

    fn tier(&self) -> Tier {
        Tier::Network
    }

    fn enabled(&self) -> bool {
        self.service.any_enabled()
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Detailed description of the video to generate, including motion and camera."
                },
                "negative_prompt": {
                    "type": "string",
                    "description": "Things to avoid (used by local ComfyUI/WAN2GP backends; ignored by OpenAI)."
                },
                "model": {
                    "type": "string",
                    "description": "Backend-specific model (e.g. sora-2 / sora-2-pro for OpenAI). Defaults to the backend's configured model."
                },
                "size": {
                    "type": "string",
                    "description": "Frame size as WIDTHxHEIGHT, e.g. 1280x720. Defaults to the backend's configured size."
                },
                "seconds": {
                    "type": "integer",
                    "description": "Clip length in seconds. Defaults to the backend's configured length."
                },
                "backend": {
                    "type": "string",
                    "description": "Which video backend to use: openai | comfyui | wan2gp. Omit for the configured default."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        if !self.service.any_enabled() {
            return Ok(ToolResult::failure(
                "video_generate is unavailable — no video backend is configured. \
                 Set an OpenAI key (providers.openai.api_key), or enable a local \
                 backend under [video] (comfyui / wan2gp).",
            ));
        }
        let Some(prompt) = args.get("prompt").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        else {
            return Ok(ToolResult::failure("video_generate: `prompt` is required."));
        };
        let backend = args.get("backend").and_then(Value::as_str);
        let (width, height) = VideoRequest::dims_from_size(
            args.get("size").and_then(Value::as_str), (1280, 720));
        let seconds = args.get("seconds")
            .and_then(|v| v.as_u64().map(|n| n as u32)
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
            .unwrap_or(0); // 0 → backend default

        let req = VideoRequest {
            prompt:          prompt.to_string(),
            negative_prompt: args.get("negative_prompt").and_then(Value::as_str).map(str::to_string),
            model:           args.get("model").and_then(Value::as_str).map(str::to_string),
            width,
            height,
            seconds,
            seed:            args.get("seed").and_then(Value::as_i64),
        };

        info!("video_generate: backend={} {}x{} {}s", backend.unwrap_or("default"), width, height, seconds);
        let out = match self.service.generate(backend, &req).await {
            Ok(o)  => o,
            Err(e) => return Ok(ToolResult::failure(format!("video_generate: {e}"))),
        };

        let id = self
            .artifacts
            .save_bytes(&out.bytes, &out.ext)
            .map_err(|e| MiraError::ToolError(format!("video_generate: store artifact: {e}")))?;
        info!("video_generate: produced {} ({} bytes)", id.filename(), out.bytes.len());

        let mut md = id.markdown_image("Generated video");
        if let Some(note) = out.note {
            md.push_str(&format!("\n\n_{note}_"));
        }
        Ok(ToolResult::success(md))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(key: Option<&str>) -> VideoGenerateTool {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(ArtifactStore::new(dir.path()).unwrap());
        let mut cfg = MiraConfig::default();
        cfg.providers.openai.api_key = key.map(str::to_string);
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        VideoGenerateTool::new(&cfg, store)
    }

    #[test]
    fn disabled_without_key_enabled_with_key() {
        assert!(!tool(None).enabled());
        assert!(tool(Some("sk-test")).enabled());
    }

    #[test]
    fn schema_requires_prompt_and_tier_is_network() {
        let t = tool(Some("sk-test"));
        assert_eq!(t.name(), "video_generate");
        assert_eq!(t.tier(), Tier::Network);
        let schema = t.args_schema();
        assert_eq!(schema["required"][0], "prompt");
        assert!(schema["properties"]["seconds"].is_object());
        assert!(schema["properties"]["size"].is_object());
    }

    #[tokio::test]
    async fn execute_without_key_fails_gracefully() {
        let r = tool(None).execute(serde_json::json!({ "prompt": "a cat" })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("no video backend is configured"));
    }

    #[tokio::test]
    async fn execute_requires_prompt() {
        let r = tool(Some("sk-test")).execute(serde_json::json!({})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("prompt"));
    }
}
