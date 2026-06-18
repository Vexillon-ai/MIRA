// SPDX-License-Identifier: AGPL-3.0-or-later

// src/types/tool.rs

use serde::{Deserialize, Serialize};

/// OpenAI-compatible tool specification sent to the provider so the model
/// can emit a structured `tool_calls` response. Small local models (Qwen,
/// Hermes, etc. running via LM Studio or Ollama) are trained on this exact
/// shape — *not* sending it is the single biggest reason a tool-capable
/// model falls back to hallucinated prose like "I've saved that".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String, // always "function"
    pub function: ToolFunctionSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: ToolFunctionSpec {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// Tool call from model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
    pub call_id: String,
}

impl ToolCall {
    /// Create a new tool call
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            arguments,
            call_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// Result of tool execution
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub success: bool,
    pub output: String,
}

impl ToolResult {
    /// Create a successful tool result
    pub fn success(call_id: impl Into<String>, name: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            success: true,
            output: output.into(),
        }
    }
    
    /// Create a failed tool result
    pub fn error(call_id: impl Into<String>, name: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            success: false,
            output: error.into(),
        }
    }
}
