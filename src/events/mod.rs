// SPDX-License-Identifier: AGPL-3.0-or-later

// src/events/mod.rs
//! internal event bus.
//!
//! A tokio broadcast channel that lets any subsystem emit a typed [`Event`]
//! and lets the automations [`subscriber`] task fan those out to user-defined
//! event subscriptions. Distinct from [`crate::notifications::NotificationBus`],
//! which is a SSE feed pushed to open browsers — events here are structured
//! triggers consumed by automation rules, not user-facing notifications.
//!
//! Why broadcast and not mpsc? Multiple consumers (the subscriber loop, plus
//! future subsystems like analytics or eval probes) need to see every event.
//! Receivers that fall behind drop oldest messages — cheaper than back-
//! pressuring the producer, since events are advisory: missing one
//! `tool.failed` ping is not load-bearing.
//!
//! Producers call [`EventBus::emit`] (lossy when no subscribers), consumers
//! call [`EventBus::subscribe`] for a fresh receiver.

pub mod subscriber;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

// Buffer size for the broadcast channel. Slow subscribers older than this
// many messages start losing the oldest ones. 1024 is generous for the
// volume we expect (ones-of-events-per-second under normal load).
const CHANNEL_CAPACITY: usize = 1024;

// Names of events the system emits. Stored as strings so user-authored
// subscriptions can reference them in their `event_name` column. Adding a
// new event = (1) append a constant here, (2) call `EventBus::emit` from
// the producing subsystem.
pub mod names {
    // A user message landed on a channel (web/telegram/signal). Payload
    // includes `{user_id, channel, conversation_id, text}`.
    pub const MESSAGE_RECEIVED:   &str = "message.received";
    // A tool execution failed. Payload `{user_id, tool, error, args?}`.
    pub const TOOL_FAILED:        &str = "tool.failed";
    // A conversation has been idle for the configured threshold. Payload
    // `{user_id, conversation_id, channel, idle_secs}`.
    pub const CONVERSATION_IDLE:  &str = "conversation.idle";
    // Memory store crossed a size threshold. Payload `{user_id, count, threshold}`.
    pub const MEMORY_THRESHOLD:   &str = "memory.threshold.crossed";
    // An onboarding group is stuck — user opened it then stopped responding.
    // Payload `{user_id, group_id, idle_secs}`.
    pub const ONBOARDING_STALE:   &str = "onboarding.group.stale";
    // A spawned worker reached a terminal state. Payload
    // `{task_id, skill, status, summary, failure_reason?, spent_usd}`.
    // Powers `spawn_background_task`'s "ping me when done" delivery —
    // auto-registered subscriptions filter by `payload.task_id`.
    pub const AGENT_WORKER_COMPLETED: &str = "agent.worker.completed";
    // A workflow run (Phase C orchestration) reached a terminal state.
    // Payload `{run_id, workflow, status, summary, failure_reason?,
    // status_emoji, status_label, summary_or_error}`. Powers
    // `run_workflow`'s completion delivery — auto-registered subscriptions
    // filter by `payload.run_id`.
    pub const AGENT_WORKFLOW_COMPLETED: &str = "agent.workflow.completed";
    // Slice W1 watchdog matched a log/audit line above its severity
    // threshold. Payload `{severity, source, module, message,
    // fingerprint, first_seen_at, recent_count}`. A system-seeded
    // event_subscription routes this to a ChannelMessage whenever
    // `automations.watchdog.notify_user_id` is set.
    pub const WATCHDOG_ALERT: &str = "watchdog.alert";
}

// One event flowing through the bus.
// // `name` identifies the event class (matches the `event_name` of any
// subscription); `payload` carries the event-specific JSON read by the
// subscription's predicate and any action templates. `user_id` is split out
// of the payload for the subscriber loop's authorization filter — only
// subscriptions owned by the same user (or system-owned) fire. `None` means
// the event is system-wide (e.g. a global heartbeat result).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub name:    String,
    pub user_id: Option<String>,
    pub payload: Value,
    pub at:      i64,
}

impl Event {
    pub fn new(name: impl Into<String>, user_id: Option<String>, payload: Value) -> Self {
        Self {
            name: name.into(),
            user_id,
            payload,
            at: chrono::Utc::now().timestamp(),
        }
    }
}

// Multi-producer multi-consumer event bus. Cheap to clone (`Arc`-backed
// internally); pass clones to subsystems that need to emit events.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    // Publish an event. Returns silently if no subscriber is listening —
    // events are advisory, and "no automation cares" is a normal state.
    pub fn emit(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    // Convenience wrapper — most call sites just want to push a payload
    // against a fixed event name + user.
    pub fn emit_named(&self, name: &str, user_id: Option<String>, payload: Value) {
        self.emit(Event::new(name, user_id, payload));
    }

    // Subscribe for events. Each receiver sees every event emitted *after*
    // `subscribe()` is called — replay isn't supported, so wire subscribers
    // before the systems that emit.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    // Approximate count of active receivers. Useful in tests to confirm a
    // subscriber wired up before the producer fired.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self { Self::new() }
}

// Helper used by the predicate evaluator and template renderer to flatten an
// `Event` into the activation context. Returns `{event: {name, at},
// payload: <event.payload>, user: {id}}` so subscriptions can write
// `payload.foo` exactly the same way webhooks do.
pub fn event_to_context(ev: &Event) -> Value {
    let mut event_obj: HashMap<&str, Value> = HashMap::new();
    event_obj.insert("name", Value::String(ev.name.clone()));
    event_obj.insert("at",   Value::Number(ev.at.into()));
    let user = match &ev.user_id {
        Some(id) => serde_json::json!({"id": id}),
        None     => serde_json::json!({"id": null}),
    };
    serde_json::json!({
        "event":   event_obj,
        "payload": ev.payload,
        "user":    user,
        "now":     chrono::Utc::now().timestamp(),
    })
}

// Type alias for the shareable event bus passed via `Arc` into subsystems.
pub type SharedEventBus = Arc<EventBus>;

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn emit_and_receive() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.emit_named("test.event", Some("u1".into()), json!({"k": 1}));
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.name, "test.event");
        assert_eq!(ev.user_id.as_deref(), Some("u1"));
        assert_eq!(ev.payload, json!({"k": 1}));
    }

    #[tokio::test]
    async fn no_subscriber_is_not_an_error() {
        let bus = EventBus::new();
        // No subscribers — emit must not panic or block.
        bus.emit_named("dropped.into.void", None, json!({}));
    }

    #[tokio::test]
    async fn multiple_subscribers_each_see_event() {
        let bus = EventBus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.emit_named("fanout", None, json!({}));
        assert_eq!(a.recv().await.unwrap().name, "fanout");
        assert_eq!(b.recv().await.unwrap().name, "fanout");
    }

    #[test]
    fn event_to_context_shape() {
        let ev = Event::new("my.event", Some("alice".into()), json!({"branch": "main"}));
        let ctx = event_to_context(&ev);
        assert_eq!(ctx["payload"]["branch"], json!("main"));
        assert_eq!(ctx["user"]["id"], json!("alice"));
        assert_eq!(ctx["event"]["name"], json!("my.event"));
    }
}
