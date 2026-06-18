// SPDX-License-Identifier: AGPL-3.0-or-later

// src/security/log.rs
//! Structured request/response logging middleware.
//!
//! Logs one line per request:
//! ```text
//! → POST /webhook/telegram  (from 127.0.0.1)
//! ← 200 POST /webhook/telegram  [42ms]
//! ```
//!
//! Uses `tracing::info!` so the output is routed through the same logging
//! pipeline as the rest of MIRA. Does not log request or response bodies.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, Response};
use tower::{Layer, Service};
use tracing::info;

// ─────────────────────────────────────────────────────────────────────────────

/// Tower `Layer` that adds request/response logging to any service.
#[derive(Clone, Default)]
pub struct RequestLogLayer;

impl<S> Layer<S> for RequestLogLayer {
    type Service = RequestLogService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestLogService { inner }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Tower `Service` that logs each HTTP request/response.
#[derive(Clone)]
pub struct RequestLogService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for RequestLogService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error    = S::Error;
    type Future   = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let method = req.method().clone();
        let path   = req.uri().path().to_owned();
        let ip     = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .map(str::to_owned)
            .unwrap_or_else(|| "unknown".to_string());

        info!("→ {} {}  (from {})", method, path, ip);

        let start = Instant::now();
        let fut   = self.inner.call(req);

        Box::pin(async move {
            let resp = fut.await?;
            let elapsed = start.elapsed();
            info!(
                "← {} {} {}  [{:.0}ms]",
                resp.status().as_u16(),
                method,
                path,
                elapsed.as_secs_f64() * 1000.0,
            );
            Ok(resp)
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn log_layer_passes_response_through() {
        let layer = RequestLogLayer;
        let svc = tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder().status(201).body(Body::empty()).unwrap()
            )
        });
        let mut svc = layer.layer(svc);
        let req = Request::builder()
            .method("POST")
            .uri("/test")
            .body(Body::empty()).unwrap();
        let resp = svc.ready().await.unwrap().call(req).await.unwrap();
        assert_eq!(resp.status(), 201);
    }
}
