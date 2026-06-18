// SPDX-License-Identifier: AGPL-3.0-or-later

// src/stt/backend/openai_compat.rs
//! OpenAI-compatible STT backend.
//!
//! Drives both:
//!   * `openai`        — `https://api.openai.com/v1/audio/transcriptions`
//!                       with the user's OpenAI key.
//!   * `openai_compat` — any self-hosted server speaking the same spec —
//!                       whisper.cpp's bundled HTTP server, faster-whisper-
//!                       server, OpenedAI-Speech-style transcribers, etc.
//!
//! Single implementation; only the configured `id`, base URL, and key
//! differ. Mirrors the layout of `tts::backend::openai`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::stt::backend::SttBackend;
use crate::stt::types::{ProbeResult, SttError, TranscribeRequest, Transcript};

#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// Stable backend id (`"openai"` or `"openai_compat"`).
    pub id:           &'static str,
    /// Root URL — must include the `/v1` prefix for stock OpenAI; many
    /// self-hosted servers also expect it.
    pub base_url:     String,
    /// Bearer token. Empty string = no Authorization header (most self-
    /// hosted whisper servers run unauthenticated inside a LAN).
    pub api_key:      String,
    pub model:        String,
    pub timeout_secs: u64,
}

pub struct OpenAiCompatBackend {
    cfg:  OpenAiCompatConfig,
    http: reqwest::Client,
}

impl OpenAiCompatBackend {
    pub fn new(cfg: OpenAiCompatConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(5)))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { cfg, http }
    }

    pub fn config(&self) -> &OpenAiCompatConfig { &self.cfg }

    fn endpoint(&self) -> String {
        let trimmed = self.cfg.base_url.trim_end_matches('/');
        format!("{trimmed}/audio/transcriptions")
    }

    fn auth_header(&self) -> Option<String> {
        let k = self.cfg.api_key.trim();
        if k.is_empty() { None } else { Some(format!("Bearer {k}")) }
    }
}

/// Subset of the OpenAI transcription response we actually consume. The
/// `verbose_json` shape adds `language` + `duration`; the plain `json`
/// shape ships only `text` and we treat the others as `None`.
#[derive(Debug, Deserialize, Default)]
struct TranscriptionResponse {
    #[serde(default)]
    text:     String,
    #[serde(default)]
    language: Option<String>,
    /// Seconds — present only with `response_format=verbose_json`.
    #[serde(default)]
    duration: Option<f64>,
}

#[async_trait]
impl SttBackend for OpenAiCompatBackend {
    fn id(&self) -> &'static str { self.cfg.id }

    async fn transcribe(&self, req: &TranscribeRequest) -> Result<Transcript, SttError> {
        let started = Instant::now();

        let filename = format!("audio.{}", req.format.extension());
        let mime     = req.format.mime();
        let file_part = Part::bytes(req.audio_bytes.clone())
            .file_name(filename)
            .mime_str(mime)
            .map_err(|e| SttError::BadRequest(format!("invalid mime '{mime}': {e}")))?;

        let mut form = Form::new()
            .text("model", self.cfg.model.clone())
            // verbose_json gets us language + duration; plain servers that
            // don't support it return regular json — both shapes deserialise
            // into TranscriptionResponse without errors.
            .text("response_format", "verbose_json")
            .part("file", file_part);
        if let Some(lang) = req.language.as_ref().filter(|s| !s.is_empty()) {
            form = form.text("language", lang.clone());
        }

        let mut builder = self.http.post(self.endpoint());
        if let Some(auth) = self.auth_header() {
            builder = builder.header("Authorization", auth);
        }
        let resp = builder.multipart(form).send().await.map_err(|e| {
            if e.is_timeout() {
                SttError::Timeout
            } else {
                SttError::Http(e)
            }
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(match status.as_u16() {
                401 | 403 => SttError::Unauthorized,
                400       => SttError::BadRequest(body),
                _         => SttError::Upstream(format!("HTTP {status}: {body}")),
            });
        }

        let body = resp.text().await.map_err(SttError::Http)?;
        let parsed: TranscriptionResponse = serde_json::from_str(&body).unwrap_or_else(|e| {
            warn!("stt: openai_compat returned non-JSON / unexpected shape: {e}; falling back to raw text");
            TranscriptionResponse {
                text: body.clone(),
                ..Default::default()
            }
        });

        let duration_ms = parsed.duration.map(|s| (s * 1000.0).round().max(0.0) as u64);
        let latency_ms  = started.elapsed().as_millis() as u64;
        debug!(
            "stt: {} transcribed {} chars in {} ms",
            self.cfg.id, parsed.text.len(), latency_ms
        );

        Ok(Transcript {
            text: parsed.text,
            language: parsed.language.or_else(|| req.language.clone()),
            duration_ms,
            latency_ms,
            backend_id: self.cfg.id.to_string(),
        })
    }

    async fn probe(&self) -> Result<ProbeResult, SttError> {
        // Cheap probe: GET the `/v1/models` endpoint. OpenAI cloud answers
        // with a JSON list; whisper.cpp's bundled server, faster-whisper-
        // server, and similar all expose the same shape. We don't parse
        // the body — just observing the status code is enough.
        let url = format!("{}/models", self.cfg.base_url.trim_end_matches('/'));
        let started = Instant::now();
        let mut req = self.http.get(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        match req.send().await {
            Ok(r) => {
                let latency = started.elapsed().as_millis() as u64;
                let status  = r.status();
                if status.is_success() {
                    Ok(ProbeResult {
                        healthy:    true,
                        latency_ms: Some(latency),
                        note:       Some(format!("models endpoint OK ({status})")),
                    })
                } else {
                    Ok(ProbeResult {
                        healthy:    false,
                        latency_ms: Some(latency),
                        note:       Some(format!("HTTP {status} from {url}")),
                    })
                }
            }
            Err(e) => Ok(ProbeResult {
                healthy:    false,
                latency_ms: None,
                note:       Some(format!("probe failed: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(id: &'static str, url: &str) -> OpenAiCompatConfig {
        OpenAiCompatConfig {
            id,
            base_url:     url.to_string(),
            api_key:      String::new(),
            model:        "whisper-1".to_string(),
            timeout_secs: 5,
        }
    }

    #[test]
    fn endpoint_appends_audio_transcriptions() {
        let b = OpenAiCompatBackend::new(cfg("openai", "https://api.openai.com/v1"));
        assert_eq!(b.endpoint(), "https://api.openai.com/v1/audio/transcriptions");
    }

    #[test]
    fn endpoint_strips_trailing_slash() {
        let b = OpenAiCompatBackend::new(cfg("openai_compat", "http://localhost:8080/v1/"));
        assert_eq!(b.endpoint(), "http://localhost:8080/v1/audio/transcriptions");
    }

    #[test]
    fn auth_header_omitted_when_no_key() {
        let b = OpenAiCompatBackend::new(cfg("openai_compat", "http://x/v1"));
        assert!(b.auth_header().is_none());
    }

    #[test]
    fn auth_header_built_when_key_present() {
        let mut c = cfg("openai", "https://api.openai.com/v1");
        c.api_key = "sk-test".into();
        let b = OpenAiCompatBackend::new(c);
        assert_eq!(b.auth_header().as_deref(), Some("Bearer sk-test"));
    }

    #[tokio::test]
    async fn probe_reports_unreachable_url_as_unhealthy() {
        // Bogus port that nothing's listening on — connection refused.
        let b = OpenAiCompatBackend::new(cfg("openai_compat", "http://127.0.0.1:1/v1"));
        let p = b.probe().await.unwrap();
        assert!(!p.healthy);
    }

    #[test]
    fn id_round_trips() {
        let b = OpenAiCompatBackend::new(cfg("openai", "https://api.openai.com/v1"));
        assert_eq!(b.id(), "openai");
    }
}
