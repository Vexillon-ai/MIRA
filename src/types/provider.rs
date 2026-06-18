// SPDX-License-Identifier: AGPL-3.0-or-later

// src/types/provider.rs

use serde::{Deserialize, Serialize};
use crate::types::TokenUsage;

/// Provider identifier
#[derive(Debug, Clone, PartialEq, Hash, Serialize, Deserialize)]
pub enum ProviderId {
    #[serde(rename = "local")]
    Local(String),      // "ollama/llama3"

    #[serde(rename = "openrouter")]
    OpenRouter(String), // "openrouter/gpt-4o"

    /// Generic OpenAI-compatible providers (OpenAI, DeepSeek, Kimi/Moonshot,
    /// Groq, xAI, Azure OpenAI, vLLM-self-hosted, …). The shared client
    /// in `providers::openai_compat` returns this variant; the `provider`
    /// field is the slug used in config (`"openai"`, `"deepseek"`, etc.)
    /// and `model` is the model id within that provider.
    #[serde(rename = "openai_compat")]
    OpenAiCompat { provider: String, model: String },

    /// Anthropic native provider (Claude). Distinct from OpenAiCompat
    /// because Anthropic's `/v1/messages` API uses a different wire
    /// shape (system as top-level, tool_use as content blocks, etc.).
    #[serde(rename = "anthropic")]
    Anthropic(String), // "claude-sonnet-4-5"

    /// Google Gemini native provider. Distinct from OpenAiCompat
    /// because Gemini's :generateContent endpoint uses contents/parts
    /// with role:"user"/"model", functionCall/functionResponse parts,
    /// and systemInstruction as a top-level field.
    #[serde(rename = "gemini")]
    Gemini(String), // "gemini-2.5-pro"
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderId::Local(name) => write!(f, "local/{}", name),
            ProviderId::OpenRouter(name) => write!(f, "openrouter/{}", name),
            ProviderId::OpenAiCompat { provider, model } => {
                write!(f, "{}/{}", provider, model)
            }
            ProviderId::Anthropic(model) => write!(f, "anthropic/{}", model),
            ProviderId::Gemini(model)    => write!(f, "gemini/{}", model),
        }
    }
}

/// Generation options for model requests
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationOptions {
    #[serde(default = "default_temperature")]
    pub temperature: f32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,

    /// OpenAI-compatible tool specs. When present, the provider serializes
    /// these into the request body as `"tools": [...]` so the model can emit
    /// structured `tool_calls` in its response. When `None`, no tools are
    /// advertised and the provider omits the field entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<crate::types::ToolSpec>>,

    /// OpenAI-compatible `tool_choice`: `"auto"`, `"none"`, `"required"`,
    /// or `{"type":"function","function":{"name":"..."}}`. Only meaningful
    /// when `tools` is `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,

    /// Reasoning-effort hint for reasoning-capable models. `"low"` | `"medium"`
    /// | `"high"`. Flattened into OpenAI-compatible request bodies as
    /// `reasoning_effort`; the Anthropic wire layer maps it to a `thinking`
    /// token budget. `None` → the provider's default (no extended thinking).
    /// Set by reasoning auto-routing (roadmap #13) when a turn is routed up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

fn default_temperature() -> f32 { 0.7 }

impl Default for GenerationOptions {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            max_tokens: None,
            top_p: None,
            stop_sequences: None,
            tools: None,
            tool_choice: None,
            reasoning_effort: None,
        }
    }
}

/// Response from model generation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationResponse {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<crate::types::ToolCall>>,
    /// Private chain-of-thought / extended-thinking content emitted by
    /// reasoning models (DeepSeek R1's `reasoning_content`, xAI
    /// Grok-3-mini, Anthropic's `thinking` blocks, OpenAI o-series
    /// reasoning summaries). Returned alongside `content` rather than
    /// concatenated so the UI can present it separately (collapsible,
    /// styled differently) — most users want the answer, not the
    /// scratchpad. `None` for providers that don't emit reasoning or
    /// when the request didn't opt in to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    pub usage: TokenUsage,
    pub provider_id: ProviderId,
    pub model_name: String,
    /// Set when this response did NOT come from the requested provider — the
    /// primary failed and a failover provider answered instead. The agent
    /// loop surfaces this as a `StreamEvent::Warning` so the user knows their
    /// configured model was bypassed. `None` on the normal (primary) path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<FallbackNotice>,
}

/// Records a silent provider failover so it can be surfaced to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackNotice {
    /// The provider that was requested/primary and failed.
    pub from: String,
    /// The provider that actually answered.
    pub to: String,
    /// Short reason the primary failed (e.g. "401 Unauthorized").
    pub reason: String,
}

/// Configuration for a provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: ProviderId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,  // None for local providers
    pub default_model: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_timeout() -> u64 { 120 }
fn default_max_retries() -> u32 { 3 }
