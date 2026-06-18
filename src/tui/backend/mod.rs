// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/backend/mod.rs
//! Pluggable TUI backends.
//!
//! `run_inner` drives the UI through one of these instead of talking to
//! `AgentCore` directly, so the same loop can be powered by either the
//! in-process `LocalBackend` (behavior) or the `ServerBackend`
//! (HTTP/SSE client against a MIRA server).

pub mod local;
pub mod server;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedReceiver;

use mira::agent::stream::StreamEvent;

// A started turn: the conversation id the backend is using (may be newly
// created if the caller passed `None`) plus the stream of events to relay
// to the UI.
// // The UI must treat `conv_id` as authoritative — both backends may create
// a conversation synchronously before the first token arrives, and the UI
// should adopt the returned id for subsequent turns.
pub struct TurnHandle {
    pub conv_id: Option<String>,
    pub rx:      UnboundedReceiver<StreamEvent>,
}

// Role tag for a replayed message. Uses a tiny local enum rather than
// leaking `mira::history::MessageRole` across the trait, so `ServerBackend`
// which only ever sees string roles from JSON — doesn't need a dependency
// on the history crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumedRole {
    User,
    Assistant,
    System,
}

// Subset of a past conversation loaded for the `tui.resume_last` feature.
pub struct ResumedConversation {
    pub conv_id:  String,
    pub messages: Vec<(ResumedRole, String)>,
}

// Lightweight view of a single memory row, sized for TUI listing — the
// backend converts whatever richer record the source store uses (local
// `MemoryItem` or server `MemoryResponse`) into this shape.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub id:       u64,
    pub content:  String,
    pub category: String,
}

// Name + human description for a single registered tool. Returned by
// `list_tools_detailed`. Backends convert whatever richer record the
// source (local `ToolRegistry` or server `ToolInfo` JSON) uses.
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name:        String,
    pub description: String,
}

// Result of a single tool invocation, projected from `ToolResult` for the
// TUI. Keeps the same `success / output / error` shape as the underlying
// `tools::ToolResult` so the UI can render success and failure uniformly.
#[derive(Debug, Clone)]
pub struct ToolExecOutcome {
    pub success: bool,
    pub output:  String,
    pub error:   Option<String>,
}

// Compact view of one OpenRouter catalog entry — only the fields the TUI
// renders. Pricing values are USD per token; zero means "no per-token charge".
#[derive(Debug, Clone)]
pub struct CatalogModel {
    pub id:             String,
    pub name:           String,
    pub context_length: u64,
    pub modality:       String,
    pub price_prompt:     f64,
    pub price_completion: f64,
    pub price_request:    f64,
}

// Snapshot returned by `fetch_openrouter_catalog`. `fetched_at` is unix
// seconds; the caller decides whether to surface "served from cache".
#[derive(Debug, Clone)]
pub struct CatalogSnapshot {
    pub fetched_at: u64,
    pub models:     Vec<CatalogModel>,
}

impl CatalogSnapshot {
    pub fn find(&self, id: &str) -> Option<&CatalogModel> {
        self.models.iter().find(|m| m.id == id)
    }
}

#[async_trait]
pub trait TuiBackend: Send + Sync {
    // Cheap connectivity probe used for the status bar indicator.
    async fn health_check(&self) -> bool;

    // Number of tools registered in the active provider/registry. Shown on
    // the status bar and in `/tool-list`.
    async fn tool_count(&self) -> usize;

    // Number of memories the backend reports. Shown on the status bar.
    async fn memory_count(&self) -> usize;

    // Start a turn. The backend is responsible for:
    // * creating a conversation row if `conv_id` is `None`,
    // * persisting the user message before streaming,
    // * streaming tokens back via the returned receiver,
    // * persisting the assistant message when the stream completes.
    //     // `model` / `provider` are persisted as metadata on the user / assistant
    // messages; they're passed explicitly because the UI owns the "current
    // model" label and may have overridden it via a slash command.
    async fn send_message(
        &self,
        conv_id:  Option<String>,
        msg:      String,
        model:    String,
        provider: String,
    ) -> Result<TurnHandle, String>;

    // Load the tail of the most-recent `tui` conversation for resume-on-open.
    // Default returns `None`; backends opt in.
    //     // `limit` caps the number of messages returned. Callers should request a
    // small window (e.g. 20) since the goal is continuity, not a full replay.
    async fn fetch_last_tui_conversation(&self, _limit: usize) -> Option<ResumedConversation> {
        None
    }

    // List stored memories (most recent first). Implementations should cap
    // the result at `limit`. Errors are surfaced as an `Err(String)` so the
    // UI can show a system message instead of a silent empty list.
    async fn list_memories(&self, limit: usize) -> Result<Vec<MemoryEntry>, String>;

    // Keyword/semantic search — backends pick whichever they support.
    async fn search_memories(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, String>;

    // Store a new memory; returns the assigned id.
    async fn store_memory(&self, content: String) -> Result<u64, String>;

    // Delete a memory by id. Returns `Ok(false)` when the id is unknown
    // (matches `MemorySystem::delete` / HTTP 404) so the UI can render
    // "not found" separately from transport errors.
    async fn delete_memory(&self, id: u64) -> Result<bool, String>;

    // List all registered tools with name + description. Used by
    // `/tool-list` to show the user what's available.
    async fn list_tools_detailed(&self) -> Result<Vec<ToolInfo>, String>;

    // Execute a tool by name with JSON arguments. Transport/registry errors
    // surface as `Err(String)`; tool-level failures come back as
    // `Ok(ToolExecOutcome { success: false, error: Some(..),.. })` so the
    // UI can distinguish them from a broken connection.
    async fn run_tool(
        &self,
        name: String,
        args: serde_json::Value,
    ) -> Result<ToolExecOutcome, String>;

    // Fetch the OpenRouter model catalog. `force = true` bypasses the disk
    // cache. Backends without OpenRouter configured should return
    // `Err("OpenRouter not configured")` so the UI can render a hint.
    async fn fetch_openrouter_catalog(
        &self,
        force: bool,
    ) -> Result<CatalogSnapshot, String>;
}
