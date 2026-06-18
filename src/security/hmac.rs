// SPDX-License-Identifier: AGPL-3.0-or-later

// src/security/hmac.rs
//! HMAC-SHA256 body signature verification middleware (Signal webhook).
//!
//! When `signal_hmac_key` is configured, the middleware:
//! 1. Buffers the full request body.
//! 2. Computes `HMAC-SHA256(key, body)`.
//! 3. Compares the hex digest against the `X-Signal-Signature` header using
//!    constant-time comparison to prevent timing oracles.
//! 4. Returns `401` on mismatch; replaces the body with a buffered copy on
//!    success so downstream handlers can still read it.
//!
//! When `signal_hmac_key` is `None`, the middleware passes through with a
//! startup warning (logged once at layer construction time).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tower::{Layer, Service};
use tracing::warn;

type HmacSha256 = Hmac<Sha256>;

// ─────────────────────────────────────────────────────────────────────────────

/// Tower `Layer` that wraps services with HMAC-SHA256 body verification.
#[derive(Clone)]
pub struct HmacLayer {
    key: Option<Arc<Vec<u8>>>,
}

impl HmacLayer {
    /// `key` — raw bytes of the shared HMAC secret. `None` builds a
    /// pass-through layer (legacy / test convenience). The router no
    /// longer mounts the Signal webhook at all when no key is set, so
    /// in production this constructor only sees `Some(...)`.
    pub fn new(key: Option<&str>) -> Self {
        Self {
            key: key.map(|k| Arc::new(k.as_bytes().to_vec())),
        }
    }
}

impl<S> Layer<S> for HmacLayer {
    type Service = HmacService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HmacService {
            inner,
            key: self.key.clone(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Tower `Service` that performs HMAC body verification.
#[derive(Clone)]
pub struct HmacService<S> {
    inner: S,
    key:   Option<Arc<Vec<u8>>>,
}

/// Compute `HMAC-SHA256(key, data)` and return the lowercase hex string.
pub fn compute_hmac(key: &[u8], data: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key)
        .expect("HMAC accepts any key length");
    mac.update(data);
    let result = mac.finalize().into_bytes();
    hex::encode(result)
}

fn fixed_response<E: Send + 'static>(status: StatusCode, body: &'static str)
    -> Pin<Box<dyn Future<Output = Result<Response<Body>, E>> + Send>>
{
    Box::pin(std::future::ready(Ok(
        Response::builder().status(status).body(Body::from(body)).unwrap()
    )))
}

impl<S> Service<Request<Body>> for HmacService<S>
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

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let key = self.key.clone();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let Some(key) = key else {
                // No key configured — pass through.
                return inner.call(req).await;
            };

            // Extract signature header before consuming the request.
            let sig_header = req
                .headers()
                .get("x-signal-signature")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);

            let Some(sig) = sig_header else {
                warn!("HmacLayer: missing X-Signal-Signature header");
                return fixed_response(StatusCode::UNAUTHORIZED, "Missing signature").await;
            };

            // Buffer body so we can both verify and forward it.
            let (parts, body) = req.into_parts();
            let body_bytes: Bytes = match body.collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(_) => {
                    return fixed_response(StatusCode::BAD_REQUEST, "Failed to read body").await;
                }
            };

            let expected = compute_hmac(&key, &body_bytes);
            let ok = bool::from(expected.as_bytes().ct_eq(sig.as_bytes()));

            if !ok {
                warn!("HmacLayer: HMAC mismatch");
                return fixed_response(StatusCode::UNAUTHORIZED, "Invalid signature").await;
            }

            // Reassemble request with the buffered body.
            let req = Request::from_parts(parts, Body::from(body_bytes));
            inner.call(req).await
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[test]
    fn compute_hmac_is_deterministic() {
        let a = compute_hmac(b"key", b"body");
        let b = compute_hmac(b"key", b"body");
        assert_eq!(a, b);
    }

    #[test]
    fn compute_hmac_differs_on_different_input() {
        let a = compute_hmac(b"key", b"body1");
        let b = compute_hmac(b"key", b"body2");
        assert_ne!(a, b);
    }

    async fn run(layer: HmacLayer, req: Request<Body>) -> u16 {
        let svc = tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder().status(200).body(Body::empty()).unwrap()
            )
        });
        let mut svc = layer.layer(svc);
        svc.ready().await.unwrap().call(req).await.unwrap().status().as_u16()
    }

    #[tokio::test]
    async fn valid_signature_passes() {
        let key = "testsecret";
        let body = b"hello world";
        let sig = compute_hmac(key.as_bytes(), body);
        let layer = HmacLayer::new(Some(key));
        let req = Request::builder()
            .header("x-signal-signature", sig)
            .body(Body::from(body.as_ref()))
            .unwrap();
        assert_eq!(run(layer, req).await, 200);
    }

    #[tokio::test]
    async fn invalid_signature_returns_401() {
        let layer = HmacLayer::new(Some("testsecret"));
        let req = Request::builder()
            .header("x-signal-signature", "badsignature")
            .body(Body::from("hello world"))
            .unwrap();
        assert_eq!(run(layer, req).await, 401);
    }

    #[tokio::test]
    async fn no_key_passes_through() {
        let layer = HmacLayer::new(None);
        let req = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(run(layer, req).await, 200);
    }
}
