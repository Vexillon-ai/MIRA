// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/types.rs
//! Shared types for the automations subsystem.
//!
//! Three activator sources (schedules, webhooks, event subscriptions) all
//! funnel into a single [`Action`] enum processed by the action dispatcher.
//! This file holds the data shapes; behavior lives next door.

use serde::{Deserialize, Serialize};

// ── Owner ────────────────────────────────────────────────────────────────────

// Who created this automation. Drives audit display and quota enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnerKind {
    // User-authored from the web UI.
    User,
    // Agent-authored via the `automations.*` tool surface.
    Agent,
    // Built-in heartbeat seeded on first boot. Editable but not deletable.
    System,
}

impl OwnerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OwnerKind::User   => "user",
            OwnerKind::Agent  => "agent",
            OwnerKind::System => "system",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "agent"  => OwnerKind::Agent,
            "system" => OwnerKind::System,
            _        => OwnerKind::User,
        }
    }
}

// ── Schedule kind / spec ─────────────────────────────────────────────────────

// How a schedule fires. Stored as the `schedule_kind` column plus a JSON
// `trigger_spec` whose shape depends on the kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSpec {
    // Fire once at `at` (unix seconds, UTC). After running, status flips
    // to `expired`.
    OneOff { at: i64 },
    // Fire every `every_secs`, anchored to last run (or `created_at` if
    // never run).
    Interval { every_secs: u64 },
    // Standard 5-field cron expression evaluated in the schedule's
    // timezone. Resolution is per-minute.
    Cron { expr: String },
}

// ── Status ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleStatus {
    Active,
    Paused,
    PendingApproval,
    Expired,
    Failed,
}

impl ScheduleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ScheduleStatus::Active          => "active",
            ScheduleStatus::Paused          => "paused",
            ScheduleStatus::PendingApproval => "pending_approval",
            ScheduleStatus::Expired         => "expired",
            ScheduleStatus::Failed          => "failed",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "paused"           => ScheduleStatus::Paused,
            "pending_approval" => ScheduleStatus::PendingApproval,
            "expired"          => ScheduleStatus::Expired,
            "failed"           => ScheduleStatus::Failed,
            _                  => ScheduleStatus::Active,
        }
    }
}

// ── Action ───────────────────────────────────────────────────────────────────

// What a fired automation does.  only implements [`Action::Internal`]
// the other variants are wired in  Defined here in full so
// migrations and storage stay stable across slices.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    // Drop a user message into a conversation and run the agent loop.
    Prompt(PromptAction),
    // Invoke a registered backend tool by name.
    ToolCall {
        tool: String,
        args: serde_json::Value,
    },
    // Built-in heartbeat handler keyed by name.
    Internal {
        task: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    // Outbound webhook.
    HttpPost {
        url:           String,
        #[serde(default)]
        headers:       std::collections::HashMap<String, String>,
        body_template: String,
        #[serde(default = "default_http_timeout")]
        timeout_secs:  u64,
        // Optional HMAC-SHA256 secret. When set, the dispatcher signs the
        // rendered body and adds `X-Mira-Signature: sha256=<hex>` plus an
        // `X-Mira-Timestamp: <unix_secs>` header for replay-protection on
        // the receiver side.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret:        Option<String>,
        // How many extra attempts to make on transient failures (network
        // errors and 5xx). Default 3 (so up to 4 attempts total). Hard
        // 4xx responses never retry.
        #[serde(default = "default_http_max_retries")]
        max_retries:   u32,
    },
    // Fire-and-forget message on a user-facing channel.
    ChannelMessage {
        channel:       String,
        #[serde(default)]
        to:            Option<String>,
        // When set (web only), write the rendered message into this
        // existing conversation. None falls back to the per-user
        // "Notifications" thread (auto-create-if-missing). Lets
        // `spawn_background_task` deliver completion notices back into
        // the conversation that started the task instead of a sibling
        // thread the user has to go find. Other channels ignore.
        #[serde(default)]
        conversation_id: Option<String>,
        text_template: String,
    },
}

fn default_http_timeout()      -> u64 { 10 }
fn default_http_max_retries()  -> u32 { 3 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptAction {
    pub conversation_strategy: ConversationStrategy,
    #[serde(default)]
    pub conversation_id:       Option<String>,
    #[serde(default)]
    pub conversation_name:     Option<String>,
    pub channel:               String,
    pub prompt:                String,
    #[serde(default)]
    pub tools_allowed:         Option<Vec<String>>,
    #[serde(default = "default_max_iter")]
    pub max_iterations:        u32,
}

fn default_max_iter() -> u32 { 10 }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationStrategy {
    // Resume an existing conversation by `conversation_id`.
    Existing,
    // Always start a new conversation. Title from `conversation_name` if set.
    New,
    // Find by `conversation_name` if exists, otherwise create with that name.
    Named,
}

// ── Run outcome ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Success,
    Failure,
    Skipped,
    Coalesced,
}

impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Success   => "success",
            RunOutcome::Failure   => "failure",
            RunOutcome::Skipped   => "skipped",
            RunOutcome::Coalesced => "coalesced",
        }
    }
}

// ── Quiet hours ──────────────────────────────────────────────────────────────

// Optional per-schedule quiet window. When firing inside the window, an
// action with user-visible side effects (`Prompt`, `ChannelMessage`) is
// skipped and rescheduled to the next non-quiet minute. Times are local to
// the schedule's `timezone`. Half-open: `[start, end)`. End-before-start
// is interpreted as overnight (e.g. `22:00`–`07:00`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuietHours {
    // `HH:MM`, 24-hour.
    pub start: String,
    // `HH:MM`, 24-hour.
    pub end:   String,
}

// ── Schedule row ─────────────────────────────────────────────────────────────

// In-memory representation of a `schedules` row.
#[derive(Debug, Clone, Serialize)]
pub struct Schedule {
    pub id:             String,
    pub user_id:        String,
    pub owner_kind:     OwnerKind,
    pub name:           String,
    pub description:    Option<String>,
    pub rationale:      Option<String>,
    pub trigger:        TriggerSpec,
    pub timezone:       String,
    pub quiet_hours:    Option<QuietHours>,
    pub action:         Action,
    pub status:         ScheduleStatus,
    pub created_at:     i64,
    pub expires_at:     Option<i64>,
    pub last_run_at:    Option<i64>,
    pub next_run_at:    Option<i64>,
    pub run_count:      i64,
    pub failure_count:  i64,
    pub max_failures:   i64,
    pub last_error:     Option<String>,
}

// Fields a caller supplies when creating a new schedule. The store fills
// in `id`, `created_at`, `next_run_at`, counters, and status defaults.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub user_id:     String,
    pub owner_kind:  OwnerKind,
    pub name:        String,
    pub description: Option<String>,
    pub rationale:   Option<String>,
    pub trigger:     TriggerSpec,
    pub timezone:    String,
    pub quiet_hours: Option<QuietHours>,
    pub action:      Action,
    pub expires_at:  Option<i64>,
    // Optional override; defaults to [`ScheduleStatus::Active`]. Used by
    // approval-gated agent creation to land in `pending_approval`.
    pub status:      Option<ScheduleStatus>,
}

// Mutable fields the PUT /api/schedules/{id} editor passes through. Owner,
// user, status, and counters stay put.
#[derive(Debug, Clone)]
pub struct UpdateSchedule {
    pub name:        String,
    pub description: Option<String>,
    pub rationale:   Option<String>,
    pub trigger:     TriggerSpec,
    pub timezone:    String,
    pub quiet_hours: Option<QuietHours>,
    pub action:      Action,
    pub expires_at:  Option<i64>,
}

// ── Webhook ────────────────────────────────────────────────────────

// Status flag for webhook + event-subscription rows. Same semantics as
// `ScheduleStatus` but a smaller universe — webhooks never `expire` (they
// stay until deleted) and don't have a `failed` terminal state distinct
// from `paused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationStatus {
    Active,
    Paused,
    PendingApproval,
}

impl AutomationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AutomationStatus::Active          => "active",
            AutomationStatus::Paused          => "paused",
            AutomationStatus::PendingApproval => "pending_approval",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "paused"           => AutomationStatus::Paused,
            "pending_approval" => AutomationStatus::PendingApproval,
            _                  => AutomationStatus::Active,
        }
    }
}

// In-memory representation of a `webhooks` row. Note `secret` is *only*
// returned to the caller at create-time; subsequent reads omit it.
#[derive(Debug, Clone, Serialize)]
pub struct Webhook {
    pub id:                  String,
    pub user_id:             String,
    pub owner_kind:          OwnerKind,
    pub name:                String,
    pub description:         Option<String>,
    pub rationale:           Option<String>,
    pub token:               String,
    // Always `None` on read; only populated by `create_webhook` /
    // `rotate_token` so the UI can display once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret:              Option<String>,
    pub predicate:           Option<serde_json::Value>,
    pub payload_template:    Option<String>,
    pub action:              Action,
    pub rate_limit_per_min:  i64,
    pub debounce_secs:       Option<i64>,
    pub status:              AutomationStatus,
    pub created_at:          i64,
    pub expires_at:          Option<i64>,
    pub last_seen_at:        Option<i64>,
    pub last_error:          Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewWebhook {
    pub user_id:            String,
    pub owner_kind:         OwnerKind,
    pub name:               String,
    pub description:        Option<String>,
    pub rationale:          Option<String>,
    pub predicate:          Option<serde_json::Value>,
    pub payload_template:   Option<String>,
    pub action:             Action,
    pub rate_limit_per_min: Option<i64>,
    pub debounce_secs:      Option<i64>,
    pub expires_at:         Option<i64>,
    pub status:             Option<AutomationStatus>,
}

#[derive(Debug, Clone)]
pub struct UpdateWebhook {
    pub name:               String,
    pub description:        Option<String>,
    pub rationale:          Option<String>,
    pub predicate:          Option<serde_json::Value>,
    pub payload_template:   Option<String>,
    pub action:             Action,
    pub rate_limit_per_min: Option<i64>,
    pub debounce_secs:      Option<i64>,
    pub expires_at:         Option<i64>,
}

// Last-N payload buffer entry — the public webhook handler keeps a
// short ring per webhook so the UI can show "last 5 payloads" and offer
// a "test replay" button.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookPayload {
    pub id:           i64,
    pub webhook_id:   String,
    pub received_at:  i64,
    pub headers_json: String,
    pub body:         String,
    pub matched:      bool,
}

// ── Event subscription ─────────────────────────────────────────────

// In-memory representation of an `event_subscriptions` row.
#[derive(Debug, Clone, Serialize)]
pub struct EventSubscription {
    pub id:           String,
    pub user_id:      String,
    pub owner_kind:   OwnerKind,
    pub name:         String,
    pub description:  Option<String>,
    pub rationale:    Option<String>,
    pub event_name:   String,
    pub predicate:    Option<serde_json::Value>,
    pub action:       Action,
    pub status:       AutomationStatus,
    pub created_at:   i64,
    pub expires_at:   Option<i64>,
    pub last_fired_at: Option<i64>,
    pub last_error:   Option<String>,
    // One-shot semantics — when `true`, the event subscriber deletes
    // this row after the first successful dispatch. Used by helpers
    // that auto-register a per-target subscription (e.g.
    // `spawn_background_task` keys delivery on `task_id` which is
    // unique per worker, so the row is dead weight after fire).
    // Failed dispatches leave the row alone for retry.
    pub delete_after_fire: bool,
}

#[derive(Debug, Clone)]
pub struct NewEventSubscription {
    pub user_id:     String,
    pub owner_kind:  OwnerKind,
    pub name:        String,
    pub description: Option<String>,
    pub rationale:   Option<String>,
    pub event_name:  String,
    pub predicate:   Option<serde_json::Value>,
    pub action:      Action,
    pub expires_at:  Option<i64>,
    pub status:      Option<AutomationStatus>,
    // See [`EventSubscription::delete_after_fire`]. Default `false`
    // preserves the historical "subscriptions persist" behaviour.
    pub delete_after_fire: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateEventSubscription {
    pub name:        String,
    pub description: Option<String>,
    pub rationale:   Option<String>,
    pub event_name:  String,
    pub predicate:   Option<serde_json::Value>,
    pub action:      Action,
    pub expires_at:  Option<i64>,
}
