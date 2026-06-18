// SPDX-License-Identifier: AGPL-3.0-or-later

// src/history/models.rs

use serde::{Deserialize, Serialize};

// ── MessageRole ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageRole::User      => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System    => "system",
            MessageRole::Tool      => "tool",
        }
    }
}

impl std::str::FromStr for MessageRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user"      => Ok(MessageRole::User),
            "assistant" => Ok(MessageRole::Assistant),
            "system"    => Ok(MessageRole::System),
            "tool"      => Ok(MessageRole::Tool),
            other       => Err(format!("Unknown message role: {}", other)),
        }
    }
}

// ── Conversation ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id:         String,
    pub user_id:    String,
    pub channel:    String,
    pub title:      Option<String>,
    pub model:      Option<String>,
    pub provider:   Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Sender id from the external channel (Signal phone, Telegram user id).
    /// `None` for web/TUI conversations whose owner *is* the participant.
    /// Used to dedup multiple senders under the same Signal/Telegram account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_user_id: Option<String>,
    /// Conversation flow. `"chat"` (default) or flow-specific tags like
    /// `"onboarding"`. Branches the system prompt and tool set.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Slice H — when true, the wiki context-injection hook is skipped
    /// for this conversation. The auto-extractor and agent wiki tools
    /// still run (the user can opt those out separately via config).
    #[serde(default)]
    pub skip_wiki: bool,
}

fn default_mode() -> String { "chat".to_owned() }

// ── Message ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id:              String,
    pub conversation_id: String,
    pub role:            MessageRole,
    pub content:         String,
    /// "text" | "image" | "tool_call" | "tool_result"
    pub content_type:    String,
    pub token_count:     Option<i32>,
    pub model:           Option<String>,
    /// JSON-encoded tool calls, if any.
    pub tool_calls:      Option<String>,
    pub created_at:      i64,
    /// JSON metadata blob.
    pub metadata:        Option<String>,
}

// ── NewConversation ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct NewConversation {
    pub user_id:          String,
    pub channel:          String,
    pub title:            Option<String>,
    pub model:            Option<String>,
    pub provider:         Option<String>,
    /// Optional sender identifier from the external channel — phone number
    /// for Signal, user id for Telegram. `None` for web/TUI conversations.
    pub external_user_id: Option<String>,
    /// Optional flow tag; `None` = default `"chat"`.
    pub mode:             Option<String>,
}

// ── HistoryStats ──────────────────────────────────────────────────────────────

/// Per-channel aggregate used by [`HistoryStats`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelStats {
    pub channel:       String,
    pub conversations: i64,
    pub messages:      i64,
    /// Sum of `token_count` (or its `LENGTH(content)/4` estimate when null).
    pub tokens:        i64,
}

/// Aggregate over the conversation history scoped to one user's visibility
/// (admins see everything). Tokens are **estimated** when message rows lack
/// a recorded `token_count`: we fall back to `LENGTH(content) / 4`, the
/// standard ~4-chars-per-token heuristic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryStats {
    pub total_conversations: i64,
    pub total_messages:      i64,
    pub user_messages:       i64,
    pub assistant_messages:  i64,
    pub tool_messages:       i64,
    /// Sum of `token_count` (falling back to `LENGTH(content) / 4` when null).
    pub estimated_tokens:    i64,
    pub per_channel:         Vec<ChannelStats>,
    /// Model with the most assistant/tool messages, or `None` when no row
    /// has a recorded model name.
    pub top_model:           Option<String>,
    /// Earliest message `created_at` (ms epoch) the user can see.
    pub first_message_at:    Option<i64>,
    /// Latest message `created_at` (ms epoch) the user can see.
    pub last_message_at:     Option<i64>,
}

// ── NewMessage ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub conversation_id: String,
    pub role:            MessageRole,
    pub content:         String,
    pub content_type:    String,
    pub token_count:     Option<i32>,
    pub model:           Option<String>,
    pub tool_calls:      Option<String>,
    pub metadata:        Option<String>,
}
