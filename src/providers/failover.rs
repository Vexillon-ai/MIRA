// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/failover.rs

//! Failover provider - tries multiple providers in order until one succeeds

use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::providers::ModelProvider;
use crate::types::{ChatMessage, FallbackNotice, GenerationOptions, GenerationResponse};

/// Trim a provider error down to a short, user-presentable reason.
fn short_reason(e: &crate::MiraError) -> String {
    let s = e.to_string();
    let s = s.split(['\n', '{']).next().unwrap_or(&s).trim().to_string();
    if s.chars().count() > 140 { format!("{}…", s.chars().take(139).collect::<String>()) } else { s }
}

/// A provider that falls back to alternatives on failure
pub struct FailoverProvider {
    primary: Box<dyn ModelProvider>,
    fallbacks: Vec<Box<dyn ModelProvider>>,
}

impl FailoverProvider {
    /// Create a new failover provider with primary and fallback chain
    pub fn new(primary: Box<dyn ModelProvider>, fallbacks: Vec<Box<dyn ModelProvider>>) -> Self {
        // Callers commonly construct with an empty fallback vec and append via
        // `with_fallback`, so an info-level line here would misleadingly print
        // `-> []`. The authoritative assembled-chain log lives in
        // `build_provider_chain`; keep only a debug breadcrumb here.
        tracing::debug!("FailoverProvider::new: primary={} (+{} fallbacks at construction)",
                        primary.name(), fallbacks.len());
        Self { primary, fallbacks }
    }
    
    /// Add a fallback provider
    pub fn with_fallback(mut self, fallback: Box<dyn ModelProvider>) -> Self {
        self.fallbacks.push(fallback);
        self
    }
}

#[async_trait]
impl ModelProvider for FailoverProvider {
    fn name(&self) -> &str {
        "failover"
    }
    
    async fn generate(
        &self,
        messages: &[ChatMessage],
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        // Try primary first
        debug!("Trying primary provider: {}", self.primary.name());
        let primary_reason = match self.primary.generate(messages, options).await {
            Ok(response) => return Ok(response),
            Err(e) => {
                warn!("Primary provider '{}' failed: {}, trying fallbacks...",
                     self.primary.name(), e);
                short_reason(&e)
            }
        };

        // Try each fallback in order
        for (i, fallback) in self.fallbacks.iter().enumerate() {
            debug!("Trying fallback {} ({})", i + 1, fallback.name());
            match fallback.generate(messages, options).await {
                Ok(mut response) => {
                    info!("Fallback provider '{}' succeeded", fallback.name());
                    response.fallback = Some(FallbackNotice {
                        from:   self.primary.name().to_string(),
                        to:     fallback.name().to_string(),
                        reason: primary_reason.clone(),
                    });
                    return Ok(response);
                }
                Err(e) => {
                    warn!("Fallback provider '{}' failed: {}", fallback.name(), e);
                }
            }
        }
        
        // All providers failed
        Err(crate::MiraError::AllProvidersUnavailable)
    }
    
    async fn generate_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
        on_token: &mut (dyn FnMut(String) + Send),
    ) -> Result<GenerationResponse, crate::MiraError> {
        debug!("Trying primary provider (streaming): {}", self.primary.name());
        let primary_reason = match self.primary.generate_stream(messages, options, on_token).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                warn!("Primary provider '{}' stream failed: {}, trying fallbacks…", self.primary.name(), e);
                short_reason(&e)
            }
        };
        for (i, fallback) in self.fallbacks.iter().enumerate() {
            debug!("Trying fallback {} ({}) for streaming", i + 1, fallback.name());
            match fallback.generate_stream(messages, options, on_token).await {
                Ok(mut r) => {
                    info!("Fallback provider '{}' stream succeeded", fallback.name());
                    r.fallback = Some(FallbackNotice {
                        from:   self.primary.name().to_string(),
                        to:     fallback.name().to_string(),
                        reason: primary_reason.clone(),
                    });
                    return Ok(r);
                }
                Err(e) => warn!("Fallback provider '{}' stream failed: {}", fallback.name(), e),
            }
        }
        Err(crate::MiraError::AllProvidersUnavailable)
    }

    async fn health_check(&self) -> bool {
        if self.primary.health_check().await { return true; }
        for fallback in &self.fallbacks {
            if fallback.health_check().await { return true; }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use crate::types::{ChatMessage, GenerationOptions, GenerationResponse, TokenUsage, ProviderId};

    struct AlwaysSucceeds(String);
    struct AlwaysFails;

    #[async_trait]
    impl ModelProvider for AlwaysSucceeds {
        fn name(&self) -> &str { "always_succeeds" }
        async fn generate(&self, _m: &[ChatMessage], _o: &GenerationOptions) -> Result<GenerationResponse, crate::MiraError> {
            Ok(GenerationResponse {
                content: self.0.clone(),
                tool_calls: None,
                reasoning: None,
                usage: TokenUsage::default(),
                provider_id: ProviderId::Local("mock".to_string()),
                model_name: "mock".to_string(),
                fallback: None,
            })
        }
        async fn health_check(&self) -> bool { true }
    }

    #[async_trait]
    impl ModelProvider for AlwaysFails {
        fn name(&self) -> &str { "always_fails" }
        async fn generate(&self, _m: &[ChatMessage], _o: &GenerationOptions) -> Result<GenerationResponse, crate::MiraError> {
            Err(crate::MiraError::ProviderError("intentional failure".to_string()))
        }
        async fn health_check(&self) -> bool { false }
    }

    fn msgs() -> Vec<ChatMessage> { vec![ChatMessage::user("Hello")] }
    fn opts() -> GenerationOptions { GenerationOptions::default() }

    #[tokio::test]
    async fn test_primary_succeeds() {
        let provider = FailoverProvider::new(
            Box::new(AlwaysSucceeds("primary response".to_string())),
            vec![],
        );
        let result = provider.generate(&msgs(), &opts()).await.unwrap();
        assert_eq!(result.content, "primary response");
    }

    #[tokio::test]
    async fn test_primary_fails_fallback_succeeds() {
        let provider = FailoverProvider::new(
            Box::new(AlwaysFails),
            vec![Box::new(AlwaysSucceeds("fallback response".to_string()))],
        );
        let result = provider.generate(&msgs(), &opts()).await.unwrap();
        assert_eq!(result.content, "fallback response");
        // The failover must be recorded so the agent loop can warn the user.
        let fb = result.fallback.expect("fallback notice set on failover");
        assert_eq!(fb.from, "always_fails");
        assert_eq!(fb.to, "always_succeeds");
        assert!(fb.reason.contains("intentional failure"), "reason: {}", fb.reason);
    }

    #[tokio::test]
    async fn test_primary_succeeds_no_fallback_notice() {
        let provider = FailoverProvider::new(
            Box::new(AlwaysSucceeds("ok".to_string())),
            vec![Box::new(AlwaysSucceeds("nope".to_string()))],
        );
        let result = provider.generate(&msgs(), &opts()).await.unwrap();
        assert!(result.fallback.is_none(), "no fallback notice on the happy path");
    }

    #[tokio::test]
    async fn test_all_providers_fail() {
        let provider = FailoverProvider::new(
            Box::new(AlwaysFails),
            vec![Box::new(AlwaysFails), Box::new(AlwaysFails)],
        );
        let err = provider.generate(&msgs(), &opts()).await.unwrap_err();
        assert!(matches!(err, crate::MiraError::AllProvidersUnavailable));
    }

    #[tokio::test]
    async fn test_health_check_primary_healthy() {
        let provider = FailoverProvider::new(
            Box::new(AlwaysSucceeds("x".to_string())),
            vec![Box::new(AlwaysFails)],
        );
        assert!(provider.health_check().await);
    }

    #[tokio::test]
    async fn test_health_check_fallback_healthy() {
        let provider = FailoverProvider::new(
            Box::new(AlwaysFails),
            vec![Box::new(AlwaysSucceeds("x".to_string()))],
        );
        assert!(provider.health_check().await);
    }

    #[tokio::test]
    async fn test_health_check_all_unhealthy() {
        let provider = FailoverProvider::new(
            Box::new(AlwaysFails),
            vec![Box::new(AlwaysFails)],
        );
        assert!(!provider.health_check().await);
    }

    #[tokio::test]
    async fn test_with_fallback_builder() {
        let provider = FailoverProvider::new(Box::new(AlwaysFails), vec![])
            .with_fallback(Box::new(AlwaysSucceeds("added fallback".to_string())));
        let result = provider.generate(&msgs(), &opts()).await.unwrap();
        assert_eq!(result.content, "added fallback");
    }
}
