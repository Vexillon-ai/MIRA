// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/worker.rs
//! Time-driven worker.
//!
//! Ticks at a fixed cadence, claims due schedules from the store, and
//! dispatches each through the [`Dispatcher`]. Per row, the post-run
//! step recomputes `next_run_at` (or marks the schedule expired for
//! one-off rows that just fired) and persists the outcome.

use std::sync::Arc;

use chrono::Utc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

use super::dispatch::{Activation, Dispatcher};
use super::next_run_at::next_run_at;
use super::quiet_hours;
use super::store::AutomationsStore;
use super::types::{Action, RunOutcome, Schedule, ScheduleStatus, TriggerSpec};

// How often the worker wakes to look for due schedules. The design doc
// targets 30s — anything finer-grained costs DB churn for no UX win
// since cron resolution is per-minute.
pub const TICK_SECS: u64 = 30;

// Maximum schedules processed in a single tick. A burst is still
// possible when a long sleep skips many fires; capping keeps each tick
// bounded so other tokio work isn't starved.
pub const PER_TICK_LIMIT: usize = 32;

pub struct Worker {
    store:      Arc<AutomationsStore>,
    dispatcher: Arc<Dispatcher>,
}

impl Worker {
    pub fn new(store: Arc<AutomationsStore>, dispatcher: Arc<Dispatcher>) -> Self {
        Self { store, dispatcher }
    }

    // Borrow the dispatcher. The webhook + event-subscription paths drive
    // activations directly (they're not on the schedule timer).
    pub fn dispatcher(&self) -> Arc<Dispatcher> {
        Arc::clone(&self.dispatcher)
    }

    // Spawn the worker. Returns the `JoinHandle` — drop to cancel.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        info!("automations worker starting (tick={TICK_SECS}s)");
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(TICK_SECS));
            // First tick fires immediately by default — skip it so we
            // don't start by claiming everything that happened to be
            // due exactly at boot before subsystems are warm.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                self.run_once().await;
            }
        })
    }

    // Fire a specific schedule immediately, bypassing the time gate and
    // the quiet-hours gate (run-now is an explicit user action — if the
    // user wanted quiet-hours behaviour they wouldn't be hitting this
    // endpoint). Updates `last_run_at`, `next_run_at`, counters, and
    // records a run row, the same as a normal tick.
    //     // Errors only on a missing/unloadable row — handler failures are
    // captured by the dispatcher and surfaced through the run history.
    pub async fn run_now(&self, id: &str) -> Result<(), crate::MiraError> {
        let s = self.store.get_schedule(id)?
            .ok_or_else(|| crate::MiraError::ConfigError(format!("schedule {id} not found")))?;
        let now = Utc::now().timestamp();

        let activation = Activation {
            source_kind: "schedule",
            source_id:   &s.id,
            user_id:     &s.user_id,
            action:      &s.action,
            payload:     None,
            chain_ids:   &[],
        };
        let result = self.dispatcher.dispatch(activation).await;

        let expire_now = matches!(s.trigger, TriggerSpec::OneOff { .. });
        let next = if expire_now {
            None
        } else {
            super::next_run_at::next_run_at(&s.trigger, &s.timezone, now).unwrap_or(None)
        };

        match result.outcome {
            RunOutcome::Success => {
                self.store.record_success(&s.id, now, next, expire_now)?;
            }
            _ => {
                let err = result.error.unwrap_or_else(|| "unknown error".into());
                self.store.record_failure(&s.id, now, next, &err, expire_now)?;
                self.maybe_notify_dead_letter(&s.id).await;
            }
        }
        Ok(())
    }

    // when `record_failure` flipped status to `failed`, ping the
    // owner with a `channel_message` so a quiet schedule doesn't go
    // permanently dark without anyone noticing. Re-reads the row to see
    // the post-update status; logs and swallows any lookup error.
    async fn maybe_notify_dead_letter(&self, id: &str) {
        let s = match self.store.get_schedule(id) {
            Ok(Some(s)) => s,
            Ok(None)    => return,
            Err(e)      => {
                warn!("automations: dead-letter lookup({id}) failed: {e}");
                return;
            }
        };
        if !matches!(s.status, ScheduleStatus::Failed) { return; }
        let err = s.last_error.as_deref().unwrap_or("unknown");
        let outcome = self.dispatcher.notify_dead_letter(
            &s.user_id, &s.id, &s.name, s.failure_count, err,
        ).await;
        if let Some(e) = outcome.error.as_ref() {
            warn!("automations: dead-letter notify({id}) failed: {e}");
        } else {
            info!("automations: dead-letter notification sent for schedule {id}");
        }
    }

    // One pass: claim due rows, run them, persist outcomes. Public so
    // tests and `/api/automations/{id}/run-now` can drive a
    // tick without waiting on the timer.
    pub async fn run_once(&self) {
        let now = Utc::now().timestamp();
        let due = match self.store.claim_due(now, PER_TICK_LIMIT) {
            Ok(rows) => rows,
            Err(e)   => { warn!("automations: claim_due failed: {e}"); return; }
        };
        if due.is_empty() { return; }
        debug!("automations: claimed {} due schedule(s)", due.len());

        for sched in due {
            self.run_one(sched, now).await;
        }
    }

    async fn run_one(&self, sched: Schedule, now: i64) {
        // Quiet-hours gate. Only `Prompt` and `ChannelMessage` are
        // user-visible; tool/heartbeat/outbound actions run regardless of
        // window so internal hygiene isn't blocked by user-set windows.
        if let Some(qh) = sched.quiet_hours.as_ref() {
            let user_visible = matches!(
                sched.action,
                Action::Prompt(_) | Action::ChannelMessage { .. }
            );
            if user_visible && quiet_hours::is_quiet(qh, &sched.timezone, now) {
                let next = quiet_hours::quiet_end_after(qh, &sched.timezone, now);
                if let Err(e) = self.dispatcher.store.record_run(
                    "schedule", &sched.id, &sched.user_id,
                    now, Some(now), RunOutcome::Skipped,
                    None, Some("quiet_hours"), None,
                ) {
                    warn!("automations: record_run(skip) failed: {e}");
                }
                if let Err(e) = self.dispatcher.store.record_skipped(&sched.id, now, next) {
                    warn!("automations: record_skipped({}) failed: {e}", sched.id);
                }
                debug!(
                    "automations: schedule {} skipped (quiet_hours), next_run_at={:?}",
                    sched.id, next
                );
                return;
            }
        }

        let activation = Activation {
            source_kind: "schedule",
            source_id:   &sched.id,
            user_id:     &sched.user_id,
            action:      &sched.action,
            payload:     None,
            chain_ids:   &[],
        };
        let result = self.dispatcher.dispatch(activation).await;

        let expire_now = matches!(sched.trigger, TriggerSpec::OneOff { .. });

        // Recompute the next fire only for non-expiring rows. One-offs are
        // single-shot; computing a "next" is meaningless and would just
        // get overwritten with NULL by the expire path.
        let next = if expire_now {
            None
        } else {
            match next_run_at(&sched.trigger, &sched.timezone, now) {
                Ok(n)  => n,
                Err(e) => {
                    warn!("automations: next_run_at({}) failed: {e}", sched.id);
                    None
                }
            }
        };

        match result.outcome {
            super::types::RunOutcome::Success => {
                if let Err(e) = self.store.record_success(&sched.id, now, next, expire_now) {
                    warn!("automations: record_success({}) failed: {e}", sched.id);
                }
            }
            _ => {
                let err = result.error.unwrap_or_else(|| "unknown error".into());
                if let Err(e) = self.store.record_failure(&sched.id, now, next, &err, expire_now) {
                    warn!("automations: record_failure({}) failed: {e}", sched.id);
                }
                self.maybe_notify_dead_letter(&sched.id).await;
            }
        }
    }
}
