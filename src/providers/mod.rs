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

use async_trait::async_trait;
use crate::types::{ChatMessage, GenerationOptions, GenerationResponse};

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
