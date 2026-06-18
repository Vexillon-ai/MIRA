// SPDX-License-Identifier: AGPL-3.0-or-later

// src/stt/types.rs
//! Shared STT types — audio request, transcript, probe result, error.
//!
//! Kept independent of the TTS module so the two subsystems can evolve
//! separately even though they look similar at the surface.

use thiserror::Error;

/// Audio container hint passed alongside the bytes. Backends use this to
/// decide whether to attempt a fast path (e.g. whisper accepts 16 kHz mono
/// PCM directly) or to invoke the decoder. Cloud backends generally ignore
/// the hint and trust the server's content sniffing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioInputFormat {
    /// Container unknown — backend must sniff or decode.
    Unknown,
    Wav,
    Mp3,
    OggOpus,
    /// Browser `MediaRecorder` default on Chrome/Firefox.
    WebmOpus,
    Mp4,
    Flac,
}

impl AudioInputFormat {
    /// Best-effort guess from a MIME type. Returns `Unknown` when the MIME
    /// is empty or unrecognised — we try to decode anyway.
    pub fn from_mime(mime: &str) -> AudioInputFormat {
        let m = mime.to_ascii_lowercase();
        if m.contains("wav") || m.contains("wave") || m.contains("x-wav") {
            AudioInputFormat::Wav
        } else if m.contains("mpeg") || m.contains("mp3") {
            AudioInputFormat::Mp3
        } else if m.contains("webm") {
            AudioInputFormat::WebmOpus
        } else if m.contains("ogg") || m.contains("opus") {
            AudioInputFormat::OggOpus
        } else if m.contains("mp4") || m.contains("m4a") || m.contains("aac") {
            AudioInputFormat::Mp4
        } else if m.contains("flac") {
            AudioInputFormat::Flac
        } else {
            AudioInputFormat::Unknown
        }
    }

    /// Suggested file extension. Used by the openai_compat backend when it
    /// uploads the bytes as a multipart `file` part, since some servers key
    /// on the filename suffix to dispatch their own decoder.
    pub fn extension(self) -> &'static str {
        match self {
            AudioInputFormat::Wav      => "wav",
            AudioInputFormat::Mp3      => "mp3",
            AudioInputFormat::OggOpus  => "ogg",
            AudioInputFormat::WebmOpus => "webm",
            AudioInputFormat::Mp4      => "m4a",
            AudioInputFormat::Flac     => "flac",
            AudioInputFormat::Unknown  => "bin",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            AudioInputFormat::Wav      => "audio/wav",
            AudioInputFormat::Mp3      => "audio/mpeg",
            AudioInputFormat::OggOpus  => "audio/ogg",
            AudioInputFormat::WebmOpus => "audio/webm",
            AudioInputFormat::Mp4      => "audio/mp4",
            AudioInputFormat::Flac     => "audio/flac",
            AudioInputFormat::Unknown  => "application/octet-stream",
        }
    }
}

/// One transcription call as it flows from the API through the router into a
/// backend. `audio_bytes` is the raw container as received from the client —
/// the internal backend decodes via Symphonia, the cloud backends pass it
/// through to the upstream.
#[derive(Debug, Clone)]
pub struct TranscribeRequest {
    pub audio_bytes: Vec<u8>,
    /// MIME hint when known; the backend may still re-sniff.
    pub format:      AudioInputFormat,
    /// BCP-47 hint (`"en"`, `"de-DE"`, …). `None` lets multilingual models
    /// auto-detect.
    pub language:    Option<String>,
}

impl TranscribeRequest {
    pub fn new(audio: Vec<u8>) -> Self {
        Self {
            audio_bytes: audio,
            format:      AudioInputFormat::Unknown,
            language:    None,
        }
    }
}

/// One transcript returned to the caller. `duration_ms` is the audio length
/// when the backend can determine it (the internal backend computes it from
/// the decoded PCM length; cloud backends report it when they include it in
/// the response, otherwise `None`). `latency_ms` is the wall-clock round
/// trip from issuing the call to receiving the transcript.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text:        String,
    pub language:    Option<String>,
    pub duration_ms: Option<u64>,
    pub latency_ms:  u64,
    pub backend_id:  String,
}

/// Backend liveness report. Same shape as [`crate::tts::ProbeResult`] but
/// kept distinct so the two subsystems don't entangle their public types.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub healthy:    bool,
    pub latency_ms: Option<u64>,
    /// Backend-specific note — e.g. `"model not yet downloaded"` or the
    /// upstream version string. Surfaced verbatim in the settings UI.
    pub note:       Option<String>,
}

/// STT-specific error type. Pattern-matches at the service / HTTP boundary
/// the same way [`crate::tts::TtsError`] does.
#[derive(Debug, Error)]
pub enum SttError {
    #[error("STT backend '{0}' is not configured")]
    BackendNotConfigured(String),

    #[error("STT backend '{0}' is not available: {1}")]
    BackendUnavailable(String, String),

    #[error("STT model '{0}' is not installed")]
    ModelNotInstalled(String),

    #[error("STT request rejected: {0}")]
    BadRequest(String),

    #[error("Authentication with STT provider failed")]
    Unauthorized,

    #[error("STT request timed out")]
    Timeout,

    #[error("Audio decoding error: {0}")]
    Decoding(String),

    #[error("STT backend returned an error: {0}")]
    Upstream(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
}

impl From<SttError> for crate::error::MiraError {
    fn from(e: SttError) -> crate::error::MiraError {
        crate::error::MiraError::ServerError(format!("stt: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_mime_recognises_common_browser_types() {
        assert_eq!(AudioInputFormat::from_mime("audio/webm;codecs=opus"), AudioInputFormat::WebmOpus);
        assert_eq!(AudioInputFormat::from_mime("audio/ogg;codecs=opus"),  AudioInputFormat::OggOpus);
        assert_eq!(AudioInputFormat::from_mime("audio/wav"),              AudioInputFormat::Wav);
        assert_eq!(AudioInputFormat::from_mime("audio/x-wav"),            AudioInputFormat::Wav);
        assert_eq!(AudioInputFormat::from_mime("audio/mpeg"),             AudioInputFormat::Mp3);
        assert_eq!(AudioInputFormat::from_mime("audio/mp4"),              AudioInputFormat::Mp4);
        assert_eq!(AudioInputFormat::from_mime("audio/flac"),             AudioInputFormat::Flac);
        assert_eq!(AudioInputFormat::from_mime(""),                       AudioInputFormat::Unknown);
        assert_eq!(AudioInputFormat::from_mime("application/octet-stream"), AudioInputFormat::Unknown);
    }

    #[test]
    fn extensions_are_sensible() {
        for fmt in [
            AudioInputFormat::Wav, AudioInputFormat::Mp3, AudioInputFormat::OggOpus,
            AudioInputFormat::WebmOpus, AudioInputFormat::Mp4, AudioInputFormat::Flac,
        ] {
            assert!(!fmt.extension().is_empty(), "empty ext for {fmt:?}");
            assert!(fmt.mime().contains('/'),    "bad mime for {fmt:?}");
        }
    }

    #[test]
    fn transcribe_request_defaults_to_unknown_format() {
        let r = TranscribeRequest::new(vec![1, 2, 3]);
        assert_eq!(r.format, AudioInputFormat::Unknown);
        assert!(r.language.is_none());
    }

    #[test]
    fn stt_error_converts_to_mira_error() {
        let err: crate::error::MiraError = SttError::Unauthorized.into();
        assert!(err.to_string().contains("stt"));
    }
}
