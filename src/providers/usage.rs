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

use serde::{Deserialize, Deserializer};

use crate::types::TokenUsage;

/// Token-count fields tolerant of the shapes real gateways actually send.
/// `#[serde(default)]` alone only covers an *absent* key — but several
/// OpenAI-compatible gateways send the key present as `null` (or occasionally a
/// float / numeric string), which makes a plain `u32` field error with
/// "invalid type: null" and fails the *whole* response parse. That bricked
/// chats on OpenRouter (GH #1). Accept null → 0, floats by truncation, and
/// numeric strings.
fn lenient_u32<'de, D>(d: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    use serde_json::Value;
    Ok(match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => 0,
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| v.min(u32::MAX as u64) as u32)
            .or_else(|| n.as_f64().map(|f| f.max(0.0).min(u32::MAX as f64) as u32))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

/// OpenAI-compatible `usage` object. Every field is optional/lenient so a
/// provider that omits `usage`, sends a sub-field as `null`, or uses a slightly
/// different numeric type still deserializes instead of failing the response.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default, deserialize_with = "lenient_u32")]
    pub prompt_tokens: u32,
    #[serde(default, deserialize_with = "lenient_u32")]
    pub completion_tokens: u32,
    #[serde(default, deserialize_with = "lenient_u32")]
    pub total_tokens: u32,
    /// OpenAI automatic prefix caching: `{ "cached_tokens": N }`.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Some gateways (notably Anthropic models routed through OpenRouter)
    /// surface a first-fill cache-write count under this Anthropic-style key.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub cache_creation_input_tokens: u32,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PromptTokensDetails {
    #[serde(default, deserialize_with = "lenient_u32")]
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

    #[test]
    fn null_token_counts_deserialize_to_zero_not_error() {
        // A gateway sending token fields (or the details object) as `null`
        // must not fail the parse — this was the GH #1 bricked-chat cause.
        let json = r#"{
            "prompt_tokens": null,
            "completion_tokens": 7,
            "total_tokens": null,
            "prompt_tokens_details": null
        }"#;
        let wire: WireUsage = serde_json::from_str(json).expect("null usage fields must parse");
        let usage: TokenUsage = wire.into();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 0);
    }

    #[test]
    fn float_and_string_token_counts_are_coerced() {
        let json = r#"{ "prompt_tokens": 12.0, "completion_tokens": "5", "total_tokens": 17 }"#;
        let wire: WireUsage = serde_json::from_str(json).expect("float/string counts must parse");
        assert_eq!(wire.prompt_tokens, 12);
        assert_eq!(wire.completion_tokens, 5);
        assert_eq!(wire.total_tokens, 17);
    }
}
