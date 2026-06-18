// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/health.rs
//! GET /health — liveness probe.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::extract::State;
use std::sync::Arc;

use crate::agent::AgentCore;

/// Health check handler.
///
/// Returns `200 ok` when the provider is healthy, `503 Service Unavailable`
/// when the provider is unreachable.  Callers (nginx, load-balancers,
/// Kubernetes probes) should interpret `200` as "ready to serve".
pub async fn health_handler(
    State(core): State<Arc<AgentCore>>,
) -> impl IntoResponse {
    if core.health_check().await {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "provider unavailable")
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

    async fn make_router(healthy: bool) -> Router {
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

        let core = Arc::new(AgentCore::new(
            Arc::new(cfg),
            Arc::new(StubProvider(healthy)) as Arc<dyn ModelProvider>,
            Arc::new(MemorySystem::new_keyword_only(dir.path().join("mem.db")).unwrap()),
            Arc::new(ToolRegistry::new()),
            Arc::new(SessionStore::new()),
        ));

        Router::new()
            .route("/health", get(health_handler))
            .with_state(core)
    }

    #[tokio::test]
    async fn healthy_provider_returns_200() {
        let app = make_router(true).await;
        let req = Request::builder().uri("/health").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn unhealthy_provider_returns_503() {
        let app = make_router(false).await;
        let req = Request::builder().uri("/health").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 503);
    }
}
