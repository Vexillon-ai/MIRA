// SPDX-License-Identifier: AGPL-3.0-or-later

// src/stt/service.rs
//! `SttService` — public entry point for the STT subsystem. Mirrors the
//! shape and concurrency model of [`crate::tts::service::TtsService`]:
//! configured backends owned in an `Arc`, an outer `RwLock<Arc<Inner>>`
//! that the live config watcher swaps atomically on `PUT /api/config`,
//! per-call snapshots so in-flight transcribes never get yanked mid-flight.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::{MiraConfig, SttConfig, SttOpenaiCompatConfig};
use crate::stt::backend::{
    OpenAiCompatBackend, OpenAiCompatConfig, SttBackend, WhisperBackend, WhisperConfig,
};
use crate::stt::types::{ProbeResult, SttError, TranscribeRequest, Transcript};

const BACKEND_INTERNAL:      &str = "internal";
const BACKEND_OPENAI:        &str = "openai";
const BACKEND_OPENAI_COMPAT: &str = "openai_compat";

/// Resolve the OpenAI API key from config → environment. Empty string =
/// "skip Authorization", same convention as TTS.
fn resolve_openai_key(cfg_key: Option<&str>) -> String {
    if let Some(k) = cfg_key.map(str::trim).filter(|s| !s.is_empty()) {
        return k.to_string();
    }
    std::env::var("OPENAI_API_KEY").unwrap_or_default()
}

#[derive(Clone)]
pub struct SttService {
    inner: Arc<RwLock<Arc<SttServiceInner>>>,
    /// Optional fallback tracker — records + notifies when transcription
    /// degrades to the internal whisper backend. Wired at gateway startup.
    degradations: Option<Arc<crate::health::degradation::DegradationTracker>>,
}

struct SttServiceInner {
    cfg:      SttConfig,
    backends: HashMap<String, Arc<dyn SttBackend>>,
}

impl SttService {
    pub fn from_config(mira: &MiraConfig) -> Self {
        let inner = Self::build_inner(mira);
        Self { inner: Arc::new(RwLock::new(Arc::new(inner))), degradations: None }
    }

    /// Wire the subsystem-fallback tracker so degraded transcription surfaces.
    pub fn with_degradations(
        mut self,
        tracker: Arc<crate::health::degradation::DegradationTracker>,
    ) -> Self {
        self.degradations = Some(tracker);
        self
    }

    /// Atomically swap in a new backend map / config.
    pub fn reload(&self, mira: &MiraConfig) {
        let new_inner = Arc::new(Self::build_inner(mira));
        if let Ok(mut guard) = self.inner.write() {
            *guard = new_inner;
            info!("stt: service reloaded from updated config");
        } else {
            warn!("stt: reload skipped — inner RwLock poisoned");
        }
    }

    fn snapshot(&self) -> Arc<SttServiceInner> {
        match self.inner.read() {
            Ok(g)  => Arc::clone(&*g),
            Err(p) => Arc::clone(&*p.into_inner()),
        }
    }

    fn build_inner(mira: &MiraConfig) -> SttServiceInner {
        let cfg = mira.stt.clone();
        let mut backends: HashMap<String, Arc<dyn SttBackend>> = HashMap::new();

        if cfg.enabled {
            // Internal whisper — always wired so the user has something out
            // of the box. The model file is downloaded lazily on first use.
            let mut whisper_cfg = WhisperConfig::under_data_dir(&mira.data_dir_path());
            whisper_cfg.model_id = cfg.internal.model.clone();
            if !cfg.internal.models_dir.is_empty() {
                whisper_cfg.models_dir = crate::config::expand_path(&cfg.internal.models_dir);
            }
            whisper_cfg.auto_download = cfg.internal.auto_download_model;
            whisper_cfg.threads       = cfg.internal.threads;
            whisper_cfg.use_gpu       = cfg.internal.use_gpu;
            backends.insert(BACKEND_INTERNAL.into(),
                Arc::new(WhisperBackend::new(whisper_cfg)));
            info!("stt: internal whisper backend ready (model={})", cfg.internal.model);

            // OpenAI cloud — wired whenever a key is resolvable.
            let openai_key = resolve_openai_key(cfg.openai.api_key.as_deref());
            if !openai_key.is_empty() {
                backends.insert(BACKEND_OPENAI.into(),
                    Arc::new(OpenAiCompatBackend::new(OpenAiCompatConfig {
                        id:           "openai",
                        base_url:     cfg.openai.base_url.clone(),
                        api_key:      openai_key,
                        model:        cfg.openai.model.clone(),
                        timeout_secs: cfg.request_timeout_secs,
                    })));
                info!("stt: openai backend ready (model={})", cfg.openai.model);
            }

            // OpenAI-compat self-hosted — register whenever the URL has been
            // overridden away from the default placeholder. Auth optional.
            let compat_url = cfg.openai_compat.url.trim().to_string();
            if !compat_url.is_empty() && compat_url != SttOpenaiCompatConfig::default().url {
                let key = cfg.openai_compat.api_key.clone().unwrap_or_default();
                backends.insert(BACKEND_OPENAI_COMPAT.into(),
                    Arc::new(OpenAiCompatBackend::new(OpenAiCompatConfig {
                        id:           "openai_compat",
                        base_url:     compat_url.clone(),
                        api_key:      key,
                        model:        cfg.openai_compat.model.clone(),
                        timeout_secs: cfg.request_timeout_secs,
                    })));
                info!("stt: openai_compat backend ready (url={compat_url})");
            }
        } else {
            info!("stt: subsystem disabled in config");
        }

        SttServiceInner { cfg, backends }
    }

    pub fn config(&self) -> SttConfig { self.snapshot().cfg.clone() }
    pub fn enabled(&self) -> bool     { self.snapshot().cfg.enabled }

    /// Backend ids currently registered. Sorted for deterministic UI lists.
    pub fn backend_ids(&self) -> Vec<String> {
        let snap = self.snapshot();
        let mut v: Vec<String> = snap.backends.keys().cloned().collect();
        v.sort();
        v
    }

    /// Resolve a backend id for one transcribe call:
    ///   1. an explicit per-request override (if non-empty),
    ///   2. the per-channel routing rule,
    ///   3. the global `stt.default_backend`.
    pub fn resolve_backend(&self, requested: Option<&str>, channel: Option<&str>) -> String {
        let snap = self.snapshot();
        if let Some(b) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            return b.to_string();
        }
        if let Some(c) = channel {
            let r = &snap.cfg.routing;
            let route = match c {
                "web"      => &r.web,
                "tui"      => &r.tui,
                "telegram" => &r.telegram,
                "signal"   => &r.signal,
                _          => "",
            };
            if !route.is_empty() {
                return route.to_string();
            }
        }
        snap.cfg.default_backend.clone()
    }

    fn backend_from(snap: &SttServiceInner, id: &str) -> Result<Arc<dyn SttBackend>, SttError> {
        snap.backends.get(id).cloned().ok_or_else(||
            SttError::BackendNotConfigured(id.to_string())
        )
    }

    /// Run one transcription. Used by `POST /api/stt/transcribe`, channel
    /// adapters that ingest voice notes, and any future CLI command.
    pub async fn transcribe(
        &self,
        mut req:  TranscribeRequest,
        backend:  Option<&str>,
        channel:  Option<&str>,
    ) -> Result<Transcript, SttError> {
        let snap = self.snapshot();
        if !snap.cfg.enabled {
            return Err(SttError::BackendNotConfigured("stt disabled".into()));
        }
        if req.audio_bytes.is_empty() {
            return Err(SttError::BadRequest("empty audio payload".into()));
        }

        // Apply the configured language hint when caller didn't set one.
        if req.language.is_none() && !snap.cfg.default_language.is_empty() {
            req.language = Some(snap.cfg.default_language.clone());
        }

        let backend_id = self.resolve_backend(backend, channel);
        let degr = self.degradations.clone();
        let backend = Self::backend_from(&snap, &backend_id).or_else(|orig| {
            // If the requested backend isn't wired (e.g. `openai` selected
            // but no key configured), fall back to internal whisper so the
            // user still gets *something*.
            warn!("stt: backend '{backend_id}' not configured, falling back to internal");
            if let Some(tracker) = &degr {
                tracker.record(
                    "stt", "Speech recognition (STT)",
                    &backend_id, BACKEND_INTERNAL, "backend not configured", false,
                );
            }
            Self::backend_from(&snap, BACKEND_INTERNAL).map_err(|_| orig)
        })?;

        let dur = Duration::from_secs(snap.cfg.request_timeout_secs.max(1));
        debug!(
            "stt: transcribe via '{}' ({} bytes, lang={:?}, channel={:?})",
            backend.id(), req.audio_bytes.len(), req.language, channel
        );
        match timeout(dur, backend.transcribe(&req)).await {
            Ok(Ok(t))  => Ok(t),
            Ok(Err(e)) => Err(e),
            Err(_)     => Err(SttError::Timeout),
        }
    }

    pub async fn probe(&self, backend: Option<&str>) -> Result<ProbeResult, SttError> {
        let snap = self.snapshot();
        let id = backend.map(str::to_string)
            .unwrap_or_else(|| self.resolve_backend(None, None));
        Self::backend_from(&snap, &id)?.probe().await
    }

    pub async fn ensure_ready(&self, backend: Option<&str>) -> Result<(), SttError> {
        let snap = self.snapshot();
        let id = backend.map(str::to_string)
            .unwrap_or_else(|| self.resolve_backend(None, None));
        Self::backend_from(&snap, &id)?.ensure_ready().await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn mira_cfg(data_dir: &std::path::Path) -> MiraConfig {
        let mut cfg = MiraConfig::default();
        cfg.data_dir = data_dir.to_string_lossy().into_owned();
        // Tests must never touch the network.
        cfg.stt.internal.auto_download_model = false;
        cfg
    }

    #[test]
    fn from_config_wires_internal_backend_by_default() {
        let dir = tempdir().unwrap();
        let svc = SttService::from_config(&mira_cfg(dir.path()));
        assert!(svc.backend_ids().contains(&"internal".to_string()));
    }

    #[test]
    fn disabled_config_wires_no_backends() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.stt.enabled = false;
        let svc = SttService::from_config(&cfg);
        assert!(svc.backend_ids().is_empty());
    }

    #[test]
    fn from_config_skips_openai_when_no_key() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.stt.openai.api_key = None;
        // Hermetic: shadow any inherited env var.
        let _g = EnvGuard::unset("OPENAI_API_KEY");
        let svc = SttService::from_config(&cfg);
        assert!(!svc.backend_ids().contains(&"openai".to_string()));
    }

    #[test]
    fn from_config_wires_openai_when_key_present() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.stt.openai.api_key = Some("sk-test-123".into());
        let svc = SttService::from_config(&cfg);
        assert!(svc.backend_ids().contains(&"openai".to_string()));
    }

    #[test]
    fn from_config_skips_openai_compat_when_url_is_default_placeholder() {
        let dir = tempdir().unwrap();
        let svc = SttService::from_config(&mira_cfg(dir.path()));
        assert!(!svc.backend_ids().contains(&"openai_compat".to_string()));
    }

    #[test]
    fn from_config_wires_openai_compat_when_url_overridden() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.stt.openai_compat.url = "http://my-whisper.local:9000/v1".into();
        let svc = SttService::from_config(&cfg);
        assert!(svc.backend_ids().contains(&"openai_compat".to_string()));
    }

    #[test]
    fn resolve_backend_per_request_wins() {
        let dir = tempdir().unwrap();
        let svc = SttService::from_config(&mira_cfg(dir.path()));
        assert_eq!(svc.resolve_backend(Some("openai"), None), "openai");
        assert_eq!(svc.resolve_backend(None,            None), "internal");
    }

    #[test]
    fn resolve_backend_falls_through_routing_to_default() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.stt.routing.signal = "openai".into();
        let svc = SttService::from_config(&cfg);
        assert_eq!(svc.resolve_backend(None, Some("signal")),   "openai");
        assert_eq!(svc.resolve_backend(None, Some("telegram")), "internal");
        assert_eq!(svc.resolve_backend(None, None),             "internal");
    }

    #[tokio::test]
    async fn transcribe_rejects_disabled_subsystem() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.stt.enabled = false;
        let svc = SttService::from_config(&cfg);
        let err = svc.transcribe(
            TranscribeRequest::new(vec![1, 2, 3]),
            None, None,
        ).await.unwrap_err();
        assert!(matches!(err, SttError::BackendNotConfigured(_)));
    }

    #[tokio::test]
    async fn transcribe_rejects_empty_audio() {
        let dir = tempdir().unwrap();
        let svc = SttService::from_config(&mira_cfg(dir.path()));
        let err = svc.transcribe(
            TranscribeRequest::new(vec![]),
            None, None,
        ).await.unwrap_err();
        assert!(matches!(err, SttError::BadRequest(_)));
    }

    #[tokio::test]
    async fn probe_internal_when_no_model_returns_pending() {
        let dir = tempdir().unwrap();
        // auto_download_model = true on default — probe should flag pending.
        let mut cfg = mira_cfg(dir.path());
        cfg.stt.internal.auto_download_model = true;
        let svc = SttService::from_config(&cfg);
        let p = svc.probe(Some("internal")).await.unwrap();
        assert!(p.healthy);
        assert!(p.note.as_deref().unwrap_or("").contains("not yet downloaded"));
    }

    /// Tiny scoped env-var guard for hermetic key tests.
    struct EnvGuard {
        key:   &'static str,
        prior: Option<String>,
    }
    impl EnvGuard {
        fn unset(key: &'static str) -> Self {
            let prior = std::env::var(key).ok();
            unsafe { std::env::remove_var(key); }
            Self { key, prior }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prior {
                    Some(v) => std::env::set_var(self.key, v),
                    None    => std::env::remove_var(self.key),
                }
            }
        }
    }
}
