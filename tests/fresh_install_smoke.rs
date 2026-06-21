// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/fresh_install_smoke.rs
//! Fresh-install smoke tests.
//!
//! Guards the class of bugs that shipped in 0.272.x and the 0.273.x audit: a
//! brand-new install with DEFAULT settings should boot and be usable, and the
//! defaults must be self-hosting-safe (work behind NAT / on localhost / with no
//! external services). Each prior incident gets a regression check here:
//!
//! - server boots from a pure-default config and serves (the startup-crash class)
//! - memory embeddings default to the in-process engine (no external model)
//! - Telegram defaults to polling (works behind NAT, not webhook)
//!
//! Deterministic + offline by design — no model downloads, no network — so it's
//! safe to run on every push. Wired into the `test:linux` CI gate via
//! `cargo test --test fresh_install_smoke`.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
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
            content:     "stub".to_string(),
            tool_calls:  None,
            reasoning:   None,
            usage:       TokenUsage::default(),
            provider_id: ProviderId::Local("stub".to_string()),
            model_name:  "stub".to_string(),
            fallback:    None,
        })
    }
    async fn health_check(&self) -> bool { true }
}

// Build the router from a PURE-DEFAULT config (the fresh-install path), unlike
// api_smoke which tweaks a few fields. data_dir is the only override (a temp
// dir) so the test doesn't touch a real ~/.mira.
fn fresh_default_router() -> (axum::Router, TempDir) {
    let dir = TempDir::new().unwrap();
    let mut cfg = MiraConfig::default();
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
    let event_bus = Arc::new(EventBus::new());
    let restart_notify = Arc::new(tokio::sync::Notify::new());
    let agent_registry = Arc::new(mira::agent::AgentRegistry::new());
    let supervisor = Arc::new(mira::agent::Supervisor::new(agent_registry.clone()));
    let mcp_servers = Arc::new(mira::mcp::McpServerRegistry::empty());
    let email_pollers = Arc::new(mira::email::EmailPollerRegistry::empty());
    let router = build_router(
        core, &sec, &cfg, None, None, None, bus,
        Arc::new(std::collections::HashMap::new()),
        Arc::new(std::collections::HashMap::new()),
        Arc::new(std::collections::HashMap::new()),
        Arc::new(std::collections::HashMap::new()),
        None, None, None, None, None,
        None, None, None, event_bus, restart_notify, agent_registry, supervisor,
        None, None, None, None, None, None, mcp_servers, None, None, None,
        email_pollers, None, None, None, None,
        None, // degradations
        None, // guardian_actions
        None, // audit_store
    );
    (router, dir)
}

#[tokio::test]
async fn fresh_default_install_boots_and_serves() {
    // The whole point: a default-config install comes up and answers. If the
    // wiring panics or the config can't build, this fails — catching the
    // startup-crash class that bricked 0.272.0–0.272.2.
    let (app, _dir) = fresh_default_router();
    let req  = Request::builder().uri("/health").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "fresh default install must serve /health");
}

#[tokio::test]
async fn default_embeddings_are_self_contained() {
    // Memory must work on a fresh box with no external embedding server. The
    // previous lmstudio default named a model that doesn't exist in stock LM
    // Studio, so memory silently degraded.
    let cfg = MiraConfig::default();
    assert_eq!(
        cfg.memory.embedding.provider, "internal",
        "default embedding provider must be the in-process engine"
    );
}

#[tokio::test]
async fn default_telegram_mode_is_polling() {
    // Polling works behind NAT/localhost; webhook needs a public URL. A fresh
    // Telegram account must default to polling so self-hosted installs receive
    // messages without a reverse proxy.
    let cfg: mira::channel_accounts::TelegramAccountConfig =
        serde_json::from_value(json!({ "bot_token": "x" })).unwrap();
    assert_eq!(cfg.mode, "polling", "default Telegram mode must be polling");
}
