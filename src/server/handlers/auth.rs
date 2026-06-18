// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/auth.rs
//! Auth endpoints: login, logout, refresh, me.

use std::sync::Arc;

use axum::{
    extract::Json,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension,
};
use serde::{Deserialize, Serialize};

use crate::auth::{AuthUser, LocalAuthService};
use crate::server::handlers::users::UserResponse;

// ── DTOs ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub user:         UserResponse,
}

#[derive(Deserialize)]
pub struct RefreshRequest {
    // Refresh token can come from cookie or body.
    pub refresh_token: Option<String>,
}

// ── Cookie helpers ────────────────────────────────────────────────────────────

const COOKIE_NAME: &str = "mira_refresh";

pub(crate) fn set_refresh_cookie(token: &str, max_age: i64) -> String {
    format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/api/auth; Max-Age={}",
        COOKIE_NAME, token, max_age
    )
}

fn clear_refresh_cookie() -> String {
    format!(
        "{}=; HttpOnly; SameSite=Strict; Path=/api/auth; Max-Age=0",
        COOKIE_NAME
    )
}

fn extract_refresh_from_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie_hdr = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())?;

    cookie_hdr.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix(COOKIE_NAME)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|v| v.to_owned())
    })
}

// ── POST /api/auth/login ──────────────────────────────────────────────────────

pub async fn login_handler(
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Extension(ldap): Extension<Arc<crate::auth::ldap::LdapService>>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Local auth first — keeps the bootstrap admin + any local account working
    // even when LDAP is on or the directory is down.
    // We don't extract real IP at this layer (no ConnectInfo in extension here).
    let outcome = auth
        .login(&req.username, &req.password, user_agent.as_deref(), None)
        .await;

    let result = match outcome {
        Ok(pair_user) => Ok(pair_user),
        // A correct local password on an unapproved account — don't fall
        // through to LDAP; surface the approval state.
        Err(crate::MiraError::PendingApproval) => {
            return (StatusCode::FORBIDDEN, "Your account is awaiting administrator approval.").into_response();
        }
        // Local rejected — try the directory (Q2 #11) if enabled.
        Err(_) if ldap.is_enabled() => {
            ldap_login(&ldap, &auth, &req.username, &req.password, user_agent.as_deref()).await
        }
        Err(e) => Err(e),
    };

    match result {
        Err(_) => (StatusCode::UNAUTHORIZED, "Invalid credentials").into_response(),
        Ok((pair, user)) => {
            let cookie = set_refresh_cookie(&pair.refresh_token, 7 * 24 * 3600);
            let body = axum::Json(LoginResponse {
                access_token: pair.access_token,
                user:         UserResponse::from(user),
            });
            let mut resp = body.into_response();
            resp.headers_mut().insert(
                axum::http::header::SET_COOKIE,
                cookie.parse().unwrap(),
            );
            resp
        }
    }
}

/// Authenticate against LDAP and resolve/provision the MIRA user, returning a
/// fresh session. Identity is bound by `(ldap:<url>, dn)`; we link an existing
/// account matched by username or email, else auto-provision when configured.
async fn ldap_login(
    ldap: &crate::auth::ldap::LdapService,
    auth: &LocalAuthService,
    username: &str,
    password: &str,
    user_agent: Option<&str>,
) -> Result<(crate::auth::TokenPair, crate::auth::User), crate::MiraError> {
    use crate::MiraError;
    let ident = ldap.authenticate(username, password).await?;
    let issuer = ldap.realm();

    let user = if let Some(u) = auth.find_user_by_identity(&issuer, &ident.dn)? {
        u
    } else if let Some(u) = auth.find_by_username(&ident.username)? {
        auth.link_identity(&issuer, &ident.dn, &u.id, "ldap")?;
        u
    } else if let Some(u) = ident.email.as_deref().and_then(|e| auth.find_by_email(e).ok().flatten()) {
        auth.link_identity(&issuer, &ident.dn, &u.id, "ldap")?;
        u
    } else {
        let role = ldap.auto_provision_role(ident.email.as_deref()).map_err(MiraError::AuthError)?;
        let u = auth.create_sso_user(&ident.username, ident.email.as_deref(), ident.display_name.as_deref(), role)?;
        auth.link_identity(&issuer, &ident.dn, &u.id, "ldap")?;
        u
    };

    if !user.is_active {
        return Err(MiraError::Unauthorized);
    }
    let pair = auth.issue_session(&user, user_agent, None)?;
    Ok((pair, user))
}

// ── POST /api/auth/logout ─────────────────────────────────────────────────────

pub async fn logout_handler(
    Extension(auth): Extension<Arc<LocalAuthService>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(raw) = extract_refresh_from_cookie(&headers) {
        let _ = auth.logout(&raw);
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        clear_refresh_cookie().parse().unwrap(),
    );
    resp
}

// ── POST /api/auth/refresh ────────────────────────────────────────────────────

pub async fn refresh_handler(
    Extension(auth): Extension<Arc<LocalAuthService>>,
    headers: HeaderMap,
    body: Option<Json<RefreshRequest>>,
) -> impl IntoResponse {
    // Try cookie first, then request body.
    let raw = extract_refresh_from_cookie(&headers)
        .or_else(|| body.and_then(|b| b.refresh_token.clone()));

    let raw = match raw {
        Some(t) => t,
        None    => return (StatusCode::UNAUTHORIZED, "No refresh token").into_response(),
    };

    match auth.refresh(&raw).await {
        Err(_) => (StatusCode::UNAUTHORIZED, "Invalid or expired refresh token").into_response(),
        Ok((pair, _user)) => {
            let cookie = set_refresh_cookie(&pair.refresh_token, 7 * 24 * 3600);
            let body = axum::Json(serde_json::json!({ "access_token": pair.access_token }));
            let mut resp = body.into_response();
            resp.headers_mut().insert(
                axum::http::header::SET_COOKIE,
                cookie.parse().unwrap(),
            );
            resp
        }
    }
}

// ── GET /api/auth/me ──────────────────────────────────────────────────────────

pub async fn me_handler(AuthUser(user): AuthUser) -> impl IntoResponse {
    axum::Json(UserResponse::from(user)).into_response()
}

// ── SSO / OIDC (Q2 #11) ─────────────────────────────────────────────────────
//
// Three public endpoints. /providers feeds the login buttons; /authorize
// redirects the browser to the IdP; /callback finishes the exchange, maps the
// identity to a MIRA user, sets the same `mira_refresh` cookie as a password
// login, and 302s to the SPA — whose boot-time refresh then mints the access
// token (so no token ever rides the redirect URL).

use crate::auth::oidc::{username_seed, OidcService, ProvisionDecision};

#[derive(Serialize)]
pub struct OidcProviderButton {
    pub id:           String,
    pub display_name: String,
}

#[derive(Deserialize)]
pub struct AuthorizeQuery {
    pub provider: String,
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code:  Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Redirect the browser back to the login screen with a surfaced error.
fn login_error_redirect(msg: &str) -> axum::response::Response {
    let encoded = urlencode(msg);
    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut().insert(
        axum::http::header::LOCATION,
        format!("/login?sso_error={encoded}").parse().unwrap(),
    );
    resp
}

/// Minimal percent-encoding for the query value (we control the message text).
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// GET /api/auth/oidc/providers
pub async fn oidc_providers_handler(
    Extension(oidc): Extension<Arc<OidcService>>,
) -> impl IntoResponse {
    let list: Vec<OidcProviderButton> = oidc
        .provider_buttons()
        .into_iter()
        .map(|(id, display_name)| OidcProviderButton { id, display_name })
        .collect();
    axum::Json(list).into_response()
}

// GET /api/auth/oidc/authorize?provider=<id>
pub async fn oidc_authorize_handler(
    Extension(oidc): Extension<Arc<OidcService>>,
    axum::extract::Query(q): axum::extract::Query<AuthorizeQuery>,
) -> impl IntoResponse {
    match oidc.begin(&q.provider).await {
        Ok(url) => {
            let mut resp = StatusCode::FOUND.into_response();
            resp.headers_mut().insert(
                axum::http::header::LOCATION,
                url.parse().unwrap(),
            );
            resp
        }
        Err(e) => login_error_redirect(&format!("Could not start sign-in: {e}")),
    }
}

// GET /api/auth/oidc/callback?code=&state=
pub async fn oidc_callback_handler(
    Extension(oidc): Extension<Arc<OidcService>>,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<CallbackQuery>,
) -> impl IntoResponse {
    // The IdP can redirect back with an error (user denied consent, etc.).
    if let Some(err) = q.error.as_deref() {
        let detail = q.error_description.as_deref().unwrap_or(err);
        return login_error_redirect(&format!("Sign-in was not completed: {detail}"));
    }
    let (Some(code), Some(state)) = (q.code.as_deref(), q.state.as_deref()) else {
        return login_error_redirect("Sign-in callback was missing its code or state.");
    };

    let (provider_id, claims) = match oidc.complete(code, state).await {
        Ok(v) => v,
        Err(_) => return login_error_redirect("Sign-in could not be verified (expired or invalid)."),
    };

    // Look up an existing account: stable identity first, then email.
    let by_identity = auth.find_user_by_identity(&claims.issuer, &claims.sub).ok().flatten();
    let by_email = claims
        .email
        .as_deref()
        .and_then(|e| auth.find_by_email(e).ok().flatten());

    let decision = oidc.decide(
        &provider_id,
        &claims,
        by_identity.is_some(),
        by_email.is_some(),
    );

    let user = match decision {
        ProvisionDecision::UseExisting => by_identity.expect("matched_by_identity"),
        ProvisionDecision::LinkExisting => {
            let u = by_email.expect("matched_by_email");
            if let Err(e) = auth.link_identity(&claims.issuer, &claims.sub, &u.id, &provider_id) {
                return login_error_redirect(&format!("Could not link your account: {e}"));
            }
            u
        }
        ProvisionDecision::Create { role } => {
            let created = auth.create_sso_user(
                &username_seed(&claims),
                claims.email.as_deref(),
                claims.name.as_deref(),
                role,
            );
            match created {
                Ok(u) => {
                    if let Err(e) = auth.link_identity(&claims.issuer, &claims.sub, &u.id, &provider_id) {
                        return login_error_redirect(&format!("Account created but linking failed: {e}"));
                    }
                    u
                }
                Err(e) => return login_error_redirect(&format!("Could not create your account: {e}")),
            }
        }
        ProvisionDecision::Reject(msg) => return login_error_redirect(&msg),
    };

    if !user.is_active {
        return login_error_redirect("Your account is disabled. Contact an administrator.");
    }

    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());

    match auth.issue_session(&user, user_agent, None) {
        Ok(pair) => {
            // Set the refresh cookie + bounce to the SPA; its boot refresh
            // mints the access token. Token never touches the URL.
            let cookie = set_refresh_cookie(&pair.refresh_token, 7 * 24 * 3600);
            let mut resp = StatusCode::FOUND.into_response();
            resp.headers_mut().insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
            resp.headers_mut().insert(axum::http::header::LOCATION, "/?sso=ok".parse().unwrap());
            resp
        }
        Err(e) => login_error_redirect(&format!("Could not start your session: {e}")),
    }
}
