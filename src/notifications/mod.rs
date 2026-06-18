// SPDX-License-Identifier: AGPL-3.0-or-later

// src/notifications/mod.rs
//! In-process notification bus for cross-channel events.
//!
//! When a Signal or Telegram message arrives while users have the web UI open,
//! the `NotificationBus` broadcasts a `Notification` to all SSE subscribers.

pub mod web_push;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// A new inbound message arrived on a non-web channel (Signal, Telegram, TUI).
    InboundMessage,
    /// An existing conversation received a new assistant reply (any channel).
    ConversationUpdated,
    /// A subsystem silently fell back to a degraded path (e.g. TTS → Piper,
    /// the embedding server → internal fastembed). `message` carries the
    /// human-readable description; surfaced as a toast + a health indicator.
    SystemDegraded,
    /// MIRA-Guardian's proactive watch loop surfaced a health alert (P3).
    /// `message` carries the Guardian's operator-facing summary.
    GuardianAlert,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub kind:            NotificationKind,
    pub conversation_id: Option<String>,
    pub channel:         Option<String>,
    pub user_id:         Option<String>,
    /// Short preview of the message (for inbound notifications).
    pub message:         Option<String>,
}

// ── Bus ───────────────────────────────────────────────────────────────────────

/// Cheap-to-clone handle to the notification broadcast channel.
pub struct NotificationBus {
    sender: broadcast::Sender<Notification>,
}

impl NotificationBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self { sender }
    }

    /// Broadcast a notification to all SSE subscribers (fire-and-forget).
    pub fn send(&self, notif: Notification) {
        let _ = self.sender.send(notif);
    }

    /// Subscribe to the notification stream.
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.sender.subscribe()
    }
}

impl Default for NotificationBus {
    fn default() -> Self { Self::new() }
}
