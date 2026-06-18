// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/stream.rs
//! Events emitted by [`AgentCore::process`] during a reasoning turn.
//!
//! Callers (TUI, server handlers, tests) receive a
//! `tokio::sync::mpsc::Receiver<StreamEvent>` and can handle each event type
//! independently — collecting only tokens, displaying tool activity, or
//! forwarding everything to a browser via SSE.

use crate::types::TokenUsage;

/// A single event in an agent response stream.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A text token produced by the model.
    Token(String),

    /// The agent is invoking a tool.
    ToolCall {
        /// Tool name from the registry.
        name: String,
        /// JSON-serialised arguments passed to the tool.
        args: String,
        /// Opaque call identifier (matches the subsequent `ToolResult`).
        call_id: String,
    },

    /// Result returned from a tool execution.
    ToolResult {
        /// Tool name.
        name: String,
        /// Tool output (plain text or JSON).
        output: String,
        /// Whether the tool succeeded.
        success: bool,
        /// Opaque call identifier matching the preceding `ToolCall`.
        call_id: String,
    },

    /// Wiki pages injected into this turn's system context (Slice H).
    /// Emitted once, before the first `Token`, when the wiki hook
    /// produced any context. The chat UI uses this to render context
    /// pills under the assistant message.
    WikiContext { pages: Vec<String> },

    /// Private chain-of-thought / extended-thinking content surfaced by
    /// reasoning models (DeepSeek R1, xAI Grok-3-mini, Anthropic Claude
    /// extended thinking, etc.). May fire multiple times during a
    /// multi-round tool loop (one per `provider.generate*()` call that
    /// returned reasoning). Always arrives AFTER the round's `Token`
    /// or `ToolCall` events and BEFORE any subsequent round's
    /// `ToolResult`, so the UI can attach reasoning to the right
    /// response chunk. Not interleaved with `Token` events to keep the
    /// live answer stream uncluttered.
    Reasoning(String),

    /// The agent has finished reasoning; all tokens have been emitted.
    Done { usage: TokenUsage },

    /// A non-fatal error occurred during streaming (e.g. tool execution
    /// failed but the agent recovered). The stream continues.
    Warning(String),

    /// A fatal error terminated the reasoning loop early.
    Error(String),
}

impl StreamEvent {
    /// Return the contained token text, if any.
    pub fn as_token(&self) -> Option<&str> {
        match self {
            Self::Token(t) => Some(t),
            _              => None,
        }
    }

    /// Collect all `Token` events from an iterator into a `String`.
    pub fn collect_tokens(events: impl IntoIterator<Item = Self>) -> String {
        events.into_iter().filter_map(|e| {
            if let Self::Token(t) = e { Some(t) } else { None }
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_token_returns_text_for_token_event() {
        let e = StreamEvent::Token("hello".to_string());
        assert_eq!(e.as_token(), Some("hello"));
    }

    #[test]
    fn as_token_returns_none_for_non_token() {
        let e = StreamEvent::Done { usage: TokenUsage::default() };
        assert_eq!(e.as_token(), None);
    }

    #[test]
    fn collect_tokens_filters_non_tokens() {
        let events = vec![
            StreamEvent::Token("foo".to_string()),
            StreamEvent::Warning("w".to_string()),
            StreamEvent::Token(" bar".to_string()),
            StreamEvent::Done { usage: TokenUsage::default() },
        ];
        assert_eq!(StreamEvent::collect_tokens(events), "foo bar");
    }
}
