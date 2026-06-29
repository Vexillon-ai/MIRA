// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/tts.rs
//! Text-to-Speech HTTP API.
//!
//! surface (full-buffer only — streaming + voice-download progress
//! land in later stages):
//! * `POST /api/tts/speak`         body → binary audio (wav/mp3/ogg)
//! * `GET  /api/tts/voices`        list voices for one backend
//! * `GET  /api/tts/status`        backend health + cache stats
//!
//! All endpoints are behind the standard `AuthLayer` — see `router.rs`.

use std::convert::Infallible;

use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use std::sync::Arc;

use crate::auth::{AuthUser, LocalAuthService};
use crate::tts::types::{AudioCodec, OutputFormat, TtsError};
use crate::tts::TtsService;
use crate::voice::{parse_user_prefs, resolve_voice, ChannelRegistry};
use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

// Pick the voice id to send to the backend. The explicit `voice` field on
// the request wins (per-call override from the client). When omitted, fall
// back to the user's `voice_prefs.<channel>.voice_id` layered over the
// server defaults — matching the resolution that messaging-channel
// dispatchers use, so a user gets the same voice everywhere.
fn resolve_request_voice(
    svc:     &TtsService,
    auth:    &LocalAuthService,
    user_id: &str,
    channel: &str,
    explicit: Option<&str>,
) -> Option<String> {
    if let Some(v) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(v.to_owned());
    }
    let user_prefs = auth.get_user(user_id).ok().flatten()
        .map(|u| parse_user_prefs(u.voice_prefs.as_deref()))
        .unwrap_or_default();
    let resolved = resolve_voice(channel, Some(&user_prefs), &svc.voice_prefs_defaults());
    resolved.voice_id
}

fn map_err(e: TtsError) -> Response {
    let status = match &e {
        TtsError::BackendNotConfigured(_) => StatusCode::SERVICE_UNAVAILABLE,
        TtsError::BackendUnavailable(..)  => StatusCode::SERVICE_UNAVAILABLE,
        TtsError::VoiceNotInstalled(_)    => StatusCode::NOT_FOUND,
        TtsError::BadRequest(_)           => StatusCode::BAD_REQUEST,
        TtsError::Unauthorized            => StatusCode::UNAUTHORIZED,
        TtsError::Timeout                 => StatusCode::GATEWAY_TIMEOUT,
        TtsError::Upstream(_)             => StatusCode::BAD_GATEWAY,
        TtsError::Encoding(_)             => StatusCode::INTERNAL_SERVER_ERROR,
        TtsError::Io(_)                   => StatusCode::INTERNAL_SERVER_ERROR,
        TtsError::Http(_)                 => StatusCode::BAD_GATEWAY,
    };
    (status, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/tts/speak
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SpeakRequest {
    pub text:    String,
    #[serde(default)] pub voice:   Option<String>,
    #[serde(default)] pub speed:   Option<f32>,
    #[serde(default)] pub format:  Option<String>,
    #[serde(default)] pub backend: Option<String>,
    // Optional channel hint (`web` | `tui` | `telegram` | `signal` | `mobile`)
    // so the router applies per-channel pinning + voice. When omitted, the
    // channel is inferred from the `X-Mira-Client` header (so the native mobile
    // app gets its own routing/voice without sending a body field); absent →
    // `web`.
    #[serde(default)] pub channel: Option<String>,
}

// Resolve the effective channel id for a TTS request: an explicit body
// `channel` wins; otherwise mirror the chat handler and read `X-Mira-Client`
// (any non-"web" native client → `mobile`); default `web`.
fn effective_channel(req_channel: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(c) = req_channel.map(str::trim).filter(|s| !s.is_empty()) {
        return c.to_ascii_lowercase();
    }
    match headers
        .get("x-mira-client")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("") | Some("web") => "web".to_string(),
        Some(_) => "mobile".to_string(),
    }
}

pub async fn speak(
    AuthUser(user):   AuthUser,
    Extension(svc):   Extension<TtsService>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    headers:          HeaderMap,
    Json(req):        Json<SpeakRequest>,
) -> Response {
    let fmt     = req.format.as_deref().and_then(OutputFormat::parse);
    let channel = effective_channel(req.channel.as_deref(), &headers);
    let channel = channel.as_str();
    let voice   = resolve_request_voice(&svc, auth.as_ref(), &user.id, channel, req.voice.as_deref());
    let buf = match svc.speak(
        &req.text,
        voice.as_deref(),
        req.speed,
        fmt,
        req.backend.as_deref(),
        Some(channel),
    ).await {
        Ok(b)  => b,
        Err(e) => return map_err(e),
    };
    let ct = buf.codec.content_type();
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE,   ct.to_string()),
            (header::CACHE_CONTROL,  "private, max-age=3600".to_string()),
        ],
        buf.bytes,
    ).into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/tts/speak/stream  (Server-Sent Events)
// ─────────────────────────────────────────────────────────────────────────────
//
// Per design doc §4: emits one `chunk` event per sentence with
// `{ codec, b64, is_final }`, a terminal `done` event, and an `error` event
// on backend failure. The web client decodes the base64 bytes into Blobs and
// queues them for sequential playback.

#[derive(Debug, Serialize)]
struct ChunkPayload<'a> {
    codec:    &'a str,
    b64:      String,
    is_final: bool,
}

#[derive(Debug, Serialize)]
struct ErrorPayload {
    message: String,
}

pub async fn speak_stream(
    AuthUser(user):   AuthUser,
    Extension(svc):   Extension<TtsService>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    headers:          HeaderMap,
    Json(req):        Json<SpeakRequest>,
) -> Response {
    let fmt     = req.format.as_deref().and_then(OutputFormat::parse);
    let channel = effective_channel(req.channel.as_deref(), &headers);
    let channel = channel.as_str();
    let voice   = resolve_request_voice(&svc, auth.as_ref(), &user.id, channel, req.voice.as_deref());
    let stream = match svc.speak_stream(
        &req.text,
        voice.as_deref(),
        req.speed,
        fmt,
        req.backend.as_deref(),
        Some(channel),
    ).await {
        Ok(s)  => s,
        Err(e) => return map_err(e),
    };

    // Tag each backend chunk into an SSE event, then append a terminal `done`.
    let chunk_events = stream.map(|res| -> Result<Event, Infallible> {
        match res {
            Ok(chunk) => {
                let payload = ChunkPayload {
                    codec:    codec_label(&chunk.codec),
                    b64:      B64.encode(&chunk.bytes),
                    is_final: chunk.is_final,
                };
                let data = serde_json::to_string(&payload)
                    .unwrap_or_else(|_| "{}".into());
                Ok(Event::default().event("chunk").data(data))
            }
            Err(e) => {
                let payload = ErrorPayload { message: e.to_string() };
                let data = serde_json::to_string(&payload)
                    .unwrap_or_else(|_| "{\"message\":\"unknown\"}".into());
                Ok(Event::default().event("error").data(data))
            }
        }
    });
    let done = futures::stream::once(async {
        Ok::<Event, Infallible>(Event::default().event("done").data("{}"))
    });
    let combined = chunk_events.chain(done);

    Sse::new(combined).keep_alive(KeepAlive::default()).into_response()
}

fn codec_label(c: &AudioCodec) -> &'static str {
    match c {
        AudioCodec::Wav { .. } => "wav",
        AudioCodec::Mp3        => "mp3",
        AudioCodec::OggOpus    => "ogg-opus",
        AudioCodec::Pcm { .. } => "pcm",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/tts/voices
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct VoicesQuery {
    #[serde(default)]
    pub backend: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VoiceDto {
    pub id:             String,
    pub name:           String,
    pub language:       String,
    pub gender:         Option<String>,
    pub sample_rate:    Option<u32>,
    pub is_downloaded:  bool,
    pub backend:        String,
}

pub async fn voices(
    AuthUser(_user):  AuthUser,
    Extension(svc):   Extension<TtsService>,
    Query(q):         Query<VoicesQuery>,
) -> Response {
    match svc.list_voices(q.backend.as_deref()).await {
        Ok(list) => {
            let dtos: Vec<VoiceDto> = list.into_iter().map(|v| VoiceDto {
                id:            v.id,
                name:          v.name,
                language:      v.language,
                gender:        v.gender,
                sample_rate:   v.sample_rate,
                is_downloaded: v.is_downloaded,
                backend:       v.backend_id,
            }).collect();
            (StatusCode::OK, Json(dtos)).into_response()
        }
        Err(e) => map_err(e),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/tts/voices/download — pre-fetch a voice's model files
// ─────────────────────────────────────────────────────────────────────────────
//
// Local backends (Piper) download the `.onnx` model pair on first speak.
// That makes the user's first audible sentence pause for several seconds
// while the network fetch completes. The Settings UI calls this endpoint
// when the user picks a voice from the dropdown so the download happens
// up-front and the dropdown can flip the "(not downloaded)" suffix off
// without a synthesise call.
//
// Cloud backends use the default no-op trait impl, so a misrouted call
// returns 200 silently.

#[derive(Debug, Deserialize)]
pub struct DownloadVoiceRequest {
    #[serde(default)] pub backend: Option<String>,
    pub voice_id: String,
}

pub async fn download_voice(
    AuthUser(_user):  AuthUser,
    Extension(svc):   Extension<TtsService>,
    Json(req):        Json<DownloadVoiceRequest>,
) -> Response {
    if req.voice_id.trim().is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "voice_id is empty" })))
            .into_response();
    }
    match svc.ensure_voice(req.backend.as_deref(), req.voice_id.trim()).await {
        Ok(())  => (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
                       .into_response(),
        Err(e)  => map_err(e),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/tts/status
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    #[serde(default)]
    pub backend: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusDto {
    pub enabled:         bool,
    pub backend:         String,
    pub backends:        Vec<String>,
    // `{channel_id → resolved backend}` for every channel in the
    // `ChannelRegistry`. Lets the UI scope the voice-id picker to only
    // the voices the routed backend actually accepts, so a Piper voice
    // can't accidentally be saved for a channel pinned to `openai_compat`.
    pub routing:         HashMap<String, String>,
    pub healthy:         bool,
    pub last_latency_ms: Option<u64>,
    pub note:            Option<String>,
    pub cache:           CacheDto,
}

#[derive(Debug, Serialize)]
pub struct CacheDto {
    pub entries:     usize,
    pub total_bytes: u64,
}

pub async fn status(
    AuthUser(_user):    AuthUser,
    Extension(svc):     Extension<TtsService>,
    Extension(channels): Extension<Arc<ChannelRegistry>>,
    Query(q):           Query<StatusQuery>,
) -> Response {
    let resolved = svc.resolve_backend(q.backend.as_deref(), None);
    let probe = svc.probe(Some(&resolved)).await;
    let stats = svc.cache_stats().await;

    let routing: HashMap<String, String> = channels.list().into_iter()
        .map(|c| {
            let b = svc.resolve_backend(None, Some(&c.id));
            (c.id, b)
        })
        .collect();

    let dto = match probe {
        Ok(p) => StatusDto {
            enabled:         svc.enabled(),
            backend:         resolved,
            backends:        svc.backend_ids(),
            routing,
            healthy:         p.healthy,
            last_latency_ms: p.latency_ms,
            note:            p.note,
            cache:           CacheDto { entries: stats.entries, total_bytes: stats.total_bytes },
        },
        Err(e) => StatusDto {
            enabled:         svc.enabled(),
            backend:         resolved,
            backends:        svc.backend_ids(),
            routing,
            healthy:         false,
            last_latency_ms: None,
            note:            Some(e.to_string()),
            cache:           CacheDto { entries: stats.entries, total_bytes: stats.total_bytes },
        },
    };
    (StatusCode::OK, Json(dto)).into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MiraConfig;

    #[tokio::test]
    async fn voices_returns_curated_piper_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = MiraConfig::default();
        cfg.data_dir = dir.path().to_string_lossy().into_owned();
        cfg.tts.internal.auto_download_voices = false;
        let svc = TtsService::from_config(&cfg);

        let list = svc.list_voices(None).await.unwrap();
        assert!(list.iter().any(|v| v.id == "en_US-amy-medium"));
    }
}
