// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/backend/server.rs
//! HTTP/SSE backend. The TUI speaks to a running MIRA server over the same
//! API the web UI uses, so sessions, history, memory, and tool state are
//! unified across channels.
//!
//! All calls go out with a bearer token. For same-host setups the server
//! mints a long-lived token at startup (see `Gateway::run_until_shutdown`);
//! for remote setups the token comes from `MIRA_TOKEN`.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedSender};

use mira::agent::stream::StreamEvent;
use mira::types::TokenUsage;

use super::{
    CatalogModel, CatalogSnapshot, MemoryEntry, ResumedConversation, ResumedRole,
    ToolExecOutcome, ToolInfo, TuiBackend, TurnHandle,
};

pub struct ServerBackend {
    client:   Client,
    base_url: String,
    token:    String,
}

impl ServerBackend {
    /// Build a client with a reasonable default connect timeout. The request
    /// timeout is set to `None` for `send_message` (SSE can be long-lived);
    /// every other call uses a 10-second timeout.
    pub fn new(base_url: String, token: String) -> Result<Self, String> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("reqwest client: {}", e))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

#[async_trait]
impl TuiBackend for ServerBackend {
    async fn health_check(&self) -> bool {
        match self
            .client
            .get(self.url("/health"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(r)  => r.status().is_success(),
            Err(_) => false,
        }
    }

    async fn tool_count(&self) -> usize {
        let Ok(resp) = self
            .client
            .get(self.url("/api/tools"))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        else { return 0; };
        if !resp.status().is_success() { return 0; }
        resp.json::<Vec<serde_json::Value>>()
            .await
            .map(|v| v.len())
            .unwrap_or(0)
    }

    async fn memory_count(&self) -> usize {
        // /api/status exposes `memory_count` directly; cheaper than listing.
        let Ok(resp) = self
            .client
            .get(self.url("/api/status"))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        else { return 0; };
        if !resp.status().is_success() { return 0; }
        #[derive(Deserialize)]
        struct StatusMem { memory_count: Option<usize> }
        resp.json::<StatusMem>()
            .await
            .ok()
            .and_then(|s| s.memory_count)
            .unwrap_or(0)
    }

    async fn send_message(
        &self,
        conv_id:  Option<String>,
        msg:      String,
        model:    String,
        provider: String,
    ) -> Result<TurnHandle, String> {
        // If the UI doesn't have a conv_id yet, ask the server to create one
        // up-front. This mirrors `LocalBackend` semantics — the UI learns
        // the conv_id synchronously and subsequent messages target the same
        // conversation even if the stream drops.
        let conv_id = match conv_id {
            Some(id) => id,
            None => create_conversation(&self.client, &self.base_url, &self.token, &msg, &model, &provider).await?,
        };

        let req = ChatRequest {
            conversation_id:   Some(conv_id.clone()),
            message:           msg,
            model_override:    Some(model),
            provider_override: Some(provider),
            attachments:       None,
        };

        let resp = self
            .client
            .post(self.url("/api/chat"))
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("POST /api/chat: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body   = resp.text().await.unwrap_or_default();
            return Err(format!("/api/chat returned {}: {}", status, body.trim()));
        }

        let (tx, rx) = mpsc::unbounded_channel::<StreamEvent>();
        tokio::spawn(forward_sse(resp, tx));

        Ok(TurnHandle { conv_id: Some(conv_id), rx })
    }

    async fn list_memories(&self, limit: usize) -> Result<Vec<MemoryEntry>, String> {
        let rows: Vec<MemoryRow> = self
            .client
            .get(self.url("/api/memory"))
            .bearer_auth(&self.token)
            .query(&[("limit", limit.to_string().as_str())])
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("GET /api/memory: {}", e))?
            .error_for_status()
            .map_err(|e| format!("GET /api/memory: {}", e))?
            .json()
            .await
            .map_err(|e| format!("parse /api/memory: {}", e))?;
        Ok(rows.into_iter().map(MemoryEntry::from).collect())
    }

    async fn search_memories(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, String> {
        // Prefer the keyword-style list endpoint: semantic search requires
        // embeddings and the server's /search path may not always be wired
        // up, while /api/memory?q= always works.
        let rows: Vec<MemoryRow> = self
            .client
            .get(self.url("/api/memory"))
            .bearer_auth(&self.token)
            .query(&[
                ("q",     query.to_owned()),
                ("limit", limit.to_string()),
            ])
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("GET /api/memory?q: {}", e))?
            .error_for_status()
            .map_err(|e| format!("GET /api/memory?q: {}", e))?
            .json()
            .await
            .map_err(|e| format!("parse /api/memory?q: {}", e))?;
        Ok(rows.into_iter().map(MemoryEntry::from).collect())
    }

    async fn store_memory(&self, content: String) -> Result<u64, String> {
        let row: MemoryRow = self
            .client
            .post(self.url("/api/memory"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "content": content }))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("POST /api/memory: {}", e))?
            .error_for_status()
            .map_err(|e| format!("POST /api/memory: {}", e))?
            .json()
            .await
            .map_err(|e| format!("parse POST /api/memory: {}", e))?;
        Ok(row.id)
    }

    async fn delete_memory(&self, id: u64) -> Result<bool, String> {
        let resp = self
            .client
            .delete(self.url(&format!("/api/memory/{}", id)))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("DELETE /api/memory/{}: {}", id, e))?;
        match resp.status().as_u16() {
            204       => Ok(true),
            404       => Ok(false),
            other     => Err(format!("DELETE /api/memory/{} returned {}", id, other)),
        }
    }

    async fn list_tools_detailed(&self) -> Result<Vec<ToolInfo>, String> {
        let rows: Vec<ToolInfoRow> = self
            .client
            .get(self.url("/api/tools"))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("GET /api/tools: {}", e))?
            .error_for_status()
            .map_err(|e| format!("GET /api/tools: {}", e))?
            .json()
            .await
            .map_err(|e| format!("parse /api/tools: {}", e))?;
        Ok(rows.into_iter().map(ToolInfo::from).collect())
    }

    async fn run_tool(
        &self,
        name: String,
        args: serde_json::Value,
    ) -> Result<ToolExecOutcome, String> {
        // Tools can be slow (shell commands, file I/O). Give them room —
        // the server still enforces its own per-tool timeout.
        let resp = self
            .client
            .post(self.url("/api/tools/run"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "name": name, "args": args }))
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("POST /api/tools/run: {}", e))?;

        let status = resp.status();
        // 404 → unknown tool. Surface as transport-style error since the UI
        // treats "not found" like a configuration problem (the user typed a
        // name that doesn't exist) rather than a successful "tool failed".
        if status.as_u16() == 404 {
            return Err(format!("Unknown tool: {}", name));
        }

        let row: ToolExecRow = resp
            .json()
            .await
            .map_err(|e| format!("parse /api/tools/run: {}", e))?;

        // 200 → success/failure determined by the `success` field.
        // 500 → registry-level error, server returns a ToolResult shape we
        // can still parse; fold into Err so the UI shows it as a transport
        // failure rather than a tool-level one.
        if status.as_u16() >= 500 {
            return Err(row.error.unwrap_or_else(|| "tool execution failed".to_string()));
        }

        Ok(ToolExecOutcome {
            success: row.success,
            output:  row.output,
            error:   row.error,
        })
    }

    async fn fetch_openrouter_catalog(&self, force: bool) -> Result<CatalogSnapshot, String> {
        let url = self.url("/api/providers/openrouter/models");
        let resp = self.client
            .get(&url)
            .bearer_auth(&self.token)
            .query(&[("refresh", if force { "1" } else { "0" })])
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("GET /api/providers/openrouter/models: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body   = resp.text().await.unwrap_or_default();
            return Err(format!("openrouter models returned {status}: {}", body.trim()));
        }
        let body: CatalogResp = resp.json().await
            .map_err(|e| format!("parse openrouter models: {e}"))?;
        Ok(CatalogSnapshot {
            fetched_at: body.fetched_at,
            models: body.models.into_iter().map(|m| CatalogModel {
                id:               m.id,
                name:             m.name,
                context_length:   m.context_length,
                modality:         m.modality,
                price_prompt:     m.pricing.prompt,
                price_completion: m.pricing.completion,
                price_request:    m.pricing.request,
            }).collect(),
        })
    }

    async fn fetch_last_tui_conversation(&self, limit: usize) -> Option<ResumedConversation> {
        #[derive(Deserialize)]
        struct ConvSummary { id: String }
        #[derive(Deserialize)]
        struct MsgRow { role: String, content: String }

        let convs: Vec<ConvSummary> = self
            .client
            .get(self.url("/api/conversations"))
            .bearer_auth(&self.token)
            .query(&[("channel", "tui"), ("limit", "1")])
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let conv_id = convs.into_iter().next()?.id;

        let msgs: Vec<MsgRow> = self
            .client
            .get(self.url(&format!("/api/conversations/{}/messages", conv_id)))
            .bearer_auth(&self.token)
            .query(&[("limit", limit.to_string().as_str())])
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;

        let messages = msgs
            .into_iter()
            .map(|m| {
                let role = match m.role.as_str() {
                    "user"      => ResumedRole::User,
                    "assistant" => ResumedRole::Assistant,
                    _           => ResumedRole::System,
                };
                (role, m.content)
            })
            .collect();

        Some(ResumedConversation { conv_id, messages })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest {
    conversation_id:   Option<String>,
    message:           String,
    model_override:    Option<String>,
    provider_override: Option<String>,
    attachments:       Option<Vec<serde_json::Value>>,
}

#[derive(Serialize)]
struct CreateConvRequest<'a> {
    channel:  &'a str,
    title:    String,
    model:    &'a str,
    provider: &'a str,
}

#[derive(Deserialize)]
struct ConversationResponse {
    id: String,
}

/// Shape of `MemoryResponse` from `src/server/handlers/memory.rs`. We decode
/// only the three fields the TUI displays — the rest (tags, timestamps, score)
/// are ignored via serde's default behaviour.
#[derive(Deserialize)]
struct MemoryRow {
    id:       u64,
    content:  String,
    category: String,
}

impl From<MemoryRow> for MemoryEntry {
    fn from(r: MemoryRow) -> Self {
        Self { id: r.id, content: r.content, category: r.category }
    }
}

/// Shape of `ToolInfo` from `src/server/handlers/tools.rs`.
#[derive(Deserialize)]
struct ToolInfoRow {
    name:        String,
    description: String,
}

impl From<ToolInfoRow> for ToolInfo {
    fn from(r: ToolInfoRow) -> Self {
        Self { name: r.name, description: r.description }
    }
}

/// Shape of `ToolResult` returned by `/api/tools/run`.
#[derive(Deserialize)]
struct ToolExecRow {
    success: bool,
    #[serde(default)]
    output:  String,
    #[serde(default)]
    error:   Option<String>,
}

// ── OpenRouter catalog wire shape ──────────────────────────────────────
// Mirrors `server::handlers::providers::CatalogResponse` and
// `providers::openrouter::CatalogEntry` — kept local to avoid pulling the
// full provider-side types across the trait.

#[derive(Deserialize)]
struct CatalogResp {
    fetched_at: u64,
    #[serde(default)]
    models:     Vec<CatalogEntryRow>,
}

#[derive(Deserialize)]
struct CatalogEntryRow {
    id:             String,
    name:           String,
    #[serde(default)] context_length: u64,
    #[serde(default)] modality:       String,
    pricing:        PricingRow,
}

#[derive(Deserialize, Default)]
struct PricingRow {
    #[serde(default)] prompt:     f64,
    #[serde(default)] completion: f64,
    #[serde(default)] request:    f64,
}

async fn create_conversation(
    client:   &Client,
    base_url: &str,
    token:    &str,
    msg:      &str,
    model:    &str,
    provider: &str,
) -> Result<String, String> {
    let body = CreateConvRequest {
        channel:  "tui",
        title:    truncate_title(msg),
        model,
        provider,
    };
    let resp = client
        .post(format!("{}/api/conversations", base_url))
        .bearer_auth(token)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("POST /api/conversations: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        return Err(format!("/api/conversations returned {}: {}", status, body.trim()));
    }
    let conv: ConversationResponse = resp
        .json()
        .await
        .map_err(|e| format!("parse conversation response: {}", e))?;
    Ok(conv.id)
}

fn truncate_title(msg: &str) -> String {
    let trimmed = msg.trim();
    let first_line = trimmed.lines().next().unwrap_or(trimmed);
    if first_line.chars().count() <= 80 {
        first_line.to_owned()
    } else {
        let cut: String = first_line.chars().take(77).collect();
        format!("{}...", cut)
    }
}

/// Consume the SSE response and forward parsed events to `tx`. Closes `tx`
/// on `done`, `error`, or transport failure.
async fn forward_sse(resp: reqwest::Response, tx: UnboundedSender<StreamEvent>) {
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b)  => b,
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(format!("sse transport: {}", e)));
                return;
            }
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));

        // Events are separated by blank lines ("\n\n"). Drain every complete
        // event the buffer currently contains.
        while let Some(idx) = buf.find("\n\n") {
            let raw = buf[..idx].to_owned();
            buf.drain(..idx + 2);

            if let Some(ev) = parse_sse_event(&raw) {
                let stop = matches!(ev, StreamEvent::Done { .. } | StreamEvent::Error(_));
                let _ = tx.send(ev);
                if stop { return; }
            }
        }
    }

    // Stream ended without a `done` event — surface as error so the UI
    // stops spinning and the user sees something.
    let _ = tx.send(StreamEvent::Error("stream closed unexpectedly".into()));
}

/// Parse a single SSE event block (no trailing blank line) into a
/// `StreamEvent`. Returns `None` on malformed input or unknown event names.
fn parse_sse_event(raw: &str) -> Option<StreamEvent> {
    let mut event: Option<&str> = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim());
        } else if let Some(rest) = line.strip_prefix("data:") {
            // Per SSE spec, data: lines may be concatenated with '\n'.
            // A leading space after the colon is optional and stripped.
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // ignore id:, retry:, comments (":..."), and unknown fields
    }

    let event = event?;
    let data  = data_lines.join("\n");

    match event {
        "token"   => Some(StreamEvent::Token(data)),
        "warning" => Some(StreamEvent::Warning(data)),
        "error"   => Some(StreamEvent::Error(data)),
        "done"    => {
            // `data` is JSON like `{"conversation_id":"...","usage":{...}}`.
            // We extract usage when present so the per-turn cost footer can
            // be rendered without a second round-trip; missing/old payloads
            // fall back to zeroed usage and the footer shows tokens only.
            #[derive(Deserialize)]
            struct DoneData {
                #[serde(default)]
                usage: Option<TokenUsage>,
            }
            let usage = serde_json::from_str::<DoneData>(&data)
                .ok()
                .and_then(|d| d.usage)
                .unwrap_or_default();
            Some(StreamEvent::Done { usage })
        }
        "tool_call" => {
            #[derive(Deserialize)]
            struct Payload { tool: String, args: Option<String> }
            let p: Payload = serde_json::from_str(&data).ok()?;
            Some(StreamEvent::ToolCall {
                name:    p.tool,
                args:    p.args.unwrap_or_default(),
                call_id: String::new(),
            })
        }
        "tool_result" => {
            #[derive(Deserialize)]
            struct Payload { tool: String, output: Option<String>, success: Option<bool> }
            let p: Payload = serde_json::from_str(&data).ok()?;
            Some(StreamEvent::ToolResult {
                name:    p.tool,
                output:  p.output.unwrap_or_default(),
                success: p.success.unwrap_or(false),
                call_id: String::new(),
            })
        }
        _ => None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_event() {
        let ev = parse_sse_event("event: token\ndata: hello world").unwrap();
        match ev {
            StreamEvent::Token(t) => assert_eq!(t, "hello world"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_token_event_with_space_stripped() {
        let ev = parse_sse_event("event:token\ndata: leading-space").unwrap();
        match ev {
            StreamEvent::Token(t) => assert_eq!(t, "leading-space"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_multiline_data_joined_with_newline() {
        let ev = parse_sse_event("event: token\ndata: line1\ndata: line2").unwrap();
        match ev {
            StreamEvent::Token(t) => assert_eq!(t, "line1\nline2"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_done_event() {
        let ev = parse_sse_event("event: done\ndata: {\"conversation_id\":\"abc\"}").unwrap();
        assert!(matches!(ev, StreamEvent::Done { .. }));
    }

    #[test]
    fn parse_error_event() {
        let ev = parse_sse_event("event: error\ndata: bad things").unwrap();
        match ev {
            StreamEvent::Error(e) => assert_eq!(e, "bad things"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_tool_call_event() {
        let raw = "event: tool_call\ndata: {\"type\":\"call\",\"tool\":\"shell\",\"args\":\"{}\"}";
        match parse_sse_event(raw).unwrap() {
            StreamEvent::ToolCall { name, args, .. } => {
                assert_eq!(name, "shell");
                assert_eq!(args, "{}");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_tool_result_event() {
        let raw = "event: tool_result\ndata: {\"type\":\"result\",\"tool\":\"shell\",\"success\":true,\"output\":\"hi\"}";
        match parse_sse_event(raw).unwrap() {
            StreamEvent::ToolResult { name, output, success, .. } => {
                assert_eq!(name, "shell");
                assert_eq!(output, "hi");
                assert!(success);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn unknown_event_returns_none() {
        assert!(parse_sse_event("event: heartbeat\ndata: ping").is_none());
    }

    #[test]
    fn event_without_name_returns_none() {
        assert!(parse_sse_event("data: lone data line").is_none());
    }

    #[test]
    fn comment_lines_ignored() {
        let ev = parse_sse_event(": keep-alive\nevent: token\ndata: x").unwrap();
        assert!(matches!(ev, StreamEvent::Token(ref t) if t == "x"));
    }

    #[test]
    fn truncate_title_keeps_short_lines() {
        assert_eq!(truncate_title("hello world"), "hello world");
    }

    #[test]
    fn truncate_title_caps_long_lines() {
        let long = "a".repeat(200);
        let got  = truncate_title(&long);
        assert_eq!(got.chars().count(), 80);
        assert!(got.ends_with("..."));
    }

    #[test]
    fn truncate_title_uses_first_line_only() {
        assert_eq!(truncate_title("first line\nsecond"), "first line");
    }

    // Integration-style test: drive forward_sse via a mock HTTP stream.
    #[tokio::test]
    async fn forward_sse_emits_events_in_order() {
        // Spin up a tiny server that emits a canned SSE body.
        let app = axum::Router::new().route(
            "/chat",
            axum::routing::post(|| async {
                use axum::response::sse::{Event, Sse};
                use futures::stream;
                let events = stream::iter(vec![
                    Ok::<_, std::convert::Infallible>(Event::default().event("token").data("he")),
                    Ok(Event::default().event("token").data("llo")),
                    Ok(Event::default().event("done").data("{\"conversation_id\":\"x\"}")),
                ]);
                Sse::new(events)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let resp = client.post(format!("http://{}/chat", addr)).send().await.unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        tokio::spawn(forward_sse(resp, tx));

        let mut tokens = String::new();
        let mut saw_done = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Token(t) => tokens.push_str(&t),
                StreamEvent::Done { .. } => { saw_done = true; break; }
                _ => {}
            }
        }
        assert_eq!(tokens, "hello");
        assert!(saw_done);
    }
}
