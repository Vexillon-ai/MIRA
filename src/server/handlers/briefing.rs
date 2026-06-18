// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/briefing.rs
//
//! Q1.6 — Daily Briefing endpoints (user-scoped).
//!
//! `GET    /api/me/briefing`           — current config (enabled / hour / last fire)
//! `PATCH  /api/me/briefing`           — toggle + change hour
//! `POST   /api/me/briefing/send-now`  — fire on demand for testing
//!
//! Available only when the companion system opened cleanly. When
//! companion isn't installed (channel-only / minimal builds) these
//! return 503.

use std::sync::Arc;

use axum::{Extension, Json};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use crate::agent::AgentCore;
use crate::auth::AuthUser;
use crate::companion::dispatcher::DispatchOutcome;

/// Read the caller's briefing settings.
pub async fn get_briefing(
    AuthUser(me):     AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let sys = agent.companion().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "companion not enabled on this server",
    ))?;
    let row = sys.store().get(&me.id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("get: {e}")))?;
    let Some(s) = row else {
        // No row yet — return defaults so the UI can render a fresh
        // form. (Companion must be enabled before briefings can fire;
        // this isn't an error path, just an initial state.)
        return Ok(Json(json!({
            "enabled":          false,
            "hour":             7,
            "last_briefing_at": null,
            "companion_active": false,
        })));
    };
    Ok(Json(json!({
        "enabled":          s.daily_briefing_enabled,
        "hour":             s.daily_briefing_hour,
        "last_briefing_at": s.last_briefing_at.map(|d| d.timestamp_millis()),
        // Last *actual* delivery (stamped only on a successful send), so the
        // UI can surface a real proactive-delivery health signal rather than
        // the carried-forward settings timestamp.
        "last_checkin_at":  s.last_checkin_at.map(|d| d.timestamp_millis()),
        "companion_active": s.is_active(chrono::Utc::now()),
    })))
}

#[derive(Debug, Deserialize)]
pub struct PatchBriefingRequest {
    pub enabled: Option<bool>,
    /// Local-hour 0..=23.
    pub hour:    Option<u8>,
}

/// Update briefing toggle + hour. Other companion fields are
/// untouched — we re-upsert with everything else from the existing
/// row so a briefing edit can't accidentally drop safety-contact /
/// quiet-hours / etc.
pub async fn patch_briefing(
    AuthUser(me):     AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Json(body):       Json<PatchBriefingRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let sys = agent.companion().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "companion not enabled on this server",
    ))?;
    let store = sys.store();
    let mut s = match store.get(&me.id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("get: {e}")))?
    {
        Some(row) => row,
        None => return Err(err(
            StatusCode::CONFLICT,
            "enable companion mode before configuring daily briefing",
        )),
    };
    if let Some(en) = body.enabled { s.daily_briefing_enabled = en; }
    if let Some(h)  = body.hour {
        if h > 23 {
            return Err(err(StatusCode::BAD_REQUEST, "hour must be 0..=23"));
        }
        s.daily_briefing_hour = h;
    }
    s.updated_at = chrono::Utc::now();
    store.upsert(&s)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("upsert: {e}")))?;
    info!(user = %me.username, "briefing config updated (enabled={}, hour={})",
          s.daily_briefing_enabled, s.daily_briefing_hour);
    Ok(Json(json!({
        "enabled":          s.daily_briefing_enabled,
        "hour":             s.daily_briefing_hour,
        "last_briefing_at": s.last_briefing_at.map(|d| d.timestamp_millis()),
    })))
}

/// Fire a briefing on demand. Used for testing — bypasses the
/// once-per-day guard and the local-hour gate. Channel routing /
/// content gathering is identical to the scheduled path.
pub async fn send_briefing_now(
    AuthUser(me):     AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let _sys = agent.companion().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "companion not enabled on this server",
    ))?;
    let dispatcher = agent.companion_dispatcher().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "companion dispatcher not installed (scheduler didn't spawn — usually means \
         the history store failed at boot; check server logs)",
    ))?;
    match dispatcher.send_briefing(&me.id).await {
        Ok(DispatchOutcome::Sent { conversation_id, channel, chars }) => Ok(Json(json!({
            "status":          "sent",
            "channel":         channel,
            "chars":           chars,
            "conversation_id": conversation_id,
        }))),
        Ok(DispatchOutcome::SkippedNoChannel) => Err(err(
            StatusCode::CONFLICT,
            "no channel resolved — configure a preferred companion channel (Signal / Telegram / web)",
        )),
        Ok(DispatchOutcome::Failed(msg)) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("briefing failed: {msg}"),
        )),
        Err(e) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("send_briefing error: {e}"),
        )),
    }
}

fn err(s: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (s, Json(json!({ "error": msg.into() })))
}
