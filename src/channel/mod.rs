// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel/mod.rs

//! Multi-channel interface for MIRA
//! 
//! This module defines the Channel trait that unifies different communication
//! channels (CLI, Telegram, Signal) into a single interface.

pub mod cli_channel;
pub mod telegram_channel;
pub mod signal_channel;
pub use cli_channel::CliChannel;
pub use telegram_channel::{TelegramChannel, TelegramSessionManager};
pub use signal_channel::{SignalChannel, SignalConfig, SignalMode, parse_signal_webhook};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Message received from any channel
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    /// Unique identifier for this message
    pub id: String,
    /// Sender identifier (username, chat_id, etc.)
    pub sender: String,
    /// Message content
    pub content: String,
    /// Timestamp in ISO 8601 format
    pub timestamp: String,
}

/// Response to send back through a channel
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMessage {
    /// Content to send
    pub content: String,
    /// Optional reply-to message ID
    pub reply_to: Option<String>,
}

/// Unified interface for all communication channels
#[async_trait]
pub trait Channel: Send + Sync {
    /// Name of this channel type (e.g., "cli", "telegram", "signal")
    fn name(&self) -> &str;
    
    /// Display name shown to users
    fn display_name(&self) -> String;
    
    /// Receive the next incoming message (blocks until available)
    async fn receive(&mut self) -> Option<IncomingMessage>;
    
    /// Send a response message
    async fn send(&self, msg: OutgoingMessage) -> Result<(), crate::MiraError>;
    
    /// Check if channel is still connected/active
    fn is_active(&self) -> bool;
    
    /// Gracefully shutdown the channel
    async fn shutdown(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_incoming_message_creation() {
        let msg = IncomingMessage {
            id: "test-123".to_string(),
            sender: "user".to_string(),
            content: "Hello, MIRA!".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        
        assert_eq!(msg.content, "Hello, MIRA!");
        assert_eq!(msg.sender, "user");
    }
    
    #[test]
    fn test_outgoing_message_creation() {
        let msg = OutgoingMessage {
            content: "Hello back!".to_string(),
            reply_to: Some("test-123".to_string()),
        };
        
        assert_eq!(msg.content, "Hello back!");
        assert!(msg.reply_to.is_some());
    }

    #[test]
    fn test_outgoing_message_no_reply_to() {
        let msg = OutgoingMessage { content: "Broadcast".to_string(), reply_to: None };
        assert!(msg.reply_to.is_none());
    }

    #[test]
    fn test_incoming_message_serialization() {
        let msg = IncomingMessage {
            id: "abc".to_string(),
            sender: "alice".to_string(),
            content: "Test".to_string(),
            timestamp: "2026-04-13T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: IncomingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.content, "Test");
        assert_eq!(deserialized.sender, "alice");
    }
}
