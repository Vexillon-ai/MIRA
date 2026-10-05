// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/empty_guard.rs
//! Empty-completion retry guard.
//!
//! Some providers intermittently return a *successful* chat completion whose
//! assistant message has no content and no tool_calls — HTTP 200,
//! `finish_reason: "stop"`, no error. The user then sees a blank reply. This is
//! provider-side (e.g. an OpenRouter upstream like Novita returning empty ~40%
//! of the time for some models while another upstream returns ~0%), not a MIRA
//! bug, but MIRA should recover rather than show an empty bubble.
//!
//! [`guard`] wraps the assembled provider chain as a sibling of the degeneracy
//! guard, so EVERY call path inherits it — chat turns, auto-title, and the
//! memory/wiki extractors. On a detected empty it RE-ISSUES the completion up to
//! `max_retries` times. For a load-balancing gateway (OpenRouter) a re-issue is
//! routed to a possibly-different upstream endpoint — exactly the recovery the
//! spec asks for; for a single plain provider it is just a re-issue. If every
//! attempt is still empty it fails the turn with a provider error, which the
//! agent surfaces as a graceful "please try again" and counts like any other
//! provider failure (feeding failover / health exactly as the degeneracy guard's
//! trip does).
//!
//! What is NOT treated as empty:
//!   * a legitimate tool-call turn — empty `content` WITH `tool_calls` present,
//!   * a response that produced reasoning but no final content — it generated
//!     *something*, so re-issuing would not obviously help and the reasoning is
//!     still surfaced to the user.
//! (`finish_reason` is not modelled on `GenerationResponse`; a `length`-truncated
//! reply carries partial content so it is never detected as empty, and a rare
//! `content_filter` blank degrades to the bounded retry + graceful error.)

use async_trait::async_trait;
use std::sync::Arc;
use tracing::warn;

use crate::config::EmptyResponseRetryConfig;
use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions, GenerationResponse};

/// True when a completion is a silent blank: no textual content, no tool_calls,
/// and no reasoning. An empty `content` WITH `tool_calls` is a real tool-call
/// turn (not a blank) and returns `false`.
pub fn is_empty_completion(resp: &GenerationResponse) -> bool {
    let no_content    = resp.content.trim().is_empty();
    let no_tool_calls = resp.tool_calls.as_ref().map_or(true, |tc| tc.is_empty());
    let no_reasoning  = resp.reasoning.as_deref().map_or(true, |r| r.trim().is_empty());
    no_content && no_tool_calls && no_reasoning
}

/// Wrap `inner` in the empty-completion retry guard when enabled; otherwise
/// return it unchanged (zero overhead when off).
pub fn guard(
    inner: Arc<dyn ModelProvider>,
    cfg:   EmptyResponseRetryConfig,
) -> Arc<dyn ModelProvider> {
    if cfg.enabled {
        Arc::new(EmptyRetryProvider { inner, max_retries: cfg.max_retries })
    } else {
        inner
    }
}

/// A provider decorator that re-issues on a detected empty completion.
pub struct EmptyRetryProvider {
    inner:       Arc<dyn ModelProvider>,
    max_retries: u32,
}

impl EmptyRetryProvider {
    fn exhausted_error(attempts: u32) -> crate::MiraError {
        crate::MiraError::ProviderError(format!(
            "the model returned an empty response {attempts} time(s) in a row — this is \
             usually a transient provider-side issue. Please try again."
        ))
    }
}

#[async_trait]
impl ModelProvider for EmptyRetryProvider {
    fn name(&self) -> &str { self.inner.name() }

    // Transparent about the inner guard so callers/tests asserting the degeneracy
    // guard is installed still see it through this wrapper.
    fn guards_degeneracy(&self) -> bool { self.inner.guards_degeneracy() }

    async fn generate(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        let mut attempt = 0u32;
        loop {
            let resp = self.inner.generate(messages, options).await?;
            if !is_empty_completion(&resp) {
                return Ok(resp);
            }
            attempt += 1;
            if attempt > self.max_retries {
                warn!(
                    "empty-retry: provider '{}' returned an empty completion on all {} \
                     attempt(s); failing the turn as a provider error",
                    self.inner.name(), attempt
                );
                return Err(Self::exhausted_error(attempt));
            }
            warn!(
                "empty-retry: provider '{}' returned an empty completion; re-issuing \
                 (retry {}/{})",
                self.inner.name(), attempt, self.max_retries
            );
        }
    }

    async fn generate_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
        on_token: &mut (dyn FnMut(String) + Send),
    ) -> Result<GenerationResponse, crate::MiraError> {
        // An empty stream forwards no content tokens to `on_token`, so re-issuing
        // and forwarding the next attempt's tokens never double-prints anything.
        let mut attempt = 0u32;
        loop {
            let resp = self.inner.generate_stream(messages, options, on_token).await?;
            if !is_empty_completion(&resp) {
                return Ok(resp);
            }
            attempt += 1;
            if attempt > self.max_retries {
                warn!(
                    "empty-retry: provider '{}' streamed an empty completion on all {} \
                     attempt(s); failing the turn as a provider error",
                    self.inner.name(), attempt
                );
                return Err(Self::exhausted_error(attempt));
            }
            warn!(
                "empty-retry: provider '{}' streamed an empty completion; re-issuing \
                 (retry {}/{})",
                self.inner.name(), attempt, self.max_retries
            );
        }
    }

    async fn health_check(&self) -> bool { self.inner.health_check().await }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ProviderId, TokenUsage, ToolCall};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn resp(content: &str, tool_calls: Option<Vec<ToolCall>>, reasoning: Option<&str>) -> GenerationResponse {
        GenerationResponse {
            content: content.to_string(),
            tool_calls,
            reasoning: reasoning.map(str::to_string),
            usage: TokenUsage::default(),
            provider_id: ProviderId::OpenRouter("test".into()),
            model_name: "test".to_string(),
            fallback: None,
        }
    }

    fn a_tool_call() -> ToolCall {
        ToolCall { name: "t".into(), arguments: serde_json::json!({}), call_id: "1".into() }
    }

    #[test]
    fn detects_blank_but_not_tool_calls_or_content_or_reasoning() {
        assert!(is_empty_completion(&resp("", None, None)), "total blank is empty");
        assert!(is_empty_completion(&resp("   \n ", Some(vec![]), Some("  "))), "whitespace-only is empty");
        assert!(!is_empty_completion(&resp("hello", None, None)), "content is not empty");
        assert!(!is_empty_completion(&resp("", Some(vec![a_tool_call()]), None)), "tool-call turn is NOT empty");
        assert!(!is_empty_completion(&resp("", None, Some("thinking..."))), "reasoning-only is NOT treated as empty");
    }

    // A provider that returns empty for the first `empties` calls, then content.
    struct FlakyProvider { calls: AtomicUsize, empties: usize }

    #[async_trait]
    impl ModelProvider for FlakyProvider {
        fn name(&self) -> &str { "flaky" }
        async fn generate(&self, _m: &[ChatMessage], _o: &GenerationOptions)
            -> Result<GenerationResponse, crate::MiraError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if n < self.empties { resp("", None, None) } else { resp("real answer", None, None) })
        }
        async fn health_check(&self) -> bool { true }
    }

    #[tokio::test]
    async fn retries_past_empties_then_succeeds() {
        let inner = Arc::new(FlakyProvider { calls: AtomicUsize::new(0), empties: 2 });
        let g = guard(inner.clone(), EmptyResponseRetryConfig { enabled: true, max_retries: 2 });
        let r = g.generate(&[], &GenerationOptions::default()).await.unwrap();
        assert_eq!(r.content, "real answer");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 3, "2 empties + 1 good = 3 calls");
    }

    #[tokio::test]
    async fn exhausts_and_errors_when_always_empty() {
        let inner = Arc::new(FlakyProvider { calls: AtomicUsize::new(0), empties: 99 });
        let g = guard(inner.clone(), EmptyResponseRetryConfig { enabled: true, max_retries: 2 });
        let err = g.generate(&[], &GenerationOptions::default()).await.unwrap_err();
        assert!(matches!(err, crate::MiraError::ProviderError(_)));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 3, "1 initial + 2 retries = 3 attempts, then fail");
    }

    #[tokio::test]
    async fn disabled_guard_is_passthrough() {
        let inner = Arc::new(FlakyProvider { calls: AtomicUsize::new(0), empties: 99 });
        let g = guard(inner.clone(), EmptyResponseRetryConfig { enabled: false, max_retries: 2 });
        let r = g.generate(&[], &GenerationOptions::default()).await.unwrap();
        assert!(r.content.is_empty(), "disabled: the empty passes straight through");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1, "no retries when disabled");
    }
}
