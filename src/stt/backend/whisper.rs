// SPDX-License-Identifier: AGPL-3.0-or-later

// src/stt/backend/whisper.rs
//! Internal whisper.cpp STT backend via the `whisper-rs` FFI.
//!
//! Loading the ggml model is expensive (hundreds of milliseconds plus a
//! large mmap), so we keep a single [`WhisperContext`] alive for the
//! lifetime of the backend and create a fresh per-request state from it.
//! Inference is CPU-bound and synchronous; we hand each call to
//! `tokio::task::spawn_blocking` so the async runtime stays responsive.
//!
//! The first call lazily downloads the configured model under
//! `<data_dir>/stt/models/ggml-<id>.bin` when `auto_download_model` is on.
//! After that, the file is reused indefinitely.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::stt::backend::SttBackend;
use crate::stt::encoder::decode_to_pcm16k;
use crate::stt::manifest::{curated_model, model_file_name};
use crate::stt::types::{ProbeResult, SttError, TranscribeRequest, Transcript};

/// Construction params for [`WhisperBackend`]. Mirrors the way
/// [`crate::tts::backend::PiperConfig`] is shaped — built once from
/// [`crate::config::SttInternalConfig`] in the service factory.
#[derive(Debug, Clone)]
pub struct WhisperConfig {
    /// Model id (e.g. `"base.en"`). Resolved against the curated manifest.
    pub model_id:        String,
    /// Directory holding `ggml-<id>.bin` files.
    pub models_dir:      PathBuf,
    /// Auto-download the configured model on first use.
    pub auto_download:   bool,
    /// Inference threads (0 = let whisper.cpp pick `num_cpus`).
    pub threads:         u32,
    /// GPU offload — only effective when whisper-rs was built with a GPU
    /// feature; otherwise ignored.
    pub use_gpu:         bool,
}

impl WhisperConfig {
    /// Sensible defaults rooted under `<data_dir>/stt/models`.
    pub fn under_data_dir(data_dir: &Path) -> Self {
        Self {
            model_id:      crate::stt::manifest::DEFAULT_MODEL_ID.to_string(),
            models_dir:    data_dir.join("stt").join("models"),
            auto_download: true,
            threads:       0,
            use_gpu:       false,
        }
    }
}

/// Lazy-loaded whisper.cpp context. We need interior mutability and async
/// awareness because two concurrent transcribes may both arrive before the
/// model finishes loading; the mutex serialises the load (and inference
/// itself, which whisper.cpp doesn't parallelise on a single state).
pub struct WhisperBackend {
    cfg:     WhisperConfig,
    ctx:     Arc<AsyncMutex<Option<Arc<WhisperContext>>>>,
}

impl WhisperBackend {
    pub fn new(cfg: WhisperConfig) -> Self {
        Self {
            cfg,
            ctx: Arc::new(AsyncMutex::new(None)),
        }
    }

    /// Resolve the path to the configured model file. The file may not yet
    /// exist — [`Self::ensure_model_file`] handles fetching it.
    fn model_path(&self) -> PathBuf {
        self.cfg.models_dir.join(model_file_name(&self.cfg.model_id))
    }

    async fn ensure_model_file(&self) -> Result<PathBuf, SttError> {
        let path = self.model_path();
        if path.exists() {
            return Ok(path);
        }
        let manifest = curated_model(&self.cfg.model_id)
            .ok_or_else(|| SttError::ModelNotInstalled(self.cfg.model_id.clone()))?;

        if !self.cfg.auto_download {
            return Err(SttError::ModelNotInstalled(format!(
                "{} (auto_download_model = false; place {} under {} manually)",
                self.cfg.model_id,
                manifest.url,
                self.cfg.models_dir.display(),
            )));
        }

        tokio::fs::create_dir_all(&self.cfg.models_dir).await?;
        info!(
            "stt: downloading whisper model '{}' (~{} MB) to {}",
            self.cfg.model_id, manifest.size_mb, path.display()
        );
        let resp = reqwest::get(manifest.url).await?;
        if !resp.status().is_success() {
            return Err(SttError::Upstream(format!(
                "model download failed: HTTP {} from {}", resp.status(), manifest.url
            )));
        }
        let bytes = resp.bytes().await?;
        let tmp = path.with_extension("bin.partial");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        info!("stt: model '{}' ready ({} bytes)", self.cfg.model_id, bytes.len());
        Ok(path)
    }

    async fn load_context(&self) -> Result<Arc<WhisperContext>, SttError> {
        let mut guard = self.ctx.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let path = self.ensure_model_file().await?;
        let path_str = path.to_string_lossy().into_owned();

        let mut params = WhisperContextParameters::default();
        params.use_gpu(self.cfg.use_gpu);

        // WhisperContext::new_with_params is synchronous and slow — run on
        // a blocking thread so we don't stall the runtime.
        let ctx = tokio::task::spawn_blocking(move || {
            WhisperContext::new_with_params(&path_str, params)
        })
        .await
        .map_err(|e| SttError::Upstream(format!("whisper load join error: {e}")))?
        .map_err(|e| SttError::Upstream(format!("whisper load failed: {e}")))?;

        let arc = Arc::new(ctx);
        *guard = Some(arc.clone());
        Ok(arc)
    }
}

#[async_trait]
impl SttBackend for WhisperBackend {
    fn id(&self) -> &'static str { "internal" }

    async fn transcribe(&self, req: &TranscribeRequest) -> Result<Transcript, SttError> {
        let started = Instant::now();

        // Decode → 16 kHz mono f32 PCM (cheap; CPU-bound but short).
        let audio_bytes = req.audio_bytes.clone();
        let format = req.format;
        let decoded = tokio::task::spawn_blocking(move || decode_to_pcm16k(&audio_bytes, format))
            .await
            .map_err(|e| SttError::Decoding(format!("decode join error: {e}")))??;
        debug!(
            "stt: decoded {} ms of audio ({} samples) for whisper",
            decoded.duration_ms, decoded.samples.len()
        );

        let ctx = self.load_context().await?;
        let language = req.language.clone();
        let threads  = self.cfg.threads;
        let backend_id = self.id().to_string();

        let (text, detected_lang) = tokio::task::spawn_blocking(move || -> Result<(String, Option<String>), SttError> {
            let mut state = ctx.create_state()
                .map_err(|e| SttError::Upstream(format!("whisper state init: {e}")))?;
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            if threads > 0 {
                params.set_n_threads(threads as i32);
            }
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_special(false);
            params.set_print_timestamps(false);
            if let Some(lang) = language.as_ref().filter(|s| !s.is_empty()) {
                params.set_language(Some(lang.as_str()));
            } else {
                // Whisper auto-detects when language is None.
                params.set_language(None);
            }

            state.full(params, &decoded.samples)
                .map_err(|e| SttError::Upstream(format!("whisper inference: {e}")))?;

            let n = state.full_n_segments()
                .map_err(|e| SttError::Upstream(format!("whisper n_segments: {e}")))?;
            let mut text = String::new();
            for i in 0..n {
                let seg = state.full_get_segment_text(i)
                    .map_err(|e| SttError::Upstream(format!("whisper segment: {e}")))?;
                text.push_str(&seg);
            }
            // Whisper exposes detected language as a lang-id index; mapping
            // it back to a BCP-47 tag is more code than it's worth here, so
            // we just echo the requested language when one was given.
            Ok((text.trim().to_string(), language))
        })
        .await
        .map_err(|e| SttError::Upstream(format!("whisper join: {e}")))??;

        let latency = started.elapsed().as_millis() as u64;
        Ok(Transcript {
            text,
            language:    detected_lang,
            duration_ms: Some(decoded.duration_ms),
            latency_ms:  latency,
            backend_id,
        })
    }

    async fn probe(&self) -> Result<ProbeResult, SttError> {
        // A "probe" for the local backend means "do we have a model ready
        // to go?". Don't actually run inference — that's far too expensive
        // for the settings UI to call on every render.
        let path = self.model_path();
        if path.exists() {
            Ok(ProbeResult {
                healthy:    true,
                latency_ms: None,
                note:       Some(format!(
                    "model {} ready at {}", self.cfg.model_id, path.display()
                )),
            })
        } else if self.cfg.auto_download && curated_model(&self.cfg.model_id).is_some() {
            Ok(ProbeResult {
                healthy:    true,
                latency_ms: None,
                note:       Some(format!(
                    "model {} not yet downloaded — will fetch on first use",
                    self.cfg.model_id
                )),
            })
        } else {
            Ok(ProbeResult {
                healthy:    false,
                latency_ms: None,
                note:       Some(format!(
                    "model {} missing under {}", self.cfg.model_id, self.cfg.models_dir.display()
                )),
            })
        }
    }

    async fn ensure_ready(&self) -> Result<(), SttError> {
        // Pre-fetch the model so the first transcribe doesn't pay download
        // latency. We don't load the context here — that's still deferred
        // until the first real call to keep memory low when STT is wired
        // but never used in a session.
        self.ensure_model_file().await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn under_data_dir_picks_default_model() {
        let dir = tempdir().unwrap();
        let cfg = WhisperConfig::under_data_dir(dir.path());
        assert_eq!(cfg.model_id, "base.en");
        assert!(cfg.models_dir.ends_with("stt/models"));
    }

    #[tokio::test]
    async fn probe_reports_pending_download_when_model_missing_with_auto_download() {
        let dir = tempdir().unwrap();
        let cfg = WhisperConfig::under_data_dir(dir.path());
        let b = WhisperBackend::new(cfg);
        let p = b.probe().await.unwrap();
        assert!(p.healthy, "should report healthy when auto_download is on");
        assert!(p.note.as_deref().unwrap_or("").contains("not yet downloaded"));
    }

    #[tokio::test]
    async fn probe_reports_missing_when_unknown_model_and_no_auto_download() {
        let dir = tempdir().unwrap();
        let mut cfg = WhisperConfig::under_data_dir(dir.path());
        cfg.model_id = "this-model-does-not-exist".into();
        cfg.auto_download = false;
        let b = WhisperBackend::new(cfg);
        let p = b.probe().await.unwrap();
        assert!(!p.healthy);
    }

    #[tokio::test]
    async fn ensure_ready_errors_when_unknown_model_and_no_auto_download() {
        let dir = tempdir().unwrap();
        let mut cfg = WhisperConfig::under_data_dir(dir.path());
        cfg.model_id = "totally-fake-model".into();
        cfg.auto_download = false;
        let b = WhisperBackend::new(cfg);
        let err = b.ensure_ready().await.unwrap_err();
        assert!(matches!(err, SttError::ModelNotInstalled(_)));
    }

    #[test]
    fn id_is_internal() {
        let dir = tempdir().unwrap();
        let b = WhisperBackend::new(WhisperConfig::under_data_dir(dir.path()));
        assert_eq!(b.id(), "internal");
    }
}
