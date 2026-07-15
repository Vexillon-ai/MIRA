// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/mod.rs

//! Model provider router and implementations

pub mod local;
pub mod lmstudio;
pub mod openrouter;
pub mod openai_compat;
pub mod anthropic;
pub mod gemini;
pub mod failover;
pub mod signal_cli;
pub mod catalog;
pub mod overlays;
pub(crate) mod usage;
pub(crate) mod errors;

use async_trait::async_trait;
use crate::types::{ChatMessage, GenerationOptions, GenerationResponse};

/// Make an OpenAI-style base URL forgiving about the API version path.
///
/// The OpenAI-compatible endpoints (`/chat/completions`, `/models`) live under
/// a version path — `/v1` for most, `/openai/v1` for Groq — but users routinely
/// paste just the host (`http://localhost:1234`, `https://api.groq.com`) without
/// knowing that convention. Posting to `{host}/chat/completions` then fails in
/// an opaque way. This grafts the provider's `canonical_path` onto a bare host
/// so those URLs "just work":
///
/// - bare host (no path after the authority) → append `canonical_path`
/// - already ends with `canonical_path` → unchanged
/// - any other non-empty path → trusted as-is (custom gateways/proxies)
///
/// Trailing slashes are trimmed. Idempotent.
pub fn normalize_openai_base_url(url: &str, canonical_path: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return canonical_path.to_string();
    }
    if trimmed.ends_with(canonical_path) {
        return trimmed.to_string();
    }
    // Bare host = nothing after the scheme's authority. Only then do we graft
    // the version path; if the user supplied any path we trust it.
    let after_scheme = trimmed.split("://").nth(1).unwrap_or(trimmed);
    if after_scheme.contains('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}{canonical_path}")
    }
}

/// Trait that all model providers must implement.
///
/// All providers support both non-streaming (`generate`) and streaming
/// (`generate_stream`) generation. Providers that do not natively support
/// streaming get a default implementation that calls `generate` and emits
/// the full response as a single token.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Provider identifier used in logs and diagnostics.
    fn name(&self) -> &str;

    /// Generate a complete (non-streaming) response.
    async fn generate(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError>;

    /// Generate a response and call `on_token` for each token as it arrives.
    ///
    /// Returns the complete `GenerationResponse` after streaming finishes.
    /// The default implementation calls `generate` and emits the full content
    /// as one token — suitable for providers that do not support streaming.
    async fn generate_stream(
        &self,
        messages: &[ChatMessage],
        options:  &GenerationOptions,
        on_token: &mut (dyn FnMut(String) + Send),
    ) -> Result<GenerationResponse, crate::MiraError> {
        let resp = self.generate(messages, options).await?;
        on_token(resp.content.clone());
        Ok(resp)
    }

    /// Return `true` if the provider is reachable and ready to serve requests.
    async fn health_check(&self) -> bool;
}

#[cfg(test)]
mod base_url_tests {
    use super::normalize_openai_base_url;

    #[test]
    fn bare_host_gets_path() {
        assert_eq!(normalize_openai_base_url("http://localhost:1234", "/v1"), "http://localhost:1234/v1");
        assert_eq!(normalize_openai_base_url("https://api.groq.com", "/openai/v1"), "https://api.groq.com/openai/v1");
        assert_eq!(normalize_openai_base_url("  http://localhost:1234/  ", "/v1"), "http://localhost:1234/v1");
    }

    #[test]
    fn already_correct_is_idempotent() {
        assert_eq!(normalize_openai_base_url("http://localhost:1234/v1", "/v1"), "http://localhost:1234/v1");
        assert_eq!(normalize_openai_base_url("http://localhost:1234/v1/", "/v1"), "http://localhost:1234/v1");
    }

    #[test]
    fn custom_path_is_trusted() {
        // A user pointing at a reverse-proxy mount shouldn't have /v1 forced on.
        assert_eq!(normalize_openai_base_url("https://gw.example.com/llm", "/v1"), "https://gw.example.com/llm");
    }
}
