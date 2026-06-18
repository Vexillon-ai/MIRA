// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/stt.rs
//! Speech-to-Text HTTP API.
//!
//! Endpoints:
//! * `POST /api/stt/transcribe`   multipart audio → `{ text, … }`
//! * `GET  /api/stt/status`       backend probe + currently-active id
//!
//! All endpoints sit behind the standard `AuthLayer`; a logged-in user can
//! transcribe their own voice notes from the web UI or from a channel
//! integration. Anonymous webhook ingest (Signal/Telegram) does NOT call
//! these handlers — it goes through `SttService::transcribe(...)` directly
//! from the channel adapter, bypassing AuthLayer.

use axum::extract::{Multipart, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::auth::AuthUser;
use crate::stt::types::{AudioInputFormat, SttError, TranscribeRequest};
use crate::stt::SttService;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn map_err(e: SttError) -> Response {
    let status = match &e {
        SttError::BackendNotConfigured(_) => StatusCode::SERVICE_UNAVAILABLE,
        SttError::BackendUnavailable(..)  => StatusCode::SERVICE_UNAVAILABLE,
        SttError::ModelNotInstalled(_)    => StatusCode::NOT_FOUND,
        SttError::BadRequest(_)           => StatusCode::BAD_REQUEST,
        SttError::Unauthorized            => StatusCode::UNAUTHORIZED,
        SttError::Timeout                 => StatusCode::GATEWAY_TIMEOUT,
        SttError::Decoding(_)             => StatusCode::BAD_REQUEST,
        SttError::Upstream(_)             => StatusCode::BAD_GATEWAY,
        SttError::Io(_)                   => StatusCode::INTERNAL_SERVER_ERROR,
        SttError::Http(_)                 => StatusCode::BAD_GATEWAY,
    };
    (status, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/stt/transcribe
// ─────────────────────────────────────────────────────────────────────────────
//
// Browser side uses `MediaRecorder` to produce a WebM/Opus blob and uploads
// it as a multipart form. We accept any container Symphonia can decode —
// the `file` part is the audio bytes; optional text parts override the
// language hint or pin a specific backend.

#[derive(Debug, Serialize)]
pub struct TranscribeResponse {
    pub text:        String,
    pub language:    Option<String>,
    pub duration_ms: Option<u64>,
    pub latency_ms:  u64,
    pub backend:     String,
}

pub async fn transcribe(
    AuthUser(_user): AuthUser,
    Extension(svc):  Extension<SttService>,
    mut form:        Multipart,
) -> Response {
    // Multipart parse — we expect one `file` part (required) plus optional
    // `language` / `backend` / `channel` text parts.
    let mut audio_bytes:    Option<Vec<u8>> = None;
    let mut audio_format:   AudioInputFormat = AudioInputFormat::Unknown;
    let mut language_hint:  Option<String> = None;
    let mut backend_hint:   Option<String> = None;
    let mut channel_hint:   Option<String> = None;

    loop {
        let field = match form.next_field().await {
            Ok(Some(f)) => f,
            Ok(None)    => break,
            Err(e)      => return map_err(SttError::BadRequest(format!("multipart: {e}"))),
        };
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" | "audio" => {
                // Sniff format from the part's Content-Type or filename
                // before we consume the bytes.
                let mime    = field.content_type().unwrap_or("").to_string();
                let fname   = field.file_name().unwrap_or("").to_string();
                audio_format = AudioInputFormat::from_mime(&mime);
                if matches!(audio_format, AudioInputFormat::Unknown) && !fname.is_empty() {
                    audio_format = AudioInputFormat::from_mime(&fname);
                }
                let bytes = match field.bytes().await {
                    Ok(b)  => b.to_vec(),
                    Err(e) => return map_err(SttError::BadRequest(format!("read file part: {e}"))),
                };
                audio_bytes = Some(bytes);
            }
            "language" => {
                if let Ok(t) = field.text().await {
                    let t = t.trim().to_string();
                    if !t.is_empty() { language_hint = Some(t); }
                }
            }
            "backend" => {
                if let Ok(t) = field.text().await {
                    let t = t.trim().to_string();
                    if !t.is_empty() { backend_hint = Some(t); }
                }
            }
            "channel" => {
                if let Ok(t) = field.text().await {
                    let t = t.trim().to_string();
                    if !t.is_empty() { channel_hint = Some(t); }
                }
            }
            other => debug!("stt: ignoring unknown multipart field '{other}'"),
        }
    }

    let bytes = match audio_bytes {
        Some(b) if !b.is_empty() => b,
        _ => return map_err(SttError::BadRequest("missing 'file' part".into())),
    };

    // Per-config max audio bytes — we don't know the actual decoded length
    // until the backend decodes, but a *byte* upper bound stops a runaway
    // upload before we touch ffmpeg/symphonia. 32 MB covers ~1h of
    // 64 kbit Opus, well above any sane voice note.
    if bytes.len() > 32 * 1024 * 1024 {
        return map_err(SttError::BadRequest(format!(
            "audio too large: {} bytes (limit 32 MB)", bytes.len()
        )));
    }

    let req = TranscribeRequest {
        audio_bytes: bytes,
        format:      audio_format,
        language:    language_hint,
    };

    match svc.transcribe(req, backend_hint.as_deref(), channel_hint.as_deref().or(Some("web"))).await {
        Ok(t) => {
            let dto = TranscribeResponse {
                text:        t.text,
                language:    t.language,
                duration_ms: t.duration_ms,
                latency_ms:  t.latency_ms,
                backend:     t.backend_id,
            };
            (StatusCode::OK, Json(dto)).into_response()
        }
        Err(e) => {
            warn!("stt: transcribe failed: {e}");
            map_err(e)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/stt/status
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    #[serde(default)]
    pub backend: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub enabled:        bool,
    pub backend:        String,
    pub backends:       Vec<String>,
    pub healthy:        bool,
    pub latency_ms:     Option<u64>,
    pub note:           Option<String>,
}

pub async fn status(
    AuthUser(_user):  AuthUser,
    Extension(svc):   Extension<SttService>,
    Query(q):         Query<StatusQuery>,
) -> Response {
    let backend_id = q.backend.clone()
        .unwrap_or_else(|| svc.resolve_backend(None, None));

    let probe = svc.probe(Some(&backend_id)).await;
    let resp = match probe {
        Ok(p) => StatusResponse {
            enabled:    svc.enabled(),
            backend:    backend_id,
            backends:   svc.backend_ids(),
            healthy:    p.healthy,
            latency_ms: p.latency_ms,
            note:       p.note,
        },
        Err(e) => StatusResponse {
            enabled:    svc.enabled(),
            backend:    backend_id,
            backends:   svc.backend_ids(),
            healthy:    false,
            latency_ms: None,
            note:       Some(e.to_string()),
        },
    };
    (StatusCode::OK, Json(resp)).into_response()
}
