// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/waitlist.rs
//
//! Q1.7 — Hosted-MIRA waitlist HTTP endpoints.
//!
//! `POST  /api/waitlist/signup`         — public; takes `{email, source?}`
//! `GET   /api/admin/waitlist`          — admin; list + count
//! `GET   /api/admin/waitlist/export`   — admin; CSV download
//! `DELETE /api/admin/waitlist/{id}`    — admin; remove
//!
//! Public signup is the only unauthenticated endpoint MIRA exposes
//! besides /health and the SPA itself. To keep abuse manageable:
//!   - request body capped at 4 KiB
//!   - per-IP rate limit of 5 signups / 60 s (TODO; v1 has no
//!     rate-limiter — the existing ChannelRateLimiter is per-user)
//!   - email shape validation in the store before any DB write
//!
//! The store is created at boot under `<data_dir>/waitlist.db`. When
//! it's not available (open failure) the handler returns 503.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::auth::AdminUser;
use crate::waitlist::WaitlistStore;

#[derive(Debug, Deserialize)]
pub struct SignupRequest {
    pub email:  String,
    #[serde(default)]
    pub source: Option<String>,
}

pub async fn signup(
    Extension(store): Extension<Option<Arc<WaitlistStore>>>,
    headers:          HeaderMap,
    Json(body):       Json<SignupRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let store = store.ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "waitlist disabled — operator has not opened the waitlist store",
    ))?;
    let ua = headers.get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    match store.signup(body.email.trim(), ua, body.source.as_deref()) {
        Ok(entry) => {
            info!("waitlist: signup {} (source={:?})", entry.email, entry.source);
            Ok(Json(json!({
                "status":   "ok",
                "email":    entry.email,
                "position": store.count().unwrap_or(0),
            })))
        }
        Err(e) => Err(err(StatusCode::BAD_REQUEST, e.to_string())),
    }
}

pub async fn list(
    AdminUser(_):     AdminUser,
    Extension(store): Extension<Option<Arc<WaitlistStore>>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let store = store.ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "waitlist store not initialised",
    ))?;
    let entries = store.list(200)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("list: {e}")))?;
    let count = store.count().unwrap_or(entries.len() as u64);
    Ok(Json(json!({
        "count":   count,
        "entries": entries,
    })))
}

/// CSV export — the entire table, comma-separated, RFC-4180-ish
/// quoting on fields that contain commas / quotes / newlines.
pub async fn export_csv(
    AdminUser(_):     AdminUser,
    Extension(store): Extension<Option<Arc<WaitlistStore>>>,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let store = store.ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "waitlist store not initialised",
    ))?;
    let entries = store.list(100_000)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("list: {e}")))?;
    let mut csv = String::from("email,created_at,source,user_agent\n");
    for e in entries {
        csv.push_str(&format!(
            "{},{},{},{}\n",
            csv_field(&e.email),
            e.created_at.to_rfc3339(),
            csv_field(e.source.as_deref().unwrap_or("")),
            csv_field(e.user_agent.as_deref().unwrap_or("")),
        ));
    }
    let filename = format!(
        "mira-waitlist-{}.csv",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
    );
    let response = Response::builder()
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            format!(r#"attachment; filename="{filename}""#),
        )
        .body(Body::from(csv))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("response: {e}")))?;
    Ok(response)
}

pub async fn delete_entry(
    AdminUser(_):     AdminUser,
    Extension(store): Extension<Option<Arc<WaitlistStore>>>,
    Path(id):         Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = store.ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "waitlist store not initialised",
    ))?;
    store.delete(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("delete: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

fn err(s: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    let m = msg.into();
    warn!("waitlist endpoint: {m}");
    (s, Json(json!({ "error": m })))
}
