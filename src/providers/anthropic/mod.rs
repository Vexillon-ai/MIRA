// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/anthropic/mod.rs

//! Anthropic native LLM provider (Claude).
//!
//! Distinct from the OpenAI-compatible client because Anthropic's
//! `/v1/messages` endpoint uses a meaningfully different wire shape:
//!
//! - `system` is a top-level request field, NOT a message with
//!   `role: system`.
//! - Messages must strictly alternate `user` / `assistant`. Multiple
//!   consecutive user messages must be merged into a single user
//!   message; same for assistant.
//! - There's no `tool` role. Tool *results* are sent as user messages
//!   with a `tool_result` content block.
//! - Assistant messages with both prose and tool calls are a single
//!   message with multiple content blocks: a `text` block plus one
//!   or more `tool_use` blocks.
//! - `max_tokens` is required.
//! - Streaming is SSE with `content_block_start` / `content_block_delta`
//!   / `content_block_stop` / `message_delta` / `message_stop` events,
//!   and tool-use input arrives as `input_json_delta` chunks that must
//!   be accumulated into a single JSON object per `content_block_stop`.
//!
//! Auth headers: `x-api-key: <key>` plus `anthropic-version: 2023-06-01`
//! (the most current stable version that's a strict superset of every
//! prior Claude release; feature dates only matter for opt-ins like
//! prompt caching, extended thinking, etc.).

mod client;
mod wire;

pub use client::AnthropicProvider;
