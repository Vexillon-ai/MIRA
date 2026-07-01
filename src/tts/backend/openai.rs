// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/backend/openai.rs
//! OpenAI-compatible TTS backend.
//!
//! Drives both:
//!   * `openai`        — `https://api.openai.com/v1/audio/speech` with the
//!                       user's OpenAI key.
//!   * `openai_compat` — any self-hosted server speaking the same spec
//!                       (OpenedAI-Speech, LiteLLM, LocalAI, …).
//!
//! Single implementation; the discriminant is the configured `id` and base URL.
//! See `design-docs/phase8-tts.md` §2.5.

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use serde::Serialize;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::tts::backend::TtsBackend;
use crate::tts::types::{
    AudioBuffer, AudioChunk, AudioCodec, OutputFormat,
    ProbeResult, SynthesiseRequest, TtsError, Voice,
};

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

/// Static config for one backend instance. The same struct serves both the
/// hosted OpenAI API and self-hosted OpenAI-spec servers; only `id` and
/// `base_url` differ.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Stable backend id surfaced to the router and the API
    /// (`"openai"` or `"openai_compat"`).
    pub id:            &'static str,
    /// Root URL — must include the `/v1` prefix for stock OpenAI.
    pub base_url:      String,
    /// Bearer token. Empty string = no Authorization header (some self-hosted
    /// servers don't require auth).
    pub api_key:       String,
    pub model:         String,
    pub default_voice: String,
    pub timeout_secs:  u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Backend
// ─────────────────────────────────────────────────────────────────────────────

pub struct OpenAiBackend {
    cfg:  OpenAiConfig,
    http: reqwest::Client,
}

impl OpenAiBackend {
    pub fn new(mut cfg: OpenAiConfig) -> Self {
        // The OpenAI audio endpoints live under /v1; forgive a host-only base.
        cfg.base_url = crate::providers::normalize_openai_base_url(&cfg.base_url, "/v1");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(5)))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { cfg, http }
    }

    pub fn config(&self) -> &OpenAiConfig { &self.cfg }

    fn endpoint(&self) -> String {
        let trimmed = self.cfg.base_url.trim_end_matches('/');
        format!("{trimmed}/audio/speech")
    }

    /// Base URL with any trailing slash trimmed.
    fn base(&self) -> &str {
        self.cfg.base_url.trim_end_matches('/')
    }

    /// Server root (base URL with `/v1` stripped). Some OpenAI-compat servers
    /// — Chatterbox in particular — expose helper endpoints at the root, not
    /// under `/v1`.
    fn root(&self) -> String {
        let b = self.base();
        if let Some(stripped) = b.strip_suffix("/v1") {
            stripped.trim_end_matches('/').to_string()
        } else {
            b.to_string()
        }
    }

    /// GET a JSON document with the same auth as `synthesise`. Returns `None`
    /// on any non-200 / non-JSON response so callers can chain attempts.
    async fn try_get_json(&self, url: &str) -> Option<serde_json::Value> {
        let mut rb = self.http.get(url);
        if !self.cfg.api_key.is_empty() {
            rb = rb.bearer_auth(&self.cfg.api_key);
        }
        let resp = match rb.send().await {
            Ok(r)  => r,
            Err(e) => { debug!("tts/{}: GET {url} failed: {e}", self.cfg.id); return None; }
        };
        if !resp.status().is_success() {
            debug!("tts/{}: GET {url} → HTTP {}", self.cfg.id, resp.status());
            return None;
        }
        match resp.json::<serde_json::Value>().await {
            Ok(v)  => Some(v),
            Err(e) => { warn!("tts/{}: {url} returned non-JSON: {e}", self.cfg.id); None }
        }
    }

    /// Walk a probe chain of voice-list endpoints commonly implemented by
    /// OpenAI-compat TTS servers. Returns the first non-empty result, or an
    /// empty vec if none responded usefully.
    ///
    /// Endpoints tried (in order):
    ///   1. `<base>/voices`            — OpenedAI-Speech, LocalAI
    ///   2. `<base>/audio/voices`      — Kokoro-FastAPI
    ///   3. `<root>/get_predefined_voices` + `<root>/get_reference_files`
    ///                                  — Chatterbox-TTS-Server (results merged)
    async fn discover_compat_voices(&self) -> Vec<Voice> {
        let base = self.base().to_string();
        let root = self.root();

        for url in [
            format!("{base}/voices"),
            format!("{base}/audio/voices"),
        ] {
            if let Some(json) = self.try_get_json(&url).await {
                let v = extract_voices_from_json(&json, self.cfg.id);
                if !v.is_empty() { return v; }
            }
        }

        // Chatterbox: predefined catalog + the operator's uploaded reference
        // files. Either may be empty on its own; merge whatever we get.
        let mut merged: Vec<Voice> = Vec::new();
        for url in [
            format!("{root}/get_predefined_voices"),
            format!("{root}/get_reference_files"),
        ] {
            if let Some(json) = self.try_get_json(&url).await {
                merged.extend(extract_voices_from_json(&json, self.cfg.id));
            }
        }
        merged
    }

    /// Map our [`OutputFormat`] to OpenAI's `response_format` string. Defaults
    /// to MP3 because it's the smallest universally-playable option and the
    /// only one that streams cleanly out of `chunked` HTTP without per-frame
    /// fix-ups (WAV would need a header rewrite, OGG-Opus needs a container).
    fn response_format(fmt: OutputFormat) -> &'static str {
        match fmt {
            OutputFormat::Wav     => "wav",
            OutputFormat::OggOpus => "opus",
            OutputFormat::Mp3     => "mp3",
        }
    }

    fn codec_for(fmt: OutputFormat) -> AudioCodec {
        match fmt {
            // Sample rate / channels are baked into the WAV header itself, so
            // the values below are only used by callers that don't re-parse
            // the header (none today). 24 kHz mono is OpenAI's actual output.
            OutputFormat::Wav     => AudioCodec::Wav { sample_rate: 24_000, channels: 1 },
            OutputFormat::OggOpus => AudioCodec::OggOpus,
            OutputFormat::Mp3     => AudioCodec::Mp3,
        }
    }

    async fn post_audio(&self, req: &SynthesiseRequest) -> Result<(Vec<u8>, AudioCodec), TtsError> {
        let voice = req.voice_id.as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.cfg.default_voice);

        let body = SpeechBody {
            model:           &self.cfg.model,
            input:           &req.text,
            voice,
            response_format: Self::response_format(req.format),
            speed:           req.speed.clamp(0.25, 4.0),
        };

        debug!("tts/{}: POST {} model={} voice={} fmt={} chars={}",
            self.cfg.id, self.endpoint(), self.cfg.model, voice,
            body.response_format, req.text.len());

        let mut rb = self.http.post(self.endpoint()).json(&body);
        if !self.cfg.api_key.is_empty() {
            rb = rb.bearer_auth(&self.cfg.api_key);
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                // Surface the *whole* error chain. The Display impl on
                // reqwest::Error is intentionally terse ("error sending
                // request for url …") and the underlying hyper / io::Error
                // (connection refused, broken pipe, RST after handshake,
                // …) is buried in the source chain. Without this, the
                // user-visible 502 is a black box.
                use std::error::Error as _;
                let mut chain = format!("{e}");
                let mut src: Option<&dyn std::error::Error> = e.source();
                while let Some(s) = src {
                    chain.push_str(" — caused by: ");
                    chain.push_str(&s.to_string());
                    src = s.source();
                }
                warn!(
                    "tts/{}: send failed for {} (model={} voice={} fmt={}): {chain}",
                    self.cfg.id, self.endpoint(), self.cfg.model, voice,
                    body.response_format,
                );
                return Err(TtsError::Http(e));
            }
        };
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(TtsError::Unauthorized);
        }
        if !status.is_success() {
            // Try to surface the upstream JSON error verbatim so admins can
            // see model-doesn't-exist / voice-doesn't-exist quickly.
            let detail = resp.text().await.unwrap_or_default();
            // 404 + "voice file 'X' not found" is the Chatterbox shape for
            // an unknown voice id; surface as VoiceNotInstalled so the
            // service layer can retry with the backend's own default
            // instead of bubbling up a 502 the user can't act on. Other
            // OpenAI-compat servers we care about (openedai-speech,
            // kokoro-fastapi) use similar wording.
            let lower = detail.to_ascii_lowercase();
            if status == reqwest::StatusCode::NOT_FOUND
                && (lower.contains("voice file") && lower.contains("not found"))
            {
                return Err(TtsError::VoiceNotInstalled(voice.to_string()));
            }
            // Many local OpenAI-compatible TTS servers (Chatterbox included)
            // only produce WAV and reject `response_format: mp3/opus` with a
            // 4xx or a "format"-flavoured message. Rather than fail over to the
            // internal engine (losing this backend's voice), retry once with
            // `wav` — the lowest-common-denominator container. MIRA reports the
            // real codec back via the response Content-Type, so the client
            // plays whatever it receives. This is why a mobile client asking
            // for mp3 used to fall back to internal while wav-based channels
            // worked. The retry only fires when the request wasn't already wav,
            // so it can't recurse.
            let format_rejected = req.format != OutputFormat::Wav
                && (matches!(status.as_u16(), 400 | 415 | 422)
                    || lower.contains("response_format")
                    || lower.contains("format")
                    || lower.contains("codec"));
            if format_rejected {
                warn!("tts/{}: {} rejected response_format={} ({status}); retrying as wav",
                    self.cfg.id, self.endpoint(), body.response_format);
                let mut wav_req = req.clone();
                wav_req.format = OutputFormat::Wav;
                // Box the recursive future (one level only — wav never re-retries).
                return Box::pin(self.post_audio(&wav_req)).await;
            }
            let detail = if detail.is_empty() { format!("HTTP {status}") }
                         else { format!("HTTP {status}: {detail}") };
            return Err(TtsError::Upstream(detail));
        }
        let bytes = resp.bytes().await?.to_vec();
        Ok((bytes, Self::codec_for(req.format)))
    }
}

#[derive(Debug, Serialize)]
struct SpeechBody<'a> {
    model:           &'a str,
    input:           &'a str,
    voice:           &'a str,
    response_format: &'static str,
    speed:           f32,
}

#[async_trait]
impl TtsBackend for OpenAiBackend {
    fn id(&self) -> &'static str { self.cfg.id }

    async fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        // OpenAI itself doesn't expose a voice-list endpoint, so for
        // `id == "openai"` we just return the documented catalog.
        //
        // For self-hosted OpenAI-compat servers there's no agreed-upon
        // standard either, but most implementations expose *something*.
        // We walk a probe chain through the popular shapes and surface
        // whatever the server is willing to tell us. If nothing answers,
        // fall back to the configured default voice so the UI still has a
        // working hint to display.
        if self.cfg.id == "openai" {
            return Ok(openai_curated_voices(self.cfg.id));
        }
        let discovered = self.discover_compat_voices().await;
        if !discovered.is_empty() {
            return Ok(discovered);
        }
        Ok(vec![Voice {
            backend_id:    self.cfg.id.to_string(),
            id:            self.cfg.default_voice.clone(),
            name:          self.cfg.default_voice.clone(),
            language:      "multi".into(),
            gender:        None,
            sample_rate:   None,
            is_downloaded: true,
        }])
    }

    async fn synthesise(&self, req: &SynthesiseRequest) -> Result<AudioBuffer, TtsError> {
        if req.text.trim().is_empty() {
            return Err(TtsError::BadRequest("text is empty".into()));
        }
        let (bytes, codec) = self.post_audio(req).await?;
        Ok(AudioBuffer { bytes, codec })
    }

    async fn synthesise_stream(
        &self,
        req: &SynthesiseRequest,
    ) -> Result<BoxStream<'static, Result<AudioChunk, TtsError>>, TtsError> {
        // OpenAI returns `Transfer-Encoding: chunked` MP3, but a partial MP3
        // stream isn't independently playable in a fresh `<Audio>` element
        // (we'd need MediaSource + a frame-aware splitter). The sentence
        // chunker upstream in `TtsService` already gives us low first-byte
        // latency by issuing one HTTP call per sentence, so here we just
        // return the full per-sentence buffer as a single final chunk.
        let buf = self.synthesise(req).await?;
        let chunk = AudioChunk { bytes: buf.bytes, codec: buf.codec, is_final: true };
        Ok(stream::once(async move { Ok(chunk) }).boxed())
    }

    async fn probe(&self) -> Result<ProbeResult, TtsError> {
        // Health probes should be CHEAP — they fire from the settings
        // UI status badge and the watchdog feed. Running a real synth
        // (the historical behaviour) made the probe time scale with
        // the model's per-character latency; a Chatterbox container
        // on CPU could take 10–30 s just to say "Hello", which then
        // looked like the server itself was unhealthy when really it
        // was just busy synthesising the probe text.
        //
        // Probe chain (lightweight GETs, first 2xx wins):
        //   /audio/voices                — Chatterbox / Kokoro-style compat
        //   /models                      — OpenAI proper, every OpenAI-shape gateway
        // Falls back to the synth probe only when both are absent so
        // the badge still reports the right thing for stripped-down
        // servers (rare).
        let start = Instant::now();

        let urls = [
            format!("{}/audio/voices", self.base()),
            format!("{}/models",       self.base()),
        ];
        for url in &urls {
            let mut rb = self.http.get(url);
            if !self.cfg.api_key.is_empty() {
                rb = rb.bearer_auth(&self.cfg.api_key);
            }
            if let Ok(resp) = rb.send().await {
                if resp.status().is_success() {
                    return Ok(ProbeResult {
                        healthy:    true,
                        latency_ms: Some(start.elapsed().as_millis() as u64),
                        note:       Some(format!("{} {} (via {})",
                            self.cfg.id, self.cfg.model,
                            url.rsplit('/').next().unwrap_or(""))),
                    });
                }
            }
        }

        // Last-ditch synth probe — needed for stripped-down compat
        // servers that expose only /audio/speech. Picks up a voice
        // from the catalog when default_voice is empty so the probe
        // reflects backend liveness, not config completeness.
        let mut req = SynthesiseRequest::new("Hello.");
        req.format = OutputFormat::Mp3;
        if self.cfg.default_voice.trim().is_empty() {
            if let Ok(voices) = self.list_voices().await {
                if let Some(first) = voices.into_iter().find(|v| !v.id.is_empty()) {
                    req.voice_id = Some(first.id);
                }
            }
        }
        match self.synthesise(&req).await {
            Ok(_) => Ok(ProbeResult {
                healthy:    true,
                latency_ms: Some(start.elapsed().as_millis() as u64),
                note:       Some(format!("{} {} (via synth fallback)",
                    self.cfg.id, self.cfg.model)),
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
// OpenAI voice catalogue
// ─────────────────────────────────────────────────────────────────────────────

/// Pull a `Vec<Voice>` out of whatever JSON shape an OpenAI-compat server
/// returned. We accept:
///   * a top-level array of strings           — `["alloy","echo"]`
///   * a top-level array of objects           — `[{"id":"alloy"}, …]`
///   * `{"voices": [...]}` / `{"data": [...]}` wrappers
///
/// For object items the id falls back through `filename → id → name → voice`
/// (Chatterbox uses `filename`, OpenAI-style uses `id`, others vary). The
/// human label preserves `display_name` / `name` when present.
fn extract_voices_from_json(json: &serde_json::Value, backend_id: &str) -> Vec<Voice> {
    let arr: Vec<serde_json::Value> = if let Some(a) = json.as_array() {
        a.clone()
    } else if let Some(obj) = json.as_object() {
        obj.get("voices").or_else(|| obj.get("data"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let (id, name) = if let Some(s) = item.as_str() {
            (s.to_string(), s.to_string())
        } else if let Some(obj) = item.as_object() {
            let id = ["filename", "id", "name", "voice"]
                .iter()
                .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
                .unwrap_or_default()
                .to_string();
            let name = ["display_name", "name", "label"]
                .iter()
                .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
                .map(str::to_string)
                .unwrap_or_else(|| id.clone());
            (id, name)
        } else {
            continue;
        };
        if id.is_empty() { continue; }
        out.push(Voice {
            backend_id:    backend_id.to_string(),
            id,
            name,
            language:      "multi".into(),
            gender:        None,
            sample_rate:   None,
            is_downloaded: true,
        });
    }
    out
}

/// Documented OpenAI TTS voices as of early 2026. The original `tts-1` /
/// `tts-1-hd` set plus the `gpt-4o-mini-tts` additions. All are multilingual.
fn openai_curated_voices(backend_id: &str) -> Vec<Voice> {
    const SPEC: &[(&str, &str, Option<&str>)] = &[
        ("alloy",   "Alloy",   None),
        ("echo",    "Echo",    Some("male")),
        ("fable",   "Fable",   None),
        ("onyx",    "Onyx",    Some("male")),
        ("nova",    "Nova",    Some("female")),
        ("shimmer", "Shimmer", Some("female")),
        // gpt-4o-mini-tts additions
        ("coral",   "Coral",   Some("female")),
        ("verse",   "Verse",   None),
        ("ballad",  "Ballad",  None),
        ("ash",     "Ash",     Some("male")),
        ("sage",    "Sage",    None),
    ];
    SPEC.iter().map(|(id, name, gender)| Voice {
        backend_id:    backend_id.to_string(),
        id:            (*id).to_string(),
        name:          (*name).to_string(),
        language:      "multi".into(),
        gender:        gender.map(|g| g.to_string()),
        sample_rate:   Some(24_000),
        is_downloaded: true,
    }).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Spin up a fake HTTP/1.1 server bound to a random port that returns the
    /// same canned response to every request, counting hits. Stays alive for
    /// the whole test (until the listener is dropped at scope end). Hand-
    /// rolled instead of pulling hyper-util — matches `tools::http_policy`'s
    /// existing test style.
    async fn serve_fake(
        body:   Vec<u8>,
        status: u16,
    ) -> (SocketAddr, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr     = listener.local_addr().unwrap();
        let hits     = Arc::new(AtomicUsize::new(0));
        let hits_cl  = hits.clone();

        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s)  => s,
                    Err(_) => break,
                };
                let body    = body.clone();
                let hits_cl = hits_cl.clone();
                tokio::spawn(async move {
                    // Read the whole request — drain until the headers/body
                    // boundary or until reqwest closes the write half. A
                    // single read is enough for our small JSON bodies.
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    hits_cl.fetch_add(1, Ordering::SeqCst);
                    let reason = match status {
                        200 => "OK",
                        401 => "Unauthorized",
                        403 => "Forbidden",
                        500 => "Internal Server Error",
                        _   => "Status",
                    };
                    let mut resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len(),
                    ).into_bytes();
                    resp.extend_from_slice(&body);
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (addr, hits)
    }

    fn cfg_for(addr: SocketAddr) -> OpenAiConfig {
        OpenAiConfig {
            id:            "openai_compat",
            base_url:      format!("http://{addr}/v1"),
            api_key:       "".into(),
            model:         "tts-1".into(),
            default_voice: "alloy".into(),
            timeout_secs:  5,
        }
    }

    #[tokio::test]
    async fn synthesise_returns_body_bytes() {
        let canned = vec![0xFFu8, 0xFB, 0x90, 0x44, 0x00, 0x00];
        let (addr, hits) = serve_fake(canned.clone(), 200).await;
        let backend = OpenAiBackend::new(cfg_for(addr));

        let mut req = SynthesiseRequest::new("Hello world.");
        req.format = OutputFormat::Mp3;
        let buf = backend.synthesise(&req).await.unwrap();
        assert_eq!(buf.bytes, canned);
        assert!(matches!(buf.codec, AudioCodec::Mp3));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// Server rejects the first (mp3) request with 400, accepts the wav retry.
    async fn serve_reject_then_ok(wav_body: Vec<u8>) -> (SocketAddr, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_cl = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await { Ok(s) => s, Err(_) => break };
                let n = hits_cl.fetch_add(1, Ordering::SeqCst);
                let wav = wav_body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let resp = if n == 0 {
                        // First request (mp3) → reject with a format complaint.
                        let body = b"{\"error\":\"response_format 'mp3' is not supported\"}";
                        let mut r = format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
                        r.extend_from_slice(body); r
                    } else {
                        // Retry (wav) → succeed.
                        let mut r = format!("HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", wav.len()).into_bytes();
                        r.extend_from_slice(&wav); r
                    };
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (addr, hits)
    }

    #[tokio::test]
    async fn format_rejection_retries_as_wav() {
        let wav = b"RIFF....WAVEfake".to_vec();
        let (addr, hits) = serve_reject_then_ok(wav.clone()).await;
        let backend = OpenAiBackend::new(cfg_for(addr));

        let mut req = SynthesiseRequest::new("Hello from mobile.");
        req.format = OutputFormat::Mp3; // server rejects this, MIRA retries wav
        let buf = backend.synthesise(&req).await.expect("retry should succeed");
        assert_eq!(buf.bytes, wav, "should return the wav retry's body");
        assert!(matches!(buf.codec, AudioCodec::Wav { .. }), "codec should reflect the wav retry");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn synthesise_maps_401_to_unauthorized() {
        let (addr, _) = serve_fake(b"unauthorized".to_vec(), 401).await;
        let backend = OpenAiBackend::new(cfg_for(addr));
        let err = backend.synthesise(&SynthesiseRequest::new("hi")).await.unwrap_err();
        assert!(matches!(err, TtsError::Unauthorized), "got {err:?}");
    }

    #[tokio::test]
    async fn synthesise_maps_500_to_upstream() {
        let (addr, _) = serve_fake(br#"{"error":"oops"}"#.to_vec(), 500).await;
        let backend = OpenAiBackend::new(cfg_for(addr));
        let err = backend.synthesise(&SynthesiseRequest::new("hi")).await.unwrap_err();
        match err {
            TtsError::Upstream(msg) => assert!(msg.contains("oops"), "got {msg}"),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn synthesise_rejects_empty_text() {
        let backend = OpenAiBackend::new(OpenAiConfig {
            id: "openai", base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "".into(), model: "tts-1".into(),
            default_voice: "alloy".into(), timeout_secs: 1,
        });
        let err = backend.synthesise(&SynthesiseRequest::new("   ")).await.unwrap_err();
        assert!(matches!(err, TtsError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn synthesise_stream_yields_one_final_chunk() {
        let canned = vec![0x49, 0x44, 0x33, 0x04]; // ID3 magic — anything works
        let (addr, _) = serve_fake(canned.clone(), 200).await;
        let backend = OpenAiBackend::new(cfg_for(addr));

        let mut req = SynthesiseRequest::new("hi.");
        req.format = OutputFormat::Mp3;
        let stream = backend.synthesise_stream(&req).await.unwrap();
        let chunks: Vec<_> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        let chunk = chunks.into_iter().next().unwrap().unwrap();
        assert!(chunk.is_final);
        assert_eq!(chunk.bytes, canned);
    }

    #[tokio::test]
    async fn list_voices_openai_returns_curated_set() {
        let backend = OpenAiBackend::new(OpenAiConfig {
            id: "openai", base_url: "http://x/v1".into(),
            api_key: "".into(), model: "tts-1".into(),
            default_voice: "alloy".into(), timeout_secs: 1,
        });
        let voices = backend.list_voices().await.unwrap();
        assert!(voices.iter().any(|v| v.id == "alloy"));
        assert!(voices.iter().any(|v| v.id == "nova"));
        assert!(voices.iter().all(|v| v.backend_id == "openai"));
    }

    #[tokio::test]
    async fn list_voices_compat_returns_only_default() {
        let backend = OpenAiBackend::new(OpenAiConfig {
            id: "openai_compat", base_url: "http://x/v1".into(),
            api_key: "".into(), model: "tts-1".into(),
            default_voice: "echo".into(), timeout_secs: 1,
        });
        let voices = backend.list_voices().await.unwrap();
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].id, "echo");
        assert_eq!(voices[0].backend_id, "openai_compat");
    }

    #[test]
    fn endpoint_strips_trailing_slash() {
        let backend = OpenAiBackend::new(OpenAiConfig {
            id: "openai", base_url: "https://api.openai.com/v1/".into(),
            api_key: "".into(), model: "tts-1".into(),
            default_voice: "alloy".into(), timeout_secs: 1,
        });
        assert_eq!(backend.endpoint(), "https://api.openai.com/v1/audio/speech");
    }

    #[test]
    fn response_format_maps_to_openai_strings() {
        assert_eq!(OpenAiBackend::response_format(OutputFormat::Mp3),     "mp3");
        assert_eq!(OpenAiBackend::response_format(OutputFormat::Wav),     "wav");
        assert_eq!(OpenAiBackend::response_format(OutputFormat::OggOpus), "opus");
    }

    #[test]
    fn root_strips_v1_suffix_for_compat_helpers() {
        let mk = |url: &str| OpenAiBackend::new(OpenAiConfig {
            id: "openai_compat", base_url: url.into(),
            api_key: "".into(), model: "tts-1".into(),
            default_voice: "alloy".into(), timeout_secs: 1,
        });
        assert_eq!(mk("http://x:8000/v1").root(),  "http://x:8000");
        assert_eq!(mk("http://x:8000/v1/").root(), "http://x:8000");
        assert_eq!(mk("http://x:8000").root(),     "http://x:8000");
        assert_eq!(mk("http://x:8000/").root(),    "http://x:8000");
    }

    #[test]
    fn extract_voices_handles_array_of_strings() {
        let j = serde_json::json!(["alloy", "echo", "nova"]);
        let v = extract_voices_from_json(&j, "openai_compat");
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].id,   "alloy");
        assert_eq!(v[0].name, "alloy");
    }

    #[test]
    fn extract_voices_handles_chatterbox_predefined_shape() {
        let j = serde_json::json!([
            {"display_name": "Abigail", "filename": "Abigail.wav"},
            {"display_name": "Adrian",  "filename": "Adrian.wav"},
        ]);
        let v = extract_voices_from_json(&j, "openai_compat");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id,   "Abigail.wav");
        assert_eq!(v[0].name, "Abigail");
    }

    #[test]
    fn extract_voices_handles_chatterbox_reference_files_shape() {
        // Reference-files endpoint returns a bare array of filename strings.
        let j = serde_json::json!(["Gianna.wav", "Robert.wav"]);
        let v = extract_voices_from_json(&j, "openai_compat");
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].id, "Robert.wav");
    }

    #[test]
    fn extract_voices_handles_voices_wrapper() {
        let j = serde_json::json!({"voices": ["alloy", "echo"]});
        let v = extract_voices_from_json(&j, "openai_compat");
        assert_eq!(v.iter().map(|x| x.id.clone()).collect::<Vec<_>>(),
                   vec!["alloy", "echo"]);
    }

    #[test]
    fn extract_voices_handles_data_wrapper_with_objects() {
        let j = serde_json::json!({"data": [
            {"id": "tts-voice-1", "name": "Voice One"},
            {"id": "tts-voice-2"},
        ]});
        let v = extract_voices_from_json(&j, "openai_compat");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id,   "tts-voice-1");
        assert_eq!(v[0].name, "Voice One");
        assert_eq!(v[1].name, "tts-voice-2"); // falls back to id
    }

    #[test]
    fn extract_voices_drops_items_with_no_id() {
        let j = serde_json::json!([
            {"display_name": "no id here"},
            {"id": "good"},
            {},
        ]);
        let v = extract_voices_from_json(&j, "openai_compat");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, "good");
    }
}
