// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/backend/kokoro.rs
//! Native Kokoro TTS backend (K1 / Q2 #10).
//!
//! Runs the Kokoro-82M model **in-process** via the `any-tts` crate on a
//! pure-Rust Candle backend. Unlike Piper (which shells out to a downloaded
//! executable) or the openai_compat path (which needs a separate server),
//! Kokoro here is just a library call — no subprocess, no ONNX native lib,
//! and no system `espeak-ng` (any-tts ships an in-tree pure-Rust
//! phonemizer). That makes it the "good voice with zero setup" default for
//! the wider audience the v1.0 installer targets.
//!
//! Compiled only under `--features kokoro` — the whole module is behind a
//! cfg gate in `backend/mod.rs`, so a stock build pulls neither Candle nor
//! this code.
//!
//! Constraints (v1):
//!   * American/British **English only** — that's the phonemizer's scope.
//!     Other languages stay on eSpeak / cloud backends.
//!   * No speech-rate control — `any-tts` doesn't expose a speed knob on the
//!     request, so `SynthesiseRequest::speed` is ignored here. (The chat UI
//!     still has eSpeak/Piper for rate-sensitive use.)
//!   * The model (~0.3–0.4 GB) loads lazily on first synthesis and is then
//!     held resident; the first call also downloads weights when missing.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use any_tts::{load_model, DeviceSelection, ModelType, SynthesisRequest, TtsConfig as AnyTtsConfig};
use any_tts::traits::TtsModel;
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// HuggingFace repo any-tts pulls Kokoro weights + voices from.
const KOKORO_HF_REPO: &str = "hexgrad/Kokoro-82M";

use crate::tts::backend::TtsBackend;
use crate::tts::types::{
    AudioBuffer, AudioChunk, AudioCodec, ProbeResult, SynthesiseRequest, TtsError, Voice,
};

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

/// Resolved settings for one Kokoro install. Built from `tts.kokoro.*` by
/// the service; kept separate from the serde config so this module owns no
/// config-schema knowledge.
#[derive(Debug, Clone)]
pub struct KokoroConfig {
    /// Model directory — holds the safetensors weights + `voices/*.pt`.
    /// Defaults to `<data_dir>/tts/kokoro/Kokoro-82M`.
    pub model_path:    PathBuf,
    pub default_voice: String,
    /// `auto` | `cpu` | `cuda` | `metal`.
    pub device:        String,
    /// Allow `any-tts` to pull missing weights from HuggingFace on first use.
    pub auto_download: bool,
}

impl KokoroConfig {
    pub fn under_data_dir(data_dir: &std::path::Path) -> Self {
        Self {
            model_path:    data_dir.join("tts").join("kokoro").join("Kokoro-82M"),
            default_voice: "af_heart".to_string(),
            device:        "auto".to_string(),
            auto_download: true,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backend
// ─────────────────────────────────────────────────────────────────────────────

pub struct KokoroBackend {
    cfg: KokoroConfig,
    /// Lazily-loaded model, shared across calls. The tokio mutex serialises
    /// the (expensive, possibly downloading) first load so concurrent
    /// first-callers don't each spin up a copy. `TtsModel: Send + Sync`, so
    /// the `Arc` hands cheaply to the blocking synthesis thread.
    model: Mutex<Option<Arc<dyn TtsModel>>>,
}

impl KokoroBackend {
    pub fn new(cfg: KokoroConfig) -> Self {
        Self { cfg, model: Mutex::new(None) }
    }

    /// Best-effort check for whether the Kokoro model is already on disk.
    /// any-tts may honour our configured `model_path`, but its auto-download
    /// actually lands in the HuggingFace hub cache, so we check both. Used
    /// only for the UI "downloaded?" hint when auto_download is off — never
    /// gates synthesis.
    fn model_present(&self) -> bool {
        if self.cfg.model_path.exists() {
            return true;
        }
        hf_hub_cache_dir()
            .map(|c| c.join("models--hexgrad--Kokoro-82M").exists())
            .unwrap_or(false)
    }

    /// Locate the `voices/` directory any-tts reads `.pt` files from. any-tts
    /// only fetches the *default* voice at model-load and never downloads
    /// other voices on demand, so MIRA drops the rest in here itself. Checks a
    /// self-managed `model_path/voices` first, then any-tts's HF-cache
    /// snapshot dir (`…/models--hexgrad--Kokoro-82M/snapshots/*/voices`).
    /// Returns None until the model has been loaded once (the dir doesn't
    /// exist before that).
    fn voices_dir(&self) -> Option<PathBuf> {
        let managed = self.cfg.model_path.join("voices");
        if managed.is_dir() {
            return Some(managed);
        }
        let snapshots = hf_hub_cache_dir()?
            .join("models--hexgrad--Kokoro-82M")
            .join("snapshots");
        std::fs::read_dir(&snapshots).ok()?.flatten().find_map(|e| {
            let v = e.path().join("voices");
            v.is_dir().then_some(v)
        })
    }

    /// Ensure `<voices_dir>/<voice>.pt` exists, downloading it from the Kokoro
    /// HF repo if missing. Best-effort: on any failure it returns Ok and lets
    /// the synth attempt proceed (any-tts will then error on the missing file,
    /// which `map_synth_error` turns into a graceful default-voice retry).
    /// No-op when auto_download is off or the voices dir can't be located yet.
    async fn ensure_voice_file(&self, voice_id: &str) {
        if !self.cfg.auto_download || voice_id.is_empty() {
            return;
        }
        let Some(dir) = self.voices_dir() else { return };
        let dest = dir.join(format!("{voice_id}.pt"));
        if dest.exists() {
            return;
        }
        let url = format!(
            "https://huggingface.co/{KOKORO_HF_REPO}/resolve/main/voices/{voice_id}.pt"
        );
        let fetch = async {
            let resp = reqwest::get(&url).await?.error_for_status()?;
            let bytes = resp.bytes().await?;
            // Write via a temp file + rename so a partial download can't leave
            // a corrupt .pt that any-tts would then fail to parse forever.
            let tmp = dir.join(format!(".{voice_id}.pt.partial"));
            tokio::fs::write(&tmp, &bytes).await?;
            tokio::fs::rename(&tmp, &dest).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        };
        match fetch.await {
            Ok(())  => info!("tts: kokoro fetched voice '{voice_id}'"),
            Err(e)  => debug!("tts: kokoro could not pre-fetch voice '{voice_id}': {e}"),
        }
    }

    /// Eager warm-up: load the model and pre-fetch every curated voice so the
    /// first play of any voice has no download lag. Called after enabling
    /// Kokoro.
    pub async fn warm_up(&self) -> Result<(), TtsError> {
        self.ensure_model().await?;
        for v in curated_voices() {
            self.ensure_voice_file(v.id).await;
        }
        Ok(())
    }

    fn device_selection(&self) -> DeviceSelection {
        match self.cfg.device.to_ascii_lowercase().as_str() {
            "cpu"   => DeviceSelection::Cpu,
            "cuda"  => DeviceSelection::Cuda(0),
            "metal" => DeviceSelection::Metal(0),
            // `auto` (and anything unrecognised) → let any-tts pick the
            // fastest safe device. Without the cuda/metal *build* features
            // compiled in, this resolves to CPU.
            _ => DeviceSelection::Auto,
        }
    }

    /// Get the resident model, loading it on first use. Heavy: the load
    /// (and any first-run download) runs on a blocking thread so the async
    /// runtime keeps serving other requests.
    async fn ensure_model(&self) -> Result<Arc<dyn TtsModel>, TtsError> {
        let mut guard = self.model.lock().await;
        if let Some(m) = guard.as_ref() {
            return Ok(m.clone());
        }

        if !self.cfg.auto_download && !self.cfg.model_path.exists() {
            return Err(TtsError::BackendUnavailable(
                "kokoro".into(),
                format!(
                    "model not present at {} and auto_download is disabled",
                    self.cfg.model_path.display()
                ),
            ));
        }

        let model_path = self.cfg.model_path.clone();
        let device     = self.device_selection();
        info!("tts: loading Kokoro model from {} (device={:?})", model_path.display(), device);

        let boxed = tokio::task::spawn_blocking(move || {
            let cfg = AnyTtsConfig::new(ModelType::Kokoro)
                .with_model_path(model_path.to_string_lossy().to_string())
                .with_device(device);
            load_model(cfg)
        })
        .await
        .map_err(|e| TtsError::BackendUnavailable("kokoro".into(), format!("load task panicked: {e}")))?
        .map_err(|e| TtsError::BackendUnavailable("kokoro".into(), format!("model load failed: {e}")))?;

        let model: Arc<dyn TtsModel> = Arc::from(boxed);
        *guard = Some(model.clone());
        info!("tts: Kokoro model ready ({} Hz)", model.sample_rate());
        Ok(model)
    }
}

#[async_trait]
impl TtsBackend for KokoroBackend {
    fn id(&self) -> &'static str { "kokoro" }

    async fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        // Curated list so the settings dropdown renders without loading the
        // model. Kokoro has no per-voice downloads — all presets ship inside
        // one bundled model — so "downloaded" really means "is the model
        // available to this backend?". With auto_download on it always will
        // be (MIRA fetches it on first use); otherwise report per-voice
        // presence from the on-disk voices dir.
        let voices_dir = self.voices_dir();
        Ok(curated_voices()
            .iter()
            .map(|v| {
                let downloaded = self.cfg.auto_download
                    || voices_dir.as_ref()
                        .map(|d| d.join(format!("{}.pt", v.id)).exists())
                        .unwrap_or(false);
                Voice {
                    backend_id:    "kokoro".into(),
                    id:            v.id.into(),
                    name:          v.name.into(),
                    language:      v.language.into(),
                    gender:        Some(v.gender.into()),
                    sample_rate:   Some(24_000),
                    is_downloaded: downloaded,
                }
            })
            .collect())
    }

    async fn synthesise(&self, req: &SynthesiseRequest) -> Result<AudioBuffer, TtsError> {
        if req.text.trim().is_empty() {
            return Err(TtsError::BadRequest("text is empty".into()));
        }
        let model    = self.ensure_model().await?;
        let voice    = req.voice_id.clone().unwrap_or_else(|| self.cfg.default_voice.clone());
        // any-tts only fetched the default voice at load; make sure this one's
        // .pt is on disk before we hand off to the (local-only) loader.
        self.ensure_voice_file(&voice).await;
        let voice_err = voice.clone();
        let text     = req.text.clone();

        let (samples, sample_rate) = tokio::task::spawn_blocking(move || {
            let request = SynthesisRequest::new(text)
                .with_voice(voice)
                .with_language("en");
            let audio = model
                .synthesize(&request)
                .map_err(|e| map_synth_error(e, &voice_err))?;
            Ok::<_, TtsError>((audio.samples, model.sample_rate()))
        })
        .await
        .map_err(|e| TtsError::Upstream(format!("kokoro synth task panicked: {e}")))??;

        Ok(AudioBuffer {
            bytes: pcm_f32_to_wav(&samples, sample_rate),
            codec: AudioCodec::Wav { sample_rate, channels: 1 },
        })
    }

    async fn synthesise_stream(
        &self,
        req: &SynthesiseRequest,
    ) -> Result<BoxStream<'static, Result<AudioChunk, TtsError>>, TtsError> {
        // any-tts synthesises a whole utterance at once; the sentence
        // chunker upstream still gives the player early audio. Wrap the
        // buffer as a single final chunk.
        let buf = self.synthesise(req).await?;
        let chunk = AudioChunk { bytes: buf.bytes, codec: buf.codec, is_final: true };
        Ok(stream::once(async move { Ok(chunk) }).boxed())
    }

    async fn probe(&self) -> Result<ProbeResult, TtsError> {
        let start = Instant::now();
        match self.synthesise(&SynthesiseRequest::new("Hello.")).await {
            Ok(_) => Ok(ProbeResult {
                healthy:    true,
                latency_ms: Some(start.elapsed().as_millis() as u64),
                note:       Some("Kokoro-82M (any-tts / Candle)".into()),
            }),
            Err(e) => Ok(ProbeResult {
                healthy:    false,
                latency_ms: None,
                note:       Some(e.to_string()),
            }),
        }
    }

    async fn ensure_voice(&self, voice_id: &str) -> Result<(), TtsError> {
        // Ensure the model is loaded, then make sure this voice's .pt is on
        // disk (an empty id, used by warm-up, just loads the model).
        self.ensure_model().await?;
        self.ensure_voice_file(voice_id).await;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Curated voice list
// ─────────────────────────────────────────────────────────────────────────────

struct KokoroVoice {
    id:       &'static str,
    name:     &'static str,
    language: &'static str,
    gender:   &'static str,
}

/// The English Kokoro v1.0 presets. `af_*`/`am_*` are American, `bf_*`/`bm_*`
/// British. Names match the `voices/<id>.pt` files in the HuggingFace
/// Kokoro-82M distribution.
fn curated_voices() -> &'static [KokoroVoice] {
    &[
        KokoroVoice { id: "af_heart",    name: "Heart (US)",     language: "en-US", gender: "female" },
        KokoroVoice { id: "af_bella",    name: "Bella (US)",     language: "en-US", gender: "female" },
        KokoroVoice { id: "af_nicole",   name: "Nicole (US)",    language: "en-US", gender: "female" },
        KokoroVoice { id: "af_sarah",    name: "Sarah (US)",     language: "en-US", gender: "female" },
        KokoroVoice { id: "af_sky",      name: "Sky (US)",       language: "en-US", gender: "female" },
        KokoroVoice { id: "am_michael",  name: "Michael (US)",   language: "en-US", gender: "male"   },
        KokoroVoice { id: "am_adam",     name: "Adam (US)",      language: "en-US", gender: "male"   },
        KokoroVoice { id: "am_echo",     name: "Echo (US)",      language: "en-US", gender: "male"   },
        KokoroVoice { id: "am_puck",     name: "Puck (US)",      language: "en-US", gender: "male"   },
        KokoroVoice { id: "bf_emma",     name: "Emma (UK)",      language: "en-GB", gender: "female" },
        KokoroVoice { id: "bf_isabella", name: "Isabella (UK)",  language: "en-GB", gender: "female" },
        KokoroVoice { id: "bm_george",   name: "George (UK)",    language: "en-GB", gender: "male"   },
        KokoroVoice { id: "bm_lewis",    name: "Lewis (UK)",     language: "en-GB", gender: "male"   },
    ]
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve the HuggingFace hub cache directory the way the `hf-hub` crate
/// does — `HF_HUB_CACHE`, else `HF_HOME/hub`, else `~/.cache/huggingface/hub`.
/// any-tts's downloader stores Kokoro weights here.
fn hf_hub_cache_dir() -> Option<PathBuf> {
    if let Ok(c) = std::env::var("HF_HUB_CACHE") {
        return Some(PathBuf::from(c));
    }
    if let Ok(h) = std::env::var("HF_HOME") {
        return Some(PathBuf::from(h).join("hub"));
    }
    dirs::home_dir().map(|h| h.join(".cache").join("huggingface").join("hub"))
}

/// Classify an any-tts synthesis error. An unknown/missing voice is mapped to
/// [`TtsError::VoiceNotInstalled`] so the service's voice-recovery path
/// retries this backend with its own default voice — the common case is a
/// per-user voice_pref that names a Piper voice (e.g. `en_GB-jenny_*`) while
/// the active backend is Kokoro. Everything else is a genuine upstream error.
fn map_synth_error(e: impl std::fmt::Display, voice: &str) -> TtsError {
    let msg = e.to_string();
    let low = msg.to_ascii_lowercase();
    let voice_problem = low.contains("unknown voice")
        || low.contains("voice file not found")
        || (low.contains("voice") && low.contains("not found"));
    if voice_problem {
        TtsError::VoiceNotInstalled(voice.to_string())
    } else {
        TtsError::Upstream(format!("kokoro synth failed: {msg}"))
    }
}

/// Encode mono f32 PCM (range [-1.0, 1.0]) as a canonical 16-bit little-endian
/// WAV. Self-contained so we don't pull a WAV crate just for the header.
fn pcm_f32_to_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let channels: u16     = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate    = sample_rate * channels as u32 * (bits_per_sample / 8) as u32;
    let block_align  = channels * (bits_per_sample / 8);
    let data_len     = (samples.len() * 2) as u32;
    let riff_len     = 36 + data_len;

    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());          // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes());           // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let v = (clamped * i16::MAX as f32).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn under_data_dir_path() {
        let cfg = KokoroConfig::under_data_dir(Path::new("/tmp/mira-data"));
        assert_eq!(cfg.model_path, Path::new("/tmp/mira-data/tts/kokoro/Kokoro-82M"));
        assert_eq!(cfg.default_voice, "af_heart");
        assert!(cfg.auto_download);
    }

    #[test]
    fn backend_id() {
        let cfg = KokoroConfig::under_data_dir(Path::new("/tmp/x"));
        assert_eq!(KokoroBackend::new(cfg).id(), "kokoro");
    }

    #[tokio::test]
    async fn list_voices_available_when_auto_download_on() {
        // auto_download defaults on → voices report available even with no
        // model on disk, because any-tts will fetch transparently.
        let cfg = KokoroConfig::under_data_dir(Path::new("/tmp/mira-kokoro-absent"));
        assert!(cfg.auto_download);
        let voices = KokoroBackend::new(cfg).list_voices().await.unwrap();
        assert!(voices.iter().any(|v| v.id == "af_heart"));
        assert!(voices.iter().all(|v| v.backend_id == "kokoro"));
        assert!(voices.iter().all(|v| v.is_downloaded), "auto_download on → available");
    }

    #[tokio::test]
    async fn list_voices_reflects_per_voice_presence_without_autodownload() {
        // With auto_download off, is_downloaded tracks the actual .pt on disk.
        // Use a self-managed model_path/voices dir so the check is
        // deterministic (voices_dir() prefers it over the HF cache).
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = KokoroConfig::under_data_dir(dir.path());
        cfg.auto_download = false;
        let vdir = cfg.model_path.join("voices");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("af_heart.pt"), b"stub").unwrap();

        let voices = KokoroBackend::new(cfg).list_voices().await.unwrap();
        assert!(voices.iter().find(|v| v.id == "af_heart").unwrap().is_downloaded,
            "af_heart present on disk → downloaded");
        assert!(!voices.iter().find(|v| v.id == "bf_emma").unwrap().is_downloaded,
            "bf_emma absent → not downloaded");
    }

    #[tokio::test]
    async fn synthesise_rejects_empty_text() {
        let cfg = KokoroConfig::under_data_dir(Path::new("/tmp/x"));
        let err = KokoroBackend::new(cfg)
            .synthesise(&SynthesiseRequest::new("   "))
            .await.unwrap_err();
        assert!(matches!(err, TtsError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn synthesise_errors_when_model_missing_and_no_download() {
        let mut cfg = KokoroConfig::under_data_dir(Path::new("/tmp/mira-kokoro-absent-2"));
        cfg.auto_download = false;
        let err = KokoroBackend::new(cfg)
            .synthesise(&SynthesiseRequest::new("hello"))
            .await.unwrap_err();
        assert!(matches!(err, TtsError::BackendUnavailable(..)), "got {err:?}");
    }

    #[test]
    fn unknown_voice_maps_to_voice_not_installed() {
        // any-tts's real message for a missing voice file.
        let e = map_synth_error("Unknown voice: Voice file not found: en_GB-jenny_dioco-medium.pt", "en_GB-jenny_dioco-medium");
        match e {
            TtsError::VoiceNotInstalled(v) => assert_eq!(v, "en_GB-jenny_dioco-medium"),
            other => panic!("expected VoiceNotInstalled, got {other:?}"),
        }
    }

    #[test]
    fn other_errors_stay_upstream() {
        let e = map_synth_error("tensor shape mismatch", "af_heart");
        assert!(matches!(e, TtsError::Upstream(_)), "got {e:?}");
    }

    #[test]
    fn wav_header_is_well_formed() {
        let wav = pcm_f32_to_wav(&[0.0, 0.5, -0.5, 1.0, -1.0], 24_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        // 44-byte header + 5 samples * 2 bytes
        assert_eq!(wav.len(), 44 + 10);
        // sample_rate field at offset 24
        assert_eq!(u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]), 24_000);
    }
}
