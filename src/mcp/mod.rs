// SPDX-License-Identifier: AGPL-3.0-or-later

// src/mcp/mod.rs
//! MCP host (Q2 #7,).
//!
//! MIRA acts as an MCP **client** here: at startup we spawn each
//! enabled `[mcp.servers.*]` entry from the config as a stdio child
//! process, run the `initialize` + `tools/list` handshake via the
//! official `rmcp` crate, and wrap each remote tool in an
//! [`adapter::McpToolAdapter`] that implements the regular
//! [`crate::tools::Tool`] trait. Those adapters are registered with
//! the agent's `ToolRegistry` under the namespace
//! `mcp__<server>__<tool>`, so the LLM treats them like any builtin.
//!
//! Out of scope in this slice: Streamable-HTTP transport,
//! `resources/*` + live reconnect, per-user server lists
//!, prompts + sampling. Adding them later only
//! grows the existing structs; nothing in the v1 wire format needs
//! to be revisited.

pub mod adapter;
pub mod browser;
pub mod catalog;
pub mod client;
pub mod handler;
pub mod legacy_migrate;
pub mod registry;
pub mod store;

pub use adapter::{
    McpGetPromptTool, McpListPromptsTool,
    McpListResourcesTool, McpReadResourceTool, McpToolAdapter,
};
pub use catalog::{McpCatalogEntry, McpCatalogStore, UpsertCatalogEntry};
pub use client::{McpClient, McpToolMeta};
pub use handler::McpClientHandler;
pub use registry::{McpServerRegistry, McpServerStatus, McpToolInfo};
pub use store::{McpServerRow, McpServerStore, NewMcpServer, UpdateMcpServer};
