// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/stream.rs
//! Events emitted by [`AgentCore::process`] during a reasoning turn.
//!
//! Callers (TUI, server handlers, tests) receive a
//! `tokio::sync::mpsc::Receiver<StreamEvent>` and can handle each event type
//! independently — collecting only tokens, displaying tool activity, or
//! forwarding everything to a browser via SSE.

use crate::types::TokenUsage;

/// Per-turn latency measurements. Emitted twice: a
/// `first_token` snapshot (the pre-token breakdown + time-to-first-token) right
/// before the first `Token`, and a `final` snapshot at `Done`. Fields not
/// relevant to a snapshot are `None`, so only present fields are serialised.
/// Measurement only — nothing here changes the pipeline.
#[derive(Debug, Clone, Default)]
pub struct TurnTimings {
    /// Which snapshot this is: `"first_token"` or `"final"`.
    pub phase: &'static str,
    // ── first_token snapshot ──
    pub memory_recall_ms: Option<u64>,
    pub wiki_ms: Option<u64>,
    pub tool_selection_ms: Option<u64>,
    pub prompt_build_ms: Option<u64>,
    pub llm_request_to_first_token_ms: Option<u64>,
    pub total_to_first_token_ms: Option<u64>,
    // ── final snapshot ──
    pub total_ms: Option<u64>,
    pub tool_rounds: Option<u32>,
    pub llm_tokens_out: Option<u32>,
    // ── cache ──
    pub prompt_tokens: Option<u32>,
    pub cached_tokens: Option<u32>,
}

/// A single event in an agent response stream.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Opens every turn, before any other event. Carries the
    /// turn's stable id (also sent as the `X-Mira-Turn-Id` response header) so a
    /// client can cancel / commit it mid-flight, and the resolved channel. SSE
    /// clients that don't know the event ignore it; it never replaces or reorders
    /// the existing events, so older clients are unaffected.
    TurnStarted { turn_id: String, channel: String },

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

    /// Memory providers that fed (or failed) this turn's recall (Part B2d).
    /// Emitted once, before the first `Token`, when any memory provider
    /// served facts or was unavailable. `served` are provider ids that
    /// contributed facts; `unavailable` are provider ids that could not be
    /// reached. The chat UI renders per-source pills + a read/unavailable
    /// badge from this.
    MemoryContext { served: Vec<String>, unavailable: Vec<String> },

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

    /// Per-turn latency snapshot. Emitted only when timings
    /// are on (the `voice` channel, or `debug_timings`): a `first_token`
    /// snapshot just before the first `Token`, and a `final` one at `Done`.
    Timing(TurnTimings),

    /// The agent has finished reasoning; all tokens have been emitted.
    Done { usage: TokenUsage },

    /// The turn was cancelled mid-flight (barge-in or an
    /// explicit cancel request). Terminal, emitted in place of `Done`: no
    /// further `Token`/`Done` follows. The partial reply already streamed stands;
    /// the chat handler persists the truncated/spoken turn from the cancel
    /// request's payload.
    Cancelled { turn_id: String },

    /// A speculative turn was committed with a CHANGED message:
    /// the speculative work is discarded and the turn restarts under the same
    /// `turn_id` with the corrected message. The client should drop whatever it
    /// streamed so far; fresh events follow for the restarted turn.
    Restarted { turn_id: String },

    /// The voice tool-round cap was reached: the
    /// turn used `rounds_used` tool rounds and is now answering from what it has
    /// (one final tools-disabled round). Surfaced as the `voice_tool_cap_reached`
    /// SSE event so the voice client can show a holding phrase.
    ToolCapReached { rounds_used: u32 },

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
