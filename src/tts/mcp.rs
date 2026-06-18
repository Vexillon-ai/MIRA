// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/mcp.rs
//! Model Context Protocol (MCP) stdio server exposing MIRA's text-to-speech.
//!
//! `mira tts mcp-serve` runs this: any MCP-aware client (including MIRA's
//! own host) gets a `synthesize` tool that turns text into speech using the
//! configured TTS backend (Kokoro / Piper / …) and returns the clip as a
//! standard MCP **audio content block** (base64 WAV). MIRA's MCP adapter
//! then saves it as an artifact and the chat renders an `<audio>` player.
//!
//! No API key — it's MIRA's own voice. Also makes that voice usable from
//! Claude Desktop and other MCP hosts.
//!
//! Transport: newline-delimited JSON-RPC 2.0 over stdin/stdout (logs to
//! stderr only — stdout must be pure JSON-RPC). Mirrors `wiki::mcp`.

use std::io::{BufRead, BufReader, Read, Write};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::MiraConfig;
use crate::tts::types::OutputFormat;
use crate::tts::TtsService;

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    #[serde(default)]
    jsonrpc: String,
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
}

impl RpcError {
    fn method_not_found(m: &str) -> Self { Self { code: -32601, message: format!("method not found: {m}") } }
    fn invalid_params(m: impl Into<String>) -> Self { Self { code: -32602, message: m.into() } }
}

/// Entry point for `mira tts mcp-serve`.
pub fn run_stdio(config: &MiraConfig) -> std::io::Result<()> {
    if !config.tts.enabled {
        eprintln!("mira tts mcp-serve: TTS is disabled in config (tts.enabled = false)");
        return Err(std::io::Error::other("tts disabled"));
    }
    let tts = TtsService::from_config(config);
    // The serve loop is synchronous; synthesis is async, so we own a runtime
    // and block_on each call.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    serve(reader, &mut writer, &tts, &rt)
}

fn serve<R: Read, W: Write>(
    reader: BufReader<R>,
    writer: &mut W,
    tts:    &TtsService,
    rt:     &tokio::runtime::Runtime,
) -> std::io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                write_response(writer, &Response {
                    jsonrpc: "2.0", id: Value::Null, result: None,
                    error: Some(RpcError { code: -32700, message: format!("parse error: {e}") }),
                })?;
                continue;
            }
        };
        if req.method == "exit" { break; }
        let is_notification = req.id.is_none();
        let id = req.id.clone().unwrap_or(Value::Null);

        let result = dispatch(tts, rt, &req.method, &req.params);
        if is_notification { continue; }
        let resp = match result {
            Ok(v)  => Response { jsonrpc: "2.0", id, result: Some(v), error: None },
            Err(e) => Response { jsonrpc: "2.0", id, result: None, error: Some(e) },
        };
        write_response(writer, &resp)?;
    }
    Ok(())
}

fn write_response<W: Write>(writer: &mut W, resp: &Response) -> std::io::Result<()> {
    let s = serde_json::to_string(resp)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writer.write_all(s.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn dispatch(tts: &TtsService, rt: &tokio::runtime::Runtime, method: &str, params: &Value)
    -> Result<Value, RpcError>
{
    match method {
        "initialize"  => Ok(handle_initialize()),
        "initialized" => Ok(Value::Null),
        "ping"        => Ok(json!({})),
        "tools/list"  => Ok(handle_tools_list()),
        "tools/call"  => handle_tools_call(tts, rt, params),
        "resources/list" => Ok(json!({ "resources": [] })),
        "prompts/list"   => Ok(json!({ "prompts": [] })),
        "shutdown"    => Ok(Value::Null),
        other         => Err(RpcError::method_not_found(other)),
    }
}

fn handle_initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "mira-tts", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn handle_tools_list() -> Value {
    json!({
        "tools": [{
            "name": "speak",
            "description": "Text-to-speech: speak the given text aloud with MIRA's configured \
                            voice (Kokoro/Piper) and return a playable audio clip. Use this \
                            whenever the user asks to say/speak/read something out loud or wants \
                            a voice/audio version. (This is spoken audio — not a research \
                            summary.) Optionally pass a `voice` id and a `speed` multiplier.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text":  { "type": "string", "description": "Text to speak.", "minLength": 1 },
                    "voice": { "type": "string", "description": "Backend voice id (optional; defaults to the configured voice)." },
                    "speed": { "type": "number", "description": "Speech-rate multiplier 0.5–2.0 (optional)." }
                },
                "required": ["text"]
            }
        }]
    })
}

fn handle_tools_call(tts: &TtsService, rt: &tokio::runtime::Runtime, params: &Value)
    -> Result<Value, RpcError>
{
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name != "speak" {
        return Err(RpcError::invalid_params(format!("unknown tool: {name}")));
    }
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
    if text.is_empty() {
        return Err(RpcError::invalid_params("`text` is required"));
    }
    let voice = args.get("voice").and_then(|v| v.as_str());
    let speed = args.get("speed").and_then(|v| v.as_f64()).map(|s| s as f32);

    // Always WAV — universally decodable + maps cleanly to an `audio` block.
    match rt.block_on(tts.speak(text, voice, speed, Some(OutputFormat::Wav), None, None)) {
        Ok(buf) => {
            let b64 = B64.encode(&buf.bytes);
            Ok(json!({
                "content": [
                    { "type": "audio", "data": b64, "mimeType": "audio/wav" },
                    { "type": "text", "text": format!("Synthesised {} characters of speech.", text.chars().count()) }
                ],
                "isError": false
            }))
        }
        Err(e) => Ok(json!({
            "content": [{ "type": "text", "text": format!("TTS failed: {e}") }],
            "isError": true
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_advertises_tools() {
        let r = handle_initialize();
        assert_eq!(r["serverInfo"]["name"], "mira-tts");
        assert!(r["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_exposes_speak() {
        let r = handle_tools_list();
        let tools = r["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "speak");
        assert!(tools[0]["inputSchema"]["properties"]["text"].is_object());
    }
}
