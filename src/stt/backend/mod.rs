// SPDX-License-Identifier: AGPL-3.0-or-later

// src/stt/backend/mod.rs
//! Backend trait and registry for the STT subsystem.
//!
//! Each backend (whisper.cpp local, OpenAI cloud, OpenAI-compatible
//! self-hosted, …) implements [`SttBackend`] and is owned by `SttService`.
//! The router picks one per request based on the per-channel default and
//! the global config — mirrors the TTS layout in `src/tts/backend`.

use async_trait::async_trait;

use super::types::{ProbeResult, SttError, TranscribeRequest, Transcript};

pub mod openai_compat;
pub mod whisper;
pub use openai_compat::{OpenAiCompatBackend, OpenAiCompatConfig};
pub use whisper::{WhisperBackend, WhisperConfig};

/// Unified contract for any STT engine MIRA can drive.
///
/// Like TTS backends, these should be cheap to instantiate — defer model
/// loads, downloads, and connections until first use so a configured-but-
/// unused backend never delays startup.
#[async_trait]
pub trait SttBackend: Send + Sync {
    /// Stable id used in routing config and API responses (`"internal"` for
    /// the whisper.cpp engine, `"openai"` for the cloud, `"openai_compat"`
    /// for self-hosted whisper-compatible servers).
    fn id(&self) -> &'static str;

    /// Run one transcription. Backends are responsible for getting from the
    /// raw container bytes in `req.audio_bytes` to a transcript — the
    /// internal backend uses `crate::stt::encoder` to land at 16 kHz mono
    /// f32 PCM that whisper.cpp wants; cloud backends pass the bytes through
    /// to the upstream multipart endpoint.
    async fn transcribe(&self, req: &TranscribeRequest) -> Result<Transcript, SttError>;

    /// Quick liveness probe — used by the settings UI status indicator.
    /// Should be cheap (an HTTP HEAD / a model-load check); do NOT actually
    /// transcribe a sample.
    async fn probe(&self) -> Result<ProbeResult, SttError>;

    /// Pre-fetch backend assets so the first real transcribe doesn't pay the
    /// download / load latency. Local backends use this to download the ggml
    /// model file ahead of time. Remote backends typically have nothing to
    /// fetch and leave the default no-op `Ok(())`.
    async fn ensure_ready(&self) -> Result<(), SttError> {
        Ok(())
    }
}
