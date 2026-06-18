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

use crate::auth::local::LocalAuthService;
use crate::auth::models::{Role, User};

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

    Ok(AuthUser(user))
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
    let AuthUser(user) = extract_auth_user(parts)?;

    if user.role != Role::Admin {
        return Err(AuthError(StatusCode::FORBIDDEN, "Admin access required").into_response());
    }

    Ok(AdminUser(user))
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
