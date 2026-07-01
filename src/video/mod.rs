// SPDX-License-Identifier: AGPL-3.0-or-later

// src/video/mod.rs
//! Video-generation backends + router (0.292.0). Mirrors `crate::image`: a
//! [`VideoBackend`] trait with multiple implementations behind a
//! [`VideoService`] router, so the `video_generate` tool is backend-agnostic —
//! OpenAI Videos (Sora), local ComfyUI (a video workflow), or local WAN2GP all
//! look the same to callers.

pub mod backend;

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::VideoConfig;

/// Backend-neutral video request. Backends use what they understand.
#[derive(Debug, Clone)]
pub struct VideoRequest {
    pub prompt:          String,
    pub negative_prompt: Option<String>,
    pub model:           Option<String>,
    pub width:           u32,
    pub height:          u32,
    pub seconds:         u32,
    pub seed:            Option<i64>,
}

impl VideoRequest {
    pub fn dims_from_size(size: Option<&str>, default: (u32, u32)) -> (u32, u32) {
        let parse = |s: Option<&str>| -> Option<(u32, u32)> {
            let (w, h) = s?.split_once(['x', 'X', '*'])?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        };
        parse(size).unwrap_or(default)
    }
}

/// What a backend produces.
#[derive(Debug)]
pub struct VideoOutput {
    pub bytes: Vec<u8>,
    /// File extension, e.g. `"mp4"` / `"webm"` / `"gif"`.
    pub ext:   String,
    pub note:  Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("no video backend is enabled — configure one under [video] or providers.openai")]
    NoBackend,
    #[error("video backend {0:?} is not enabled")]
    UnknownBackend(String),
    #[error("video backend error: {0}")]
    Backend(String),
    #[error("video generation timed out after {0}s")]
    Timeout(u64),
}

#[async_trait]
pub trait VideoBackend: Send + Sync {
    async fn generate(&self, req: &VideoRequest) -> Result<VideoOutput, VideoError>;
    fn name(&self) -> &'static str;
    fn enabled(&self) -> bool;
}

/// Routes a request to a backend by name / config default / first-enabled.
pub struct VideoService {
    backends:        Vec<Arc<dyn VideoBackend>>,
    default_backend: String,
}

impl VideoService {
    pub fn from_config(cfg: &VideoConfig, openai: &crate::config::OpenAiConfig) -> Self {
        let mut backends: Vec<Arc<dyn VideoBackend>> = Vec::new();
        // Local first (preferred under "auto").
        if cfg.comfyui.enabled {
            backends.push(Arc::new(backend::comfyui::ComfyUiVideoBackend::new(&cfg.comfyui)));
        }
        if cfg.wan2gp.enabled {
            backends.push(Arc::new(backend::wan2gp::Wan2gpBackend::new(&cfg.wan2gp)));
        }
        let oa = backend::openai::OpenAiVideoBackend::new(openai, &cfg.openai);
        if oa.enabled() {
            backends.push(Arc::new(oa));
        }
        backends.retain(|b| b.enabled());
        Self { backends, default_backend: cfg.default_backend.trim().to_string() }
    }

    pub fn any_enabled(&self) -> bool { !self.backends.is_empty() }
    pub fn backend_ids(&self) -> Vec<&'static str> { self.backends.iter().map(|b| b.name()).collect() }

    fn resolve(&self, requested: Option<&str>) -> Option<&Arc<dyn VideoBackend>> {
        if let Some(name) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            return self.backends.iter().find(|b| b.name() == name);
        }
        let d = self.default_backend.trim();
        if !d.is_empty() && d != "auto" {
            if let Some(b) = self.backends.iter().find(|b| b.name() == d) {
                return Some(b);
            }
        }
        self.backends.first()
    }

    pub async fn generate(
        &self,
        backend: Option<&str>,
        req:     &VideoRequest,
    ) -> Result<VideoOutput, VideoError> {
        let b = self.resolve(backend).ok_or_else(|| match backend {
            Some(n) if !n.trim().is_empty() => VideoError::UnknownBackend(n.to_string()),
            _ => VideoError::NoBackend,
        })?;
        b.generate(req).await
    }
}
