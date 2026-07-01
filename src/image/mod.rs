// SPDX-License-Identifier: AGPL-3.0-or-later

// src/image/mod.rs
//! Image-generation backends + router (0.292.0).
//!
//! Mirrors the TTS subsystem: a [`ImageBackend`] trait with multiple
//! implementations behind a [`ImageService`] router. The `image_generate` tool
//! dispatches through the service, so the agent (and the inline-artifact
//! rendering) is **backend-agnostic** — local Stable Diffusion (Automatic1111),
//! local ComfyUI, OpenAI Images, or an OpenAI-compatible endpoint all look the
//! same to callers.
//!
//! Backends:
//! - `openai` — OpenAI Images / compatible (`providers.openai`); on when a key
//!   resolves. (The existing, cloud default.)
//! - `automatic1111` — local SD WebUI (`/sdapi/v1/txt2img`).
//! - `comfyui` — local ComfyUI (`/prompt` → poll `/history` → fetch `/view`).

pub mod backend;

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ImageConfig;

/// A backend-neutral image request. Backends use what they understand and
/// ignore the rest (e.g. OpenAI ignores `seed`/`negative_prompt`).
#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub prompt:          String,
    pub negative_prompt: Option<String>,
    /// Backend-specific model/checkpoint override (None = backend default).
    pub model:           Option<String>,
    pub width:           u32,
    pub height:          u32,
    /// None / negative = random.
    pub seed:            Option<i64>,
}

impl ImageRequest {
    /// Parse a `"WIDTHxHEIGHT"` size string (e.g. `"1024x768"`), falling back to
    /// `default` for either dimension that doesn't parse.
    pub fn dims_from_size(size: Option<&str>, default: u32) -> (u32, u32) {
        let parse = |s: Option<&str>| -> Option<(u32, u32)> {
            let s = s?;
            let (w, h) = s.split_once(['x', 'X', '*'])?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        };
        parse(size).unwrap_or((default, default))
    }
}

/// What a backend produces.
#[derive(Debug)]
pub struct ImageOutput {
    pub bytes: Vec<u8>,
    /// File extension for the artifact store, e.g. `"png"`.
    pub ext:   String,
    /// Optional note to surface (e.g. a model-revised prompt).
    pub note:  Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("no image backend is enabled — configure one under [image] or providers.openai")]
    NoBackend,
    #[error("image backend {0:?} is not enabled")]
    UnknownBackend(String),
    #[error("image backend error: {0}")]
    Backend(String),
    #[error("image generation timed out after {0}s")]
    Timeout(u64),
}

#[async_trait]
pub trait ImageBackend: Send + Sync {
    async fn generate(&self, req: &ImageRequest) -> Result<ImageOutput, ImageError>;
    /// Stable id used in config + the tool's `backend` arg.
    fn name(&self) -> &'static str;
    /// Whether this backend is configured + usable.
    fn enabled(&self) -> bool;
}

/// Routes a request to a backend by name / config default / first-enabled.
/// Built once at startup from config; cheap to share behind an `Arc`.
pub struct ImageService {
    /// Priority order: local backends first (preferred under "auto"), then
    /// OpenAI. Only enabled backends are kept.
    backends:        Vec<Arc<dyn ImageBackend>>,
    default_backend: String,
}

impl ImageService {
    pub fn from_config(cfg: &ImageConfig, openai: &crate::config::OpenAiConfig) -> Self {
        let mut backends: Vec<Arc<dyn ImageBackend>> = Vec::new();
        if cfg.automatic1111.enabled {
            backends.push(Arc::new(backend::automatic1111::Automatic1111Backend::new(&cfg.automatic1111)));
        }
        if cfg.comfyui.enabled {
            backends.push(Arc::new(backend::comfyui::ComfyUiBackend::new(&cfg.comfyui)));
        }
        let oa = backend::openai::OpenAiImageBackend::new(openai, &cfg.openai.default_model);
        if oa.enabled() {
            backends.push(Arc::new(oa));
        }
        // Keep only enabled ones so resolution/listing is straightforward.
        backends.retain(|b| b.enabled());
        Self { backends, default_backend: cfg.default_backend.trim().to_string() }
    }

    pub fn any_enabled(&self) -> bool { !self.backends.is_empty() }

    /// Ids of the enabled backends, in priority order.
    pub fn backend_ids(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    fn resolve(&self, requested: Option<&str>) -> Option<&Arc<dyn ImageBackend>> {
        if let Some(name) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            return self.backends.iter().find(|b| b.name() == name);
        }
        let d = self.default_backend.trim();
        if !d.is_empty() && d != "auto" {
            if let Some(b) = self.backends.iter().find(|b| b.name() == d) {
                return Some(b);
            }
            // default names a disabled/unknown backend → fall through.
        }
        self.backends.first()
    }

    pub async fn generate(
        &self,
        backend: Option<&str>,
        req:     &ImageRequest,
    ) -> Result<ImageOutput, ImageError> {
        let b = self.resolve(backend).ok_or_else(|| match backend {
            Some(n) if !n.trim().is_empty() => ImageError::UnknownBackend(n.to_string()),
            _ => ImageError::NoBackend,
        })?;
        b.generate(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dims_parse() {
        assert_eq!(ImageRequest::dims_from_size(Some("1024x768"), 512), (1024, 768));
        assert_eq!(ImageRequest::dims_from_size(Some("512X512"), 1024), (512, 512));
        assert_eq!(ImageRequest::dims_from_size(None, 1024), (1024, 1024));
        assert_eq!(ImageRequest::dims_from_size(Some("garbage"), 768), (768, 768));
    }

    struct Dummy(&'static str);
    #[async_trait]
    impl ImageBackend for Dummy {
        async fn generate(&self, _r: &ImageRequest) -> Result<ImageOutput, ImageError> {
            Ok(ImageOutput { bytes: vec![1], ext: "png".into(), note: Some(self.0.into()) })
        }
        fn name(&self) -> &'static str { self.0 }
        fn enabled(&self) -> bool { true }
    }

    fn svc(default_backend: &str, names: &[&'static str]) -> ImageService {
        ImageService {
            backends: names.iter().map(|n| Arc::new(Dummy(n)) as Arc<dyn ImageBackend>).collect(),
            default_backend: default_backend.to_string(),
        }
    }

    async fn ran(s: &ImageService, backend: Option<&str>) -> Result<String, ImageError> {
        let r = ImageRequest { prompt: "x".into(), negative_prompt: None, model: None, width: 64, height: 64, seed: None };
        s.generate(backend, &r).await.map(|o| o.note.unwrap())
    }

    #[tokio::test]
    async fn routing_resolves_explicit_default_and_auto() {
        let s = svc("comfyui", &["automatic1111", "comfyui", "openai"]);
        // Explicit wins.
        assert_eq!(ran(&s, Some("openai")).await.unwrap(), "openai");
        // None → configured default.
        assert_eq!(ran(&s, None).await.unwrap(), "comfyui");
        // Explicit unknown → error (not silent fallback).
        assert!(matches!(ran(&s, Some("nope")).await, Err(ImageError::UnknownBackend(_))));

        // "auto"/empty default → first enabled (local preferred by order).
        let s2 = svc("auto", &["automatic1111", "openai"]);
        assert_eq!(ran(&s2, None).await.unwrap(), "automatic1111");

        // Default names a disabled/absent backend → soft fall through to first.
        let s3 = svc("comfyui", &["automatic1111"]);
        assert_eq!(ran(&s3, None).await.unwrap(), "automatic1111");

        // No backends at all → NoBackend.
        let s4 = svc("", &[]);
        assert!(matches!(ran(&s4, None).await, Err(ImageError::NoBackend)));
    }
}
