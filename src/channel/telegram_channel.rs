// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel/telegram_channel.rs

//! Telegram Channel implementation for MIRA
//!
//! Provides bidirectional communication with Telegram via the Bot API.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::info;

use super::{Channel, IncomingMessage, OutgoingMessage};

/// Session manager for per-chat conversations
#[derive(Debug, Default)]
pub struct TelegramSessionManager {
    sessions: Arc<RwLock<HashMap<i64, String>>>, // chat_id -> session_id
}

impl TelegramSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Get or create a session ID for a chat
    pub fn get_or_create_session(&self, chat_id: i64) -> String {
        let mut sessions = self.sessions.write().unwrap();
        sessions.entry(chat_id).or_insert_with(|| {
            format!("tg-{}-{}", chat_id, uuid::Uuid::new_v4())
        }).clone()
    }
}

/// Telegram channel - wraps Telegram bot functionality
pub struct TelegramChannel {
    bot_token: String,
    base_url: String,
}

impl TelegramChannel {
    pub fn new(bot_token: String) -> Self {
        info!("Initializing Telegram channel");
        Self {
            bot_token,
            // Trailing slash deliberately omitted — the Telegram URL
            // format is `https://api.telegram.org/bot<TOKEN>/<METHOD>`,
            // with the token concatenated directly to "bot". An earlier
            // `"https://api.telegram.org/bot"` + format!("{}/{}/{}", …)
            // inserted a spurious slash and produced `…/bot/<TOKEN>/…`,
            // which Telegram answers with HTTP 404 "Not Found" — the
            // server has no `/bot/<…>` resource. Caused real outbound
            // failures for the companion + briefing paths.
            base_url: "https://api.telegram.org".to_string(),
        }
    }

    /// Build API URL — `<base>/bot<TOKEN>/<METHOD>`. No slash between
    /// "bot" and the token.
    fn api_url(&self, endpoint: &str) -> String {
        format!("{}/bot{}/{}", self.base_url, self.bot_token, endpoint)
    }
    
    /// Send a message to a Telegram chat
    pub async fn send_to_chat(&self, chat_id: i64, text: &str) -> Result<(), crate::MiraError> {
        let url = self.api_url("sendMessage");

        let payload = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown"
        });

        let client = crate::server::handlers::telegram::telegram_http_client();
        let response = client.post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| crate::MiraError::ProviderError(
                // `without_url()` strips the bot token (carried in the URL).
                format!("Failed to send Telegram message: {}", e.without_url())
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("Telegram API returned {} for chat {}: {}", status, chat_id, body)
            ));
        }

        Ok(())
    }

    /// Send an OGG/Opus voice note to a chat via `sendVoice`, with `caption`
    /// as the accompanying text (truncated to Telegram's caption cap). The
    /// bytes must already be OGG/Opus — callers transcode first. Used by the
    /// companion / briefing dispatch path so proactive messages honour the
    /// user's "voice: always" preference, matching normal replies.
    pub async fn send_voice_to_chat(
        &self,
        chat_id: i64,
        ogg_opus: &[u8],
        caption: &str,
    ) -> Result<(), crate::MiraError> {
        const CAPTION_MAX: usize = 1024;
        let url = self.api_url("sendVoice");

        let cap = if caption.chars().count() > CAPTION_MAX {
            let mut t: String = caption.chars().take(CAPTION_MAX - 1).collect();
            t.push('…');
            t
        } else {
            caption.to_string()
        };

        let voice_part = reqwest::multipart::Part::bytes(ogg_opus.to_vec())
            .file_name("voice.ogg")
            .mime_str("audio/ogg")
            .expect("static mime is valid");
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("caption", cap)
            .part("voice", voice_part);

        let response = crate::server::handlers::telegram::telegram_http_client()
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| crate::MiraError::ProviderError(
                // `without_url()` strips the bot token (carried in the URL).
                format!("Failed to send Telegram voice: {}", e.without_url())
            ))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("Telegram sendVoice returned {} for chat {}: {}", status, chat_id, body)
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }
    
    fn display_name(&self) -> String {
        "Telegram Bot".to_string()
    }
    
    /// Receive messages - not used for webhook mode
    async fn receive(&mut self) -> Option<IncomingMessage> {
        None
    }
    
    async fn send(&self, msg: OutgoingMessage) -> Result<(), crate::MiraError> {
        // Extract chat_id from reply_to message ID (format: "tg-123456789-msgid")
        if let Some(reply_to) = &msg.reply_to {
            if let Some(chat_id_str) = reply_to.strip_prefix("tg-") {
                if let Ok(chat_id) = chat_id_str.split('-').next().unwrap_or("0").parse::<i64>() {
                    return self.send_to_chat(chat_id, &msg.content).await;
                }
            }
        }
        
        Err(crate::MiraError::ProviderError(
            "No chat_id specified for Telegram message".to_string()
        ))
    }
    
    fn is_active(&self) -> bool {
        !self.bot_token.is_empty()
    }
    
    async fn shutdown(&mut self) {
        info!("Shutting down Telegram channel");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_session_manager() {
        let manager = TelegramSessionManager::new();
        
        // Same chat should get same session
        let session1 = manager.get_or_create_session(123);
        let session2 = manager.get_or_create_session(123);
        assert_eq!(session1, session2);
        
        // Different chats should get different sessions
        let session3 = manager.get_or_create_session(456);
        assert_ne!(session1, session3);
    }
}
