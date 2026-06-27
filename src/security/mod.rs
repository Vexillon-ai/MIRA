// SPDX-License-Identifier: AGPL-3.0-or-later

// src/security/mod.rs
//! Security middleware layer for the MIRA Central Server.
//!
//! # Components
//!
//! | Module       | Responsibility                                         |
//! |--------------|--------------------------------------------------------|
//! | `auth`       | Bearer-token middleware (constant-time comparison)     |
//! | `rate_limit` | Per-IP token-bucket rate limiter                       |
//! | `cors`       | CORS layer configuration                               |
//! | `hmac`       | HMAC-SHA256 body signature verification (Signal)       |
//! | `log`        | Structured request / response logging                  |
//!
//! Telegram secret-token verification is per-account and lives inline in
//! the webhook handler (`server::handlers::telegram::telegram_handler`),
//! reading the secret from each `ChannelAccount` row.
//!
//! # Usage
//!
//! Build a [`SecurityConfig`] from [`crate::config::MiraConfig`] in the Gateway,
//! then pass it to `CentralServer::new()`. Each sub-module exposes a
//! [`tower::Layer`] constructor that consumes (part of) the config.

pub mod auth;
pub mod cors;
pub mod hmac;
pub mod ip_bans;
pub mod log;
pub mod rate_limit;

pub use auth::{AuthLayer, AuthContext};
pub use cors::build_cors_layer;
pub use hmac::HmacLayer;
pub use ip_bans::{IpBanCache, IpBanLayer};
pub use log::RequestLogLayer;
pub use rate_limit::RateLimitLayer;

// ─────────────────────────────────────────────────────────────────────────────

/// Aggregated security configuration passed to the server and middleware.
///
/// Constructed once by the Gateway from [`crate::config::MiraConfig`] and
/// shared (via `Arc` or clone) across Tower layers.
#[derive(Debug, Clone)]
pub struct SecurityConfig {
    /// Master API auth token (Bearer). `None` = auth disabled (dev only).
    pub auth_token:      Option<String>,
    /// Routes that bypass Bearer auth entirely (e.g. `"/health"`).
    pub public_routes:   Vec<&'static str>,
    /// Max requests per minute, per client IP.
    pub rate_limit_rpm:  u32,
    /// CORS allowed origins. Empty = allow none; `["*"]` = allow all.
    pub cors_origins:    Vec<String>,
    /// Signal webhook HMAC-SHA256 key.
    pub signal_hmac_key: Option<String>,
}

impl SecurityConfig {
    /// Build from the global [`crate::config::MiraConfig`].
    pub fn from_mira_config(cfg: &crate::config::MiraConfig) -> Self {
        Self {
            auth_token:      cfg.server.auth_token.clone(),
            // Public routes are reachable without a Bearer token. The
            // auth-flow endpoints (login/refresh) MUST be public —
            // otherwise nobody can ever obtain a token. /logout is also
            // public so a stale / revoked token doesn't permanently
            // block the user from clearing it.
            public_routes:   vec![
                "/health",
                "/avatars/*",
                "/api/artifacts/*",
                "/api/auth/login",
                "/api/auth/refresh",
                "/api/auth/logout",
                // Q2 #11 — SSO/OIDC: provider list (login page is
                // unauthenticated), the authorize redirect, and the IdP
                // callback (carries code+state, never a Bearer token).
                "/api/auth/oidc/*",
                // Q2 #11 — self-service onboarding: signup, the open-signup
                // policy probe, and invite validation are all pre-login.
                "/api/auth/signup",
                "/api/auth/signup/config",
                "/api/auth/invite",
                // 0.282.0 — QR device pairing: a phone claims a pairing the
                // logged-in web session started, exchanging the single-use
                // secret for a token pair. No Bearer token yet (that's the
                // whole point); /start + /{id}/status stay behind auth via
                // the AuthUser extractor.
                "/api/auth/pairing/claim",
                // Q1.7 — landing page + public waitlist signup. The
                // landing page is served from /landing/* and posts to
                // /api/waitlist/signup; the admin read/export/delete
                // endpoints under /api/admin/waitlist stay behind auth.
                "/landing",
                "/landing/*",
                "/api/waitlist/signup",
                // Q2 #8 E4 — OAuth callback the provider hits with
                // code+state. The state token is what binds it back
                // to a user + account; the route itself must be
                // reachable without a Bearer token.
                "/api/email/oauth/callback",
                // Q2 #8 E6 — webhook ingest from hosted-mail
                // providers (Postmark/Resend/Mailgun). Per-account
                // secret in the path is what authenticates.
                "/webhook/email/*",
            ],
            rate_limit_rpm:  cfg.security.rate_limit_rpm,
            cors_origins:    cfg.security.cors_allowed_origins.clone(),
            signal_hmac_key: cfg.channels.signal.hmac_key.clone(),
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            auth_token:      None,
            // Public routes are reachable without a Bearer token. The
            // auth-flow endpoints (login/refresh) MUST be public —
            // otherwise nobody can ever obtain a token. /logout is also
            // public so a stale / revoked token doesn't permanently
            // block the user from clearing it.
            public_routes:   vec![
                "/health",
                "/avatars/*",
                "/api/artifacts/*",
                "/api/auth/login",
                "/api/auth/refresh",
                "/api/auth/logout",
                // Q2 #11 — SSO/OIDC: provider list (login page is
                // unauthenticated), the authorize redirect, and the IdP
                // callback (carries code+state, never a Bearer token).
                "/api/auth/oidc/*",
                // Q2 #11 — self-service onboarding: signup, the open-signup
                // policy probe, and invite validation are all pre-login.
                "/api/auth/signup",
                "/api/auth/signup/config",
                "/api/auth/invite",
                // 0.282.0 — QR device pairing: a phone claims a pairing the
                // logged-in web session started, exchanging the single-use
                // secret for a token pair. No Bearer token yet (that's the
                // whole point); /start + /{id}/status stay behind auth via
                // the AuthUser extractor.
                "/api/auth/pairing/claim",
                // Q1.7 — landing page + public waitlist signup. The
                // landing page is served from /landing/* and posts to
                // /api/waitlist/signup; the admin read/export/delete
                // endpoints under /api/admin/waitlist stay behind auth.
                "/landing",
                "/landing/*",
                "/api/waitlist/signup",
                // Q2 #8 E4 — OAuth callback the provider hits with
                // code+state. The state token is what binds it back
                // to a user + account; the route itself must be
                // reachable without a Bearer token.
                "/api/email/oauth/callback",
                // Q2 #8 E6 — webhook ingest from hosted-mail
                // providers (Postmark/Resend/Mailgun). Per-account
                // secret in the path is what authenticates.
                "/webhook/email/*",
            ],
            rate_limit_rpm:  60,
            cors_origins:    vec![],
            signal_hmac_key: None,
        }
    }
}
