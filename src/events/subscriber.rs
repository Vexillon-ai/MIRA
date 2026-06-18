// SPDX-License-Identifier: AGPL-3.0-or-later

// src/events/subscriber.rs
//! Background task that turns [`Event`]s into automation activations.
//!
//! Subscribes to the [`EventBus`], looks up active rows in
//! `event_subscriptions` matching the event name, evaluates each row's
//! predicate against the event payload, and dispatches matching rows
//! through the automations [`Dispatcher`].
//!
//! Architectural note: this lives under `src/events/` (not under
//! `src/automations/`) because the event bus is the cross-cutting
//! abstraction; automations are one consumer. Future consumers (analytics,
//! eval probes) attach the same way.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

use crate::automations::{
    AutomationsStore, Worker,
    dispatch::Activation,
    predicate,
};

use super::{Event, EventBus, event_to_context};

/// Spawn the subscriber task. Returns the `JoinHandle` — drop to stop.
pub fn spawn(
    bus:    Arc<EventBus>,
    store:  Arc<AutomationsStore>,
    worker: Arc<Worker>,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let Err(e) = handle_event(&ev, &store, &worker).await {
                        warn!("event subscriber: handle_event failed: {e}");
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // Receiver fell behind — log and keep going. This means
                    // some events were dropped before we could process them;
                    // not fatal since events are advisory.
                    warn!("event subscriber: lagged, dropped {n} event(s)");
                }
                Err(RecvError::Closed) => {
                    debug!("event subscriber: bus closed, exiting");
                    break;
                }
            }
        }
    })
}

async fn handle_event(
    ev:     &Event,
    store:  &Arc<AutomationsStore>,
    worker: &Arc<Worker>,
) -> Result<(), crate::MiraError> {
    let subs = store.active_subscriptions_for(&ev.name)?;
    if subs.is_empty() {
        return Ok(());
    }

    let ctx = event_to_context(ev);
    let now = chrono::Utc::now().timestamp();

    for sub in subs {
        // Authorization gate. The event's `user_id` (if any) must equal the
        // subscription's owning user, OR the subscription is system-owned
        // (system rules see every user's events). Without this filter, a
        // subscription could match events emitted on another user's behalf.
        if let Some(ev_user) = ev.user_id.as_deref() {
            let owned = sub.user_id == ev_user
                || matches!(sub.owner_kind, crate::automations::OwnerKind::System);
            if !owned { continue; }
        }
        // Expired? Skip and don't bother dispatching.
        if let Some(exp) = sub.expires_at { if exp <= now { continue; } }

        // Predicate gate.
        let matched = match sub.predicate.as_ref() {
            Some(p) => match predicate::eval(p, &ctx) {
                Ok(b) => b,
                Err(e) => {
                    let msg = format!("predicate: {e}");
                    let _ = store.touch_event_subscription(&sub.id, now, Some(&msg));
                    warn!("event subscriber: predicate failed for sub {}: {msg}", sub.id);
                    false
                }
            }
            None => true,
        };
        if !matched { continue; }

        // Dispatch — payload feeds {{payload.…}} in templates.
        let payload_val = payload_for_dispatch(ev);
        let activation = Activation {
            source_kind: "event",
            source_id:   &sub.id,
            user_id:     &sub.user_id,
            action:      &sub.action,
            payload:     Some(&payload_val),
            chain_ids:   &[],
        };
        let outcome = worker.dispatcher().dispatch(activation).await;
        let _ = store.touch_event_subscription(&sub.id, now, outcome.error.as_deref());

        // One-shot subscriptions (e.g. spawn_background_task delivery
        // rules keyed on a unique task_id) get torn down after a
        // successful dispatch so the table doesn't accumulate dead
        // rows. Failed dispatches are left in place so the next
        // matching event can retry — flipping `last_fired_at`
        // doesn't gate re-firing on success either, but for
        // delete_after_fire we don't want to lose the row before
        // we know the action landed.
        if sub.delete_after_fire && outcome.error.is_none() {
            if let Err(e) = store.delete_event_subscription(&sub.id) {
                warn!("event subscriber: failed to clean up one-shot sub {}: {e}", sub.id);
            }
        }
    }

    Ok(())
}

/// The dispatcher's template context expects `payload` to be the raw event
/// body. We add `event_name` and `user_id` for templates that want to
/// reference them without diving through the wrapping context.
fn payload_for_dispatch(ev: &Event) -> Value {
    let mut p = ev.payload.clone();
    if let Value::Object(map) = &mut p {
        map.insert("_event_name".into(), Value::String(ev.name.clone()));
        if let Some(uid) = &ev.user_id {
            map.entry("_user_id".to_string())
                .or_insert_with(|| Value::String(uid.clone()));
        }
    }
    p
}
