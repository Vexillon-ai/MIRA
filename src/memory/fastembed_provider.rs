// SPDX-License-Identifier: AGPL-3.0-or-later

// src/memory/fastembed_provider.rs
//! Built-in embedding provider powered by [`fastembed`].
//!
//! Runs ONNX inference entirely in-process via the `ort` runtime.  No external
//! server is needed.  The chosen model file is downloaded from HuggingFace on
//! first use and cached to `model_cache_dir` (configured in
//! `memory.embedding.model_cache_dir`).
//!
//! # Supported models
//!
//! | Config `model` value (case-insensitive) | fastembed model              | Dim | Download |
//! |-----------------------------------------|------------------------------|-----|----------|
//! | `BGE-small-en-v1.5` (default)           | `BGESmallENV15`              | 384 | ~24 MB   |
//! | `BGE-base-en-v1.5`                      | `BGEBaseENV15`               | 768 | ~90 MB   |
//! | `all-MiniLM-L6-v2`                      | `AllMiniLML6V2`              | 384 | ~23 MB   |
//! | `all-MiniLM-L12-v2`                     | `AllMiniLML12V2`             | 384 | ~34 MB   |
//! | `nomic-embed-text-v1.5`                 | `NomicEmbedTextV15`          | 768 | ~70 MB   |
//!
//! Any unrecognised name falls back to `BGESmallENV15` with a warning.

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use lru::LruCache;
use tracing::{info, warn};

use super::semantic::{Embedding, EmbeddingProvider};

// ─────────────────────────────────────────────────────────────────────────────

pub struct FastEmbedProvider {
    /// Shared reference so we can move it into `spawn_blocking` closures.
    model:       Arc<TextEmbedding>,
    dim:         usize,
    /// The user-facing model name resolved to a fastembed identifier. Stored
    /// purely so `label()` can report it alongside persisted vectors — the
    /// transcript indexer uses the label to detect model switches.
    model_label: String,
    cache:       StdMutex<LruCache<String, Embedding>>,
}

impl FastEmbedProvider {
    /// Build a provider, downloading the model if it is not already cached.
    ///
    /// This call is **blocking** (file I/O + ONNX session init).  Call it
    /// from within [`tokio::task::spawn_blocking`] when you need an async
    /// context, or accept the brief startup block.
    pub fn try_new(
        model_name:  &str,
        cache_dir:   &Path,
        cache_size:  usize,
    ) -> Result<Self, String> {
        let embedding_model = resolve_model(model_name);
        let dim             = model_dim(&embedding_model);

        info!(
            "Initializing fastembed provider: model={} dim={} cache_dir={}",
            model_name, dim, cache_dir.display()
        );

        // Ensure the cache directory exists before fastembed tries to write to it.
        std::fs::create_dir_all(cache_dir)
            .map_err(|e| format!("Cannot create model cache dir '{}': {}", cache_dir.display(), e))?;

        let model = TextEmbedding::try_new(
            InitOptions::new(embedding_model)
                .with_cache_dir(cache_dir.to_path_buf())
                .with_show_download_progress(true),
        )
        .map_err(|e| format!("Failed to load fastembed model '{}': {}", model_name, e))?;

        info!("fastembed model ready (dim={})", dim);

        Ok(Self {
            model:       Arc::new(model),
            dim,
            model_label: model_name.to_owned(),
            cache:       StdMutex::new(
                LruCache::new(NonZeroUsize::new(cache_size.max(1)).unwrap())
            ),
        })
    }
}

#[async_trait]
impl EmbeddingProvider for FastEmbedProvider {
    async fn embed(&self, text: &str) -> Result<Embedding, String> {
        // ── Cache check ─────────────────────────────────────────────────────
        if let Some(cached) = self.cache.lock().unwrap().get(text).cloned() {
            return Ok(cached);
        }

        // ── Run ONNX inference on a blocking thread ──────────────────────────
        // fastembed's `embed()` is synchronous CPU work; must not block the
        // Tokio runtime's async threads.
        let model   = Arc::clone(&self.model);
        let text_s  = text.to_string();

        let embeddings = tokio::task::spawn_blocking(move || {
            model.embed(vec![text_s.as_str()], None)
        })
        .await
        .map_err(|e| format!("spawn_blocking panicked: {}", e))?
        .map_err(|e| format!("fastembed inference error: {}", e))?;

        let embedding = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| "fastembed returned empty result".to_string())?;

        self.cache.lock().unwrap().put(text.to_string(), embedding.clone());
        Ok(embedding)
    }

    fn dimension(&self) -> usize {
        self.dim
    }

    fn label(&self) -> String {
        format!("fastembed:{}", self.model_label)
    }
}

// ── Model resolution ──────────────────────────────────────────────────────────

fn resolve_model(name: &str) -> EmbeddingModel {
    match name.to_lowercase().replace(['-', '_', ' ', '.'], "").as_str() {
        s if s.contains("bgesmallen") && s.contains("15") => EmbeddingModel::BGESmallENV15,
        s if s.contains("bgebaseen")  && s.contains("15") => EmbeddingModel::BGEBaseENV15,
        s if s.contains("minilml12")                      => EmbeddingModel::AllMiniLML12V2,
        s if s.contains("minilml6")                       => EmbeddingModel::AllMiniLML6V2,
        s if s.contains("nomicembed") && s.contains("15") => EmbeddingModel::NomicEmbedTextV15,
        _ => {
            warn!(
                "Unrecognised fastembed model name '{}' — defaulting to BGE-small-en-v1.5",
                name
            );
            EmbeddingModel::BGESmallENV15
        }
    }
}

fn model_dim(model: &EmbeddingModel) -> usize {
    match model {
        EmbeddingModel::BGESmallENV15   => 384,
        EmbeddingModel::BGEBaseENV15    => 768,
        EmbeddingModel::AllMiniLML6V2   => 384,
        EmbeddingModel::AllMiniLML12V2  => 384,
        EmbeddingModel::NomicEmbedTextV15 => 768,
        _                               => 384,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_model_bge_small() {
        let m = resolve_model("BGE-small-en-v1.5");
        assert!(matches!(m, EmbeddingModel::BGESmallENV15));
    }

    #[test]
    fn test_resolve_model_bge_base() {
        let m = resolve_model("bge-base-en-v1.5");
        assert!(matches!(m, EmbeddingModel::BGEBaseENV15));
    }

    #[test]
    fn test_resolve_model_minilm_l6() {
        let m = resolve_model("all-MiniLM-L6-v2");
        assert!(matches!(m, EmbeddingModel::AllMiniLML6V2));
    }

    #[test]
    fn test_resolve_model_minilm_l12() {
        let m = resolve_model("all-MiniLM-L12-v2");
        assert!(matches!(m, EmbeddingModel::AllMiniLML12V2));
    }

    #[test]
    fn test_resolve_model_nomic() {
        let m = resolve_model("nomic-embed-text-v1.5");
        assert!(matches!(m, EmbeddingModel::NomicEmbedTextV15));
    }

    #[test]
    fn test_resolve_model_unknown_defaults_to_bge_small() {
        let m = resolve_model("some-unknown-model");
        assert!(matches!(m, EmbeddingModel::BGESmallENV15));
    }

    #[test]
    fn test_model_dim_bge_small() {
        assert_eq!(model_dim(&EmbeddingModel::BGESmallENV15), 384);
    }

    #[test]
    fn test_model_dim_bge_base() {
        assert_eq!(model_dim(&EmbeddingModel::BGEBaseENV15), 768);
    }
}
