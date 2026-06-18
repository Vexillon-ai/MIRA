// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/summarize.rs
//! Conversation summariser (Tier 1 — pure).
//!
//! Model-callable tool that produces a short prose summary of one of the
//! caller's own past conversations. Useful when the user asks "what did we
//! talk about last Tuesday?" and the full transcript is too long to drop
//! into context. The tool verifies ownership before it touches anything —
//! cross-user summarisation is impossible by construction.
//!
//! Stays in the Pure tier because every resource it touches is MIRA-owned:
//! the history DB and the configured model provider. No filesystem, no
//! external network calls of the tool's own making.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::debug;

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::history::HistoryStore;
use crate::history::models::MessageRole;
use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions};
use crate::MiraError;

/// Default and hard caps. Full transcripts can be huge; the summariser
/// trims from the *end* (most recent N) so the tail of the conversation —
/// usually what the user is asking about — always makes the cut.
const DEFAULT_MAX_MESSAGES: i64 = 100;
const HARD_MAX_MESSAGES:    i64 = 400;

/// Per-message char cap used when building the prompt — stops one 50-page
/// user paste from dominating the summary budget.
const PER_MESSAGE_PREVIEW_CHARS: usize = 2_000;

pub struct SummarizeConversationTool {
    history:  Arc<HistoryStore>,
    provider: Arc<dyn ModelProvider>,
}

impl SummarizeConversationTool {
    pub fn new(history: Arc<HistoryStore>, provider: Arc<dyn ModelProvider>) -> Self {
        Self { history, provider }
    }
}

#[async_trait]
impl Tool for SummarizeConversationTool {
    fn name(&self) -> &str { "summarize_conversation" }

    fn description(&self) -> &str {
        "Summarise one of the user's past conversations. Pass a \
         `conversation_id` (from recall_history or the conversations list) and \
         receive a short prose summary covering topics, decisions, and any \
         open questions. Use this when the user references a past chat that \
         is too long to quote in full — prefer it over trying to recall the \
         whole transcript yourself. Only summarises conversations owned by \
         the caller; cross-user access is blocked."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["conversation_id"],
            "properties": {
                "conversation_id": {
                    "type": "string",
                    "description":
                        "Opaque id of the conversation to summarise, as \
                         returned by recall_history hits or the web \
                         conversations list."
                },
                "max_messages": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": HARD_MAX_MESSAGES,
                    "description":
                        "Max messages to feed the summariser (default 100, \
                         hard cap 400). Older messages are trimmed first."
                },
                "focus": {
                    "type": "string",
                    "description":
                        "Optional instruction telling the summariser what to \
                         emphasise, e.g. 'just the action items' or 'decisions \
                         about the hiring process'."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = args.get("_user_id").and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError(
                "summarize_conversation called without _user_id (chat handler must inject)".to_string()
            ))?
            .to_owned();

        let conv_id = args.get("conversation_id").and_then(|v| v.as_str()).unwrap_or("").trim();
        if conv_id.is_empty() {
            return Ok(ToolResult::failure(
                "summarize_conversation: `conversation_id` is required",
            ));
        }

        let max_messages = args.get("max_messages").and_then(|v| v.as_i64())
            .unwrap_or(DEFAULT_MAX_MESSAGES)
            .clamp(1, HARD_MAX_MESSAGES);

        let focus = args.get("focus").and_then(|v| v.as_str())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());

        // Ownership check.
        let conv = match self.history.get_conversation(conv_id)? {
            Some(c) => c,
            None    => return Ok(ToolResult::failure(
                format!("summarize_conversation: conversation {} not found", conv_id),
            )),
        };
        if conv.user_id != user_id {
            // Mirror the "not found" error shape to avoid leaking existence.
            return Ok(ToolResult::failure(
                format!("summarize_conversation: conversation {} not found", conv_id),
            ));
        }

        let messages = self.history.get_messages(conv_id, max_messages, None)?;
        if messages.is_empty() {
            return Ok(ToolResult::failure(
                format!("summarize_conversation: conversation {} has no messages", conv_id),
            ));
        }

        debug!(
            "summarize_conversation: user={} conv={} messages={} focus={:?}",
            user_id, conv_id, messages.len(), focus,
        );

        let transcript = build_transcript(&messages);
        let (system_prompt, user_prompt) = build_prompts(&transcript, conv.title.as_deref(), focus.as_deref());

        let opts = GenerationOptions {
            temperature: 0.3, // want factual, low-creativity summaries
            max_tokens:  Some(600),
            ..Default::default()
        };

        let response = self.provider.generate(
            &[
                ChatMessage::system(system_prompt),
                ChatMessage::user(user_prompt),
            ],
            &opts,
        ).await?;

        let body = json!({
            "conversation_id":    conv_id,
            "title":              conv.title,
            "messages_summarised": messages.len(),
            "summary":            response.content.trim(),
        });
        Ok(ToolResult::success(body.to_string()))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_transcript(messages: &[crate::history::Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            MessageRole::User      => "USER",
            MessageRole::Assistant => "ASSISTANT",
            MessageRole::System    => continue, // skip system turns
            MessageRole::Tool      => "TOOL",
        };
        let snippet = if m.content.chars().count() > PER_MESSAGE_PREVIEW_CHARS {
            let mut s: String = m.content.chars().take(PER_MESSAGE_PREVIEW_CHARS).collect();
            s.push('…');
            s
        } else {
            m.content.clone()
        };
        out.push_str(role);
        out.push_str(": ");
        out.push_str(snippet.trim());
        out.push_str("\n\n");
    }
    out
}

fn build_prompts(transcript: &str, title: Option<&str>, focus: Option<&str>) -> (String, String) {
    let system = "You are a conversation summariser. Produce a tight, faithful \
                  summary of the transcript the user provides. Cover: the main \
                  topics, key decisions, outstanding questions, and any concrete \
                  action items. Do not invent content that isn't in the \
                  transcript. Do not quote whole messages — paraphrase. Reply \
                  with the summary only, no preamble.".to_string();

    let mut user = String::new();
    if let Some(t) = title {
        user.push_str(&format!("Conversation title: {}\n\n", t));
    }
    if let Some(f) = focus {
        user.push_str(&format!("Focus: {}\n\n", f));
    }
    user.push_str("Transcript:\n");
    user.push_str(transcript);

    (system, user)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Message;
    use chrono::Utc;

    fn msg(role: MessageRole, content: &str) -> Message {
        Message {
            id:              format!("m-{}", content.len()),
            conversation_id: "c".into(),
            role,
            content:         content.into(),
            content_type:    "text".into(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            created_at:      Utc::now().timestamp_millis(),
            metadata:        None,
        }
    }

    #[test]
    fn build_transcript_skips_system_and_labels_roles() {
        let msgs = vec![
            msg(MessageRole::System,    "you are mira"),
            msg(MessageRole::User,      "hello"),
            msg(MessageRole::Assistant, "hi there"),
        ];
        let t = build_transcript(&msgs);
        assert!(!t.contains("you are mira"));
        assert!(t.contains("USER: hello"));
        assert!(t.contains("ASSISTANT: hi there"));
    }

    #[test]
    fn build_transcript_truncates_huge_messages() {
        let big = "x".repeat(PER_MESSAGE_PREVIEW_CHARS + 50);
        let msgs = vec![msg(MessageRole::User, &big)];
        let t = build_transcript(&msgs);
        assert!(t.contains('…'));
        // Per-message cap + role label + newlines — still must be well under
        // double the cap.
        assert!(t.chars().count() < PER_MESSAGE_PREVIEW_CHARS + 100);
    }

    #[test]
    fn build_prompts_includes_title_and_focus() {
        let (_sys, user) = build_prompts("USER: hi\n", Some("Dinner plans"), Some("action items"));
        assert!(user.contains("Dinner plans"));
        assert!(user.contains("action items"));
        assert!(user.contains("USER: hi"));
    }

    #[test]
    fn build_prompts_without_title_or_focus() {
        let (sys, user) = build_prompts("USER: hi\n", None, None);
        assert!(!sys.is_empty());
        assert!(!user.contains("Conversation title"));
        assert!(!user.contains("Focus:"));
    }
}
