// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/types.rs
//! Shared TTS types: voices, requests, audio buffers/chunks, codecs, errors.
//!
//! Kept in their own module so the trait, the backends, the cache, and the
//! HTTP layer can all depend on them without depending on each other.

use thiserror::Error;

// ─────────────────────────────────────────────────────────────────────────────
// Codecs / formats
// ─────────────────────────────────────────────────────────────────────────────

/// Codec carried by an [`AudioBuffer`] or [`AudioChunk`]. Drives content-type
/// negotiation when streaming to the web client and tells the channel-boundary
/// encoder whether transcoding is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioCodec {
    Wav     { sample_rate: u32, channels: u16 },
    Mp3,
    OggOpus,
    /// Raw signed 16-bit little-endian PCM. Used by some streaming backends.
    Pcm     { sample_rate: u32, channels: u16 },
}

impl AudioCodec {
    /// HTTP `Content-Type` for this codec.
    pub fn content_type(&self) -> &'static str {
        match self {
            AudioCodec::Wav { .. } => "audio/wav",
            AudioCodec::Mp3        => "audio/mpeg",
            AudioCodec::OggOpus    => "audio/ogg",
            AudioCodec::Pcm { .. } => "audio/L16",
        }
    }

    /// File extension (no dot) — used by the disk cache and the CLI.
    pub fn extension(&self) -> &'static str {
        match self {
            AudioCodec::Wav { .. } => "wav",
            AudioCodec::Mp3        => "mp3",
            AudioCodec::OggOpus    => "ogg",
            AudioCodec::Pcm { .. } => "pcm",
        }
    }
}

/// Format hint a caller can ask for. Backends are free to ignore the hint and
/// return their native codec — `tts::encoder` transcodes at the channel
/// boundary if the channel cares (Telegram/Signal want OGG/Opus; the browser
/// is happy with WAV).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat { Wav, Mp3, OggOpus }

impl OutputFormat {
    pub fn parse(s: &str) -> Option<OutputFormat> {
        match s.to_ascii_lowercase().as_str() {
            "wav"                       => Some(OutputFormat::Wav),
            "mp3" | "mpeg"              => Some(OutputFormat::Mp3),
            "ogg" | "ogg-opus" | "opus" => Some(OutputFormat::OggOpus),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Wav     => "wav",
            OutputFormat::Mp3     => "mp3",
            OutputFormat::OggOpus => "ogg-opus",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Audio payloads
// ─────────────────────────────────────────────────────────────────────────────

/// One complete audio buffer — used for messaging-channel voice notes and
/// cache fills.
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    pub bytes: Vec<u8>,
    pub codec: AudioCodec,
}

/// One streamed chunk. The web client queues chunks so chunk N starts playing
/// while N+1 is still being synthesised. `is_final` lets the consumer close the
/// `MediaSource` cleanly.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub bytes:    Vec<u8>,
    pub codec:    AudioCodec,
    pub is_final: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Voice / request / probe
// ─────────────────────────────────────────────────────────────────────────────

/// Unified voice metadata surfaced by `TtsBackend::list_voices` and shown in
/// the settings dropdown.
#[derive(Debug, Clone)]
pub struct Voice {
    /// Backend id this voice belongs to (`"piper"`, `"openai"`, …).
    pub backend_id:    String,
    /// Backend-scoped voice id (e.g. `"en_US-amy-medium"`, `"alloy"`).
    pub id:            String,
    pub name:          String,
    /// BCP-47 tag — `"en-US"`, `"de-DE"`, `"multi"` for multilingual voices.
    pub language:      String,
    pub gender:        Option<String>,
    pub sample_rate:   Option<u32>,
    /// Internal backends only — `false` until the voice has been downloaded.
    /// Cloud backends report `true` unconditionally.
    pub is_downloaded: bool,
}

/// One synthesise call as it flows from the API through the router to a
/// backend. SSML is opaque text — only some backends respect it.
#[derive(Debug, Clone)]
pub struct SynthesiseRequest {
    pub text:     String,
    pub voice_id: Option<String>,
    pub speed:    f32,
    pub format:   OutputFormat,
    pub is_ssml:  bool,
}

impl SynthesiseRequest {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text:     text.into(),
            voice_id: None,
            speed:    1.0,
            format:   OutputFormat::Wav,
            is_ssml:  false,
        }
    }
}

/// Backend liveness report. `latency_ms` is a tiny fixed-sample round-trip so
/// values are comparable across probes.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub healthy:    bool,
    pub latency_ms: Option<u64>,
    /// Backend-specific note — e.g. `"voice not yet downloaded"` or the
    /// upstream version string. Surfaced in the settings UI.
    pub note:       Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

/// TTS-specific error type. Kept separate from [`crate::error::MiraError`] so
/// backends can pattern-match without depending on the rest of the error
/// universe; converts at the service / HTTP boundary.
#[derive(Debug, Error)]
pub enum TtsError {
    #[error("TTS backend '{0}' is not configured")]
    BackendNotConfigured(String),

    #[error("TTS backend '{0}' is not available: {1}")]
    BackendUnavailable(String, String),

    #[error("Voice '{0}' is not installed")]
    VoiceNotInstalled(String),

    #[error("TTS request rejected: {0}")]
    BadRequest(String),

    #[error("Authentication with TTS provider failed")]
    Unauthorized,

    #[error("TTS request timed out")]
    Timeout,

    #[error("TTS backend returned an error: {0}")]
    Upstream(String),

    #[error("Audio encoding error: {0}")]
    Encoding(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
}

impl From<TtsError> for crate::error::MiraError {
    fn from(e: TtsError) -> crate::error::MiraError {
        // ServerError is the closest existing variant — TTS errors are almost
        // always surfaced through the HTTP API. A dedicated variant can come
        // later if we need finer-grained handling at call sites.
        crate::error::MiraError::ServerError(format!("tts: {e}"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_format_parse_round_trip() {
        for f in [OutputFormat::Wav, OutputFormat::Mp3, OutputFormat::OggOpus] {
            assert_eq!(OutputFormat::parse(f.as_str()), Some(f));
        }
        assert_eq!(OutputFormat::parse("MP3"),  Some(OutputFormat::Mp3));
        assert_eq!(OutputFormat::parse("opus"), Some(OutputFormat::OggOpus));
        assert_eq!(OutputFormat::parse("flac"), None);
    }

    #[test]
    fn audio_codec_content_types_are_distinct() {
        let codecs = [
            AudioCodec::Wav { sample_rate: 22_050, channels: 1 },
            AudioCodec::Mp3,
            AudioCodec::OggOpus,
            AudioCodec::Pcm { sample_rate: 22_050, channels: 1 },
        ];
        let mut seen = std::collections::HashSet::new();
        for c in &codecs {
            assert!(seen.insert(c.content_type()), "duplicate content type for {c:?}");
            assert!(!c.extension().is_empty());
        }
    }

    #[test]
    fn synthesise_request_defaults() {
        let r = SynthesiseRequest::new("hello");
        assert_eq!(r.text,     "hello");
        assert_eq!(r.voice_id, None);
        assert_eq!(r.speed,    1.0);
        assert_eq!(r.format,   OutputFormat::Wav);
        assert!(!r.is_ssml);
    }

    #[test]
    fn tts_error_converts_to_mira_error() {
        let err: crate::error::MiraError =
            TtsError::Unauthorized.into();
        let msg = err.to_string();
        assert!(msg.contains("tts"), "got: {msg}");
    }
}
