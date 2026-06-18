// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/backend/mod.rs
//! Backend trait and registry for the TTS subsystem.
//!
//! Each backend (Piper, eSpeak, OpenAI, ElevenLabs, Cartesia, …) implements
//! [`TtsBackend`] and is owned by `TtsService`. The router picks one per
//! request based on the per-user pref, the per-channel default, and the
//! global config — see `design-docs/phase8-tts.md`.

use async_trait::async_trait;
use futures::stream::BoxStream;

use super::types::{AudioBuffer, AudioChunk, ProbeResult, SynthesiseRequest, TtsError, Voice};

pub mod espeak;
pub mod openai;
pub mod piper;
#[cfg(feature = "kokoro")]
pub mod kokoro;
pub use espeak::EspeakBackend;
pub use openai::{OpenAiBackend, OpenAiConfig};
pub use piper::{PiperBackend, PiperConfig};
#[cfg(feature = "kokoro")]
pub use kokoro::{KokoroBackend, KokoroConfig};

/// Unified contract for any TTS engine MIRA can drive.
///
/// Backends should be cheap to instantiate — defer connections, downloads,
/// and child-process spawns until first use so a configured-but-unused
/// backend never delays startup.
#[async_trait]
pub trait TtsBackend: Send + Sync {
    /// Stable id used in routing config and API responses (`"piper"`,
    /// `"openai"`, `"openai_compat"`, `"elevenlabs"`, `"cartesia"`, …).
    fn id(&self) -> &'static str;

    /// Voices the backend can produce. For internal backends this includes
    /// voices that are configured but not yet downloaded — the
    /// [`Voice::is_downloaded`] flag distinguishes them.
    async fn list_voices(&self) -> Result<Vec<Voice>, TtsError>;

    /// Synthesise the full text into a single [`AudioBuffer`]. Used for
    /// messaging-channel voice notes and as a cache fill path.
    async fn synthesise(
        &self,
        req: &SynthesiseRequest,
    ) -> Result<AudioBuffer, TtsError>;

    /// Streaming synthesis. Yields chunks as they arrive from the backend.
    /// Backends that do not natively stream return one chunk equal to the
    /// full buffer with `is_final = true`.
    async fn synthesise_stream(
        &self,
        req: &SynthesiseRequest,
    ) -> Result<BoxStream<'static, Result<AudioChunk, TtsError>>, TtsError>;

    /// Quick liveness probe — used by `mira tts probe` and the settings UI
    /// status indicator. Should round-trip a tiny fixed sample so latency
    /// values are comparable across runs.
    async fn probe(&self) -> Result<ProbeResult, TtsError>;

    /// Pre-fetch the voice's model files (or whatever the backend needs to
    /// be ready to synthesise with that voice). Local backends use this to
    /// download `.onnx` model pairs ahead of the first `synthesise` call so
    /// the user gets a "Downloaded" pill in the UI immediately, rather than
    /// silently waiting on the first speak. Remote backends typically have
    /// nothing to fetch and should leave the default no-op `Ok(())`.
    async fn ensure_voice(&self, _voice_id: &str) -> Result<(), TtsError> {
        Ok(())
    }
}
