// SPDX-License-Identifier: AGPL-3.0-or-later

// src/types/message.rs

use serde::{Deserialize, Serialize};
use crate::types::tool::ToolCall;
use std::collections::HashMap;

/// Role of a message in conversation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MessageRole {
    #[serde(rename = "system")]
    System,
    
    #[serde(rename = "user")]
    User,
    
    #[serde(rename = "assistant")]
    Assistant,
    
    #[serde(rename = "tool")]
    Tool,
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageRole::System => write!(f, "system"),
            MessageRole::User => write!(f, "user"),
            MessageRole::Assistant => write!(f, "assistant"),
            MessageRole::Tool => write!(f, "tool"),
        }
    }
}

/// Chat message in conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Q1.3 — non-text inputs travelling alongside `content`. Today
    /// only inline base64-encoded images are supported (Claude, GPT-4o,
    /// Gemini all accept this form). Provider wire layers translate
    /// these into the right per-vendor block shape; providers without
    /// vision strip them with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

/// One non-text payload attached to a `ChatMessage`. Persisted to
/// history as JSON on `Message.metadata` so a reloaded conversation
/// keeps the original vision context the next time it's sent through
/// the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub kind:      AttachmentKind,
    /// e.g. `image/png`, `image/jpeg`, `image/webp`. Provider wire
    /// layers refuse types they don't accept.
    pub mime_type: String,
    /// Standard base64 (RFC 4648, with `=` padding). The browser-side
    /// `FileReader.readAsDataURL` strips the `data:` prefix before
    /// posting; the server stores the raw b64 only.
    pub data_b64:  String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
}

impl ChatMessage {
    /// Create a new system message
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        }
    }
    
    /// Create a new user message
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        }
    }
    
    /// Create a new assistant message
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
        }
    }
    
    /// Create a new tool response message
    pub fn tool(content: impl Into<String>, call_id: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            attachments: None,
        }
    }
}

/// Conversation context for agent
#[derive(Debug, Clone)]
pub struct ConversationContext {
    pub system_prompt: String,
    pub messages: Vec<ChatMessage>,
    pub metadata: HashMap<String, String>,
    
    // Tracking fields (not sent to model)
    pub active_tool_executions: Vec<String>,
    pub retrieved_memories: Vec<crate::types::MemoryId>,
}

impl ConversationContext {
    /// Create a new conversation context with system prompt
    pub fn new(system_prompt: impl Into<String>) -> Self {
        let system = system_prompt.into();
        Self {
            system_prompt: system.clone(),
            messages: vec![ChatMessage::system(system)],
            metadata: HashMap::new(),
            active_tool_executions: Vec::new(),
            retrieved_memories: Vec::new(),
        }
    }
    
    /// Add a user message to the context
    pub fn add_user_message(&mut self, content: impl Into<String>) {
        self.messages.push(ChatMessage::user(content));
    }
    
    /// Add an assistant message to the context
    pub fn add_assistant_message(&mut self, content: impl Into<String>) {
        self.messages.push(ChatMessage::assistant(content));
    }
    
    /// Estimate token count (rough approximation)
    pub fn token_count_estimate(&self) -> usize {
        // Rough estimate: 1 char ≈ 0.25 tokens
        self.messages.iter()
            .map(|m| m.content.len())
            .sum::<usize>() / 4
    }
    
    /// Get messages as Vec for sending to provider
    pub fn messages_vec(&self) -> Vec<ChatMessage> {
        self.messages.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_message_constructors() {
        let sys = ChatMessage::system("You are helpful.");
        assert_eq!(sys.role, MessageRole::System);
        assert_eq!(sys.content, "You are helpful.");

        let user = ChatMessage::user("Hello");
        assert_eq!(user.role, MessageRole::User);

        let asst = ChatMessage::assistant("Hi there!");
        assert_eq!(asst.role, MessageRole::Assistant);

        let tool = ChatMessage::tool("result", "call-123");
        assert_eq!(tool.role, MessageRole::Tool);
        assert_eq!(tool.tool_call_id, Some("call-123".to_string()));
    }

    #[test]
    fn test_message_role_display() {
        assert_eq!(MessageRole::System.to_string(), "system");
        assert_eq!(MessageRole::User.to_string(), "user");
        assert_eq!(MessageRole::Assistant.to_string(), "assistant");
        assert_eq!(MessageRole::Tool.to_string(), "tool");
    }

    #[test]
    fn test_conversation_context_initial_state() {
        let ctx = ConversationContext::new("You are MIRA.");
        assert_eq!(ctx.system_prompt, "You are MIRA.");
        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].role, MessageRole::System);
        assert_eq!(ctx.messages[0].content, "You are MIRA.");
    }

    #[test]
    fn test_add_user_and_assistant_messages() {
        let mut ctx = ConversationContext::new("System.");
        ctx.add_user_message("First question");
        ctx.add_assistant_message("First answer");
        ctx.add_user_message("Second question");
        assert_eq!(ctx.messages.len(), 4);
        assert_eq!(ctx.messages[1].role, MessageRole::User);
        assert_eq!(ctx.messages[2].role, MessageRole::Assistant);
        assert_eq!(ctx.messages[3].role, MessageRole::User);
    }

    #[test]
    fn test_token_count_estimate() {
        let mut ctx = ConversationContext::new("abcd"); // 4 chars → 1 token
        ctx.add_user_message("abcdefgh"); // 8 chars → 2 tokens
        // Total chars = 4 + 8 = 12, / 4 = 3
        assert_eq!(ctx.token_count_estimate(), 3);
    }

    #[test]
    fn test_messages_vec_is_clone() {
        let mut ctx = ConversationContext::new("System.");
        ctx.add_user_message("Hello");
        let vec = ctx.messages_vec();
        assert_eq!(vec.len(), 2);
        // Verify it's a clone (independent)
        assert_eq!(ctx.messages.len(), 2);
    }
}
