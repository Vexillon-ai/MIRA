// SPDX-License-Identifier: AGPL-3.0-or-later

// src/video/backend/openai.rs
//! OpenAI Videos (Sora) backend — async render: `POST /videos` to enqueue,
//! poll `GET /videos/{id}` to completion, download `GET /videos/{id}/content`.
//! Key/endpoint from `providers.openai`. Lifted from the original
//! `video_generate` tool behind the [`VideoBackend`] trait.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::super::{VideoBackend, VideoError, VideoOutput, VideoRequest};
use crate::config::{OpenAiConfig, VideoOpenAiConfig};

/// Overall render budget (enqueue → downloadable). Sora renders run tens of
/// seconds to a couple of minutes; this is the ceiling.
const MAX_POLL: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct OpenAiVideoBackend {
    api_key:         String,
    base_url:        String,
    default_model:   String,
    http:            reqwest::Client,
}

impl OpenAiVideoBackend {
    pub fn new(oa: &OpenAiConfig, vo: &VideoOpenAiConfig) -> Self {
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
        let default_model = if vo.default_model.trim().is_empty() { "sora-2".into() } else { vo.default_model.trim().to_string() };
        Self { api_key, base_url, default_model, http }
    }

    fn err_message(v: &Value) -> String {
        v.get("error").and_then(|e| e.get("message")).and_then(Value::as_str)
            .or_else(|| v.get("message").and_then(Value::as_str))
            .unwrap_or("unknown error").to_string()
    }
}

#[async_trait]
impl VideoBackend for OpenAiVideoBackend {
    fn name(&self) -> &'static str { "openai" }
    fn enabled(&self) -> bool { !self.api_key.is_empty() }

    async fn generate(&self, req: &VideoRequest) -> Result<VideoOutput, VideoError> {
        let model = req.model.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.default_model);
        let size = format!("{}x{}", req.width, req.height);
        let seconds = req.seconds.max(1).to_string();

        // 1) Enqueue.
        let body = json!({ "model": model, "prompt": req.prompt, "size": size, "seconds": seconds });
        let resp = self.http.post(format!("{}/videos", self.base_url))
            .bearer_auth(&self.api_key).json(&body).send().await
            .map_err(|e| VideoError::Backend(format!("request failed: {}", e.without_url())))?;
        let status = resp.status();
        let payload: Value = resp.json().await
            .map_err(|e| VideoError::Backend(format!("bad response: {e}")))?;
        if !status.is_success() {
            return Err(VideoError::Backend(format!("API error ({status}): {}", Self::err_message(&payload))));
        }
        let job_id = payload.get("id").and_then(Value::as_str)
            .ok_or_else(|| VideoError::Backend("response carried no job id".into()))?
            .to_string();

        // 2) Poll.
        let started = Instant::now();
        loop {
            let job: Value = self.http.get(format!("{}/videos/{job_id}", self.base_url))
                .bearer_auth(&self.api_key).send().await
                .map_err(|e| VideoError::Backend(format!("poll failed: {}", e.without_url())))?
                .json().await
                .map_err(|e| VideoError::Backend(format!("bad poll response: {e}")))?;
            match job.get("status").and_then(Value::as_str).unwrap_or("") {
                "completed" => break,
                s @ ("failed" | "cancelled") => {
                    let why = job.get("error").map(Self::err_message).unwrap_or_else(|| format!("job {s}"));
                    return Err(VideoError::Backend(format!("render {s} — {why}")));
                }
                _ => {
                    if started.elapsed() >= MAX_POLL {
                        return Err(VideoError::Timeout(MAX_POLL.as_secs()));
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }

        // 3) Download.
        let dl = self.http.get(format!("{}/videos/{job_id}/content", self.base_url))
            .bearer_auth(&self.api_key).send().await
            .map_err(|e| VideoError::Backend(format!("download failed: {}", e.without_url())))?;
        if !dl.status().is_success() {
            return Err(VideoError::Backend(format!("could not download (HTTP {})", dl.status())));
        }
        let bytes = dl.bytes().await
            .map_err(|e| VideoError::Backend(format!("read video: {e}")))?.to_vec();
        if bytes.is_empty() {
            return Err(VideoError::Backend("downloaded an empty video".into()));
        }
        Ok(VideoOutput { bytes, ext: "mp4".into(), note: None })
    }
}
