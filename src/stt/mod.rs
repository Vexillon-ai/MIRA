// SPDX-License-Identifier: AGPL-3.0-or-later

// src/stt/mod.rs
//! Speech-to-text subsystem.
//!
//! Pluggable backends behind a single [`SttBackend`] trait, fronted by a
//! routing service that picks the right one per channel / config — the
//! mirror image of the TTS subsystem in `src/tts`.
//!
//! Three backends ship in the first slice:
//! * `internal`       — whisper.cpp via the `whisper-rs` FFI, plus the
//!                      Symphonia + Rubato decoder so any browser/phone
//!                      container is accepted without ffmpeg.
//! * `openai`         — `https://api.openai.com/v1/audio/transcriptions`
//!                      when the user supplies an API key.
//! * `openai_compat`  — any self-hosted server speaking the same shape
//!                      (whisper.cpp's bundled HTTP server,
//!                      faster-whisper-server, …).

pub mod backend;
pub mod encoder;
pub mod manifest;
pub mod service;
pub mod types;

pub use backend::{
    OpenAiCompatBackend, OpenAiCompatConfig, SttBackend, WhisperBackend, WhisperConfig,
};
pub use encoder::{DecodedAudio, TARGET_SAMPLE_RATE, decode_to_pcm16k};
pub use manifest::{DEFAULT_MODEL_ID, WhisperModel, curated_model, curated_models, model_file_name};
pub use service::SttService;
pub use types::{AudioInputFormat, ProbeResult, SttError, TranscribeRequest, Transcript};
