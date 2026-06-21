// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/service.rs
//! `TtsService` — the public entry point used by the HTTP API, the CLI, and
//! channel adapters. Wraps a set of configured backends, the audio cache, and
//! the routing rules into a single Send + Sync façade you can stash in
//! `AppState` once.
//!
//! wires only the internal backends (Piper + eSpeak fallback). Cloud
//! backends (OpenAI, OpenAI-compat, ElevenLabs, Cartesia) plug into the same
//! map in later stages.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use futures::stream::{BoxStream, StreamExt};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::{MiraConfig, TtsConfig, TtsOpenaiCompatConfig};
use crate::tts::backend::{
    EspeakBackend, OpenAiBackend, OpenAiConfig,
    PiperBackend, PiperConfig, TtsBackend,
};
#[cfg(feature = "kokoro")]
use crate::tts::backend::{KokoroBackend, KokoroConfig};
use crate::tts::cache::{CacheStats, TtsCache, cache_key};
use crate::tts::chunker::SentenceChunker;
use crate::tts::types::{
    AudioBuffer, AudioChunk, OutputFormat, ProbeResult, SynthesiseRequest, TtsError, Voice,
};

const INTERNAL_ENGINE_PIPER:  &str = "piper";
const INTERNAL_ENGINE_ESPEAK: &str = "espeak";
const BACKEND_OPENAI:         &str = "openai";
const BACKEND_OPENAI_COMPAT:  &str = "openai_compat";
const BACKEND_CHATTERBOX:     &str = "chatterbox";
#[cfg(feature = "kokoro")]
const BACKEND_KOKORO:         &str = "kokoro";

// True if a synthesis failure is plausibly transient — network blips,
// upstream 5xx, timeouts, IO. These are worth retrying on the internal
// Piper backend so users on a remote TTS service still get audio when
// the service is down. NOT included: `BadRequest` (the text itself is
// the problem), `Unauthorized` (auth won't fix itself by switching
// backends), `VoiceNotInstalled` (handled by a separate retry that
// drops `voice_id`), and `BackendNotConfigured` (different code path).
fn is_transient_failure(e: &TtsError) -> bool {
    matches!(
        e,
        TtsError::Timeout
        | TtsError::Upstream(_)
        | TtsError::BackendUnavailable(_, _)
        | TtsError::Encoding(_)
        | TtsError::Io(_)
        | TtsError::Http(_)
    )
}

fn is_internal_backend_id(id: &str) -> bool {
    id == INTERNAL_ENGINE_PIPER || id == INTERNAL_ENGINE_ESPEAK
}

// Resolve the OpenAI API key from config → environment. Returns an empty
// string when no key is set; callers treat empty as "skip Authorization".
fn resolve_openai_key(cfg_key: Option<&str>) -> String {
    if let Some(k) = cfg_key.map(str::trim).filter(|s| !s.is_empty()) {
        return k.to_string();
    }
    std::env::var("OPENAI_API_KEY").unwrap_or_default()
}

// Public TTS façade.
// // Internally holds an `RwLock<Arc<TtsServiceInner>>` so the live config
// watcher in `server::router` can swap in a fresh backend map (with new
// URLs / API keys) without restarting the process. Every public method
// snapshots the current `Arc` once at the top, so an in-flight `speak`
// call keeps using the backend it started with even if `reload` swaps
// mid-request.
#[derive(Clone)]
pub struct TtsService {
    inner: Arc<RwLock<Arc<TtsServiceInner>>>,
    // Optional fallback tracker — records + notifies when a synthesis call
    // degrades to an internal backend. Wired at gateway startup.
    degradations: Option<Arc<crate::health::degradation::DegradationTracker>>,
}

struct TtsServiceInner {
    cfg:      TtsConfig,
    backends: HashMap<String, Arc<dyn TtsBackend>>,
    cache:    Option<Arc<TtsCache>>,
}

impl TtsService {
    // Build a service from the loaded MIRA config. Always succeeds — any
    // backend that fails to construct is simply skipped, leaving the
    // service in a degraded but usable state.
    pub fn from_config(mira: &MiraConfig) -> Self {
        let inner = Self::build_inner(mira);
        Self { inner: Arc::new(RwLock::new(Arc::new(inner))), degradations: None }
    }

    // Wire the subsystem-fallback tracker so degraded synthesis surfaces.
    pub fn with_degradations(
        mut self,
        tracker: Arc<crate::health::degradation::DegradationTracker>,
    ) -> Self {
        self.degradations = Some(tracker);
        self
    }

    // Atomically swap in a new backend map / config. Called by the live
    // config watcher whenever `PUT /api/config` lands new TTS settings.
    // In-flight requests retain their original snapshot.
    pub fn reload(&self, mira: &MiraConfig) {
        let new_inner = Arc::new(Self::build_inner(mira));
        if let Ok(mut guard) = self.inner.write() {
            *guard = new_inner;
            info!("tts: service reloaded from updated config");
        } else {
            warn!("tts: reload skipped — inner RwLock poisoned");
        }
    }

    fn snapshot(&self) -> Arc<TtsServiceInner> {
        // RwLock poisoning here would mean a panic happened inside `reload`,
        // which itself only does `Arc::clone` and a write — practically
        // unreachable. Fall back to a minimal disabled inner if it ever fires.
        match self.inner.read() {
            Ok(g)  => Arc::clone(&*g),
            Err(p) => Arc::clone(&*p.into_inner()),
        }
    }

    fn build_inner(mira: &MiraConfig) -> TtsServiceInner {
        let cfg = mira.tts.clone();
        let mut backends: HashMap<String, Arc<dyn TtsBackend>> = HashMap::new();

        if cfg.enabled {
            let mut piper_cfg = PiperConfig::under_data_dir(&mira.data_dir_path());
            if !cfg.internal.voices_dir.is_empty() {
                piper_cfg.voices_dir = crate::config::resolve_state_path(&cfg.internal.voices_dir);
            }
            if !cfg.internal.binary_path.is_empty() {
                piper_cfg.binary_path = Some(crate::config::resolve_state_path(&cfg.internal.binary_path));
            }
            piper_cfg.default_voice = cfg.internal.default_voice.clone();
            piper_cfg.auto_download = cfg.internal.auto_download_voices;

            backends.insert(INTERNAL_ENGINE_PIPER.into(),
                Arc::new(PiperBackend::new(piper_cfg)));
            backends.insert(INTERNAL_ENGINE_ESPEAK.into(),
                Arc::new(EspeakBackend::new()));
            info!("tts: internal backends ready (piper, espeak fallback)");

            // K1 (Q2 #10) — native Kokoro. Only present in a build with
            // `--features kokoro` AND when `tts.kokoro.enabled`. Registering
            // it doesn't load the model — that's deferred to first use — so
            // an enabled-but-idle Kokoro costs nothing at startup.
            #[cfg(feature = "kokoro")]
            if cfg.kokoro.enabled {
                let mut k = KokoroConfig::under_data_dir(&mira.data_dir_path());
                if !cfg.kokoro.model_path.is_empty() {
                    k.model_path = crate::config::resolve_state_path(&cfg.kokoro.model_path);
                }
                k.default_voice = cfg.kokoro.default_voice.clone();
                k.device        = cfg.kokoro.device.clone();
                k.auto_download = cfg.kokoro.auto_download;
                let kb = Arc::new(KokoroBackend::new(k));

                // Eager warm-up: download (if needed) + load the model now
                // instead of on first synthesis, so enabling Kokoro proactively
                // fetches the ~0.3 GB model and the first play has no lag.
                // Detached + best-effort; only when downloads are allowed and
                // we're inside a tokio runtime (startup / config reload).
                if cfg.kokoro.auto_download {
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        let warm = kb.clone();
                        handle.spawn(async move {
                            match warm.warm_up().await {
                                Ok(())  => info!("tts: kokoro warmed up (model + all voices ready)"),
                                Err(e)  => warn!("tts: kokoro warm-up failed (will retry on first use): {e}"),
                            }
                        });
                    }
                }

                backends.insert(BACKEND_KOKORO.into(), kb);
                info!("tts: kokoro backend ready (device={}, voice={})",
                    cfg.kokoro.device, cfg.kokoro.default_voice);
            }

            // K3 (Q2 #10) — Chatterbox AMD Vulkan server. It speaks the
            // OpenAI TTS contract, so we drive it with the same OpenAiBackend
            // pointed at the local server. The process itself is managed
            // separately by ChatterboxSupervisor (gateway), if supervision
            // is on; here we only wire the client.
            if cfg.chatterbox.enabled {
                backends.insert(BACKEND_CHATTERBOX.into(),
                    Arc::new(OpenAiBackend::new(OpenAiConfig {
                        id:            BACKEND_CHATTERBOX,
                        base_url:      format!("http://127.0.0.1:{}/v1", cfg.chatterbox.port),
                        api_key:       String::new(),  // local server, no auth
                        model:         "chatterbox".to_string(),
                        default_voice: cfg.chatterbox.default_voice.clone(),
                        timeout_secs:  cfg.request_timeout_secs,
                    })));
                info!("tts: chatterbox backend ready (port={}, voice={})",
                    cfg.chatterbox.port, cfg.chatterbox.default_voice);
            }

            // OpenAI hosted: wire whenever a key is resolvable. We can't
            // realistically validate the key offline, so just register the
            // backend and let `synthesise` surface 401s to the UI.
            let openai_key = resolve_openai_key(cfg.openai.api_key.as_deref());
            if !openai_key.is_empty() {
                backends.insert(BACKEND_OPENAI.into(),
                    Arc::new(OpenAiBackend::new(OpenAiConfig {
                        id:            "openai",
                        base_url:      cfg.openai.base_url.clone(),
                        api_key:       openai_key,
                        model:         cfg.openai.model.clone(),
                        default_voice: cfg.openai.default_voice.clone(),
                        timeout_secs:  cfg.request_timeout_secs,
                    })));
                info!("tts: openai backend ready (model={})", cfg.openai.model);
            }

            // OpenAI-compat (self-hosted): require an explicit URL since the
            // default `http://localhost:8000/v1` is just a placeholder
            // pointing at no real server. Auth is optional — many self-hosted
            // servers run open inside a LAN.
            let compat_url = cfg.openai_compat.url.trim().to_string();
            if !compat_url.is_empty() && compat_url != TtsOpenaiCompatConfig::default().url {
                let key = cfg.openai_compat.api_key.clone().unwrap_or_default();
                backends.insert(BACKEND_OPENAI_COMPAT.into(),
                    Arc::new(OpenAiBackend::new(OpenAiConfig {
                        id:            "openai_compat",
                        base_url:      compat_url.clone(),
                        api_key:       key,
                        model:         cfg.openai_compat.model.clone(),
                        default_voice: cfg.openai_compat.default_voice.clone(),
                        timeout_secs:  cfg.request_timeout_secs,
                    })));
                info!("tts: openai_compat backend ready (url={compat_url})");
            }
        } else {
            info!("tts: subsystem disabled in config");
        }

        let cache = if cfg.cache.enabled {
            let dir = mira.data_dir_path().join("tts").join("cache");
            Some(Arc::new(TtsCache::new(dir, cfg.cache.max_disk_mb, cfg.cache.ttl_days)))
        } else {
            None
        };

        TtsServiceInner { cfg, backends, cache }
    }

    pub fn config(&self) -> TtsConfig { self.snapshot().cfg.clone() }
    pub fn enabled(&self) -> bool     { self.snapshot().cfg.enabled }
    pub fn cache(&self) -> Option<Arc<TtsCache>> { self.snapshot().cache.clone() }

    // Per-channel routing override. Returns the configured backend id for a
    // channel (`"web"`, `"tui"`, `"telegram"`, `"signal"`) or an empty
    // string when no override is set. Channel adapters use this to decide
    // whether to attempt outbound voice synthesis at all.
    pub fn routing_for(&self, channel: &str) -> String {
        let snap = self.snapshot();
        let r = &snap.cfg.routing;
        match channel {
            "web"      => r.web.clone(),
            "tui"      => r.tui.clone(),
            "telegram" => r.telegram.clone(),
            "signal"   => r.signal.clone(),
            _          => String::new(),
        }
    }

    // Server-side default voice prefs keyed by channel id. Channel
    // dispatchers feed this to the resolver alongside the calling user's
    // own prefs.
    pub fn voice_prefs_defaults(&self) -> crate::voice::VoicePrefsMap {
        self.snapshot().cfg.voice_prefs.clone()
    }

    // Backend ids currently registered. Includes the eSpeak fallback when
    // the internal subsystem is on.
    pub fn backend_ids(&self) -> Vec<String> {
        let snap = self.snapshot();
        let mut v: Vec<String> = snap.backends.keys().cloned().collect();
        v.sort();
        v
    }

    // Resolve a backend id for one request, applying:
    // 1. an explicit per-request `requested` id (if non-empty),
    // 2. the per-channel routing rule (if `channel` matches a known one),
    // 3. the global `tts.default_backend`.
    // `"internal"` is then expanded to whichever engine is configured under
    // `tts.internal.engine`.
    pub fn resolve_backend(&self, requested: Option<&str>, channel: Option<&str>) -> String {
        let snap = self.snapshot();
        if let Some(b) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            return Self::expand_internal_in(&snap, b);
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
                return Self::expand_internal_in(&snap, route);
            }
        }
        Self::expand_internal_in(&snap, &snap.cfg.default_backend)
    }

    fn expand_internal_in(snap: &TtsServiceInner, name: &str) -> String {
        match name {
            "internal" => snap.cfg.internal.engine.clone(),
            other      => other.to_string(),
        }
    }

    fn backend_from(snap: &TtsServiceInner, id: &str) -> Result<Arc<dyn TtsBackend>, TtsError> {
        snap.backends.get(id).cloned().ok_or_else(||
            TtsError::BackendNotConfigured(id.to_string())
        )
    }

    // One-shot synthesise. Used by `POST /api/tts/speak`, `mira tts say`,
    // and the messaging-channel voice-note path.
    pub async fn speak(
        &self,
        text:    &str,
        voice:   Option<&str>,
        speed:   Option<f32>,
        format:  Option<OutputFormat>,
        backend: Option<&str>,
        channel: Option<&str>,
    ) -> Result<AudioBuffer, TtsError> {
        let snap = self.snapshot();
        if !snap.cfg.enabled {
            return Err(TtsError::BackendNotConfigured("tts disabled".into()));
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(TtsError::BadRequest("text is empty".into()));
        }
        let max = snap.cfg.max_chars_per_request;
        if trimmed.chars().count() > max {
            return Err(TtsError::BadRequest(format!(
                "text too long: {} chars > limit {max}",
                trimmed.chars().count()
            )));
        }

        // Strip Markdown so backends don't read "asterisk asterisk" aloud.
        let stripped = crate::tts::text_filter::strip_markdown_for_speech(trimmed);
        let trimmed  = stripped.trim();
        if trimmed.is_empty() {
            return Err(TtsError::BadRequest("text is empty after markdown stripping".into()));
        }

        let backend_id = self.resolve_backend(backend, channel);
        let backend    = Self::backend_from(&snap, &backend_id).or_else(|orig| {
            // If the requested internal engine isn't wired (e.g. user sets
            // engine = "kokoro" before), fall back to the first
            // available internal backend so the user still gets *something*.
            warn!("tts: backend '{backend_id}' not configured, attempting fallback");
            Self::backend_from(&snap, INTERNAL_ENGINE_PIPER)
                .or_else(|_| Self::backend_from(&snap, INTERNAL_ENGINE_ESPEAK))
                .map_err(|_| orig)
        })?;

        let voice_id = voice.map(str::to_string).filter(|s| !s.is_empty())
            .or_else(|| Some(snap.cfg.default_voice.clone())
                          .filter(|s| !s.is_empty()));
        let speed = speed.unwrap_or(snap.cfg.default_speed);
        let fmt   = format.unwrap_or_else(||
            OutputFormat::parse(&snap.cfg.default_format).unwrap_or(OutputFormat::Wav));

        let key = cache_key(
            trimmed,
            voice_id.as_deref().unwrap_or(""),
            backend.id(),
            speed,
        );
        if let Some(cache) = &snap.cache {
            if let Some(buf) = cache.get(&key).await {
                debug!("tts: cache hit {key}");
                return Ok(buf);
            }
        }

        let req = SynthesiseRequest {
            text:     trimmed.to_string(),
            voice_id,
            speed,
            format:   fmt,
            is_ssml:  false,
        };

        let dur = Duration::from_secs(snap.cfg.request_timeout_secs.max(1));
        let (buf, used_backend_id) =
            Self::synth_with_fallback(&snap, &backend, &req, dur, channel, self.degradations.as_deref()).await?;

        // Cache under the backend that actually produced the audio. If
        // we fell back to Piper, we don't want a future request (with
        // the primary back up) to be served stale Piper output — so
        // each backend gets its own cache entry.
        let cache_key = if used_backend_id == backend.id() {
            key
        } else {
            cache_key(
                trimmed,
                "",
                &used_backend_id,
                speed,
            )
        };
        if let Some(cache) = &snap.cache {
            cache.put(&cache_key, &buf).await;
        }
        Ok(buf)
    }

    // Try `primary` once; on a voice-not-installed error retry the same
    // backend with `voice_id: None`; on a transient failure (network,
    // timeout, upstream 5xx, IO) and only if the primary isn't already
    // internal, retry on internal Piper (then eSpeak). Returns
    // `(audio, backend_id_actually_used)` so callers cache under the
    // right key.
    //     // Voice id on the cross-backend fallback is forced to `None` since
    // remote voices (e.g. an OpenAI-compat voice name) typically don't
    // exist on Piper; Piper picks its own default.
    async fn synth_with_fallback(
        snap: &TtsServiceInner,
        primary: &Arc<dyn TtsBackend>,
        req: &SynthesiseRequest,
        dur: Duration,
        channel: Option<&str>,
        degradations: Option<&crate::health::degradation::DegradationTracker>,
    ) -> Result<(AudioBuffer, String), TtsError> {
        let primary_id = primary.id().to_string();

        let first_err = match timeout(dur, primary.synthesise(req)).await {
            Ok(Ok(b)) => return Ok((b, primary_id)),
            // Voice-not-on-backend recovery. Common case: a per-user
            // voice_pref points at a Piper voice id while the channel
            // is pinned to an openai_compat backend (Chatterbox etc.)
            // that doesn't recognise that name. Retry once on the SAME
            // backend with the backend's default voice.
            Ok(Err(TtsError::VoiceNotInstalled(bad))) => {
                warn!(
                    "tts: voice {bad:?} not available on backend {primary_id}; \
                     retrying with backend default. Update your per-user \
                     voice_prefs.{} to a voice the backend knows about \
                     to suppress this warning.",
                    channel.unwrap_or("(default)"),
                );
                let mut r = req.clone();
                r.voice_id = None;
                match timeout(dur, primary.synthesise(&r)).await {
                    Ok(Ok(b))  => return Ok((b, primary_id)),
                    Ok(Err(e)) => e,
                    Err(_)     => TtsError::Timeout,
                }
            }
            Ok(Err(e)) => e,
            Err(_)     => TtsError::Timeout,
        };

        // Primary failed. If it's already internal, there's nothing to
        // fall back to; or if the error isn't transient (BadRequest,
        // Unauthorized), don't retry — switching backends won't help.
        if is_internal_backend_id(&primary_id) || !is_transient_failure(&first_err) {
            return Err(first_err);
        }

        let fb = match Self::backend_from(snap, INTERNAL_ENGINE_PIPER)
            .or_else(|_| Self::backend_from(snap, INTERNAL_ENGINE_ESPEAK))
        {
            Ok(b) => b,
            Err(_) => return Err(first_err),
        };

        warn!(
            "tts: backend '{primary_id}' failed ({first_err}); falling back \
             to internal '{}' for this synthesis. Voice may sound different \
             — restore '{primary_id}' to get its voice back.",
            fb.id(),
        );
        if let Some(tracker) = degradations {
            tracker.record(
                "tts", "Voice synthesis (TTS)",
                &primary_id, fb.id(),
                &crate::health::degradation::DegradationTracker::short(&first_err.to_string()),
                false,
            );
        }

        let fb_id = fb.id().to_string();
        let mut r = req.clone();
        r.voice_id = None;
        match timeout(dur, fb.synthesise(&r)).await {
            Ok(Ok(b))  => Ok((b, fb_id)),
            Ok(Err(e)) => Err(e),
            Err(_)     => Err(TtsError::Timeout),
        }
    }

    // Streaming variant of [`speak`]. Splits the input through
    // [`SentenceChunker`] and synthesises each sentence in turn, yielding one
    // [`AudioChunk`] per sentence. The final chunk has `is_final = true`.
    //     // Each chunk is an independently playable audio buffer (current internal
    // backends emit a complete WAV per call); the web client queues them
    // sequentially. A future  can swap to MediaSource appends once
    // we have a single-stream codec like MP3.
    //     // Cache lookup happens per sentence — short repeated sentences (greetings,
    // system phrases) skip synthesis on subsequent plays.
    pub async fn speak_stream(
        &self,
        text:    &str,
        voice:   Option<&str>,
        speed:   Option<f32>,
        format:  Option<OutputFormat>,
        backend: Option<&str>,
        channel: Option<&str>,
    ) -> Result<BoxStream<'static, Result<AudioChunk, TtsError>>, TtsError> {
        let snap = self.snapshot();
        if !snap.cfg.enabled {
            return Err(TtsError::BackendNotConfigured("tts disabled".into()));
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(TtsError::BadRequest("text is empty".into()));
        }
        let max = snap.cfg.max_chars_per_request;
        if trimmed.chars().count() > max {
            return Err(TtsError::BadRequest(format!(
                "text too long: {} chars > limit {max}",
                trimmed.chars().count()
            )));
        }

        // Strip Markdown so backends don't read "asterisk asterisk" aloud.
        let stripped = crate::tts::text_filter::strip_markdown_for_speech(trimmed);
        let trimmed  = stripped.trim();
        if trimmed.is_empty() {
            return Err(TtsError::BadRequest("text is empty after markdown stripping".into()));
        }

        let backend_id = self.resolve_backend(backend, channel);
        let backend    = Self::backend_from(&snap, &backend_id).or_else(|orig| {
            warn!("tts: backend '{backend_id}' not configured, attempting fallback");
            Self::backend_from(&snap, INTERNAL_ENGINE_PIPER)
                .or_else(|_| Self::backend_from(&snap, INTERNAL_ENGINE_ESPEAK))
                .map_err(|_| orig)
        })?;

        let voice_id = voice.map(str::to_string).filter(|s| !s.is_empty())
            .or_else(|| Some(snap.cfg.default_voice.clone())
                          .filter(|s| !s.is_empty()));
        let speed = speed.unwrap_or(snap.cfg.default_speed);
        let fmt   = format.unwrap_or_else(||
            OutputFormat::parse(&snap.cfg.default_format).unwrap_or(OutputFormat::Wav));

        // Pre-chunk synchronously so the stream knows which is the final one.
        let mut chunker = SentenceChunker::new();
        let mut sentences = chunker.feed(trimmed);
        if let Some(tail) = chunker.flush() {
            sentences.push(tail);
        }
        if sentences.is_empty() {
            // Pathological: trimmed non-empty but chunker yielded nothing.
            sentences.push(trimmed.to_string());
        }
        let total = sentences.len();
        debug!("tts: speak_stream split into {total} sentence(s)");

        let cache    = snap.cache.clone();
        let timeout_secs = snap.cfg.request_timeout_secs.max(1);
        let primary_id = backend.id().to_string();
        // Resolve the internal fallback backend once, up front, so we
        // can skip the dead primary on every sentence after the first
        // failure. Otherwise N sentences × request_timeout_secs is a
        // multi-minute hang on a downed remote service.
        let fallback_backend: Option<Arc<dyn TtsBackend>> =
            if is_internal_backend_id(&primary_id) {
                None
            } else {
                Self::backend_from(&snap, INTERNAL_ENGINE_PIPER)
                    .or_else(|_| Self::backend_from(&snap, INTERNAL_ENGINE_ESPEAK))
                    .ok()
            };
        // Shared, mutable across sentence closures — when the first
        // sentence detects the per-user voice doesn't exist on the
        // backend, all subsequent sentences in this stream skip
        // straight to the backend's default voice (saves an upstream
        // round trip + a warn line per sentence).
        let voice_cell = std::sync::Arc::new(std::sync::Mutex::new(voice_id));
        // Same idea for "primary is dead, go straight to fallback":
        // sticky across sentences once flipped.
        let primary_dead = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let channel_for_log = channel.unwrap_or("(default)").to_string();

        let stream = futures::stream::iter(sentences.into_iter().enumerate())
            .then(move |(idx, sentence)| {
                let primary  = backend.clone();
                let fallback = fallback_backend.clone();
                let cache    = cache.clone();
                let voice_cell = std::sync::Arc::clone(&voice_cell);
                let primary_dead = std::sync::Arc::clone(&primary_dead);
                let primary_id = primary_id.clone();
                let channel_for_log = channel_for_log.clone();
                async move {
                    use std::sync::atomic::Ordering;
                    let voice_id = voice_cell.lock().expect("voice_cell").clone();
                    // Decide which backend to even attempt: if the
                    // primary already proved dead earlier in this
                    // stream and we have a fallback, skip the primary.
                    let skip_primary =
                        primary_dead.load(Ordering::Relaxed) && fallback.is_some();
                    let attempt_backend = if skip_primary {
                        fallback.as_ref().expect("fallback").clone()
                    } else {
                        primary.clone()
                    };
                    let attempt_id = attempt_backend.id().to_string();
                    // When we skip straight to the fallback, voice
                    // names from the original backend (e.g. an
                    // openai_compat voice) won't exist on Piper, so
                    // force the backend default.
                    let effective_voice = if skip_primary { None } else { voice_id.clone() };

                    let key = cache_key(
                        &sentence,
                        effective_voice.as_deref().unwrap_or(""),
                        &attempt_id,
                        speed,
                    );
                    if let Some(cache) = &cache {
                        if let Some(buf) = cache.get(&key).await {
                            return Ok(AudioChunk {
                                bytes:    buf.bytes,
                                codec:    buf.codec,
                                is_final: idx + 1 == total,
                            });
                        }
                    }
                    let req = SynthesiseRequest {
                        text:     sentence,
                        voice_id: effective_voice,
                        speed,
                        format:   fmt,
                        is_ssml:  false,
                    };
                    let dur = Duration::from_secs(timeout_secs);
                    let first_err = match timeout(dur, attempt_backend.synthesise(&req)).await {
                        Ok(Ok(b))  => {
                            if let Some(cache) = &cache {
                                cache.put(&key, &b).await;
                            }
                            return Ok(AudioChunk {
                                bytes:    b.bytes,
                                codec:    b.codec,
                                is_final: idx + 1 == total,
                            });
                        }
                        Ok(Err(TtsError::VoiceNotInstalled(bad))) if !skip_primary => {
                            // Same recovery as `speak()`: downgrade the
                            // request to `voice_id: None` so the backend
                            // picks its own default. Persist the
                            // downgrade in the cell so later sentences
                            // skip straight to the working voice.
                            warn!(
                                "tts: voice {bad:?} not available on backend {primary_id}; \
                                 retrying with backend default. Update your \
                                 per-user voice_prefs.{channel_for_log} to a voice \
                                 the backend knows about to suppress this warning.",
                            );
                            *voice_cell.lock().expect("voice_cell") = None;
                            let mut req2 = req.clone();
                            req2.voice_id = None;
                            match timeout(dur, attempt_backend.synthesise(&req2)).await {
                                Ok(Ok(b))  => {
                                    let key2 = cache_key(&req2.text, "", &attempt_id, speed);
                                    if let Some(cache) = &cache {
                                        cache.put(&key2, &b).await;
                                    }
                                    return Ok(AudioChunk {
                                        bytes:    b.bytes,
                                        codec:    b.codec,
                                        is_final: idx + 1 == total,
                                    });
                                }
                                Ok(Err(e)) => e,
                                Err(_)     => TtsError::Timeout,
                            }
                        }
                        Ok(Err(e)) => e,
                        Err(_)     => TtsError::Timeout,
                    };

                    // Primary failed. Try the fallback (if available
                    // and we didn't already start there).
                    if skip_primary
                        || is_internal_backend_id(&attempt_id)
                        || !is_transient_failure(&first_err)
                    {
                        return Err(first_err);
                    }
                    let fb = match &fallback {
                        Some(b) => b.clone(),
                        None    => return Err(first_err),
                    };
                    warn!(
                        "tts: backend '{primary_id}' failed ({first_err}); falling back \
                         to internal '{}' for the rest of this stream. Voice may sound \
                         different — restore '{primary_id}' to get its voice back.",
                        fb.id(),
                    );
                    primary_dead.store(true, Ordering::Relaxed);

                    let fb_id = fb.id().to_string();
                    let mut fb_req = req.clone();
                    fb_req.voice_id = None;
                    let fb_key = cache_key(&fb_req.text, "", &fb_id, speed);
                    if let Some(cache) = &cache {
                        if let Some(buf) = cache.get(&fb_key).await {
                            return Ok(AudioChunk {
                                bytes:    buf.bytes,
                                codec:    buf.codec,
                                is_final: idx + 1 == total,
                            });
                        }
                    }
                    let buf = match timeout(dur, fb.synthesise(&fb_req)).await {
                        Ok(Ok(b))  => b,
                        Ok(Err(e)) => return Err(e),
                        Err(_)     => return Err(TtsError::Timeout),
                    };
                    if let Some(cache) = &cache {
                        cache.put(&fb_key, &buf).await;
                    }
                    Ok(AudioChunk {
                        bytes:    buf.bytes,
                        codec:    buf.codec,
                        is_final: idx + 1 == total,
                    })
                }
            })
            .boxed();
        Ok(stream)
    }

    pub async fn list_voices(&self, backend: Option<&str>) -> Result<Vec<Voice>, TtsError> {
        let snap = self.snapshot();
        let id = backend.map(str::to_string)
            .unwrap_or_else(|| self.resolve_backend(None, None));
        Self::backend_from(&snap, &id)?.list_voices().await
    }

    // Pre-fetch voice assets (currently meaningful only for the local Piper
    // backend, where it pulls the `.onnx` model pair from huggingface). Called
    // when the user picks a voice in the Settings UI so the first
    // `/api/tts/speak` doesn't pay the download latency.
    pub async fn ensure_voice(&self, backend: Option<&str>, voice_id: &str)
        -> Result<(), TtsError>
    {
        let snap = self.snapshot();
        let id = backend.map(str::to_string)
            .unwrap_or_else(|| self.resolve_backend(None, None));
        Self::backend_from(&snap, &id)?.ensure_voice(voice_id).await
    }

    pub async fn probe(&self, backend: Option<&str>) -> Result<ProbeResult, TtsError> {
        let snap = self.snapshot();
        let id = backend.map(str::to_string)
            .unwrap_or_else(|| self.resolve_backend(None, None));
        Self::backend_from(&snap, &id)?.probe().await
    }

    pub async fn cache_stats(&self) -> CacheStats {
        match &self.snapshot().cache {
            Some(c) => c.stats().await,
            None    => CacheStats::default(),
        }
    }

    pub async fn cache_clear(&self) -> std::io::Result<()> {
        match &self.snapshot().cache {
            Some(c) => c.clear().await,
            None    => Ok(()),
        }
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
        cfg.tts.internal.auto_download_voices = false;
        cfg
    }

    #[test]
    fn from_config_wires_internal_backends() {
        let dir = tempdir().unwrap();
        let svc = TtsService::from_config(&mira_cfg(dir.path()));
        let ids = svc.backend_ids();
        assert!(ids.contains(&"piper".to_string()));
        assert!(ids.contains(&"espeak".to_string()));
    }

    #[test]
    fn disabled_config_wires_no_backends() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.tts.enabled = false;
        let svc = TtsService::from_config(&cfg);
        assert!(svc.backend_ids().is_empty());
    }

    #[test]
    fn resolve_backend_per_request_wins() {
        let dir = tempdir().unwrap();
        let svc = TtsService::from_config(&mira_cfg(dir.path()));
        assert_eq!(svc.resolve_backend(Some("espeak"), None), "espeak");
        assert_eq!(svc.resolve_backend(Some("internal"), None), "piper");
    }

    #[test]
    fn resolve_backend_falls_through_routing_to_default() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.tts.routing.web = "espeak".into();
        let svc = TtsService::from_config(&cfg);
        assert_eq!(svc.resolve_backend(None, Some("web")),      "espeak");
        assert_eq!(svc.resolve_backend(None, Some("telegram")), "piper"); // unchanged
        assert_eq!(svc.resolve_backend(None, None),             "piper");
    }

    #[tokio::test]
    async fn speak_rejects_disabled_subsystem() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.tts.enabled = false;
        let svc = TtsService::from_config(&cfg);
        let err = svc.speak("hi", None, None, None, None, None).await.unwrap_err();
        assert!(matches!(err, TtsError::BackendNotConfigured(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn speak_rejects_empty_text() {
        let dir = tempdir().unwrap();
        let svc = TtsService::from_config(&mira_cfg(dir.path()));
        let err = svc.speak("   ", None, None, None, None, None).await.unwrap_err();
        assert!(matches!(err, TtsError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn speak_rejects_too_long_text() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.tts.max_chars_per_request = 5;
        let svc = TtsService::from_config(&cfg);
        let err = svc.speak("hello world", None, None, None, None, None).await.unwrap_err();
        assert!(matches!(err, TtsError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn list_voices_returns_curated_piper_set() {
        let dir = tempdir().unwrap();
        let svc = TtsService::from_config(&mira_cfg(dir.path()));
        let voices = svc.list_voices(None).await.unwrap();
        assert!(voices.iter().any(|v| v.id == "en_US-amy-medium"));
        assert!(voices.iter().all(|v| v.backend_id == "piper"));
    }

    #[tokio::test]
    async fn speak_stream_rejects_disabled_subsystem() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.tts.enabled = false;
        let svc = TtsService::from_config(&cfg);
        match svc.speak_stream("hi.", None, None, None, None, None).await {
            Err(TtsError::BackendNotConfigured(_)) => {}
            other => panic!("expected BackendNotConfigured, got Ok/{other:?}",
                other = other.as_ref().err()),
        }
    }

    #[tokio::test]
    async fn speak_stream_rejects_empty_text() {
        let dir = tempdir().unwrap();
        let svc = TtsService::from_config(&mira_cfg(dir.path()));
        match svc.speak_stream("   ", None, None, None, None, None).await {
            Err(TtsError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got Ok/{other:?}",
                other = other.as_ref().err()),
        }
    }

    #[tokio::test]
    async fn speak_stream_chunks_long_text() {
        // Drive against the piper backend with auto_download = false. The
        // backend will error on every synth attempt (no binary present), but
        // the *count* of results proves the chunker split into the expected
        // number of sentences. Deterministic across hosts.
        use futures::StreamExt;
        let dir = tempdir().unwrap();
        let svc = TtsService::from_config(&mira_cfg(dir.path()));
        let stream = svc.speak_stream(
            "First sentence here. Second sentence follows! Third one too?",
            None, None, None, Some("piper"), None,
        ).await.expect("stream returned");
        let results: Vec<_> = stream.collect().await;
        assert_eq!(results.len(), 3, "three sentences → three chunks");
        for r in &results {
            assert!(r.is_err(), "no Piper binary in tests; expected errors, got {r:?}");
        }
    }

    #[tokio::test]
    async fn speak_stream_single_chunk_for_short_text() {
        use futures::StreamExt;
        let dir = tempdir().unwrap();
        let svc = TtsService::from_config(&mira_cfg(dir.path()));
        let stream = svc.speak_stream("Hi.", None, None, None, Some("piper"), None)
            .await.expect("stream returned");
        let results: Vec<_> = stream.collect().await;
        assert_eq!(results.len(), 1, "one sentence → one chunk");
    }

    #[tokio::test]
    async fn list_voices_for_espeak_works() {
        let dir = tempdir().unwrap();
        let svc = TtsService::from_config(&mira_cfg(dir.path()));
        let voices = svc.list_voices(Some("espeak")).await.unwrap();
        assert!(voices.iter().all(|v| v.backend_id == "espeak"));
    }

    #[test]
    fn from_config_skips_openai_when_no_key() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.tts.openai.api_key = None;
        // Shadow any inherited env var so this test stays hermetic.
        let _guard = EnvGuard::unset("OPENAI_API_KEY");
        let svc = TtsService::from_config(&cfg);
        assert!(!svc.backend_ids().contains(&"openai".to_string()));
    }

    #[test]
    fn from_config_wires_openai_when_key_present() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.tts.openai.api_key = Some("sk-test-123".into());
        let svc = TtsService::from_config(&cfg);
        assert!(svc.backend_ids().contains(&"openai".to_string()));
    }

    #[test]
    fn from_config_skips_openai_compat_when_url_is_default_placeholder() {
        let dir = tempdir().unwrap();
        let svc = TtsService::from_config(&mira_cfg(dir.path()));
        assert!(!svc.backend_ids().contains(&"openai_compat".to_string()));
    }

    #[test]
    fn from_config_wires_openai_compat_when_url_overridden() {
        let dir = tempdir().unwrap();
        let mut cfg = mira_cfg(dir.path());
        cfg.tts.openai_compat.url = "http://my-piper-daemon.local:5050".into();
        let svc = TtsService::from_config(&cfg);
        assert!(svc.backend_ids().contains(&"openai_compat".to_string()));
    }

    #[test]
    fn resolve_openai_key_prefers_config_over_env() {
        let _guard = EnvGuard::set("OPENAI_API_KEY", "from-env");
        assert_eq!(resolve_openai_key(Some("from-config")), "from-config");
        assert_eq!(resolve_openai_key(Some("   ")),         "from-env");
        assert_eq!(resolve_openai_key(None),                "from-env");
    }

    // Tiny scoped env-var guard so the key tests don't leak state across
    // the suite. Restores the prior value (or unset) when dropped.
    struct EnvGuard {
        key:   &'static str,
        prior: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value); }
            Self { key, prior }
        }
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

    // ─── Mock backends used by the fallback tests below ─────────────────
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use crate::tts::types::{AudioCodec, ProbeResult, Voice};

    struct FlakyBackend {
        id:    &'static str,
        calls: Arc<AtomicUsize>,
        error: fn() -> TtsError,
    }

    #[async_trait]
    impl TtsBackend for FlakyBackend {
        fn id(&self) -> &'static str { self.id }
        async fn list_voices(&self) -> Result<Vec<Voice>, TtsError> { Ok(vec![]) }
        async fn synthesise(&self, _req: &SynthesiseRequest) -> Result<AudioBuffer, TtsError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err((self.error)())
        }
        async fn synthesise_stream(&self, _req: &SynthesiseRequest)
            -> Result<BoxStream<'static, Result<AudioChunk, TtsError>>, TtsError>
        { Err((self.error)()) }
        async fn probe(&self) -> Result<ProbeResult, TtsError> { Err((self.error)()) }
    }

    struct OkBackend {
        id:    &'static str,
        calls: Arc<AtomicUsize>,
        marker: u8,
    }

    #[async_trait]
    impl TtsBackend for OkBackend {
        fn id(&self) -> &'static str { self.id }
        async fn list_voices(&self) -> Result<Vec<Voice>, TtsError> { Ok(vec![]) }
        async fn synthesise(&self, _req: &SynthesiseRequest) -> Result<AudioBuffer, TtsError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(AudioBuffer {
                bytes: vec![self.marker, self.marker],
                codec: AudioCodec::Wav { sample_rate: 22_050, channels: 1 },
            })
        }
        async fn synthesise_stream(&self, _req: &SynthesiseRequest)
            -> Result<BoxStream<'static, Result<AudioChunk, TtsError>>, TtsError>
        { Err(TtsError::Upstream("not used".into())) }
        async fn probe(&self) -> Result<ProbeResult, TtsError> {
            Err(TtsError::Upstream("not used".into()))
        }
    }

    fn inner_with_backends(backends: HashMap<String, Arc<dyn TtsBackend>>) -> TtsServiceInner {
        TtsServiceInner {
            cfg:      TtsConfig::default(),
            backends,
            cache:    None,
        }
    }

    #[tokio::test]
    async fn synth_with_fallback_recovers_on_transient_primary_error() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let piper_calls   = Arc::new(AtomicUsize::new(0));
        let primary: Arc<dyn TtsBackend> = Arc::new(FlakyBackend {
            id:    "openai_compat",
            calls: primary_calls.clone(),
            error: || TtsError::Upstream("simulated outage".into()),
        });
        let piper: Arc<dyn TtsBackend> = Arc::new(OkBackend {
            id:    INTERNAL_ENGINE_PIPER,
            calls: piper_calls.clone(),
            marker: 0xCA,
        });
        let mut backends: HashMap<String, Arc<dyn TtsBackend>> = HashMap::new();
        backends.insert("openai_compat".into(),   primary.clone());
        backends.insert(INTERNAL_ENGINE_PIPER.into(), piper);
        let snap = inner_with_backends(backends);

        let req = SynthesiseRequest {
            text:     "hi there".into(),
            voice_id: Some("remote-voice".into()),
            speed:    1.0,
            format:   OutputFormat::Wav,
            is_ssml:  false,
        };
        let (buf, used) = TtsService::synth_with_fallback(
            &snap, &primary, &req, Duration::from_secs(5), Some("signal"), None,
        ).await.expect("fallback should produce audio");

        assert_eq!(used, INTERNAL_ENGINE_PIPER, "fallback reports piper as actually used");
        assert_eq!(buf.bytes, vec![0xCA, 0xCA]);
        assert_eq!(primary_calls.load(Ordering::Relaxed), 1, "primary tried exactly once");
        assert_eq!(piper_calls.load(Ordering::Relaxed),   1, "piper fallback hit once");
    }

    #[tokio::test]
    async fn synth_with_fallback_propagates_bad_request_without_retry() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let piper_calls   = Arc::new(AtomicUsize::new(0));
        let primary: Arc<dyn TtsBackend> = Arc::new(FlakyBackend {
            id:    "openai_compat",
            calls: primary_calls.clone(),
            error: || TtsError::BadRequest("malformed text".into()),
        });
        let piper: Arc<dyn TtsBackend> = Arc::new(OkBackend {
            id:    INTERNAL_ENGINE_PIPER,
            calls: piper_calls.clone(),
            marker: 0xFE,
        });
        let mut backends: HashMap<String, Arc<dyn TtsBackend>> = HashMap::new();
        backends.insert("openai_compat".into(),   primary.clone());
        backends.insert(INTERNAL_ENGINE_PIPER.into(), piper);
        let snap = inner_with_backends(backends);

        let req = SynthesiseRequest {
            text: "x".into(), voice_id: None, speed: 1.0,
            format: OutputFormat::Wav, is_ssml: false,
        };
        let err = TtsService::synth_with_fallback(
            &snap, &primary, &req, Duration::from_secs(5), None, None,
        ).await.unwrap_err();
        assert!(matches!(err, TtsError::BadRequest(_)), "got {err:?}");
        assert_eq!(primary_calls.load(Ordering::Relaxed), 1);
        assert_eq!(piper_calls.load(Ordering::Relaxed),   0, "piper should not be tried");
    }

    #[tokio::test]
    async fn synth_with_fallback_no_loop_when_primary_is_internal() {
        // If the primary IS Piper and it fails transiently, we have
        // nowhere to fall back to — return the original error rather
        // than retrying the same backend in a loop.
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let primary: Arc<dyn TtsBackend> = Arc::new(FlakyBackend {
            id:    INTERNAL_ENGINE_PIPER,
            calls: primary_calls.clone(),
            error: || TtsError::Upstream("piper itself died".into()),
        });
        let mut backends: HashMap<String, Arc<dyn TtsBackend>> = HashMap::new();
        backends.insert(INTERNAL_ENGINE_PIPER.into(), primary.clone());
        let snap = inner_with_backends(backends);

        let req = SynthesiseRequest {
            text: "x".into(), voice_id: None, speed: 1.0,
            format: OutputFormat::Wav, is_ssml: false,
        };
        let err = TtsService::synth_with_fallback(
            &snap, &primary, &req, Duration::from_secs(5), None, None,
        ).await.unwrap_err();
        assert!(matches!(err, TtsError::Upstream(_)), "got {err:?}");
        assert_eq!(primary_calls.load(Ordering::Relaxed), 1, "no loop");
    }

    #[tokio::test]
    async fn synth_with_fallback_retries_voice_not_installed_on_same_backend() {
        // VoiceNotInstalled is a separate recovery: retry on the SAME
        // backend with voice_id = None. Don't escalate to Piper.
        let primary_calls = Arc::new(AtomicUsize::new(0));

        struct PickyVoiceBackend {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl TtsBackend for PickyVoiceBackend {
            fn id(&self) -> &'static str { "openai_compat" }
            async fn list_voices(&self) -> Result<Vec<Voice>, TtsError> { Ok(vec![]) }
            async fn synthesise(&self, req: &SynthesiseRequest) -> Result<AudioBuffer, TtsError> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                match &req.voice_id {
                    Some(v) => Err(TtsError::VoiceNotInstalled(v.clone())),
                    None    => Ok(AudioBuffer {
                        bytes: vec![0xAB],
                        codec: AudioCodec::Wav { sample_rate: 22_050, channels: 1 },
                    }),
                }
            }
            async fn synthesise_stream(&self, _req: &SynthesiseRequest)
                -> Result<BoxStream<'static, Result<AudioChunk, TtsError>>, TtsError>
            { Err(TtsError::Upstream("not used".into())) }
            async fn probe(&self) -> Result<ProbeResult, TtsError> {
                Err(TtsError::Upstream("not used".into()))
            }
        }

        let primary: Arc<dyn TtsBackend> = Arc::new(PickyVoiceBackend { calls: primary_calls.clone() });
        let mut backends: HashMap<String, Arc<dyn TtsBackend>> = HashMap::new();
        backends.insert("openai_compat".into(), primary.clone());
        let snap = inner_with_backends(backends);

        let req = SynthesiseRequest {
            text: "x".into(), voice_id: Some("nonsense-voice".into()),
            speed: 1.0, format: OutputFormat::Wav, is_ssml: false,
        };
        let (buf, used) = TtsService::synth_with_fallback(
            &snap, &primary, &req, Duration::from_secs(5), None, None,
        ).await.expect("retry with default voice should succeed");
        assert_eq!(used, "openai_compat", "stayed on primary, did NOT escalate to Piper");
        assert_eq!(buf.bytes, vec![0xAB]);
        assert_eq!(primary_calls.load(Ordering::Relaxed), 2, "tried twice (with voice, then without)");
    }
}
