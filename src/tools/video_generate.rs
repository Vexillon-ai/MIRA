// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/video_generate.rs

//! `video_generate` — turn a text prompt into a short video via the OpenAI
//! Videos (Sora) API, or an OpenAI-compatible endpoint configured under
//! `providers.openai`.
//!
//! Unlike `image_generate`, video generation is an **asynchronous** job: we
//! `POST /videos` to enqueue, poll `GET /videos/{id}` until it completes (or
//! fails / times out), then download the rendered MP4 from
//! `GET /videos/{id}/content`. The bytes land in the content-addressed
//! [`ArtifactStore`] and we return a markdown ref (`![alt](/api/artifacts/
//! <sha>.mp4)`) — the chat UI's `img` renderer switches on the `.mp4`
//! extension and shows a real `<video controls>` player, so no extra web
//! plumbing is needed (it rides the same path as audio/MCP media artifacts).
//!
//! Network tier; enabled only when an OpenAI key resolves (config or
//! `OPENAI_API_KEY`). One provider for now (closes the "no video generation"
//! gap — the last sub-gap under Tier-1 #5); other providers can follow the
//! same shape.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{info, warn};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::artifacts::ArtifactStore;
use crate::config::MiraConfig;
use crate::MiraError;

/// How long we'll wait for a render before giving up. Sora jobs are typically
/// tens of seconds to a couple of minutes; this is the ceiling, not the norm.
const MAX_POLL: Duration = Duration::from_secs(300);
/// Gap between status polls.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct VideoGenerateTool {
    api_key: String,
    base_url: String,
    default_model: String,
    artifacts: Arc<ArtifactStore>,
    http: reqwest::Client,
}

impl VideoGenerateTool {
    /// Resolve the OpenAI key + endpoint from config (falling back to the
    /// `OPENAI_API_KEY` env var), and the shared artifact store.
    pub fn new(config: &MiraConfig, artifacts: Arc<ArtifactStore>) -> Self {
        let oa = &config.providers.openai;
        let api_key = oa
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| std::env::var("OPENAI_API_KEY").unwrap_or_default());
        let base_url = {
            let b = oa.base_url.trim().trim_end_matches('/');
            if b.is_empty() { "https://api.openai.com/v1".to_string() } else { b.to_string() }
        };
        // Per-request timeout (each create/poll/download call is quick); the
        // overall render budget is enforced by the poll loop, not this.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self { api_key, base_url, default_model: "sora-2".into(), artifacts, http }
    }

    /// Pull a human-readable error message out of an API error envelope.
    fn err_message(payload: &Value) -> String {
        payload
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string()
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
         player (with controls/download). Not for editing an existing video."
    }

    fn tier(&self) -> Tier {
        Tier::Network
    }

    fn enabled(&self) -> bool {
        !self.api_key.is_empty()
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
                "model": {
                    "type": "string",
                    "description": "Video model (e.g. sora-2, sora-2-pro). Defaults to sora-2."
                },
                "size": {
                    "type": "string",
                    "enum": ["1280x720", "720x1280", "1792x1024", "1024x1792"],
                    "description": "Frame size (width x height). Landscape/portrait HD by default; the larger sizes need sora-2-pro. Defaults to 1280x720."
                },
                "seconds": {
                    "type": "string",
                    "enum": ["4", "8", "12"],
                    "description": "Clip length in seconds. Defaults to 4."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        if self.api_key.is_empty() {
            return Ok(ToolResult::failure(
                "video_generate is unavailable — no OpenAI API key configured \
                 (set providers.openai.api_key or OPENAI_API_KEY).",
            ));
        }
        let Some(prompt) = args.get("prompt").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        else {
            return Ok(ToolResult::failure("video_generate: `prompt` is required."));
        };
        let model = args.get("model").and_then(Value::as_str).unwrap_or(&self.default_model);
        let size = args.get("size").and_then(Value::as_str).unwrap_or("1280x720");
        let seconds = args.get("seconds").and_then(Value::as_str).unwrap_or("4");

        // 1. Enqueue the render job.
        let body = json!({ "model": model, "prompt": prompt, "size": size, "seconds": seconds });
        let resp = self
            .http
            .post(format!("{}/videos", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| MiraError::ToolError(format!("video_generate request failed: {e}")))?;
        let status = resp.status();
        let payload: Value = resp
            .json()
            .await
            .map_err(|e| MiraError::ToolError(format!("video_generate: bad response: {e}")))?;
        if !status.is_success() {
            return Ok(ToolResult::failure(format!(
                "video_generate API error ({status}): {}",
                Self::err_message(&payload)
            )));
        }
        let Some(job_id) = payload.get("id").and_then(Value::as_str).map(str::to_string) else {
            return Ok(ToolResult::failure("video_generate: response carried no job id."));
        };
        info!("video_generate: enqueued job {job_id} (model={model}, {size}, {seconds}s)");

        // 2. Poll until the job reaches a terminal state or we hit the budget.
        let started = Instant::now();
        loop {
            let job: Value = self
                .http
                .get(format!("{}/videos/{job_id}", self.base_url))
                .bearer_auth(&self.api_key)
                .send()
                .await
                .map_err(|e| MiraError::ToolError(format!("video_generate: poll failed: {e}")))?
                .json()
                .await
                .map_err(|e| MiraError::ToolError(format!("video_generate: bad poll response: {e}")))?;

            let state = job.get("status").and_then(Value::as_str).unwrap_or("");
            match state {
                "completed" => break,
                "failed" | "cancelled" => {
                    let why = job
                        .get("error")
                        .map(Self::err_message)
                        .unwrap_or_else(|| format!("job {state}"));
                    return Ok(ToolResult::failure(format!("video_generate: render {state} — {why}")));
                }
                _ => {
                    if started.elapsed() >= MAX_POLL {
                        return Ok(ToolResult::failure(format!(
                            "video_generate: timed out after {}s (job {job_id} still {state}). \
                             The render may still finish server-side; try again shortly.",
                            MAX_POLL.as_secs()
                        )));
                    }
                    if let Some(p) = job.get("progress").and_then(Value::as_i64) {
                        info!("video_generate: job {job_id} {state} ({p}%)");
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }

        // 3. Download the rendered MP4.
        let dl = self
            .http
            .get(format!("{}/videos/{job_id}/content", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| MiraError::ToolError(format!("video_generate: download failed: {e}")))?;
        if !dl.status().is_success() {
            let s = dl.status();
            warn!("video_generate: content fetch returned {s} for job {job_id}");
            return Ok(ToolResult::failure(format!(
                "video_generate: could not download rendered video (HTTP {s})."
            )));
        }
        let bytes = dl
            .bytes()
            .await
            .map_err(|e| MiraError::ToolError(format!("video_generate: read video: {e}")))?
            .to_vec();
        if bytes.is_empty() {
            return Ok(ToolResult::failure("video_generate: downloaded an empty video."));
        }

        // 4. Store + return an inline player.
        let id = self
            .artifacts
            .save_bytes(&bytes, "mp4")
            .map_err(|e| MiraError::ToolError(format!("video_generate: store artifact: {e}")))?;
        info!(
            "video_generate: produced {} ({} bytes, model={model}, {}s elapsed)",
            id.filename(),
            bytes.len(),
            started.elapsed().as_secs()
        );
        Ok(ToolResult::success(id.markdown_image("Generated video")))
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
        // Avoid the env var leaking into the test from the dev shell.
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
        assert!(r.error.unwrap().contains("no OpenAI API key"));
    }

    #[tokio::test]
    async fn execute_requires_prompt() {
        let r = tool(Some("sk-test")).execute(serde_json::json!({})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("prompt"));
    }
}
