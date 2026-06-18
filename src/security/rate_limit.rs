// SPDX-License-Identifier: AGPL-3.0-or-later

// src/security/rate_limit.rs
//! Per-IP token-bucket rate limiting middleware.
//!
//! Each client IP gets an independent bucket with capacity equal to
//! `rate_limit_rpm` (requests per minute).  Tokens are refilled at a rate of
//! `capacity / 60` tokens per second.  When a bucket is empty the middleware
//! returns `429 Too Many Requests` with a `Retry-After` header.
//!
//! IPs on the allow-list (always includes `127.0.0.1` and `::1`) bypass rate
//! limiting entirely.
//!
//! Bucket state is **in-memory only** — it resets on restart.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use dashmap::DashMap;
use tower::{Layer, Service};
use tracing::warn;

// ─────────────────────────────────────────────────────────────────────────────

/// Token-bucket state for a single IP address.
#[derive(Debug)]
struct Bucket {
    /// Tokens currently available (fractional for smooth refill).
    tokens:           f64,
    /// Maximum tokens (== rate_limit_rpm).
    capacity:         f64,
    /// Tokens added per second.
    refill_per_sec:   f64,
    /// Last time the bucket was accessed.
    last_refill:      Instant,
}

impl Bucket {
    fn new(capacity: u32) -> Self {
        let cap = capacity as f64;
        Self {
            tokens:         cap,
            capacity:       cap,
            refill_per_sec: cap / 60.0,
            last_refill:    Instant::now(),
        }
    }

    /// Attempt to consume one token.  Returns `true` if allowed.
    fn try_consume(&mut self) -> bool {
        let now    = Instant::now();
        let delta  = now.duration_since(self.last_refill).as_secs_f64();
        let refill = delta * self.refill_per_sec;
        self.tokens = (self.tokens + refill).min(self.capacity);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Seconds until one token is available again.
    fn retry_after_secs(&self) -> u64 {
        if self.tokens >= 1.0 { return 0; }
        ((1.0 - self.tokens) / self.refill_per_sec).ceil() as u64
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Shared rate-limit state (one `Arc` per middleware instance).
#[derive(Clone)]
struct RateLimitState {
    buckets:         Arc<DashMap<IpAddr, Bucket>>,
    capacity:        u32,
    allowlist:       Arc<Vec<IpAddr>>,
}

impl RateLimitState {
    fn new(rate_limit_rpm: u32, extra_allowlist: Vec<IpAddr>) -> Self {
        let mut allowlist = vec![
            "127.0.0.1".parse().unwrap(),
            "::1".parse().unwrap(),
        ];
        allowlist.extend(extra_allowlist);
        Self {
            buckets:   Arc::new(DashMap::new()),
            capacity:  rate_limit_rpm,
            allowlist: Arc::new(allowlist),
        }
    }

    /// Returns `(allowed, retry_after_secs)`.
    fn check(&self, ip: IpAddr) -> (bool, u64) {
        if self.allowlist.contains(&ip) {
            return (true, 0);
        }
        let mut bucket = self.buckets
            .entry(ip)
            .or_insert_with(|| Bucket::new(self.capacity));
        let retry = bucket.retry_after_secs();
        let ok = bucket.try_consume();
        (ok, retry)
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Tower `Layer` that wraps services with per-IP rate limiting.
#[derive(Clone)]
pub struct RateLimitLayer {
    state: RateLimitState,
}

impl RateLimitLayer {
    /// `rate_limit_rpm` — max requests per minute per IP.
    /// `extra_allowlist` — additional IPs that bypass rate limiting.
    pub fn new(rate_limit_rpm: u32, extra_allowlist: Vec<IpAddr>) -> Self {
        Self {
            state: RateLimitState::new(rate_limit_rpm, extra_allowlist),
        }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            state: self.state.clone(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Tower `Service` that enforces per-IP rate limits.
#[derive(Clone)]
pub struct RateLimitService<S> {
    inner: S,
    state: RateLimitState,
}

/// Extract the client IP from `x-forwarded-for` (set by nginx) or the
/// connection info injected by axum.  Falls back to `127.0.0.1`.
fn extract_ip(req: &Request<Body>) -> IpAddr {
    // Prefer X-Forwarded-For set by a trusted proxy.
    if let Some(xff) = req.headers().get("x-forwarded-for") {
        if let Ok(v) = xff.to_str() {
            if let Ok(ip) = v.split(',').next().unwrap_or("").trim().parse() {
                return ip;
            }
        }
    }
    // axum ConnectInfo extension (available when using `into_make_service_with_connect_info`).
    if let Some(addr) = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>() {
        return addr.0.ip();
    }
    "127.0.0.1".parse().unwrap()
}

impl<S> Service<Request<Body>> for RateLimitService<S>
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
        let ip = extract_ip(&req);
        let (allowed, retry_after) = self.state.check(ip);

        if !allowed {
            warn!("Rate limit exceeded for IP {}", ip);
            return Box::pin(std::future::ready(Ok(
                Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header("Retry-After", retry_after.to_string())
                    .body(Body::from("Too Many Requests"))
                    .unwrap(),
            )));
        }

        let fut = self.inner.call(req);
        Box::pin(async move { fut.await })
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_first_request() {
        let mut b = Bucket::new(60);
        assert!(b.try_consume());
    }

    #[test]
    fn bucket_denies_when_empty() {
        let mut b = Bucket::new(1);
        assert!(b.try_consume());  // first allowed
        assert!(!b.try_consume()); // second denied
    }

    #[test]
    fn bucket_retry_after_is_positive_when_empty() {
        let mut b = Bucket::new(60);
        // drain all tokens
        for _ in 0..60 { b.try_consume(); }
        assert!(b.retry_after_secs() > 0);
    }

    #[test]
    fn localhost_bypasses_rate_limit() {
        let state = RateLimitState::new(1, vec![]);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let (ok1, _) = state.check(ip);
        let (ok2, _) = state.check(ip); // would fail if rate-limited
        assert!(ok1 && ok2);
    }

    #[tokio::test]
    async fn rate_limit_layer_allows_normal_request() {
        let layer = RateLimitLayer::new(60, vec![]);
        let svc = tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder().status(200).body(Body::empty()).unwrap()
            )
        });
        let mut svc = layer.layer(svc);
        let req = Request::builder()
            .extension(axum::extract::ConnectInfo(
                std::net::SocketAddr::from(([192, 168, 1, 1], 1234))
            ))
            .body(Body::empty()).unwrap();
        use tower::ServiceExt;
        let resp = svc.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }
}
