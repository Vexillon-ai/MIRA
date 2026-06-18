// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/signup.rs
//! Self-service onboarding (Q2 #11): admin invite links + public signup +
//! pending-approval management.
//!
//! - Admin: mint / list / revoke invites; list + approve pending accounts.
//! - Public: read the signup policy, validate an invite token, and sign up
//!   (with an invite → active account + auto-login; open signup → active or
//!   pending depending on `auth.signup.require_approval`).
//!
//! Public routes are whitelisted in `SecurityConfig.public_routes`.

use std::sync::Arc;

use axum::{
    extract::{Json, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::{Deserialize, Serialize};

use crate::auth::models::{NewUser, Role};
use crate::auth::{AdminUser, LocalAuthService};
use crate::server::handlers::auth::set_refresh_cookie;
use crate::server::handlers::users::UserResponse;
use crate::web::LiveConfig;
use crate::MiraError;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn err_resp(e: MiraError) -> axum::response::Response {
    match e {
        MiraError::NotFound(m)  => (StatusCode::NOT_FOUND, m).into_response(),
        MiraError::Forbidden    => StatusCode::FORBIDDEN.into_response(),
        MiraError::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        MiraError::AuthError(m) => (StatusCode::BAD_REQUEST, m).into_response(),
        _                       => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Admin: invites ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateInviteRequest {
    #[serde(default)]
    pub role:            Option<String>,
    #[serde(default)]
    pub email_hint:      Option<String>,
    #[serde(default)]
    pub max_uses:        Option<i64>,
    /// Hours until the invite expires. Omitted = no expiry.
    #[serde(default)]
    pub expires_in_hours: Option<i64>,
}

#[derive(Serialize)]
pub struct CreateInviteResponse {
    pub id:    String,
    /// The raw token — returned ONCE; only its hash is stored.
    pub token: String,
    /// Ready-to-share signup link.
    pub url:   String,
    pub role:  String,
}

// POST /api/invites
pub async fn create_invite(
    AdminUser(admin): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Json(req): Json<CreateInviteRequest>,
) -> impl IntoResponse {
    let role = match req.role.as_deref().unwrap_or("user") {
        "admin" => "admin",
        _       => "user",
    };
    let max_uses = req.max_uses.unwrap_or(1).max(1);
    let expires_at = req.expires_in_hours.map(|h| {
        now_ms() + h.max(1) * 3600 * 1000
    });

    match auth.create_invite(&admin.id, role, req.email_hint.as_deref(), max_uses, expires_at) {
        Ok((inv, token)) => {
            let url = format!("/signup?invite={token}");
            (StatusCode::CREATED, axum::Json(CreateInviteResponse {
                id: inv.id, token, url, role: inv.role,
            })).into_response()
        }
        Err(e) => err_resp(e),
    }
}

// GET /api/invites
pub async fn list_invites(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
) -> impl IntoResponse {
    match auth.list_invites() {
        Ok(list) => axum::Json(list).into_response(),
        Err(e)   => err_resp(e),
    }
}

// DELETE /api/invites/{id}
pub async fn revoke_invite(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match auth.revoke_invite(&id) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_resp(e),
    }
}

// ── Admin: pending approvals ────────────────────────────────────────────────

// GET /api/admin/users/pending
pub async fn list_pending(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
) -> impl IntoResponse {
    match auth.list_pending_users() {
        Ok(users) => axum::Json(
            users.into_iter().map(UserResponse::from).collect::<Vec<_>>()
        ).into_response(),
        Err(e) => err_resp(e),
    }
}

// POST /api/users/{id}/approve
pub async fn approve_user(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match auth.set_user_approved(&id, true) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_resp(e),
    }
}

// ── Admin: session revocation ───────────────────────────────────────────────

#[derive(Serialize)]
pub struct RevokeSessionsResponse {
    /// Live sessions revoked. The user can't refresh once their current
    /// access token expires (≤15 min) → signed out everywhere.
    pub revoked: i64,
}

// POST /api/users/{id}/revoke-sessions
pub async fn revoke_user_sessions(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match auth.revoke_all_sessions(&id) {
        Ok(n)  => axum::Json(RevokeSessionsResponse { revoked: n }).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── Public: signup policy + invite validation ───────────────────────────────

#[derive(Serialize)]
pub struct SignupConfigResponse {
    /// Open (un-invited) signup allowed?
    pub open_signup:      bool,
    /// Will an open signup need admin approval before it can log in?
    pub require_approval: bool,
}

// GET /api/auth/signup/config
pub async fn signup_config(
    Extension(live): Extension<Arc<LiveConfig>>,
) -> impl IntoResponse {
    let cfg = live.get().await;
    axum::Json(SignupConfigResponse {
        open_signup:      cfg.auth.signup.enabled,
        require_approval: cfg.auth.signup.require_approval,
    }).into_response()
}

#[derive(Deserialize)]
pub struct InviteTokenQuery {
    pub token: String,
}

#[derive(Serialize)]
pub struct InviteInfoResponse {
    pub valid:      bool,
    pub role:       Option<String>,
    pub email_hint: Option<String>,
}

// GET /api/auth/invite?token=…
pub async fn invite_info(
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Query(q): Query<InviteTokenQuery>,
) -> impl IntoResponse {
    match auth.find_invite_by_token(&q.token) {
        Ok(Some(inv)) if inv.is_redeemable(now_ms()) =>
            axum::Json(InviteInfoResponse {
                valid: true, role: Some(inv.role), email_hint: inv.email_hint,
            }).into_response(),
        Ok(_)  => axum::Json(InviteInfoResponse { valid: false, role: None, email_hint: None }).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── Public: signup ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SignupRequest {
    pub username:     String,
    pub password:     String,
    #[serde(default)]
    pub email:        Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Present → invite-based signup; absent → open signup.
    #[serde(default)]
    pub invite_token: Option<String>,
}

#[derive(Serialize)]
pub struct SignupResponse {
    /// "active" (logged in — `access_token`/`user` set) or "pending".
    pub status:       String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user:         Option<UserResponse>,
}

fn valid_password(p: &str) -> bool { p.chars().count() >= 8 }

fn domain_allowed(email: Option<&str>, allowed: &[String]) -> bool {
    if allowed.is_empty() { return true; }
    match email.and_then(|e| e.rsplit_once('@')) {
        Some((_, d)) => allowed.iter().any(|a| a.trim().eq_ignore_ascii_case(d)),
        None => false,
    }
}

// POST /api/auth/signup
pub async fn signup(
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Extension(live): Extension<Arc<LiveConfig>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SignupRequest>,
) -> impl IntoResponse {
    let username = req.username.trim().to_string();
    if username.is_empty() {
        return (StatusCode::BAD_REQUEST, "Username is required.").into_response();
    }
    if !valid_password(&req.password) {
        return (StatusCode::BAD_REQUEST, "Password must be at least 8 characters.").into_response();
    }

    let cfg = live.get().await;

    // Resolve role + whether the new account is immediately active.
    let (role_str, approved): (String, bool) = if let Some(token) = req.invite_token.as_deref() {
        // Invite path — the admin vouched, so the account is active.
        match auth.redeem_invite(token) {
            Ok(inv) => (inv.role, true),
            Err(MiraError::Unauthorized) =>
                return (StatusCode::BAD_REQUEST, "This invite link is invalid.").into_response(),
            Err(e) => return err_resp(e),
        }
    } else {
        // Open signup — only if enabled; honour domain allow-list + approval.
        let s = &cfg.auth.signup;
        if !s.enabled {
            return (StatusCode::FORBIDDEN, "Sign-ups are invite-only. Ask an administrator for an invite link.").into_response();
        }
        if !domain_allowed(req.email.as_deref(), &s.allowed_domains) {
            return (StatusCode::BAD_REQUEST, "Your email domain is not permitted to sign up.").into_response();
        }
        let role = if s.default_role.eq_ignore_ascii_case("admin") { "admin".to_string() } else { "user".to_string() };
        (role, !s.require_approval)
    };

    let role = if role_str == "admin" { Role::Admin } else { Role::User };
    let new = NewUser {
        username,
        display_name: req.display_name.clone(),
        email:        req.email.clone(),
        password:     req.password.clone(),
        role,
    };

    let user = match auth.create_user_with_approval(new, approved) {
        Ok(u)  => u,
        Err(MiraError::AuthError(m)) => return (StatusCode::BAD_REQUEST, m).into_response(),
        Err(e) => return err_resp(e),
    };

    if !approved {
        return (StatusCode::ACCEPTED, axum::Json(SignupResponse {
            status: "pending".into(), access_token: None, user: None,
        })).into_response();
    }

    // Active → auto-login (same session machinery as a password login).
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    match auth.issue_session(&user, user_agent, None) {
        Ok(pair) => {
            let cookie = set_refresh_cookie(&pair.refresh_token, 7 * 24 * 3600);
            let body = axum::Json(SignupResponse {
                status:       "active".into(),
                access_token: Some(pair.access_token),
                user:         Some(UserResponse::from(user)),
            });
            let mut resp = body.into_response();
            resp.headers_mut().insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
            resp
        }
        Err(e) => err_resp(e),
    }
}
