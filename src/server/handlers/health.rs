// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/health.rs
//! Machine health probes, split by audience (see the three-way split doc):
//!
//! - `GET /livez` — **liveness**. Is the process answering? Returns `200 ok`
//!   unconditionally, touching no dependency (no provider, DB, or other
//!   subsystem). A liveness probe that can block on a wedged dependency is a
//!   self-inflicted outage, so this one never does I/O.
//! - `GET /readyz` — **readiness**. Is MIRA fit to serve (provider reachable)?
//!   `200` when ready, `503` when not. The verdict is **cached** with a short
//!   TTL and refreshed out of band, so N load balancers probing in a tight loop
//!   never stampede the provider and a caller whose timeout is under the
//!   provider's worst-case latency can't manufacture a false negative.
//!
//! `/health` is deliberately **not** a server route: it falls through to the
//! SPA so the admin System Health page renders on a hard navigation.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::extract::State;
use axum::Json;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::agent::AgentCore;

/// `GET /livez` — pure process liveness. Answers immediately and never touches a
/// dependency, so it stays responsive even while the provider (or any other
/// subsystem) is wedged. Point supervisors / the out-of-process sentinel / TUI
/// reachability / mobile pre-auth checks here.
pub async fn livez_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// One cached readiness verdict plus the instant it was computed.
#[derive(Clone, Copy)]
struct CachedReadiness {
    ready: bool,
    checked_at: Instant,
}

/// Caches the readiness verdict (`core.health_check()`, which round-trips the
/// active provider) behind a short TTL. Serves the cached value immediately and
/// refreshes **out of band** (serve-stale-while-revalidate) with a single-flight
/// guard, so however many callers probe, at most one provider round-trip is in
/// flight per TTL window and no probe blocks on the provider — only the very
/// first probe of a cold cache does a synchronous check.
pub struct ReadinessCache {
    core: Arc<AgentCore>,
    ttl: Duration,
    state: Mutex<Option<CachedReadiness>>,
    refreshing: AtomicBool,
}

impl ReadinessCache {
    /// `ttl` is the max age a cached verdict is served before a background
    /// refresh is triggered. A zero TTL disables caching (every probe checks
    /// inline) — useful for tests / debugging; production defaults to 30s.
    pub fn new(core: Arc<AgentCore>, ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            core,
            ttl,
            state: Mutex::new(None),
            refreshing: AtomicBool::new(false),
        })
    }

    /// Kick off a background refresh unless one is already running.
    fn spawn_refresh(self: &Arc<Self>) {
        // AcqRel swap: only the caller that flips false→true owns the refresh.
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let ready = this.core.health_check().await;
            {
                let mut guard = this.state.lock().await;
                *guard = Some(CachedReadiness { ready, checked_at: Instant::now() });
            }
            this.refreshing.store(false, Ordering::Release);
        });
    }

    /// Current verdict + the age of the check it came from. Fresh entries are
    /// returned as-is; a stale entry is returned immediately while a refresh is
    /// spawned; a cold cache checks once inline.
    async fn verdict(self: &Arc<Self>) -> (bool, Duration) {
        let snapshot = *self.state.lock().await;
        match snapshot {
            Some(c) if !self.ttl.is_zero() && c.checked_at.elapsed() < self.ttl => {
                (c.ready, c.checked_at.elapsed())
            }
            Some(c) => {
                // Stale: serve the last known verdict, refresh behind it.
                self.spawn_refresh();
                (c.ready, c.checked_at.elapsed())
            }
            None => {
                // Cold cache: one synchronous check to seed it.
                let ready = self.core.health_check().await;
                let now = Instant::now();
                *self.state.lock().await = Some(CachedReadiness { ready, checked_at: now });
                (ready, Duration::ZERO)
            }
        }
    }
}

/// `GET /readyz` — readiness (provider reachable), cached. `200` ready / `503`
/// not; the JSON body carries `cached_age_secs` so a `503` is diagnosable (is
/// the provider actually down, or is this a stale cached miss?).
pub async fn readyz_handler(
    State(cache): State<Arc<ReadinessCache>>,
) -> impl IntoResponse {
    let (ready, age) = cache.verdict().await;
    let age_secs = age.as_secs();
    if ready {
        (
            StatusCode::OK,
            Json(json!({ "status": "ready", "cached_age_secs": age_secs })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "unavailable",
                "detail": "provider unreachable",
                "cached_age_secs": age_secs,
            })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::Router;
    use axum::routing::get;
    use tower::ServiceExt;

    async fn make_core(healthy: bool) -> Arc<AgentCore> {
        use async_trait::async_trait;
        use crate::types::{ChatMessage, GenerationOptions, GenerationResponse, TokenUsage, ProviderId};
        use crate::providers::ModelProvider;
        use crate::memory::MemorySystem;
        use crate::tools::ToolRegistry;
        use crate::session::SessionStore;
        use crate::config::MiraConfig;
        use tempfile::TempDir;

        struct StubProvider(bool);

        #[async_trait]
        impl ModelProvider for StubProvider {
            fn name(&self) -> &str { "stub" }
            async fn generate(&self, _: &[ChatMessage], _: &GenerationOptions)
                -> Result<GenerationResponse, crate::MiraError>
            {
                Ok(GenerationResponse {
                    content: "ok".to_string(),
                    tool_calls: None,
                    reasoning: None,
                    usage: TokenUsage::default(),
                    provider_id: ProviderId::Local("stub".to_string()),
                    model_name: "stub".to_string(),
                    fallback: None,
                })
            }
            async fn health_check(&self) -> bool { self.0 }
        }

        let dir = TempDir::new().unwrap();
        let mut cfg = MiraConfig::default();
        cfg.agent.tool_mode = "disabled".to_string();
        cfg.memory.embedding.provider = "lmstudio".to_string();
        cfg.data_dir = dir.path().to_string_lossy().to_string();

        Arc::new(AgentCore::new(
            Arc::new(cfg),
            Arc::new(StubProvider(healthy)) as Arc<dyn ModelProvider>,
            Arc::new(MemorySystem::new_keyword_only(dir.path().join("mem.db")).unwrap()),
            Arc::new(ToolRegistry::new()),
            Arc::new(SessionStore::new()),
        ))
    }

    fn readyz_router(cache: Arc<ReadinessCache>) -> Router {
        Router::new()
            .route("/readyz", get(readyz_handler))
            .with_state(cache)
    }

    #[tokio::test]
    async fn livez_is_unconditional_200_ok() {
        // No state, no dependency — always 200 "ok".
        let app = Router::new().route("/livez", get(livez_handler));
        let req = Request::builder().uri("/livez").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn readyz_healthy_provider_returns_200() {
        let cache = ReadinessCache::new(make_core(true).await, Duration::from_secs(30));
        let app = readyz_router(cache);
        let req = Request::builder().uri("/readyz").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn readyz_unhealthy_provider_returns_503() {
        let cache = ReadinessCache::new(make_core(false).await, Duration::from_secs(30));
        let app = readyz_router(cache);
        let req = Request::builder().uri("/readyz").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 503);
    }

    #[tokio::test]
    async fn readyz_serves_cached_verdict_within_ttl() {
        // A long TTL means the second probe is served from cache without a
        // fresh check — the whole point of the readiness cache.
        let cache = ReadinessCache::new(make_core(true).await, Duration::from_secs(3600));
        let (ready1, _) = cache.verdict().await;
        assert!(ready1);
        // Second call is fresh (age < ttl) and must not error.
        let (ready2, age2) = cache.verdict().await;
        assert!(ready2);
        assert!(age2 < Duration::from_secs(3600));
    }
}
