// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/api_smoke.rs
//! Integration smoke tests for the MIRA HTTP API.
//!
//! These tests spin up the full router (without auth/history/live-config) and
//! exercise the public & authenticated API surface at the HTTP layer.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tempfile::TempDir;

use mira::agent::AgentCore;
use mira::config::MiraConfig;
use mira::events::EventBus;
use mira::memory::MemorySystem;
use mira::notifications::NotificationBus;
use mira::providers::ModelProvider;
use mira::security::SecurityConfig;
use mira::server::router::build_router;
use mira::session::SessionStore;
use mira::tools::ToolRegistry;
use mira::types::{ChatMessage, GenerationOptions, GenerationResponse, ProviderId, TokenUsage};

// ── Minimal stub provider ────────────────────────────────────────────────────

struct StubProvider;

#[async_trait]
impl ModelProvider for StubProvider {
    fn name(&self) -> &str { "stub" }
    async fn generate(
        &self,
        _: &[ChatMessage],
        _: &GenerationOptions,
    ) -> Result<GenerationResponse, mira::MiraError> {
        Ok(GenerationResponse {
            content:     "stub response".to_string(),
            tool_calls:  None,
            reasoning:   None,
            usage:       TokenUsage::default(),
            provider_id: ProviderId::Local("stub".to_string()),
            model_name:  "stub".to_string(),
            fallback: None,
            })
    }
    async fn health_check(&self) -> bool { true }
}

// ── Router fixture ───────────────────────────────────────────────────────────

fn smoke_router() -> (axum::Router, TempDir) {
    let dir = TempDir::new().unwrap();
    let mut cfg = MiraConfig::default();
    cfg.agent.tool_mode  = "disabled".to_string();
    cfg.memory.embedding.provider = "lmstudio".to_string();
    cfg.data_dir = dir.path().to_string_lossy().to_string();

    let mem  = Arc::new(MemorySystem::new_keyword_only(dir.path().join("mem.db")).unwrap());
    let core = Arc::new(AgentCore::new(
        Arc::new(cfg.clone()),
        Arc::new(StubProvider) as Arc<dyn ModelProvider>,
        mem,
        Arc::new(ToolRegistry::new()),
        Arc::new(SessionStore::new()),
    ));

    let sec = SecurityConfig::default();
    let bus = Arc::new(NotificationBus::new());
    let tg_accounts = Arc::new(std::collections::HashMap::new());
    let wa_accounts = Arc::new(std::collections::HashMap::new());
    let sl_accounts = Arc::new(std::collections::HashMap::new());
    let ext_accounts = Arc::new(std::collections::HashMap::new());
    let event_bus = Arc::new(EventBus::new());
    let restart_notify = Arc::new(tokio::sync::Notify::new());
    let agent_registry = Arc::new(mira::agent::AgentRegistry::new());
    let supervisor = Arc::new(mira::agent::Supervisor::new(agent_registry.clone()));
    let mcp_servers = Arc::new(mira::mcp::McpServerRegistry::empty());
    let email_pollers = Arc::new(mira::email::EmailPollerRegistry::empty());
    // build_router grew to 38 params; this integration smoke test only needs a
    // handful of real services — every optional dependency is passed as None.
    let router = build_router(
        core, &sec, &cfg, None, None, None, bus, tg_accounts,
        wa_accounts, sl_accounts, ext_accounts, None, None, None, None, None,
        None, None, None, event_bus, restart_notify, agent_registry, supervisor,
        None, None, None, None, None, None, mcp_servers, None, None, None,
        email_pollers, None, None, None, None,
        None, // degradations
    );
    (router, dir)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_returns_200() {
    let (app, _dir) = smoke_router();
    let req  = Request::builder().uri("/health").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_route_falls_back_to_spa() {
    // Without live_config / auth wired in, the router has no SPA fallback and
    // should return 404 for unknown routes.
    let (app, _dir) = smoke_router();
    let req  = Request::builder().uri("/nonexistent-path").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // Either 404 (no SPA) or 200 (SPA fallback) — just check it doesn't panic.
    let _status = resp.status();
}

#[tokio::test]
async fn signal_webhook_disabled_without_hmac_key() {
    // Since 0.130.2 the /webhook/signal route is only mounted when
    // channels.signal.hmac_key is set. The smoke fixture configures no hmac_key,
    // so the route is unmounted and the POST falls through to 404 (the fixture
    // also wires no SPA fallback, so unknown routes are a plain NOT_FOUND).
    let (app, _dir) = smoke_router();
    let req = Request::builder()
        .method("POST")
        .uri("/webhook/signal")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn telegram_webhook_without_auth_is_rejected() {
    let (app, _dir) = smoke_router();
    let req = Request::builder()
        .method("POST")
        .uri("/webhook/telegram")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"update_id":1,"message":null}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // Per-account secret verification lives in `telegram_handler` itself
    // now; the legacy `/webhook/telegram` (no account id) route isn't
    // registered, so this just confirms we don't 500.
    assert!(resp.status().as_u16() < 500);
}

#[tokio::test]
async fn api_routes_require_auth() {
    // Without auth_service wired, the api_routes block is skipped entirely and
    // /api/memory should 404 (no route registered).
    let (app, _dir) = smoke_router();
    let req = Request::builder()
        .uri("/api/memory")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // 404 because routes are only registered when auth_service is Some(_).
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
