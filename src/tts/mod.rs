// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/mod.rs
//! Text-to-speech subsystem (see `design-docs/phase8-tts.md`).
//!
//! Pluggable backends behind a single [`TtsBackend`] trait, fronted by a
//! routing service that picks the right one per user / channel / config.
//!
//! of the rollout ships the trait, the shared types, and the pinned
//! download manifest. Subsequent stages add the Piper subprocess backend,
//! the eSpeak NG fallback, the cache + service façade, the HTTP API, the
//! web 🔊 button and the `mira tts` CLI.

pub mod backend;
pub mod cache;
pub mod chatterbox;
pub mod mcp;
pub mod chunker;
pub mod encoder;
pub mod manifest;
pub mod service;
pub mod text_filter;
pub mod types;

pub use backend::{EspeakBackend, OpenAiBackend, OpenAiConfig, PiperBackend, PiperConfig, TtsBackend};
pub use cache::{CacheStats, TtsCache, cache_key};
pub use chunker::SentenceChunker;
pub use service::TtsService;
pub use manifest::{
    ArchiveKind, DEFAULT_VOICE_ID, PIPER_VERSION, PiperBinary, VoiceManifest,
    curated_voice, curated_voices, default_voice, piper_for_host,
};
pub use types::{
    AudioBuffer, AudioChunk, AudioCodec, OutputFormat, ProbeResult,
    SynthesiseRequest, TtsError, Voice,
};
