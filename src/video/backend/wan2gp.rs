// SPDX-License-Identifier: AGPL-3.0-or-later

// src/video/backend/wan2gp.rs
//! Local **WAN2GP** (deepbeepmeep's Wan2GP) video backend. WAN2GP is a Gradio
//! app, so MIRA drives its Gradio API: `POST /gradio_api/call/<api_name>` →
//! `{event_id}`, then stream `GET /gradio_api/call/<api_name>/<event_id>` (SSE)
//! for the result, whose payload includes a FileData pointing at the rendered
//! video (fetched from the app's `/gradio_api/file=<path>`).
//!
//! The input *signature* (which parameters in what order) is app/version
//! specific and is finalised against a live instance — until `api_name` is set
//! and the mapping confirmed, `generate()` returns a clear, actionable error
//! rather than guessing.

use std::time::Duration;

use async_trait::async_trait;

use super::super::{VideoBackend, VideoError, VideoOutput, VideoRequest};
use crate::config::Wan2gpConfig;

pub struct Wan2gpBackend {
    base_url: String,
    api_name: String,
    #[allow(dead_code)]
    http:     reqwest::Client,
}

impl Wan2gpBackend {
    pub fn new(cfg: &Wan2gpConfig) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build().unwrap_or_default();
        Self {
            base_url: cfg.base_url.trim().trim_end_matches('/').to_string(),
            api_name: cfg.api_name.trim().trim_start_matches('/').to_string(),
            http,
        }
    }
}

#[async_trait]
impl VideoBackend for Wan2gpBackend {
    fn name(&self) -> &'static str { "wan2gp" }
    // Reachable + told which Gradio endpoint to call.
    fn enabled(&self) -> bool { !self.base_url.is_empty() && !self.api_name.is_empty() }

    async fn generate(&self, _req: &VideoRequest) -> Result<VideoOutput, VideoError> {
        // The Gradio input mapping is finalised against the live app (its
        // `/config` lists the endpoint + ordered inputs). Until then, fail
        // loudly with guidance instead of submitting a malformed call.
        Err(VideoError::Backend(format!(
            "WAN2GP backend is wired but its Gradio input mapping isn't finalised yet \
             (base_url={}, api_name=/{}). Share the app's /config so the parameter order \
             can be confirmed.",
            self.base_url, self.api_name,
        )))
    }
}
