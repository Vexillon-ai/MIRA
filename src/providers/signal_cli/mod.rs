// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/signal_cli/mod.rs

//! signal-cli JSON-RPC client.
//!
//! signal-cli 0.13+ exposes a JSON-RPC interface at /api/v1/rpc when started
//! with `daemon --http`. MIRA starts this daemon on startup when
//! config.signal.enabled = true.

pub mod daemon;
pub mod sse_listener;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, info, warn};
use crate::MiraError;

/// JSON-RPC request envelope
#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    method: &'static str,
    id: u32,
    params: serde_json::Value,
}

/// JSON-RPC response envelope
#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    #[allow(dead_code)] // read by tests; kept for API completeness
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

/// JSON-RPC client for a running signal-cli daemon
pub struct SignalCliClient {
    client: Client,
    base_url: String,
    #[allow(dead_code)]
    phone_number: String,
}

impl SignalCliClient {
    /// Create a client pointed at `http://127.0.0.1:{port}`
    pub fn new(port: u16, phone_number: String) -> Self {
        let base_url = format!("http://127.0.0.1:{}", port);
        info!("signal-cli client configured at {} for number {}", base_url, phone_number);
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("Failed to build HTTP client for signal-cli"),
            base_url,
            phone_number,
        }
    }

    /// Send a message to one or more recipients via JSON-RPC
    pub async fn send(&self, recipients: Vec<String>, message: &str) -> Result<(), MiraError> {
        self.send_with_attachments(recipients, message, &[]).await
    }

    /// Send a message with optional file-path attachments. signal-cli's `send`
    /// JSON-RPC takes `attachments: [path, …]` — paths are read from disk by
    /// the daemon, so the caller must persist bytes to a file first (typically
    /// a tempfile that's dropped after the call returns). Pass an empty slice
    /// to send a plain text message.
    ///
    /// Voice notes go out via this method too: write the OGG/Opus bytes to a
    /// `.oga` file, then call with both `message` (transcript) and the file
    /// path. signal-cli infers voice-note semantics from the file's MIME.
    pub async fn send_with_attachments(
        &self,
        recipients:  Vec<String>,
        message:     &str,
        attachments: &[String],
    ) -> Result<(), MiraError> {
        let url = format!("{}/api/v1/rpc", self.base_url);
        let mut params = json!({
            "recipient": recipients,
            "message":   message,
        });
        if !attachments.is_empty() {
            params["attachments"] = json!(attachments);
        }
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method:  "send",
            id:      1,
            params,
        };
        debug!(
            "Sending Signal message (attachments={}) via JSON-RPC to {:?}",
            attachments.len(), &recipients,
        );
        let resp = self.client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| MiraError::ProviderError(format!("signal-cli send failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(MiraError::ProviderError(
                format!("signal-cli JSON-RPC returned {}: {}", status, text)
            ));
        }

        let rpc_resp: JsonRpcResponse = resp.json().await
            .map_err(|e| MiraError::ProviderError(format!("signal-cli response parse failed: {}", e)))?;

        if let Some(err) = rpc_resp.error {
            return Err(MiraError::ProviderError(format!("signal-cli RPC error: {}", err)));
        }

        info!("Signal message sent successfully");
        Ok(())
    }

    /// Send a typing indicator to one or more recipients.
    /// Signal's typing indicator auto-expires after ~15 seconds, so for long
    /// generations the caller should re-send every 10s until the response
    /// is ready, then call again with `stop = true` to clear the indicator
    /// immediately so the "MIRA is typing…" bubble doesn't linger.
    pub async fn send_typing(
        &self,
        recipients: Vec<String>,
        stop: bool,
    ) -> Result<(), MiraError> {
        let url = format!("{}/api/v1/rpc", self.base_url);
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "sendTyping",
            id: 1,
            params: json!({
                "recipient": recipients,
                "stop":      stop,
            }),
        };
        debug!("Signal sendTyping (stop={}) to {:?}", stop, &req.params);
        let resp = self.client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| MiraError::ProviderError(format!("signal-cli sendTyping failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text   = resp.text().await.unwrap_or_default();
            return Err(MiraError::ProviderError(
                format!("signal-cli sendTyping returned {}: {}", status, text)
            ));
        }

        let rpc_resp: JsonRpcResponse = resp.json().await
            .map_err(|e| MiraError::ProviderError(format!("signal-cli sendTyping parse failed: {}", e)))?;

        if let Some(err) = rpc_resp.error {
            return Err(MiraError::ProviderError(format!("signal-cli sendTyping RPC error: {}", err)));
        }
        Ok(())
    }

    /// Fetch a single attachment by id from the signal-cli HTTP daemon.
    ///
    /// signal-cli persists every received attachment under its data dir and
    /// exposes them at `GET /api/v1/attachments/{id}`. Voice notes show up
    /// here as Opus-in-OGG (`audio/aac` or `audio/ogg` depending on sender),
    /// so the caller is responsible for sniffing the format before handing
    /// the bytes to the STT decoder.
    ///
    /// Returns `(bytes, content_type)`. The Content-Type header is
    /// trusted as-is — Signal clients ship reliable MIME on voice notes.
    pub async fn fetch_attachment(&self, id: &str) -> Result<(Vec<u8>, String), MiraError> {
        let url = format!("{}/api/v1/attachments/{}", self.base_url, id);
        debug!("Fetching Signal attachment {}", id);
        let resp = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| MiraError::ProviderError(format!("signal-cli fetch_attachment failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text   = resp.text().await.unwrap_or_default();
            return Err(MiraError::ProviderError(
                format!("signal-cli attachment {} returned {}: {}", id, status, text)
            ));
        }

        let content_type = resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let bytes = resp.bytes().await
            .map_err(|e| MiraError::ProviderError(format!("signal-cli attachment read failed: {}", e)))?
            .to_vec();

        Ok((bytes, content_type))
    }

    /// Check whether the signal-cli daemon is reachable
    pub async fn health_check(&self) -> bool {
        let url = format!("{}/api/v1/check", self.base_url);
        match self.client.get(&url).send().await {
            Ok(r) => r.status().as_u16() < 500,
            Err(e) => {
                warn!("signal-cli health check failed: {}", e);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_construction() {
        let client = SignalCliClient::new(8080, "+15551234567".to_string());
        assert_eq!(client.base_url, "http://127.0.0.1:8080");
        assert_eq!(client.phone_number, "+15551234567");
    }

    #[test]
    fn test_json_rpc_request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "send",
            id: 1,
            params: serde_json::json!({
                "recipient": ["+15559876543"],
                "message": "Hello",
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""jsonrpc":"2.0""#));
        assert!(json.contains(r#""method":"send""#));
        assert!(json.contains("Hello"));
        assert!(json.contains("+15559876543"));
    }

    #[test]
    fn test_json_rpc_error_response_detected() {
        let resp: JsonRpcResponse = serde_json::from_str(
            r#"{"jsonrpc":"2.0","error":{"code":-1,"message":"No such account"},"id":1}"#
        ).unwrap();
        assert!(resp.error.is_some());
        assert!(resp.result.is_none());
    }

    #[test]
    fn test_send_typing_request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "sendTyping",
            id: 1,
            params: serde_json::json!({
                "recipient": ["+15559876543"],
                "stop":      false,
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""method":"sendTyping""#));
        assert!(json.contains(r#""stop":false"#));
        assert!(json.contains("+15559876543"));
    }

    #[test]
    fn test_json_rpc_success_response() {
        let resp: JsonRpcResponse = serde_json::from_str(
            r#"{"jsonrpc":"2.0","result":{"timestamp":1776437902000},"id":1}"#
        ).unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }
}
