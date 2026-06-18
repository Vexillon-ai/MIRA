// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/mcp.rs
//! Model Context Protocol (MCP) stdio server for the wiki (Slice G).
//!
//! Exposes a user's wiki pages as MCP **resources** so any MCP-aware
//! client (Claude Desktop, Continue, custom hosts) can list and read
//! them. Tools (read/search/append) live in the in-process agent — the
//! MCP surface is read-only for v1, since adding write tools requires
//! propagating the review-queue policy across process boundaries and
//! we want to do that thoughtfully (Slice I).
//!
//! Transport: newline-delimited JSON-RPC 2.0 over stdin / stdout.
//! That's what every shipped MCP host expects today.
//!
//! Methods implemented (per the MCP `2024-11-05` revision):
//! - `initialize` → returns server info + `resources` capability.
//! - `initialized` (notification) → ignored / acknowledged.
//! - `resources/list` → all wiki pages as resources.
//! - `resources/read` → the body of a single resource by URI.
//! - `ping` → empty `{}` response (some clients heartbeat).
//!
//! Anything else returns `-32601 Method not found` per JSON-RPC.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::wiki::{WikiPath, WikiRegistry};

/// MCP protocol revision we speak. Bump when adopting a newer spec.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC envelope.
#[derive(Debug, Serialize, Deserialize)]
struct Request {
    #[serde(default)]
    jsonrpc: String,
    /// `id` is `Value` so we can echo back null / number / string verbatim.
    /// Notifications omit `id` entirely.
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl RpcError {
    fn method_not_found(method: &str) -> Self {
        Self { code: -32601, message: format!("method not found: {method}"), data: None }
    }
    fn invalid_params(msg: impl Into<String>) -> Self {
        Self { code: -32602, message: msg.into(), data: None }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self { code: -32603, message: msg.into(), data: None }
    }
}

/// URI scheme used to identify wiki pages. Format: `wiki://<rel_path>`.
const URI_SCHEME: &str = "wiki://";

fn uri_for(path: &WikiPath) -> String { format!("{URI_SCHEME}{}", path.as_str()) }

fn path_from_uri(uri: &str) -> Option<&str> { uri.strip_prefix(URI_SCHEME) }

/// Run an MCP server reading from `stdin`, writing to `stdout`, scoped
/// to the user identified by `user_id`. Returns when the client closes
/// stdin (EOF) or sends `exit` per the MCP shutdown sequence.
///
/// `data_dir` is the MIRA data directory — the wiki for `user_id` is
/// resolved underneath. Errors during a request are returned as
/// JSON-RPC error responses; the loop only exits on transport-level
/// failure or EOF.
pub fn run_stdio(data_dir: &Path, user_id: &str) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    serve(reader, &mut writer, data_dir, user_id)
}

/// Inner loop, generic over reader / writer for testability.
fn serve<R: Read, W: Write>(
    reader: BufReader<R>,
    writer: &mut W,
    data_dir: &Path,
    user_id: &str,
) -> std::io::Result<()> {
    let registry = WikiRegistry::new(data_dir.to_path_buf());

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                // We can't even echo back the id — JSON-RPC says respond
                // with null id and code -32700 (Parse error).
                let resp = Response {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(RpcError {
                        code: -32700,
                        message: format!("parse error: {e}"),
                        data: None,
                    }),
                };
                write_response(writer, &resp)?;
                continue;
            }
        };
        let is_notification = req.id.is_none();
        let id = req.id.clone().unwrap_or(Value::Null);

        // Methods that end the session.
        if req.method == "exit" { break; }

        let result = dispatch(&registry, user_id, &req.method, &req.params);
        if is_notification {
            // Notifications get no response per JSON-RPC. We just continue.
            continue;
        }
        let resp = match result {
            Ok(v)  => Response { jsonrpc: "2.0", id, result: Some(v), error: None },
            Err(e) => Response { jsonrpc: "2.0", id, result: None, error: Some(e) },
        };
        write_response(writer, &resp)?;
    }
    Ok(())
}

fn write_response<W: Write>(writer: &mut W, resp: &Response) -> std::io::Result<()> {
    let s = serde_json::to_string(resp).map_err(|e| std::io::Error::new(
        std::io::ErrorKind::InvalidData, e,
    ))?;
    writer.write_all(s.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn dispatch(registry: &WikiRegistry, user_id: &str, method: &str, params: &Value)
    -> Result<Value, RpcError>
{
    match method {
        "initialize"       => Ok(handle_initialize()),
        "initialized"      => Ok(Value::Null),     // notification ack
        "ping"             => Ok(json!({})),
        "resources/list"   => handle_resources_list(registry, user_id),
        "resources/read"   => handle_resources_read(registry, user_id, params),
        "tools/list"       => Ok(json!({ "tools": [] })),
        "prompts/list"     => Ok(json!({ "prompts": [] })),
        "shutdown"         => Ok(Value::Null),
        other              => Err(RpcError::method_not_found(other)),
    }
}

fn handle_initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "resources": { "subscribe": false, "listChanged": false }
        },
        "serverInfo": {
            "name":    "mira-wiki",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

fn handle_resources_list(registry: &WikiRegistry, user_id: &str)
    -> Result<Value, RpcError>
{
    let wiki = registry.for_user(user_id)
        .map_err(|e| RpcError::internal(format!("open wiki: {e}")))?;
    let paths = wiki.store().list_pages()
        .map_err(|e| RpcError::internal(format!("list pages: {e}")))?;
    let mut resources = Vec::with_capacity(paths.len());
    for path in paths {
        let page = match wiki.store().read_page(&path) {
            Ok(p) => p,
            Err(_) => continue, // skip unreadable
        };
        let title = page.frontmatter.title.clone()
            .unwrap_or_else(|| path.as_str().to_string());
        resources.push(json!({
            "uri":         uri_for(&path),
            "name":        title,
            "mimeType":    "text/markdown",
            "description": format!("Wiki page at {}", path.as_str()),
        }));
    }
    Ok(json!({ "resources": resources }))
}

fn handle_resources_read(registry: &WikiRegistry, user_id: &str, params: &Value)
    -> Result<Value, RpcError>
{
    let uri = params.get("uri").and_then(|v| v.as_str()).ok_or_else(|| {
        RpcError::invalid_params("missing required parameter `uri`")
    })?;
    let rel = path_from_uri(uri).ok_or_else(|| {
        RpcError::invalid_params(format!("URI must start with `{URI_SCHEME}`, got: {uri}"))
    })?;
    let path = WikiPath::parse(rel).map_err(|e| {
        RpcError::invalid_params(format!("invalid wiki path: {e}"))
    })?;
    let wiki = registry.for_user(user_id)
        .map_err(|e| RpcError::internal(format!("open wiki: {e}")))?;
    let page = wiki.store().try_read_page(&path)
        .map_err(|e| RpcError::internal(format!("read page: {e}")))?
        .ok_or_else(|| RpcError::invalid_params(format!("not found: {rel}")))?;

    // Serialise the page as a "text" resource content per the MCP spec.
    // Including frontmatter as part of the body so clients see the
    // structural metadata too — they can strip it if they don't care.
    let serialised = crate::wiki::frontmatter::serialize(&page.frontmatter, &page.body)
        .map_err(|e| RpcError::internal(format!("serialise: {e}")))?;
    Ok(json!({
        "contents": [{
            "uri":      uri,
            "mimeType": "text/markdown",
            "text":     serialised,
        }]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::{PageFrontmatter, Provenance, WikiOp, WikiSystem};
    use tempfile::tempdir;

    fn seed_wiki(dir: &Path, user_id: &str) {
        let wiki = WikiSystem::for_user(dir, user_id).unwrap();
        let mut fm = PageFrontmatter::default();
        fm.title = Some("Pong project".into());
        wiki.submit_and_apply(WikiOp::WritePage {
            path: WikiPath::parse("pages/pong.md").unwrap(),
            frontmatter: fm,
            body: "# Pong\nNotes.\n".into(),
        }, Provenance::user_ui(user_id)).unwrap();
    }

    /// Drive `serve()` end-to-end with a recorded conversation.
    fn drive(data_dir: &Path, user_id: &str, requests: &[Value]) -> Vec<Value> {
        let mut input = String::new();
        for r in requests {
            input.push_str(&serde_json::to_string(r).unwrap());
            input.push('\n');
        }
        let reader = BufReader::new(input.as_bytes());
        let mut output: Vec<u8> = Vec::new();
        serve(reader, &mut output, data_dir, user_id).unwrap();
        let text = String::from_utf8(output).unwrap();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn initialize_returns_server_info_and_capabilities() {
        let dir = tempdir().unwrap();
        seed_wiki(dir.path(), "u1");
        let req = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        let resps = drive(dir.path(), "u1", &[req]);
        assert_eq!(resps.len(), 1);
        let r = &resps[0]["result"];
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["serverInfo"]["name"], "mira-wiki");
        assert!(r["capabilities"]["resources"].is_object());
    }

    #[test]
    fn resources_list_returns_pages() {
        let dir = tempdir().unwrap();
        seed_wiki(dir.path(), "u1");
        let req = json!({"jsonrpc":"2.0","id":2,"method":"resources/list","params":{}});
        let resps = drive(dir.path(), "u1", &[req]);
        let resources = resps[0]["result"]["resources"].as_array().unwrap();
        assert!(resources.iter().any(|r| r["uri"] == "wiki://pages/pong.md"));
        // Must include profile.md (always there).
        assert!(resources.iter().any(|r| r["uri"] == "wiki://profile.md"));
        // Pong's name should be the title, not the path.
        let pong = resources.iter().find(|r| r["uri"] == "wiki://pages/pong.md").unwrap();
        assert_eq!(pong["name"], "Pong project");
    }

    #[test]
    fn resources_read_returns_page_body() {
        let dir = tempdir().unwrap();
        seed_wiki(dir.path(), "u1");
        let req = json!({
            "jsonrpc":"2.0","id":3,
            "method":"resources/read",
            "params":{"uri":"wiki://pages/pong.md"},
        });
        let resps = drive(dir.path(), "u1", &[req]);
        let text = resps[0]["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(text.contains("# Pong"));
        assert!(text.contains("title: Pong project"));
    }

    #[test]
    fn resources_read_rejects_path_traversal() {
        let dir = tempdir().unwrap();
        seed_wiki(dir.path(), "u1");
        let req = json!({
            "jsonrpc":"2.0","id":4,
            "method":"resources/read",
            "params":{"uri":"wiki://../etc/passwd"},
        });
        let resps = drive(dir.path(), "u1", &[req]);
        let err = &resps[0]["error"];
        assert!(err.is_object(), "expected error response, got {:?}", resps[0]);
        assert_eq!(err["code"], -32602);
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let dir = tempdir().unwrap();
        seed_wiki(dir.path(), "u1");
        let req = json!({"jsonrpc":"2.0","id":5,"method":"nope","params":{}});
        let resps = drive(dir.path(), "u1", &[req]);
        assert_eq!(resps[0]["error"]["code"], -32601);
    }

    #[test]
    fn notifications_get_no_response() {
        let dir = tempdir().unwrap();
        seed_wiki(dir.path(), "u1");
        // `initialized` notification has no id.
        let req = json!({"jsonrpc":"2.0","method":"initialized","params":{}});
        let resps = drive(dir.path(), "u1", &[req]);
        assert!(resps.is_empty(), "expected no response, got {:?}", resps);
    }

    #[test]
    fn parse_error_returns_null_id() {
        let dir = tempdir().unwrap();
        seed_wiki(dir.path(), "u1");
        // Send malformed JSON.
        let input = "{this is not json\n";
        let reader = BufReader::new(input.as_bytes());
        let mut output: Vec<u8> = Vec::new();
        serve(reader, &mut output, dir.path(), "u1").unwrap();
        let text = String::from_utf8(output).unwrap();
        let resp: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(resp["error"]["code"], -32700);
        assert!(resp["id"].is_null());
    }

    #[test]
    fn exit_method_terminates_loop() {
        let dir = tempdir().unwrap();
        seed_wiki(dir.path(), "u1");
        let reqs = vec![
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","method":"exit"}),
            // Anything after `exit` should be ignored — the loop has exited.
            json!({"jsonrpc":"2.0","id":99,"method":"resources/list","params":{}}),
        ];
        let resps = drive(dir.path(), "u1", &reqs);
        // We expect only the initialize response.
        assert_eq!(resps.len(), 1);
        assert_eq!(resps[0]["id"], 1);
    }
}
