// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/app.rs
use std::sync::Arc;

use crate::tui::backend::CatalogSnapshot;
use crate::tui::theme::{Theme, MIRA_DARK};
use crate::tui::layout::LayoutMode;
use crate::tui::completion::CompletionItem;

/// Per-turn token + cost summary rendered in the status bar after a stream
/// finishes. `cost_usd` is `None` for local providers and for OpenRouter
/// models we don't have pricing for in the cached catalog — the renderer
/// shows tokens only in that case.
#[derive(Debug, Clone)]
pub struct LastTurnCost {
    pub prompt:     u32,
    pub completion: u32,
    pub cost_usd:   Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone)]
pub struct ChatEntry {
    pub role:      Role,
    pub content:   String,
    pub timestamp: String, // RFC3339
}

pub struct AppState {
    // chat
    pub messages:          Vec<ChatEntry>,
    pub streaming_buffer:  String,
    pub is_streaming:      bool,
    pub scroll_offset:     u16,
    pub auto_scroll:       bool,
    // Conversation id in the history DB — None until the first message
    // creates it; reset to None on /clear and /session-clear so the next
    // message starts a new persisted conversation.
    pub conv_id:           Option<String>,
    // input
    pub input:             String,
    pub input_cursor:      usize,
    // completions
    pub completions:       Vec<CompletionItem>,
    pub completion_sel:    Option<usize>,
    pub show_completions:  bool,
    // command palette
    pub palette_open:      bool,
    pub palette_query:     String,
    pub palette_sel:       usize,
    // history
    pub history:           Vec<String>,
    pub history_pos:       Option<usize>,
    // provider info (display only)
    pub provider_label:    String,
    pub model_label:       String,
    pub token_count:       usize,
    pub tool_count:        usize,
    pub memory_count:      usize,
    pub session_id:        String,
    pub health_ok:         bool,
    /// "local" or "server" — rendered as [local]/[server] in the status bar
    /// and used to gate server-only behaviour (e.g. the server-unreachable
    /// banner). Set from `TuiUiConfig::backend_label` in `tui::run`.
    pub backend_label:     String,
    // UI
    pub layout_mode:       LayoutMode,
    pub theme:             Theme,
    pub should_quit:       bool,
    /// OpenRouter catalog cached in-process for this TUI session. Populated
    /// asynchronously on startup and on `/openrouter-refresh`. `None` until
    /// the first fetch completes (or fails).
    pub openrouter_catalog: Option<Arc<CatalogSnapshot>>,
    /// Last-turn cost line shown in the status bar. Refreshed on each
    /// stream's `Done` event; cleared on `/clear` and `/session-clear`.
    pub last_turn_cost:     Option<LastTurnCost>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            messages:         Vec::new(),
            streaming_buffer: String::new(),
            is_streaming:     false,
            scroll_offset:    0,
            auto_scroll:      true,
            conv_id:          None,
            input:            String::new(),
            input_cursor:     0,
            completions:      Vec::new(),
            completion_sel:   None,
            show_completions: false,
            palette_open:     false,
            palette_query:    String::new(),
            palette_sel:      0,
            history:          Vec::new(),
            history_pos:      None,
            provider_label:   "lmstudio".to_string(),
            model_label:      "unknown".to_string(),
            token_count:      0,
            tool_count:       0,
            memory_count:     0,
            session_id:       uuid_short(),
            health_ok:        false,
            backend_label:    "local".to_string(),
            layout_mode:      LayoutMode::Standard,
            theme:            MIRA_DARK.clone(),
            should_quit:      false,
            openrouter_catalog: None,
            last_turn_cost:    None,
        }
    }

    pub fn push_message(&mut self, role: Role, content: String) {
        let timestamp = chrono::Utc::now().to_rfc3339();
        self.messages.push(ChatEntry { role, content, timestamp });
        if self.auto_scroll {
            self.scroll_offset = u16::MAX; // clamped in render
        }
    }

    pub fn flush_stream(&mut self) {
        if !self.streaming_buffer.is_empty() {
            let content = std::mem::take(&mut self.streaming_buffer);
            self.push_message(Role::Assistant, content);
        }
        self.is_streaming = false;
    }

    pub fn push_to_history(&mut self, cmd: String) {
        if self.history.last().map(|s| s.as_str()) != Some(&cmd) {
            self.history.push(cmd);
        }
        self.history_pos = None;
    }
}

fn uuid_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{:x}", t.subsec_nanos() as u64 ^ t.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_appstate_push_message() {
        let mut s = AppState::new();
        s.push_message(Role::User, "hello".to_string());
        s.push_message(Role::Assistant, "hi".to_string());
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].content, "hi");
    }
    #[test]
    fn test_appstate_input_ops() {
        let mut s = AppState::new();
        s.input = "helo".to_string();
        s.input_cursor = 4;
        if !s.input.is_empty() {
            s.input.pop();
            s.input_cursor = s.input_cursor.saturating_sub(1);
        }
        assert_eq!(s.input, "hel");
        assert_eq!(s.input_cursor, 3);
    }
    #[test]
    fn test_history_navigation() {
        let mut s = AppState::new();
        s.history = vec!["cmd1".to_string(), "cmd2".to_string()];
        s.history_pos = Some(s.history.len() - 1);
        let entry = &s.history[s.history_pos.unwrap()];
        assert_eq!(entry, "cmd2");
    }
}
