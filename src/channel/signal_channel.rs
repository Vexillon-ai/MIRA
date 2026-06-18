// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel/signal_channel.rs

//! Signal channel implementation for MIRA
//! 
//! Supports multiple modes:
//! - Webhook mode: Receives messages via HTTP webhook (third-party APIs)
//! - Polling mode: Polls signald or other daemon for new messages

use async_trait::async_trait;
use super::Channel;
use crate::{IncomingMessage, OutgoingMessage};
use crate::providers::signal_cli::SignalCliClient;
use serde::Deserialize;
use tracing::{error, info, warn};

/// Signal channel configuration
#[derive(Debug, Clone)]
pub struct SignalConfig {
    /// Mode of operation
    pub mode: SignalMode,
    /// Recipient number (for sending messages)
    pub recipient_number: Option<String>,
}

/// Signal operation modes
#[derive(Debug, Clone)]
pub enum SignalMode {
    /// Webhook mode - receives messages via HTTP endpoint
    Webhook {
        /// API key or token for authentication
        api_key: String,
        /// Base URL of the webhook service
        base_url: String,
    },
    /// Polling mode - polls signald daemon
    Polling {
        /// Path to signald socket (usually /run/signald/signald.sock)
        socket_path: String,
        /// Phone number registered with Signal
        phone_number: String,
    },
}

/// Signal webhook payload (generic format for third-party APIs)
#[derive(Debug, Deserialize)]
pub struct SignalWebhookPayload {
    pub message_id: String,
    pub sender: String,
    pub text: Option<String>,
    pub timestamp: i64,
}

/// Signal channel implementation
pub struct SignalChannel {
    config: SignalConfig,
    client: Option<SignalCliClient>,
}

impl SignalChannel {
    pub fn new(config: SignalConfig) -> Self {
        let client = match &config.mode {
            SignalMode::Webhook { api_key: _, base_url } => {
                // Extract port from "http://127.0.0.1:{port}"
                if let Some(port_str) = base_url.split(':').last() {
                    if let Ok(port) = port_str.trim_end_matches('/').parse::<u16>() {
                        config.recipient_number.as_ref().map(|num| {
                            SignalCliClient::new(port, num.clone())
                        })
                    } else { None }
                } else { None }
            }
            SignalMode::Polling { .. } => None,
        };
        info!("Signal channel initialized in {:?} mode", config.mode);
        Self { config, client }
    }

    /// Get the configuration
    pub fn config(&self) -> &SignalConfig {
        &self.config
    }
}

#[async_trait]
impl Channel for SignalChannel {
    fn name(&self) -> &str {
        "signal"
    }
    
    fn display_name(&self) -> String {
        match &self.config.mode {
            SignalMode::Webhook { .. } => "Signal (Webhook)".to_string(),
            SignalMode::Polling { .. } => "Signal (Polling)".to_string(),
        }
    }
    
    async fn receive(&mut self) -> Option<IncomingMessage> {
        match &self.config.mode {
            SignalMode::Webhook { .. } => None, // webhook mode — messages arrive via HTTP
            SignalMode::Polling { socket_path, .. } => {
                // Connect to signald UNIX socket and read one line. signald
                // speaks over a Unix domain socket (Linux + macOS); Windows has
                // no signald, so the polling transport is unix-only.
                #[cfg(unix)]
                {
                    use tokio::io::{AsyncBufReadExt, BufReader};
                    use tokio::net::UnixStream;
                    let socket_path = socket_path.clone();
                    match UnixStream::connect(&socket_path).await {
                        Ok(stream) => {
                            let mut reader = BufReader::new(stream);
                            let mut line = String::new();
                            match reader.read_line(&mut line).await {
                                Ok(0) => None,       // EOF
                                Ok(_) => parse_signald_message(&line),
                                Err(e) => {
                                    warn!("signald read error: {}", e);
                                    None
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Cannot connect to signald socket at {}: {}", socket_path, e);
                            None
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = socket_path;
                    warn!("Signal polling (signald Unix socket) is not supported on this platform");
                    None
                }
            }
        }
    }
    
    async fn send(&self, message: OutgoingMessage) -> Result<(), crate::MiraError> {
        match &self.client {
            Some(client) => {
                let recipient = self.config.recipient_number.clone()
                    .ok_or_else(|| crate::MiraError::ProviderError(
                        "No recipient number configured for Signal".to_string()
                    ))?;
                client.send(vec![recipient], &message.content).await
            }
            None => {
                warn!("Signal client not configured — cannot send: {}",
                      &message.content[..message.content.len().min(50)]);
                Err(crate::MiraError::ProviderError("Signal client not configured".to_string()))
            }
        }
    }
    
    fn is_active(&self) -> bool {
        true // Always active in webhook mode
    }
    
    async fn shutdown(&mut self) {
        info!("Shutting down Signal channel");
    }
}

// ---- signald UNIX socket message parsing ----

#[derive(Deserialize)]
struct SignaldEnvelope {
    #[serde(rename = "type")]
    msg_type: String,
    data: Option<SignaldData>,
}

#[derive(Deserialize)]
struct SignaldData {
    source: Option<SignaldSource>,
    #[serde(rename = "dataMessage")]
    data_message: Option<SignaldDataMessage>,
}

#[derive(Deserialize)]
struct SignaldSource {
    number: Option<String>,
}

#[derive(Deserialize)]
struct SignaldDataMessage {
    timestamp: Option<i64>,
    message: Option<String>,
}

/// Parse a JSON line from the signald UNIX socket into an IncomingMessage
pub fn parse_signald_message(line: &str) -> Option<IncomingMessage> {
    let envelope: SignaldEnvelope = serde_json::from_str(line).ok()?;
    if envelope.msg_type != "message" { return None; }
    let data = envelope.data?;
    let source = data.source?;
    let dm = data.data_message?;
    let text = dm.message?;
    let sender = source.number.unwrap_or_else(|| "unknown".to_string());
    let ts = dm.timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    use chrono::{Utc, TimeZone};
    let dt = Utc.timestamp_millis_opt(ts).single().unwrap_or_else(Utc::now);
    Some(IncomingMessage {
        id: format!("signald-{}", ts),
        sender,
        content: text,
        timestamp: dt.to_rfc3339(),
    })
}

// ---- webhook parsing ----

/// Parse incoming Signal webhook and convert to IncomingMessage
pub fn parse_signal_webhook(body: &str) -> Option<IncomingMessage> {
    match serde_json::from_str::<SignalWebhookPayload>(body) {
        Ok(payload) => {
            if let Some(text) = payload.text {
                info!("Parsed Signal webhook from {}: {}", payload.sender, text);
                
                use chrono::{Utc, TimeZone};
                
                // Convert Unix timestamp to RFC3339
                let dt: chrono::DateTime<Utc> = Utc.timestamp_opt(payload.timestamp, 0).single().unwrap_or(Utc::now());
                
                Some(IncomingMessage {
                    id: payload.message_id,
                    sender: payload.sender,
                    content: text,
                    timestamp: dt.to_rfc3339(),
                })
            } else {
                warn!("Signal webhook has no text content");
                None
            }
        }
        Err(e) => {
            error!("Failed to parse Signal webhook: {}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_signal_webhook() {
        let body = r#"{
            "message_id": "abc123",
            "sender": "+1234567890",
            "text": "Hello from Signal!",
            "timestamp": 1234567890
        }"#;
        
        let msg = parse_signal_webhook(body).expect("Should parse webhook");
        assert_eq!(msg.content, "Hello from Signal!");
        assert_eq!(msg.sender, "+1234567890");
    }
    
    #[test]
    fn test_parse_signald_envelope() {
        let payload = r#"{
            "type": "message",
            "data": {
                "source": {"number": "+15551234567"},
                "dataMessage": {
                    "timestamp": 1713000000000,
                    "message": "Hello from signald polling!"
                }
            }
        }"#;
        let msg = parse_signald_message(payload).expect("Should parse");
        assert_eq!(msg.content, "Hello from signald polling!");
        assert_eq!(msg.sender, "+15551234567");
    }

    #[test]
    fn test_parse_signald_non_message_type() {
        let payload = r#"{"type": "version", "data": {"version": "0.23.0"}}"#;
        assert!(parse_signald_message(payload).is_none());
    }

    #[test]
    fn test_parse_signal_webhook_no_text() {
        let body = r#"{
            "message_id": "abc123",
            "sender": "+1234567890",
            "timestamp": 1234567890
        }"#;
        
        assert!(parse_signal_webhook(body).is_none());
    }
}