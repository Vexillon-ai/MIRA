// SPDX-License-Identifier: AGPL-3.0-or-later

// src/notifications/mod.rs
//! In-process notification bus for cross-channel events.
//!
//! When a Signal or Telegram message arrives while users have the web UI open,
//! the `NotificationBus` broadcasts a `Notification` to all SSE subscribers.

pub mod fcm;
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
    /// Optional fine-grained category that overrides the kind-derived
    /// envelope `type`/`severity`. Set to `"wellbeing"` (or `"care"`) on
    /// companion safety / care-network escalations so the envelope is
    /// tagged `type:"care", severity:"high"` — the mobile app routes
    /// those to a high-priority, non-collapsible notification channel.
    /// `None` for ordinary events. Defaults to `None` so every existing
    /// call site stays source-compatible via `..Default::default()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category:        Option<String>,
}

impl Default for Notification {
    fn default() -> Self {
        Self {
            kind:            NotificationKind::ConversationUpdated,
            conversation_id: None,
            channel:         None,
            user_id:         None,
            message:         None,
            category:        None,
        }
    }
}

/// Canonical, transport-agnostic notification payload sent to every
/// proactive surface — the SSE stream, Web Push, and FCM. It is a
/// **superset** of the legacy SSE shape: the original `kind` / `channel`
/// / `message` / `conversation_id` fields are retained verbatim so the
/// existing web client keeps working, and the structured envelope fields
/// (`type` / `category` / `severity` / `title` / `body` / `url` /
/// `sent_at`) are added for native clients (mobile app).
#[derive(Debug, Clone, Serialize)]
pub struct NotificationEnvelope {
    // ── envelope (native clients) ──────────────────────────────────────
    /// Coarse class: "message" | "conversation" | "care" | "system" | "guardian".
    pub r#type:          String,
    /// Fine-grained category (e.g. "inbound", "checkin", "wellbeing",
    /// "system_degraded", "guardian"). Drives client-side channel routing.
    pub category:        String,
    /// "high" (care/wellbeing) | "normal". Maps to FCM android priority.
    pub severity:        String,
    /// Ready-to-display notification title.
    pub title:           String,
    /// Ready-to-display notification body.
    pub body:            String,
    /// Deep-link path within the app/web UI, when the event targets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url:             Option<String>,
    /// Milliseconds since the Unix epoch when the envelope was built.
    pub sent_at:         i64,

    // ── legacy SSE fields (kept for the existing web client) ───────────
    pub kind:            NotificationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel:         Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message:         Option<String>,
}

impl Notification {
    /// Build the canonical envelope for this notification, stamping
    /// `sent_at` with the current time. Centralises the title/body and
    /// type/severity derivation so the SSE stream, Web Push, and FCM all
    /// agree on one shape.
    pub fn to_envelope(&self) -> NotificationEnvelope {
        // Care/wellbeing escalations are tagged via `category` at the
        // emit site; everything else derives from the kind.
        let is_care = matches!(
            self.category.as_deref(),
            Some("wellbeing") | Some("care") | Some("safety")
        );

        let (r#type, default_category, title, body) = if is_care {
            (
                "care",
                "wellbeing",
                "MIRA — checking in".to_string(),
                self.message.clone().unwrap_or_else(|| "Reaching out about how you're doing.".to_string()),
            )
        } else {
            match self.kind {
                NotificationKind::InboundMessage => (
                    "message",
                    "inbound",
                    "New message".to_string(),
                    self.message.clone().unwrap_or_default(),
                ),
                NotificationKind::ConversationUpdated => (
                    "conversation",
                    "conversation",
                    "MIRA".to_string(),
                    self.message.clone().unwrap_or_else(|| "New activity".to_string()),
                ),
                NotificationKind::SystemDegraded => (
                    "system",
                    "system_degraded",
                    "MIRA — subsystem degraded".to_string(),
                    self.message.clone().unwrap_or_else(|| "A subsystem fell back to a degraded path".to_string()),
                ),
                NotificationKind::GuardianAlert => (
                    "guardian",
                    "guardian",
                    "MIRA-Guardian".to_string(),
                    self.message.clone().unwrap_or_else(|| "MIRA-Guardian flagged a health issue".to_string()),
                ),
            }
        };

        let severity = if is_care { "high" } else { "normal" };
        let category = self.category.clone().unwrap_or_else(|| default_category.to_string());
        let url = self.conversation_id.as_ref().map(|c| format!("/chat/{c}"));

        NotificationEnvelope {
            r#type:          r#type.to_string(),
            category,
            severity:        severity.to_string(),
            title,
            body,
            url,
            sent_at:         chrono::Utc::now().timestamp_millis(),
            kind:            self.kind.clone(),
            conversation_id: self.conversation_id.clone(),
            channel:         self.channel.clone(),
            message:         self.message.clone(),
        }
    }
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
