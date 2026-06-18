// SPDX-License-Identifier: AGPL-3.0-or-later

// src/security/auth.rs
//! Bearer-token authentication middleware.
//!
//! Accepts EITHER a static `auth_token` (typical for scripts / webhooks)
//! OR a valid JWT issued by `LocalAuthService` (the browser UI's normal
//! flow). Constant-time comparison via `subtle` prevents timing oracles
//! on the static-token path; JWT verification is delegated to
//! `LocalAuthService::verify_token`.
//!
//! # Response behaviour
//! * Public route → pass through unconditionally.
//! * Dev mode (no static token AND no auth service wired) → pass
//!   through with `authenticated: false`.
//! * Otherwise the request must carry a Bearer token (or `?token=`
//!   query param, for SSE endpoints) that matches the static token
//!   OR verifies as a JWT. Anything else returns `401 Unauthorized`.
//!
//! On success the request gains an `AuthContext` extension so
//! downstream handlers can tell whether the request was authenticated
//! by the static token (no associated user) or by a JWT (a user id is
//! available via the existing `AuthUser` extractor on routes that
//! need it).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use subtle::ConstantTimeEq;
use tower::{Layer, Service};
use tracing::debug;

use crate::auth::LocalAuthService;

// ─────────────────────────────────────────────────────────────────────────────

/// Authentication context injected into request extensions after auth passes.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// Whether the request carried a valid Bearer token.
    pub authenticated: bool,
}

// ─────────────────────────────────────────────────────────────────────────────

/// Tower `Layer` that wraps services with Bearer-token auth.
#[derive(Clone)]
pub struct AuthLayer {
    token:         Option<Arc<String>>,
    auth_service:  Option<Arc<LocalAuthService>>,
    public_routes: Arc<Vec<&'static str>>,
}

impl AuthLayer {
    pub fn new(token: Option<String>, public_routes: Vec<&'static str>) -> Self {
        Self {
            token:         token.map(Arc::new),
            auth_service:  None,
            public_routes: Arc::new(public_routes),
        }
    }

    /// Enable JWT validation as a second accepted credential. With this
    /// wired, the web client (which sends JWTs) and a static token
    /// (which scripts can use) both pass — the warning about "open API
    /// in dev mode" no longer applies.
    pub fn with_auth_service(mut self, auth: Arc<LocalAuthService>) -> Self {
        self.auth_service = Some(auth);
        self
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            token:         self.token.clone(),
            auth_service:  self.auth_service.clone(),
            public_routes: self.public_routes.clone(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Tower `Service` that performs Bearer-token auth.
#[derive(Clone)]
pub struct AuthService<S> {
    inner:         S,
    token:         Option<Arc<String>>,
    auth_service:  Option<Arc<LocalAuthService>>,
    public_routes: Arc<Vec<&'static str>>,
}

fn unauthorized<E: Send + 'static>() -> Pin<Box<dyn Future<Output = Result<Response<Body>, E>> + Send>> {
    Box::pin(std::future::ready(Ok(
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::from("Unauthorized"))
            .unwrap(),
    )))
}

impl<S> Service<Request<Body>> for AuthService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response<Body>;
    type Error    = S::Error;
    type Future   = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let path = req.uri().path().to_owned();

        // The protected surface is `/api/*`. Everything else — static SPA
        // assets (/favicon.svg, /assets/*, /manifest.json), the SPA's deep
        // links served from index.html (/, /login, /chat/*, /wiki/*), and
        // public mount points like /health — is unauthenticated by
        // default. Without this, setting `server.auth_token` 401s every
        // browser request for a static file and silently relies on the
        // browser's HTTP cache to keep the UI alive — the next JS bundle
        // hash change would brick the page.
        if !path.starts_with("/api/") {
            req.extensions_mut().insert(AuthContext { authenticated: false });
            let fut = self.inner.call(req);
            return Box::pin(async move { fut.await });
        }

        // Inside `/api/*`, the configured `public_routes` list carves
        // out the exceptions (login, refresh, artifacts, etc.) so users
        // can obtain a token and the artifact server stays open.
        // Entries ending in `/*` are treated as prefix matches.
        let is_public = self.public_routes.iter().any(|r| {
            if let Some(prefix) = r.strip_suffix("/*") {
                path.starts_with(prefix)
            } else {
                *r == path
            }
        });

        if is_public {
            req.extensions_mut().insert(AuthContext { authenticated: false });
            let fut = self.inner.call(req);
            return Box::pin(async move { fut.await });
        }

        // Dev mode: no static token AND no JWT auth service wired.
        // Pass everything through. Tests and bare-bones builds rely on
        // this to exercise handlers without seeding a user database.
        if self.token.is_none() && self.auth_service.is_none() {
            req.extensions_mut().insert(AuthContext { authenticated: true });
            let fut = self.inner.call(req);
            return Box::pin(async move { fut.await });
        }

        // Extract Bearer token from the Authorization header, falling
        // back to the `?token=` query param (used by EventSource for
        // SSE endpoints where headers can't be set). Mirrors the
        // extractor logic in `auth::middleware::extract_bearer_token`
        // so the layer and the per-handler extractor agree on what a
        // "valid" credential carrier looks like.
        let provided = extract_bearer(&req);

        let static_ok = match (&self.token, &provided) {
            (Some(expected), Some(tok)) =>
                bool::from(tok.as_bytes().ct_eq(expected.as_bytes())),
            _ => false,
        };
        let jwt_ok = !static_ok && match (&self.auth_service, &provided) {
            (Some(svc), Some(tok)) => svc.verify_token(tok).is_ok(),
            _ => false,
        };

        if !static_ok && !jwt_ok {
            // These two lines are routine for a JWT-auth deployment:
            //   - "invalid Bearer token" fires every ~15 minutes per
            //     active client when the access token's TTL expires
            //     and the SPA hasn't yet hit /api/auth/refresh.
            //   - "missing Authorization header" fires on the brief
            //     startup race where a query mounts before the auth
            //     store has hydrated from localStorage.
            // Both are recovered automatically by the client and
            // surface a 401 to the handler. Real attack patterns
            // (brute-force login) have their own dedicated logging
            // via auth::record_failed_login; emitting WARN here just
            // adds noise to the watchdog feed.
            if provided.is_none() {
                debug!("Auth: missing Authorization header for {}", path);
            } else {
                debug!("Auth: invalid Bearer token for {}", path);
            }
            return unauthorized();
        }

        req.extensions_mut().insert(AuthContext { authenticated: true });
        let fut = self.inner.call(req);
        Box::pin(async move { fut.await })
    }
}

/// Mirror of the bearer-extraction logic in `auth::middleware` so the
/// layer and the per-handler extractor stay in sync on what counts as a
/// valid carrier. Header first, then `?token=` query param.
fn extract_bearer(req: &Request<Body>) -> Option<String> {
    if let Some(t) = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Some(t.to_owned());
    }
    let q = req.uri().query()?;
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("token=") {
            return Some(v.to_owned());
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::AUTHORIZATION;
    use tower::ServiceExt;

    async fn run(layer: AuthLayer, req: Request<Body>) -> u16 {
        let svc = tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder().status(200).body(Body::empty()).unwrap()
            )
        });
        let mut svc = layer.layer(svc);
        svc.ready().await.unwrap().call(req).await.unwrap().status().as_u16()
    }

    #[tokio::test]
    async fn public_route_passes_without_token() {
        let layer = AuthLayer::new(Some("secret".to_string()), vec!["/health"]);
        let req = Request::builder().uri("/health").body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 200);
    }

    #[tokio::test]
    async fn correct_token_passes() {
        let layer = AuthLayer::new(Some("mysecret".to_string()), vec![]);
        let req = Request::builder()
            .uri("/api/chat")
            .header(AUTHORIZATION, "Bearer mysecret")
            .body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 200);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let layer = AuthLayer::new(Some("mysecret".to_string()), vec![]);
        let req = Request::builder()
            .uri("/api/chat")
            .header(AUTHORIZATION, "Bearer wrongtoken")
            .body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 401);
    }

    #[tokio::test]
    async fn missing_token_returns_401() {
        let layer = AuthLayer::new(Some("mysecret".to_string()), vec![]);
        let req = Request::builder().uri("/api/chat").body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 401);
    }

    #[tokio::test]
    async fn no_auth_configured_passes_all() {
        let layer = AuthLayer::new(None, vec![]);
        let req = Request::builder().uri("/api/chat").body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 200);
    }

    // ── Dual-mode (JWT) tests ────────────────────────────────────────────

    async fn auth_with_one_user() -> (tempfile::TempDir, Arc<LocalAuthService>, String) {
        use crate::auth::{LocalAuthService, NewUser, Role};
        let dir = tempfile::tempdir().unwrap();
        let svc = Arc::new(LocalAuthService::new(
            &dir.path().join("auth.db"),
            "jwt-test-secret".into(),
            7,
        ).unwrap());
        svc.create_user(NewUser {
            username:     "alice".into(),
            display_name: None,
            email:        None,
            password:     "test-password-1234".into(),
            role:         Role::User,
        }).unwrap();
        let (tokens, _user) = svc.login("alice", "test-password-1234", None, None)
            .await.unwrap();
        (dir, svc, tokens.access_token)
    }

    #[tokio::test]
    async fn jwt_only_mode_accepts_valid_jwt() {
        let (_dir, svc, jwt) = auth_with_one_user().await;
        let layer = AuthLayer::new(None, vec![]).with_auth_service(svc);
        let req = Request::builder()
            .uri("/api/chat")
            .header(AUTHORIZATION, format!("Bearer {jwt}"))
            .body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 200);
    }

    #[tokio::test]
    async fn jwt_only_mode_rejects_missing_token() {
        let (_dir, svc, _jwt) = auth_with_one_user().await;
        let layer = AuthLayer::new(None, vec![]).with_auth_service(svc);
        let req = Request::builder().uri("/api/chat").body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 401);
    }

    #[tokio::test]
    async fn jwt_only_mode_rejects_garbage_token() {
        let (_dir, svc, _jwt) = auth_with_one_user().await;
        let layer = AuthLayer::new(None, vec![]).with_auth_service(svc);
        let req = Request::builder()
            .uri("/api/chat")
            .header(AUTHORIZATION, "Bearer not-a-jwt")
            .body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 401);
    }

    #[tokio::test]
    async fn dual_mode_accepts_either_credential() {
        let (_dir, svc, jwt) = auth_with_one_user().await;
        let layer = AuthLayer::new(Some("static-token".into()), vec![])
            .with_auth_service(svc);

        // Static token works.
        let req_static = Request::builder()
            .uri("/api/chat")
            .header(AUTHORIZATION, "Bearer static-token")
            .body(Body::empty()).unwrap();
        assert_eq!(run(layer.clone(), req_static).await, 200);

        // JWT works on the same layer.
        let req_jwt = Request::builder()
            .uri("/api/chat")
            .header(AUTHORIZATION, format!("Bearer {jwt}"))
            .body(Body::empty()).unwrap();
        assert_eq!(run(layer, req_jwt).await, 200);
    }

    #[tokio::test]
    async fn dual_mode_still_rejects_unrelated_token() {
        let (_dir, svc, _jwt) = auth_with_one_user().await;
        let layer = AuthLayer::new(Some("static-token".into()), vec![])
            .with_auth_service(svc);
        let req = Request::builder()
            .uri("/api/chat")
            .header(AUTHORIZATION, "Bearer guess-this")
            .body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 401);
    }

    #[tokio::test]
    async fn non_api_routes_are_public_even_with_static_token() {
        let layer = AuthLayer::new(Some("static-token".into()), vec![]);
        for uri in ["/favicon.svg", "/index.html", "/assets/main.js", "/", "/login", "/chat/abc"] {
            let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
            assert_eq!(
                run(layer.clone(), req).await, 200,
                "{uri} should be public (non-/api/* route)",
            );
        }
    }

    #[tokio::test]
    async fn api_route_still_requires_credential() {
        let layer = AuthLayer::new(Some("static-token".into()), vec![]);
        let req = Request::builder().uri("/api/chat").body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 401, "/api/* should still be gated");
    }

    #[tokio::test]
    async fn api_public_route_passes_without_credential() {
        let layer = AuthLayer::new(
            Some("static-token".into()),
            vec!["/api/auth/login"],
        );
        let req = Request::builder().uri("/api/auth/login").body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 200);
    }

    #[tokio::test]
    async fn jwt_in_query_param_passes() {
        // SSE / EventSource workaround — header can't be set, so the
        // client appends ?token=<jwt>. Mirror behaviour with AuthUser.
        let (_dir, svc, jwt) = auth_with_one_user().await;
        let layer = AuthLayer::new(None, vec![]).with_auth_service(svc);
        let req = Request::builder()
            .uri(format!("/api/chat/stream?token={jwt}"))
            .body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 200);
    }
}
