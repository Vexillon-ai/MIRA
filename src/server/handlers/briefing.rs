// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/briefing.rs
//
//! Q1.6 — Daily Briefing endpoints (user-scoped).
//!
//! `GET    /api/me/briefing`           — current config (enabled / hour / last fire)
//! `PATCH  /api/me/briefing`           — toggle + change hour
//! `POST   /api/me/briefing/send-now`  — fire on demand (async; returns 202,
//!                                        delivers on the companion channel)
//!
//! Available only when the companion system opened cleanly. When
//! companion isn't installed (channel-only / minimal builds) these
//! return 503.

use std::sync::Arc;

use axum::{Extension, Json};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

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

/// Hard ceiling on a single on-demand briefing generation+delivery so a hung
/// provider call can't leak a background task forever. Generous — a briefing
/// gathers recall/context and can be a slow LLM turn — but bounded.
const BRIEFING_GEN_TIMEOUT_SECS: u64 = 180;

/// Fire a briefing on demand (bypasses the once-per-day guard and local-hour
/// gate). Generation is a full LLM turn with recall/context gathering, which
/// routinely exceeds a mobile client's read timeout (~30s), so this **does not
/// block on it**: it validates that companion is installed, kicks generation +
/// delivery off in the background, and returns `202 Accepted` immediately. The
/// briefing then lands on the user's companion channel (push / messaging / web)
/// when ready — identical to a scheduled briefing, which the scheduler already
/// runs off-thread. Outcome + failures are logged, not returned synchronously.
pub async fn send_briefing_now(
    AuthUser(me):     AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let _sys = agent.companion().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "companion not enabled on this server",
    ))?;
    let dispatcher = agent.companion_dispatcher().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "companion dispatcher not installed (scheduler didn't spawn — usually means \
         the history store failed at boot; check server logs)",
    ))?.clone();

    let user_id = me.id.clone();
    let username = me.username.clone();
    tokio::spawn(async move {
        let fut = dispatcher.send_briefing(&user_id);
        match tokio::time::timeout(
            std::time::Duration::from_secs(BRIEFING_GEN_TIMEOUT_SECS), fut,
        ).await {
            Ok(Ok(DispatchOutcome::Sent { channel, chars, conversation_id, .. })) => info!(
                user = %username,
                "on-demand briefing delivered on '{channel}' ({chars} chars, conv={conversation_id})",
            ),
            Ok(Ok(DispatchOutcome::SkippedNoChannel)) => warn!(
                user = %username,
                "on-demand briefing skipped — no channel resolved (configure a preferred companion channel)",
            ),
            Ok(Ok(DispatchOutcome::Failed(msg))) => warn!(
                user = %username, "on-demand briefing failed to deliver: {msg}",
            ),
            Ok(Err(e)) => warn!(user = %username, "on-demand briefing generation error: {e}"),
            Err(_)     => warn!(
                user = %username,
                "on-demand briefing timed out after {BRIEFING_GEN_TIMEOUT_SECS}s — abandoned",
            ),
        }
    });

    Ok((StatusCode::ACCEPTED, Json(json!({
        "status": "accepted",
        "detail": "briefing is being generated; it will arrive on your companion channel shortly",
    }))))
}

fn err(s: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (s, Json(json!({ "error": msg.into() })))
}
