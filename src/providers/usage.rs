// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/usage.rs

//! Shared OpenAI-compatible `usage` wire type.
//!
//! The internal [`TokenUsage`] is flat, but the OpenAI Chat Completions
//! shape reports prompt-cache hits in a nested
//! `usage.prompt_tokens_details.cached_tokens` object (OpenAI's automatic
//! prefix caching, mirrored by OpenRouter, DeepSeek, Groq, Together, …).
//! Deserializing straight into `TokenUsage` silently drops that, so every
//! OpenAI-compat provider would report `cache_read_tokens = 0` even when the
//! provider served most of the prompt from cache.
//!
//! [`WireUsage`] captures the nested field and folds it into
//! `TokenUsage.cache_read_tokens` on conversion, closing the Phase-0
//! measurement loop for the automatic-caching providers (the counterpart to
//! Anthropic's explicit `cache_control`, which populates the same field
//! programmatically).

use serde::Deserialize;

use crate::types::TokenUsage;

/// OpenAI-compatible `usage` object. `#[serde(default)]` on every field so a
/// provider that omits `usage` or any sub-field still deserializes.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// OpenAI automatic prefix caching: `{ "cached_tokens": N }`.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Some gateways (notably Anthropic models routed through OpenRouter)
    /// surface a first-fill cache-write count under this Anthropic-style key.
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

impl From<WireUsage> for TokenUsage {
    fn from(w: WireUsage) -> Self {
        TokenUsage {
            prompt_tokens: w.prompt_tokens,
            completion_tokens: w.completion_tokens,
            total_tokens: w.total_tokens,
            cache_read_tokens: w
                .prompt_tokens_details
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
            cache_write_tokens: w.cache_creation_input_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_cached_tokens_from_details() {
        let json = r#"{
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "total_tokens": 1050,
            "prompt_tokens_details": { "cached_tokens": 896 }
        }"#;
        let wire: WireUsage = serde_json::from_str(json).unwrap();
        let usage: TokenUsage = wire.into();
        assert_eq!(usage.prompt_tokens, 1000);
        assert_eq!(usage.cache_read_tokens, 896);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    #[test]
    fn missing_details_is_zero_not_error() {
        let json = r#"{ "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12 }"#;
        let wire: WireUsage = serde_json::from_str(json).unwrap();
        let usage: TokenUsage = wire.into();
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.prompt_tokens, 10);
    }
}
