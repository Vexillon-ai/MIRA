// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/device_tokens.rs
//! Device-token management.
//!
//! A user (or an admin, for a user) can create, list and revoke long-lived
//! `mdt_…` device tokens — opaque bearer tokens a headless device (e.g. a voice
//! client) uses instead of a human login. Tokens are shown once at creation and
//! stored hashed; listing never exposes the token. All three routes are
//! self-or-admin: the caller must be the owning user or an admin.

use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use crate::auth::{AuthUser, LocalAuthService, Role, SCOPE_CHAT, SCOPE_CHAT_CANCEL};

#[derive(Deserialize)]
pub struct CreateDeviceTokenBody {
    /// Human-readable device name (required) — shown in the list + attribution.
    pub name: String,
    /// Scopes the token may use. Defaults to `["chat","chat.cancel"]` (the
    /// minimum useful set) when omitted or empty.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// Optional lifetime in seconds. Omitted → the token never expires.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Serialize)]
pub struct CreatedDeviceToken {
    pub token_id: String,
    /// The raw `mdt_…` token — shown exactly once; never retrievable again.
    pub token:    String,
    pub name:     String,
    pub scopes:   Vec<String>,
}

#[derive(Serialize)]
pub struct DeviceTokenSummary {
    pub token_id:     String,
    pub name:         String,
    pub scopes:       Vec<String>,
    pub created_at:   i64,
    pub expires_at:   Option<i64>,
    pub last_used_at: Option<i64>,
}

// Self-or-admin guard: the caller must own `user_id` or be an admin.
fn may_manage(caller: &crate::auth::User, user_id: &str) -> bool {
    caller.id == user_id || caller.role == Role::Admin
}

/// POST /api/users/{user_id}/device-tokens — mint a token (shown once).
pub async fn create_device_token(
    AuthUser(caller): AuthUser,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Path(user_id):    Path<String>,
    Json(body):       Json<CreateDeviceTokenBody>,
) -> axum::response::Response {
    if !may_manage(&caller, &user_id) {
        return (StatusCode::FORBIDDEN, "not your account").into_response();
    }
    let name = body.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "device name is required").into_response();
    }
    // Default to the minimum useful scopes; keep only recognised scopes so a
    // typo can't silently grant nothing or something unexpected.
    let scopes: Vec<String> = body.scopes
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![SCOPE_CHAT.to_string(), SCOPE_CHAT_CANCEL.to_string()])
        .into_iter()
        .filter(|s| s == SCOPE_CHAT || s == SCOPE_CHAT_CANCEL)
        .collect();
    if scopes.is_empty() {
        return (StatusCode::BAD_REQUEST,
            "no valid scopes (allowed: chat, chat.cancel)").into_response();
    }
    match auth.create_device_token(&user_id, name, &scopes, body.ttl_secs) {
        Ok((token_id, token)) => {
            tracing::info!("device token created: user={user_id} name={name:?} by={}", caller.id);
            (StatusCode::CREATED, Json(CreatedDeviceToken {
                token_id, token, name: name.to_string(), scopes,
            })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// GET /api/users/{user_id}/device-tokens — list tokens (metadata only).
pub async fn list_device_tokens(
    AuthUser(caller): AuthUser,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Path(user_id):    Path<String>,
) -> axum::response::Response {
    if !may_manage(&caller, &user_id) {
        return (StatusCode::FORBIDDEN, "not your account").into_response();
    }
    match auth.list_device_tokens(&user_id) {
        Ok(rows) => {
            let out: Vec<DeviceTokenSummary> = rows.into_iter().map(|r| DeviceTokenSummary {
                token_id:     r.token_id,
                name:         r.name,
                scopes:       r.scopes,
                created_at:   r.created_at,
                expires_at:   r.expires_at,
                last_used_at: r.last_used_at,
            }).collect();
            Json(out).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// DELETE /api/users/{user_id}/device-tokens/{token_id} — revoke immediately.
pub async fn revoke_device_token(
    AuthUser(caller):      AuthUser,
    Extension(auth):       Extension<Arc<LocalAuthService>>,
    Path((user_id, token_id)): Path<(String, String)>,
) -> axum::response::Response {
    if !may_manage(&caller, &user_id) {
        return (StatusCode::FORBIDDEN, "not your account").into_response();
    }
    match auth.revoke_device_token(&token_id, &user_id) {
        Ok(true)  => {
            tracing::info!("device token revoked: user={user_id} token_id={token_id} by={}", caller.id);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such device token").into_response(),
        Err(e)    => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
