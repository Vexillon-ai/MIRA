// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/notifications.rs
//! GET /api/notifications/stream  — SSE stream of cross-channel notifications.
//! GET /api/notifications/push/public-key — VAPID public key for the
//!   browser's PushManager.subscribe().
//! POST /api/notifications/push/subscribe — register a browser push subscription.
//! GET /api/notifications/push/subscriptions — list this user's subscriptions.
//! DELETE /api/notifications/push/subscriptions/:id — drop one.

use std::sync::Arc;
use std::convert::Infallible;

use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Sse},
    Extension, Json,
};
use axum::response::sse::{Event, KeepAlive};
use futures_util::stream;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::auth::AuthUser;
use crate::notifications::web_push::WebPushService;
use crate::notifications::NotificationBus;

pub async fn notifications_stream(
    AuthUser(_user): AuthUser,
    Extension(bus): Extension<Arc<NotificationBus>>,
) -> impl IntoResponse {
    let rx = bus.subscribe();

    let sse_stream = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(notif) => {
                    let data = serde_json::to_string(&notif).unwrap_or_default();
                    let event = Event::default().event("notification").data(data);
                    return Some((Ok::<Event, Infallible>(event), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed)    => return None,
            }
        }
    });

    Sse::new(sse_stream).keep_alive(KeepAlive::default())
}

// ── Web Push (Q1.2) ──────────────────────────────────────────────────────────

fn err(s: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (s, Json(serde_json::json!({ "error": msg.into() })))
}

/// GET /api/notifications/push/public-key
///
/// Returns the VAPID public key (uncompressed SEC1, base64url-no-pad).
/// The browser passes this string into `PushManager.subscribe()` as
/// `applicationServerKey`. 503 when the push service didn't open
/// (missing data dir, etc).
pub async fn push_public_key(
    Extension(svc): Extension<Option<Arc<WebPushService>>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc.ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "web push not enabled"))?;
    Ok(Json(serde_json::json!({ "vapid_public_key": svc.vapid_public_key_b64url() })))
}

#[derive(Debug, Deserialize)]
pub struct SubscribeRequest {
    pub endpoint:   String,
    pub keys:       SubscribeKeys,
    pub user_agent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SubscribeKeys {
    pub p256dh: String,
    pub auth:   String,
}

/// POST /api/notifications/push/subscribe
///
/// Persist a browser push subscription against the caller's user. The
/// browser sends the exact shape it gets back from
/// `PushSubscription.toJSON()`; we stash the three load-bearing fields
/// (endpoint, p256dh, auth) plus the User-Agent (for the Settings UI
/// "Registered devices" list).
pub async fn push_subscribe(
    AuthUser(me): AuthUser,
    Extension(svc): Extension<Option<Arc<WebPushService>>>,
    Json(body): Json<SubscribeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc.ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "web push not enabled"))?;
    if body.endpoint.is_empty() || body.keys.p256dh.is_empty() || body.keys.auth.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "endpoint + keys.p256dh + keys.auth required"));
    }
    let id = svc.subscribe(
        &me.id, &body.endpoint, &body.keys.p256dh, &body.keys.auth,
        body.user_agent.as_deref(),
    ).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("subscribe: {e}")))?;
    Ok(Json(serde_json::json!({ "id": id })))
}

/// GET /api/notifications/push/subscriptions
pub async fn push_list_subscriptions(
    AuthUser(me): AuthUser,
    Extension(svc): Extension<Option<Arc<WebPushService>>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc.ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "web push not enabled"))?;
    let subs = svc.list_for_user(&me.id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("list: {e}")))?;
    // Strip the cryptographic keys from the response — the UI only
    // needs id + ua + timestamps to show the "Registered devices" list
    // and a Revoke button.
    let view: Vec<_> = subs.into_iter().map(|s| serde_json::json!({
        "id":         s.id,
        "user_agent": s.user_agent,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })).collect();
    Ok(Json(serde_json::json!({ "subscriptions": view })))
}

/// DELETE /api/notifications/push/subscriptions/:id
pub async fn push_unsubscribe(
    AuthUser(me): AuthUser,
    Extension(svc): Extension<Option<Arc<WebPushService>>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc.ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "web push not enabled"))?;
    svc.unsubscribe(&id, &me.id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("unsubscribe: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/notifications/push/test — send a synthetic push to all of
/// the caller's subscriptions so they can confirm a freshly-installed
/// service worker actually receives messages. Useful in Settings.
pub async fn push_test(
    AuthUser(me): AuthUser,
    Extension(svc): Extension<Option<Arc<WebPushService>>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = svc.ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "web push not enabled"))?;
    let payload = crate::notifications::web_push::PushPayload {
        title:   "MIRA test push".to_string(),
        body:    "If you can see this, browser push is wired correctly.".to_string(),
        url:     Some("/".to_string()),
        channel: None,
    };
    let delivered = svc.send_to_user(&me.id, &payload).await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("send: {e}")))?;
    Ok(Json(serde_json::json!({ "delivered": delivered })))
}
