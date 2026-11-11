// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/middleware.rs
//! Axum extractors for JWT authentication.

use std::future::Future;
use std::sync::Arc;

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
};

use crate::auth::local::{LocalAuthService, DEVICE_TOKEN_PREFIX, SCOPE_CHAT, SCOPE_CHAT_CANCEL};
use crate::auth::models::{Role, User};

// How a request authenticated: a normal JWT session (full scopes), or a device
// token carrying a specific scope allow-list.
enum AuthKind {
    Jwt,
    Device { scopes: Vec<String> },
}

impl AuthKind {
    // A JWT has every scope; a device token only the scopes it was granted.
    fn has_scope(&self, scope: &str) -> bool {
        match self {
            AuthKind::Jwt => true,
            AuthKind::Device { scopes } => scopes.iter().any(|s| s == scope),
        }
    }
    fn is_device(&self) -> bool { matches!(self, AuthKind::Device { .. }) }
}

// ── Error response ────────────────────────────────────────────────────────────

pub struct AuthError(StatusCode, &'static str);

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

// ── AuthUser extractor ────────────────────────────────────────────────────────

/// Extracts any authenticated (active) user from Bearer token.
pub struct AuthUser(pub User);

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let result = extract_auth_user(parts);
        std::future::ready(result)
    }
}

fn extract_auth_user(parts: &mut Parts) -> Result<AuthUser, Response> {
    let (user, _kind) = resolve_auth(parts)?;
    Ok(AuthUser(user))
}

// Shared resolution for every auth extractor: resolves a Bearer token to its
// user, by either the device-token path (`mdt_` prefix) or the
// JWT path. Returns how the request authenticated so scope-gated extractors can
// enforce device-token scopes.
fn resolve_auth(parts: &mut Parts) -> Result<(User, AuthKind), Response> {
    let auth_service = parts
        .extensions
        .get::<Arc<LocalAuthService>>()
        .cloned()
        .ok_or_else(|| {
            AuthError(StatusCode::INTERNAL_SERVER_ERROR, "Auth service not configured")
                .into_response()
        })?;

    let token = extract_bearer_token(parts).ok_or_else(|| {
        AuthError(StatusCode::UNAUTHORIZED, "Missing or invalid Authorization header")
            .into_response()
    })?;

    // Device-token path. An opaque `mdt_` bearer resolves to
    // its owner via a hashed lookup — never more than the user has, because it
    // IS the user. Expiry/active checks live in `verify_device_token`.
    if token.starts_with(DEVICE_TOKEN_PREFIX) {
        let da = auth_service
            .verify_device_token(token)
            .map_err(|_| AuthError(StatusCode::UNAUTHORIZED, "Invalid or expired device token").into_response())?;
        // Attribution: a structured log line ties the authenticated request to
        // the named device (the device token's audit trail).
        tracing::info!(
            "device-token auth: user={} device={:?} token_id={}",
            da.user.id, da.name, da.token_id,
        );
        return Ok((da.user, AuthKind::Device { scopes: da.scopes }));
    }

    let claims = auth_service
        .verify_token(token)
        .map_err(|_| AuthError(StatusCode::UNAUTHORIZED, "Invalid or expired token").into_response())?;

    let user = auth_service
        .get_user(&claims.sub)
        .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, "DB error").into_response())?
        .ok_or_else(|| AuthError(StatusCode::UNAUTHORIZED, "User not found").into_response())?;

    if !user.is_active {
        return Err(AuthError(StatusCode::UNAUTHORIZED, "Account disabled").into_response());
    }

    // per-user token revocation. A stateless access token stays
    // cryptographically valid until its `exp`; without this check, revoking a
    // user's sessions (or "sign out everywhere") wouldn't stop an already-issued
    // access token. The token embeds the `token_version` current at issue time
    // (`claims.tv`); bumping the user's `token_version` makes every prior token
    // fail this comparison immediately.
    if claims.tv != user.token_version {
        return Err(AuthError(StatusCode::UNAUTHORIZED, "Session revoked").into_response());
    }

    Ok((user, AuthKind::Jwt))
}

// ── AdminUser extractor ───────────────────────────────────────────────────────

/// Extracts an admin user — returns 403 if role != Admin.
pub struct AdminUser(pub User);

impl<S> FromRequestParts<S> for AdminUser
where
    S: Send + Sync,
{
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let result = extract_admin_user(parts);
        std::future::ready(result)
    }
}

fn extract_admin_user(parts: &mut Parts) -> Result<AdminUser, Response> {
    let (user, kind) = resolve_auth(parts)?;

    // Device tokens are scoped to chat; they never reach admin endpoints even
    // for an admin's own device (defence in depth — the minimum scopes are
    // chat/chat.cancel, and admin actions should require a real session).
    if kind.is_device() {
        return Err(AuthError(StatusCode::FORBIDDEN, "Device tokens cannot access admin endpoints").into_response());
    }
    if user.role != Role::Admin {
        return Err(AuthError(StatusCode::FORBIDDEN, "Admin access required").into_response());
    }

    Ok(AdminUser(user))
}

// ── Scope-gated extractors ──────────────────────────────────

/// Authenticates a request for the chat endpoint. A JWT passes; a device token
/// must carry the `chat` scope, else 403.
pub struct ChatAuth(pub User);

impl<S> FromRequestParts<S> for ChatAuth
where
    S: Send + Sync,
{
    type Rejection = Response;
    fn from_request_parts(parts: &mut Parts, _state: &S)
        -> impl Future<Output = Result<Self, Self::Rejection>> + Send
    {
        std::future::ready(extract_scoped(parts, SCOPE_CHAT).map(ChatAuth))
    }
}

/// Authenticates a request for the cancel endpoint. A JWT passes; a device
/// token must carry the `chat.cancel` scope, else 403.
pub struct ChatCancelAuth(pub User);

impl<S> FromRequestParts<S> for ChatCancelAuth
where
    S: Send + Sync,
{
    type Rejection = Response;
    fn from_request_parts(parts: &mut Parts, _state: &S)
        -> impl Future<Output = Result<Self, Self::Rejection>> + Send
    {
        std::future::ready(extract_scoped(parts, SCOPE_CHAT_CANCEL).map(ChatCancelAuth))
    }
}

fn extract_scoped(parts: &mut Parts, scope: &'static str) -> Result<User, Response> {
    let (user, kind) = resolve_auth(parts)?;
    if !kind.has_scope(scope) {
        return Err(AuthError(StatusCode::FORBIDDEN, "Device token lacks the required scope").into_response());
    }
    Ok(user)
}

// ── Helper ────────────────────────────────────────────────────────────────────

fn extract_bearer_token(parts: &Parts) -> Option<&str> {
    if let Some(t) = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Some(t);
    }
    // Query-param fallback for EventSource, which can't set headers.
    // The web client passes `?token=<jwt>` on SSE endpoints. Limited
    // to the query-string parse below — the JWT verifier still gates
    // accepting it, so an attacker with a stolen URL needs a valid
    // signed token, not just any string.
    let query = parts.uri.query()?;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("token=") {
            // Borrowing into `parts.uri.query()` is fine — the &str
            // lives as long as `parts`. URL-decode skipped because
            // JWTs use only [A-Za-z0-9._-].
            return Some(v);
        }
    }
    None
}
