// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/backend/espeak.rs
//! eSpeak NG fallback backend.
//!
//! Tiny, robotic, always-on. Used when the Piper auto-download fails so the
//! user still hears *something* — Section 10 of the design doc covers the
//! rationale. We shell out to the system `espeak-ng` (or the legacy
//! `espeak`) and render WAV to a temp file (never stdout — see `synthesise`).

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::tts::backend::{TtsBackend, TmpFileGuard, unique_tmp_wav_path};
use crate::tts::types::{
    AudioBuffer, AudioChunk, AudioCodec, ProbeResult, SynthesiseRequest, TtsError, Voice,
};

/// eSpeak NG backend. Cheap to construct; binary is located lazily on first
/// synthesis.
pub struct EspeakBackend {
    default_voice: String,
}

impl Default for EspeakBackend {
    fn default() -> Self { Self::new() }
}

impl EspeakBackend {
    pub fn new() -> Self {
        Self { default_voice: "en-us".to_string() }
    }

    pub fn with_default_voice(voice: impl Into<String>) -> Self {
        Self { default_voice: voice.into() }
    }

    /// Whether `espeak-ng` or `espeak` is on `PATH`. Used by the router to
    /// decide if the fallback is even reachable.
    pub fn is_available() -> bool {
        locate_espeak().is_some()
    }
}

#[async_trait]
impl TtsBackend for EspeakBackend {
    fn id(&self) -> &'static str { "espeak" }

    async fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        // Hard-coded curated list — eSpeak ships hundreds of voices but
        // surfacing them all in the dropdown would dwarf the high-quality
        // Piper options. Users who want an obscure language can pass the
        // voice id explicitly via the API.
        let voices = [
            ("en-us", "American English",   "en-US"),
            ("en-gb", "British English",    "en-GB"),
            ("en",    "English (default)",  "en"   ),
            ("de",    "German",             "de-DE"),
            ("fr",    "French",             "fr-FR"),
            ("es",    "Spanish",            "es-ES"),
            ("it",    "Italian",            "it-IT"),
        ];
        Ok(voices.into_iter().map(|(id, name, lang)| Voice {
            backend_id:    "espeak".into(),
            id:            id.into(),
            name:          name.into(),
            language:      lang.into(),
            gender:        None,
            sample_rate:   Some(22_050),
            is_downloaded: true,
        }).collect())
    }

    async fn synthesise(&self, req: &SynthesiseRequest) -> Result<AudioBuffer, TtsError> {
        if req.text.trim().is_empty() {
            return Err(TtsError::BadRequest("text is empty".into()));
        }
        let bin = locate_espeak().ok_or_else(|| TtsError::BackendUnavailable(
            "espeak".into(),
            "espeak-ng not found on PATH — install it or configure another backend".into(),
        ))?;
        let voice_id = req.voice_id.clone().unwrap_or_else(|| self.default_voice.clone());
        // Words per minute. eSpeak default is 175; clamp to the documented
        // safe band so a slider that maps 0.5..=2.0 never produces nonsense.
        let wpm = (175.0_f32 * req.speed.clamp(0.25, 4.0)).clamp(80.0, 450.0).round() as u32;

        // Render to a temp file (`-w`), not `--stdout`. On Windows, writing the
        // binary WAV to stdout corrupts it via text-mode `\n` → `\r\n`
        // translation (the same bug that turned Piper output into static). File
        // I/O is binary-safe everywhere, so we use it on all platforms.
        let out_path = unique_tmp_wav_path();
        let _guard   = TmpFileGuard(out_path.clone());

        let mut child = Command::new(&bin)
            .arg("-v").arg(&voice_id)
            .arg("-s").arg(wpm.to_string())
            .arg("-w").arg(&out_path)
            .stdin (Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| TtsError::BackendUnavailable(
                "espeak".into(), format!("spawn {} failed: {e}", bin.display()),
            ))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(req.text.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let out = child.wait_with_output().await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(TtsError::Upstream(format!("espeak exited with {}: {stderr}", out.status)));
        }
        let bytes = fs::read(&out_path).await.map_err(|e| TtsError::Upstream(
            format!("espeak produced no readable output at {}: {e}", out_path.display()),
        ))?;
        if bytes.is_empty() {
            return Err(TtsError::Upstream("espeak produced an empty audio file".into()));
        }
        Ok(AudioBuffer {
            bytes,
            codec: AudioCodec::Wav { sample_rate: 22_050, channels: 1 },
        })
    }

    async fn synthesise_stream(
        &self,
        req: &SynthesiseRequest,
    ) -> Result<BoxStream<'static, Result<AudioChunk, TtsError>>, TtsError> {
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
                note:       locate_espeak().map(|p| format!("espeak at {}", p.display())),
            }),
            Err(e) => Ok(ProbeResult {
                healthy:    false,
                latency_ms: None,
                note:       Some(e.to_string()),
            }),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn locate_espeak() -> Option<PathBuf> {
    which("espeak-ng").or_else(|| which("espeak"))
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_id() {
        assert_eq!(EspeakBackend::new().id(), "espeak");
    }

    #[tokio::test]
    async fn list_voices_includes_english_defaults() {
        let voices = EspeakBackend::new().list_voices().await.unwrap();
        assert!(voices.iter().any(|v| v.id == "en-us"));
        assert!(voices.iter().any(|v| v.id == "en-gb"));
        assert!(voices.iter().all(|v| v.is_downloaded));
        assert!(voices.iter().all(|v| v.backend_id == "espeak"));
    }

    #[tokio::test]
    async fn synthesise_rejects_empty_text() {
        let err = EspeakBackend::new()
            .synthesise(&SynthesiseRequest::new("   "))
            .await.unwrap_err();
        assert!(matches!(err, TtsError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn synthesise_errors_when_binary_missing() {
        // Wipe PATH so locate_espeak() returns None deterministically.
        let before = std::env::var_os("PATH");
        // SAFETY: tests in a single binary share env — this is the only
        // test that touches PATH and we restore it before returning.
        unsafe { std::env::remove_var("PATH"); }
        let res = EspeakBackend::new().synthesise(&SynthesiseRequest::new("hi")).await;
        if let Some(p) = before { unsafe { std::env::set_var("PATH", p); } }

        let err = res.unwrap_err();
        assert!(matches!(err, TtsError::BackendUnavailable(..)), "got {err:?}");
    }

    #[tokio::test]
    async fn probe_unhealthy_when_binary_missing() {
        let before = std::env::var_os("PATH");
        unsafe { std::env::remove_var("PATH"); }
        let p = EspeakBackend::new().probe().await.unwrap();
        if let Some(v) = before { unsafe { std::env::set_var("PATH", v); } }

        assert!(!p.healthy);
        assert!(p.latency_ms.is_none());
    }
}
