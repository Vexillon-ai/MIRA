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
                    // Serialise the canonical envelope (superset of the
                    // legacy shape) so native clients get type/severity/
                    // sent_at while the existing web client keeps reading
                    // kind/channel/message.
                    let data = serde_json::to_string(&notif.to_envelope()).unwrap_or_default();
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
    /// Transport: "webpush" (default, browser), "http" (FCM relay /
    /// UnifiedPush endpoint — PUSH-NOTIFICATIONS Part C), or "fcm" (legacy
    /// direct FCM token; superseded by "http").
    #[serde(default)]
    pub kind:       Option<String>,
    // ── Web Push fields ──
    pub endpoint:   Option<String>,
    pub keys:       Option<SubscribeKeys>,
    pub user_agent: Option<String>,
    // ── FCM fields ──
    /// FCM registration token (the device's push token).
    pub fcm_token:  Option<String>,
    /// Native platform label, e.g. "android".
    pub platform:   Option<String>,
    /// Human device label for the "Registered devices" list.
    pub device_name: Option<String>,
    // ── HTTP push-endpoint fields (Part C) ──
    /// The relay/distributor URL MIRA POSTs the envelope to (e.g.
    /// `https://push.vexillon.ai/v1/p/<push_id>`).
    pub endpoint_url: Option<String>,
    /// Optional bearer secret sent as `Authorization: Bearer <…>`.
    pub auth_secret:  Option<String>,
    // Tolerated aliases — apps in the wild name these a few different ways;
    // accept the common ones so a minor client naming mismatch doesn't 400.
    pub url:          Option<String>,
    pub push_url:     Option<String>,
    pub secret:       Option<String>,
    pub push_secret:  Option<String>,
}

impl SubscribeRequest {
    /// The HTTP-endpoint URL across all accepted field names.
    fn http_url(&self) -> Option<&str> {
        [&self.endpoint_url, &self.url, &self.push_url, &self.endpoint]
            .into_iter()
            .filter_map(|o| o.as_deref())
            .find(|s| !s.is_empty())
    }
    /// The bearer secret across all accepted field names.
    fn http_secret(&self) -> Option<&str> {
        [&self.auth_secret, &self.secret, &self.push_secret]
            .into_iter()
            .filter_map(|o| o.as_deref())
            .find(|s| !s.is_empty())
    }
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
    // Resolve the transport. Explicit `kind` wins; otherwise infer: an HTTP
    // push-endpoint URL with no VAPID keys is a `kind:"http"` (relay /
    // UnifiedPush) registration that just forgot the `kind` field.
    let kind = match body.kind.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(k) => k,
        None if body.keys.is_none()
            && (body.endpoint_url.is_some() || body.url.is_some() || body.push_url.is_some())
            => "http",
        None => "webpush",
    };
    let id = match kind {
        "fcm" => {
            let token = body.fcm_token.as_deref()
                // tolerate `endpoint` as an alias for the token
                .or(body.endpoint.as_deref())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, "fcm_token required for kind=fcm"))?;
            if !svc.fcm_enabled() {
                return Err(err(StatusCode::SERVICE_UNAVAILABLE, "FCM is not enabled on this server"));
            }
            svc.subscribe_fcm(&me.id, token, body.platform.as_deref(), body.device_name.as_deref())
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("subscribe: {e}")))?
        }
        // Part C — generic HTTP push endpoint (FCM relay / UnifiedPush). MIRA
        // just POSTs the envelope to `endpoint_url` with the optional bearer
        // secret; no Firebase credentials or `fcm_enabled` gate needed.
        "http" => {
            // Accept endpoint_url / url / push_url / endpoint for the URL, and
            // auth_secret / secret / push_secret for the bearer — clients name
            // these inconsistently.
            let endpoint_url = body.http_url()
                .ok_or_else(|| err(StatusCode::BAD_REQUEST,
                    "endpoint_url required for kind=http (also accepted: url, push_url, endpoint)"))?;
            svc.subscribe_http(
                &me.id,
                endpoint_url,
                body.http_secret(),
                body.platform.as_deref(),
                body.device_name.as_deref(),
            ).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("subscribe: {e}")))?
        }
        "webpush" => {
            let endpoint = body.endpoint.as_deref().unwrap_or("");
            let keys = body.keys.as_ref();
            let p256dh = keys.map(|k| k.p256dh.as_str()).unwrap_or("");
            let auth   = keys.map(|k| k.auth.as_str()).unwrap_or("");
            if endpoint.is_empty() || p256dh.is_empty() || auth.is_empty() {
                return Err(err(StatusCode::BAD_REQUEST, "endpoint + keys.p256dh + keys.auth required"));
            }
            svc.subscribe(&me.id, endpoint, p256dh, auth, body.user_agent.as_deref())
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("subscribe: {e}")))?
        }
        other => return Err(err(StatusCode::BAD_REQUEST, format!("unknown subscription kind: {other}"))),
    };
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
        "id":          s.id,
        "kind":        s.kind,
        "platform":    s.platform,
        "device_name": s.device_name,
        "user_agent":  s.user_agent,
        "created_at":  s.created_at,
        "updated_at":  s.updated_at,
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
    let payload = crate::notifications::Notification {
        kind:    crate::notifications::NotificationKind::ConversationUpdated,
        message: Some("If you can see this, browser push is wired correctly.".to_string()),
        channel: Some("web".to_string()),
        ..Default::default()
    }.to_envelope();
    let delivered = svc.send_to_user(&me.id, &payload).await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("send: {e}")))?;
    Ok(Json(serde_json::json!({ "delivered": delivered })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(json: serde_json::Value) -> SubscribeRequest {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn http_url_and_secret_accept_aliases() {
        // canonical
        assert_eq!(req(serde_json::json!({"endpoint_url":"u1","auth_secret":"s1"})).http_url(), Some("u1"));
        // aliases
        assert_eq!(req(serde_json::json!({"url":"u2"})).http_url(), Some("u2"));
        assert_eq!(req(serde_json::json!({"push_url":"u3"})).http_url(), Some("u3"));
        assert_eq!(req(serde_json::json!({"endpoint":"u4"})).http_url(), Some("u4"));
        assert_eq!(req(serde_json::json!({"secret":"s2"})).http_secret(), Some("s2"));
        assert_eq!(req(serde_json::json!({"push_secret":"s3"})).http_secret(), Some("s3"));
        // empty strings are ignored
        assert_eq!(req(serde_json::json!({"endpoint_url":"","url":"u"})).http_url(), Some("u"));
        assert_eq!(req(serde_json::json!({})).http_url(), None);
    }
}
