// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/mod.rs
//! Automations.
//!
//! Schedules, webhooks, and event triggers all funnel into one action
//! dispatcher.  ships the time-driven half (schedules + heartbeats);
//! later slices add webhook ingest, the internal event bus, agent-facing
//! tools, and outbound HTTP actions.
//!
//! See `design-docs/phase10-automations.md` for the full design.

pub mod agent_gate;
pub mod dispatch;
pub mod heartbeats;
pub mod next_run_at;
pub mod predicate;
pub mod quiet_hours;
pub mod rate_limit;
pub mod store;
pub mod template;
pub mod types;
pub mod worker;

pub use dispatch::Dispatcher;
pub use rate_limit::{ChannelRateLimiter, RateDecision};
pub use store::{AutomationsStore, AutomationRun, RunFilter, WatchdogIncident, NewWatchdogIncident, open_and_seed};
pub use types::{
    Action, AutomationStatus, ConversationStrategy, EventSubscription,
    NewEventSubscription, NewSchedule, NewWebhook, OwnerKind, PromptAction,
    QuietHours, RunOutcome, Schedule, ScheduleStatus, TriggerSpec,
    UpdateEventSubscription, UpdateSchedule, UpdateWebhook, Webhook, WebhookPayload,
};
pub use worker::Worker;
