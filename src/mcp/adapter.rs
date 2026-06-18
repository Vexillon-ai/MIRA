// SPDX-License-Identifier: AGPL-3.0-or-later

// src/mcp/adapter.rs
//! Adapters that wrap each remote MCP tool — and, since, each
//! server's `resources/*` surface, plus, since, the
//! `prompts/*` surface — as regular MIRA [`Tool`] impls. The agent
//! + tool registry are unaware MCP exists; these adapters look
//! exactly like a builtin to everything upstream.
//!
//! Five adapter shapes today:
//! * [`McpToolAdapter`] — one per remote tool from `tools/list`.
//! * [`McpListResourcesTool`] / [`McpReadResourceTool`] —
//!   synthesised per server when the server declared the
//!   `resources` capability at initialize time.
//! * [`McpListPromptsTool`] / [`McpGetPromptTool`] — same
//!   pattern for the `prompts` capability.
//!
//! Adapters carry a `display_name` distinct from the registry key
//! when collision resolution renames them (see
//! [`McpServerRegistry::register_tools_into`] for the rule). The
//! Tool trait's `name()` returns the registered key — that's what
//! the agent invokes and what `tools_by_user` indexes on.

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::Value;
use tracing::warn;

use crate::artifacts::ArtifactStore;
use crate::MiraError;
use crate::mcp::client::{McpClient, McpToolMeta};
use crate::tools::{Tier, Tool, ToolArgs, ToolResult, ToolVisibility};

// Adapter for one remote MCP tool. Cheap to clone (everything is
// `Arc`'d). The registry constructs one per `(server, tool)` at
// startup and registers it into the agent's `ToolRegistry`.
pub struct McpToolAdapter {
    // Namespaced public name: `mcp__<server>__<tool>`.
    qualified_name: String,
    // Original tool name as the remote server reports it. We pass
    // this — not the qualified name — over the wire.
    remote_name:    String,
    description:    String,
    schema:         Value,
    client:         Arc<McpClient>,
    // Where image content blocks from tool results get saved so the UI
    // can render them (and the model isn't fed a base64 blob). `None`
    // disables that — images fall back to a short placeholder note.
    artifacts:      Option<ArtifactStore>,
}

impl McpToolAdapter {
    // Default constructor — namespaces as `mcp__<server>__<tool>`.
    // Use [`Self::with_name`] when the registry needs to disambiguate
    // a collision across users.
    pub fn new(client: Arc<McpClient>, meta: McpToolMeta) -> Self {
        let qualified = format!("mcp__{}__{}", client.server_name, meta.name);
        Self::with_name(client, meta, qualified, None)
    }
    // Collision-resolved constructor. `qualified_name`
    // becomes the value the agent invokes — typically the default
    // `mcp__<server>__<tool>` for the first owner of that
    // (server, tool) combo, or `mcp__<server>__<tool>__u<short_id>`
    // for subsequent owners. `artifacts` lets image results be saved +
    // surfaced to the UI as markdown image links.
    pub fn with_name(
        client: Arc<McpClient>,
        meta: McpToolMeta,
        qualified_name: String,
        artifacts: Option<ArtifactStore>,
    ) -> Self {
        Self {
            qualified_name,
            remote_name:    meta.name,
            description:    meta.description,
            schema:         meta.input_schema,
            client,
            artifacts,
        }
    }

    // Turn a CallToolResult JSON into the string the model sees. Text
    // blocks pass through; **image** blocks are decoded, saved to the
    // artifact store, and replaced with a `![alt](/api/artifacts/…)`
    // markdown link so the UI renders the picture and the model isn't
    // handed a giant base64 string. Falls back to pretty JSON when the
    // result has no recognisable `content` array.
    fn render_result(&self, v: &Value) -> String {
        render_call_result(v, self.artifacts.as_ref(), &self.qualified_name)
    }
}

// Turn a CallToolResult JSON into the string the model sees. Text blocks
// pass through; **image** blocks are decoded, saved to `artifacts`, and
// replaced with a `![alt](/api/artifacts/…)` markdown link so the UI
// renders the picture and the model isn't handed a giant base64 string.
// Falls back to pretty JSON when there's no recognisable `content` array.
// Free function so it's unit-testable without a live MCP client.
fn render_call_result(v: &Value, artifacts: Option<&ArtifactStore>, label: &str) -> String {
    let Some(content) = v.get("content").and_then(|c| c.as_array()) else {
        return serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    };
    let mut out = String::new();
    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
            Some("image") => {
                let data = block.get("data").and_then(|d| d.as_str()).unwrap_or("");
                let mime = block.get("mimeType").and_then(|m| m.as_str()).unwrap_or("image/png");
                out.push_str(&save_media_artifact(data, mime, artifacts, label));
                out.push('\n');
            }
            // Standard MCP audio content block — same treatment as image:
            // save the blob, hand the model a link (the UI renders a player
            // based on the extension), never the base64.
            Some("audio") => {
                let data = block.get("data").and_then(|d| d.as_str()).unwrap_or("");
                let mime = block.get("mimeType").and_then(|m| m.as_str()).unwrap_or("audio/mpeg");
                out.push_str(&save_media_artifact(data, mime, artifacts, label));
                out.push('\n');
            }
            // Resource links / embedded resources / anything else: keep a
            // compact JSON form, but never a raw base64 image blob.
            _ => {
                out.push_str(&block.to_string());
                out.push('\n');
            }
        }
    }
    let out = out.trim_end().to_string();
    if out.is_empty() {
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    } else {
        out
    }
}

// Decode a base64 image/audio/video block, persist it as an artifact, and
// return a markdown reference (`![alt](/api/artifacts/…)`). The web UI
// renders the right element (img / audio / video) from the extension. On
// any failure (no store, bad base64, disallowed type) return a short note
// instead — never the base64.
fn save_media_artifact(b64: &str, mime: &str, artifacts: Option<&ArtifactStore>, label: &str) -> String {
    let Some(store) = artifacts else {
        return "[media returned by the tool — artifact rendering not configured]".into();
    };
    let bytes = match B64.decode(b64.trim()) {
        Ok(b) => b,
        Err(e) => { warn!("mcp: media content not valid base64: {e}"); return "[media returned (undecodable)]".into(); }
    };
    match store.save_bytes(&bytes, ext_for_mime(mime)) {
        Ok(id) => id.markdown_image(&format!("{label} {}", media_kind(mime))),
        Err(e) => { warn!("mcp: could not save media artifact ({mime}): {e}"); "[media returned (could not save)]".into() }
    }
}

// Map a content `mimeType` to an artifact extension in the allowlist.
fn ext_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/png"  => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif"  => "gif",
        "image/svg+xml" => "svg",
        "image/webp" => "webp",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav"  | "audio/x-wav" | "audio/wave" => "wav",
        "audio/ogg"  | "audio/vorbis" => "ogg",
        "audio/opus" => "opus",
        "audio/mp4"  | "audio/x-m4a" | "audio/aac" => "m4a",
        "audio/flac" | "audio/x-flac" => "flac",
        "video/mp4"  => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        // Last-segment guess; save_bytes rejects anything not allowlisted.
        other => match other.rsplit('/').next().unwrap_or("png") {
            "jpeg" => "jpg",
            seg if ALLOWED_EXT.contains(&seg) => allowlist_static(seg),
            _ => "png",
        },
    }
}

const ALLOWED_EXT: &[&str] = &["png","jpg","gif","svg","webp","mp3","wav","ogg","opus","m4a","flac","mp4","webm","mov"];
fn allowlist_static(s: &str) -> &'static str {
    ALLOWED_EXT.iter().copied().find(|e| *e == s).unwrap_or("png")
}

fn media_kind(mime: &str) -> &'static str {
    if mime.starts_with("audio/") { "audio" }
    else if mime.starts_with("video/") { "video" }
    else { "image" }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn text_blocks_pass_through() {
        let v = json!({"content":[{"type":"text","text":"Navigated to https://example.com"}],"isError":false});
        assert_eq!(render_call_result(&v, None, "t"), "Navigated to https://example.com");
    }

    #[test]
    fn image_block_becomes_artifact_markdown_not_base64() {
        // 1x1 transparent PNG.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let v = json!({"content":[{"type":"image","data":png_b64,"mimeType":"image/png"}]});
        let out = render_call_result(&v, Some(&store), "mcp__puppeteer__screenshot");
        assert!(out.starts_with("!["), "should be a markdown image, got: {out}");
        assert!(out.contains("/api/artifacts/"), "should link the artifact: {out}");
        assert!(out.ends_with(".png)"), "png extension: {out}");
        assert!(!out.contains(png_b64), "must NOT leak the base64 to the model");
    }

    #[test]
    fn image_without_store_falls_back_to_note() {
        let v = json!({"content":[{"type":"image","data":"AAAA","mimeType":"image/png"}]});
        let out = render_call_result(&v, None, "t");
        assert!(out.contains("media returned"), "got: {out}");
        assert!(!out.contains("AAAA"));
    }

    #[test]
    fn audio_block_saved_as_artifact_with_audio_ext() {
        // Tiny WAV header bytes, base64'd — enough to exercise save + link.
        let wav_b64 = B64.encode(b"RIFF\x24\x00\x00\x00WAVEfmt ");
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let v = json!({"content":[{"type":"audio","data":wav_b64,"mimeType":"audio/wav"}]});
        let out = render_call_result(&v, Some(&store), "mcp__x__say");
        assert!(out.contains("/api/artifacts/") && out.ends_with(".wav)"), "audio artifact link: {out}");
        assert!(!out.contains(&wav_b64), "must not leak base64");
    }

    #[test]
    fn mime_to_ext_mapping() {
        assert_eq!(ext_for_mime("audio/mpeg"), "mp3");
        assert_eq!(ext_for_mime("audio/wav"),  "wav");
        assert_eq!(ext_for_mime("video/mp4"),  "mp4");
        assert_eq!(ext_for_mime("image/jpeg"), "jpg");
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str { &self.qualified_name }

    fn description(&self) -> &str { &self.description }

    fn args_schema(&self) -> Value { self.schema.clone() }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        match self.client.call_tool(&self.remote_name, args).await {
            Ok(v) => {
                // Render content blocks: text passes through; image blocks
                // are saved as artifacts + linked, so the model gets a
                // useful result and the UI shows the picture (rather than a
                // base64 dump the model can only describe).
                let text = self.render_result(&v);
                if v.get("isError").and_then(|e| e.as_bool()).unwrap_or(false) {
                    Ok(ToolResult::failure(text))
                } else {
                    Ok(ToolResult::success(text))
                }
            }
            Err(e) => Ok(ToolResult::failure(format!("mcp error: {e}"))),
        }
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }

    // Tools that reach an external process are network-effect tier
    // even when the server runs locally — they can touch the
    // filesystem, the network, or subprocesses on the user's
    // behalf depending on what the MCP server is. `Tier::Network`
    // is the conservative honest pick for `/api/tools` badging.
    fn tier(&self) -> Tier { Tier::Network }
}

// ── Resources adapter pair ─────────────────────────────────────────

// Per-server `mcp__<server>__list_resources` adapter. Calls
// `resources/list` on the remote server and returns the entire
// listing JSON verbatim — the agent then picks a URI to feed to
// [`McpReadResourceTool`]. Takes no arguments; servers don't
// generally accept filters here in the current MCP spec.
pub struct McpListResourcesTool {
    qualified_name: String,
    description:    String,
    client:         Arc<McpClient>,
}

impl McpListResourcesTool {
    pub fn new(client: Arc<McpClient>) -> Self {
        let server = client.server_name.clone();
        Self::with_name(
            Arc::clone(&client),
            format!("mcp__{server}__list_resources"),
        )
    }
    pub fn with_name(client: Arc<McpClient>, qualified_name: String) -> Self {
        let server = client.server_name.clone();
        Self {
            qualified_name,
            description: format!(
                "List every resource exposed by the '{server}' MCP server. \
                 Returns a JSON object with a `resources` array, each entry \
                 carrying at least `uri`, `name`, and `mimeType`. Pass a \
                 `uri` value back to the matching read_resource tool to \
                 fetch the contents."
            ),
            client,
        }
    }
}

#[async_trait]
impl Tool for McpListResourcesTool {
    fn name(&self) -> &str { &self.qualified_name }
    fn description(&self) -> &str { &self.description }
    fn args_schema(&self) -> Value {
        // No arguments — empty object schema. Some agent prompts
        // require an explicit object schema even for zero-arg tools,
        // hence the explicit `properties: {}` rather than `null`.
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
        match self.client.list_resources().await {
            Ok(v) => Ok(ToolResult::success(
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            )),
            Err(e) => Ok(ToolResult::failure(format!("mcp error: {e}"))),
        }
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn tier(&self) -> Tier { Tier::Network }
}

// Per-server `mcp__<server>__read_resource` adapter. Calls
// `resources/read` with a single `uri` argument. URIs come from the
// list-resources adapter above.
pub struct McpReadResourceTool {
    qualified_name: String,
    description:    String,
    client:         Arc<McpClient>,
}

impl McpReadResourceTool {
    pub fn new(client: Arc<McpClient>) -> Self {
        let server = client.server_name.clone();
        Self::with_name(
            Arc::clone(&client),
            format!("mcp__{server}__read_resource"),
        )
    }
    pub fn with_name(client: Arc<McpClient>, qualified_name: String) -> Self {
        let server = client.server_name.clone();
        Self {
            qualified_name,
            description: format!(
                "Read the contents of one resource from the '{server}' MCP \
                 server. Pass a `uri` from the matching list_resources \
                 tool's response. Returns the server's `contents` array \
                 verbatim (text + mimeType per chunk, or base64 `blob` \
                 for binary)."
            ),
            client,
        }
    }
}

#[async_trait]
impl Tool for McpReadResourceTool {
    fn name(&self) -> &str { &self.qualified_name }
    fn description(&self) -> &str { &self.description }
    fn args_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "uri": {
                    "type": "string",
                    "description": "The resource URI from list_resources (e.g. file:///path, notion://page/abc).",
                }
            },
            "required": ["uri"],
            "additionalProperties": false,
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let uri = match args.get("uri").and_then(|v| v.as_str()) {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => return Ok(ToolResult::failure(
                "read_resource: missing or empty `uri` argument",
            )),
        };
        match self.client.read_resource(&uri).await {
            Ok(v) => Ok(ToolResult::success(
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            )),
            Err(e) => Ok(ToolResult::failure(format!("mcp error: {e}"))),
        }
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn tier(&self) -> Tier { Tier::Network }
}


// ── Prompts adapter pair ───────────────────────────────────────────

// Per-server `mcp__<server>__list_prompts` adapter. Calls
// `prompts/list` on the remote server and returns the full listing
// JSON verbatim — each entry includes `name`, `description`, and
// (optionally) `arguments` describing the template variables.
pub struct McpListPromptsTool {
    qualified_name: String,
    description:    String,
    client:         Arc<McpClient>,
}

impl McpListPromptsTool {
    pub fn new(client: Arc<McpClient>) -> Self {
        let server = client.server_name.clone();
        Self::with_name(Arc::clone(&client), format!("mcp__{server}__list_prompts"))
    }
    pub fn with_name(client: Arc<McpClient>, qualified_name: String) -> Self {
        let server = client.server_name.clone();
        Self {
            qualified_name,
            description: format!(
                "List every prompt template exposed by the '{server}' MCP \
                 server. Returns a `prompts` array; each entry has `name`, \
                 `description`, and an optional `arguments` schema. Pass a \
                 prompt `name` back to the matching get_prompt tool to render \
                 the template into messages."
            ),
            client,
        }
    }
}

#[async_trait]
impl Tool for McpListPromptsTool {
    fn name(&self) -> &str { &self.qualified_name }
    fn description(&self) -> &str { &self.description }
    fn args_schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
        match self.client.list_prompts().await {
            Ok(v) => Ok(ToolResult::success(
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            )),
            Err(e) => Ok(ToolResult::failure(format!("mcp error: {e}"))),
        }
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn tier(&self) -> Tier { Tier::Network }
}

// Per-server `mcp__<server>__get_prompt` adapter. Calls
// `prompts/get` with a `{name, arguments?}` payload. `name` is the
// prompt id from list_prompts; `arguments` is the optional template-
// variable object the server's prompt schema declares.
pub struct McpGetPromptTool {
    qualified_name: String,
    description:    String,
    client:         Arc<McpClient>,
}

impl McpGetPromptTool {
    pub fn new(client: Arc<McpClient>) -> Self {
        let server = client.server_name.clone();
        Self::with_name(Arc::clone(&client), format!("mcp__{server}__get_prompt"))
    }
    pub fn with_name(client: Arc<McpClient>, qualified_name: String) -> Self {
        let server = client.server_name.clone();
        Self {
            qualified_name,
            description: format!(
                "Render one prompt template from the '{server}' MCP \
                 server. Pass a `name` from list_prompts plus an optional \
                 `arguments` object (the prompt's template variables). \
                 Returns the server's `messages` array — feed those into \
                 the next agent turn as the user message."
            ),
            client,
        }
    }
}

#[async_trait]
impl Tool for McpGetPromptTool {
    fn name(&self) -> &str { &self.qualified_name }
    fn description(&self) -> &str { &self.description }
    fn args_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Prompt name from list_prompts."
                },
                "arguments": {
                    "type": "object",
                    "description": "Optional template-variable object the prompt's schema declares.",
                    "additionalProperties": true
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => return Ok(ToolResult::failure(
                "get_prompt: missing or empty `name` argument",
            )),
        };
        let arguments = args.get("arguments").cloned();
        match self.client.get_prompt(&name, arguments).await {
            Ok(v) => Ok(ToolResult::success(
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            )),
            Err(e) => Ok(ToolResult::failure(format!("mcp error: {e}"))),
        }
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn tier(&self) -> Tier { Tier::Network }
}
