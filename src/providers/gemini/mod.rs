// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/gemini/mod.rs

//! Google Gemini native LLM provider.
//!
//! Distinct from the OpenAI-compatible client because Gemini's
//! `:generateContent` endpoint uses a meaningfully different wire shape:
//!
//! - URL embeds the model name as a path segment:
//!     `/v1beta/models/{model}:generateContent`
//!     `/v1beta/models/{model}:streamGenerateContent?alt=sse`
//! - Top-level `systemInstruction` instead of a system role.
//! - Messages live under `contents` with `role: "user" | "model"`. There
//!   is no `system` role and no `tool` role — tool responses come back
//!   as user messages with `functionResponse` parts.
//! - Each `Content` carries an array of `parts`; a part is one of
//!   `{text}`, `{functionCall: {name, args}}`, `{functionResponse:
//!   {name, response}}`, or `{inlineData: {...}}` (images, dropped
//!   here for now).
//! - Tools are declared as a single `{functionDeclarations: [...]}` block.
//! - `toolConfig.functionCallingConfig.mode` is `AUTO` / `ANY` / `NONE`;
//!   the `ANY` mode optionally narrows to `allowedFunctionNames`.
//! - Tool responses match the prior call by function NAME, not by a
//!   call_id — Gemini has no concept of a tool_use_id. We resolve the
//!   name by looking back at the preceding assistant message's
//!   `tool_calls` and matching on `tool_call_id`.
//! - Streaming SSE: each `data:` frame is a full
//!   `GenerateContentResponse` carrying possibly-partial candidates.
//!   Text accumulates across frames; `functionCall` parts always
//!   arrive complete (Gemini doesn't split args mid-stream like
//!   Anthropic's `input_json_delta`).
//! - Auth header: `x-goog-api-key`. Bearer-OAuth is also supported by
//!   the API for Vertex flows; we stick to API-key auth here.

mod client;
mod wire;

pub use client::GeminiProvider;
