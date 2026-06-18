// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/automations_cleanup.rs
//! Heartbeat: prune dead agent-owned event subscriptions.
//!
//! Two kinds of dead row this sweeps:
//!
//! 1. **Successfully-fired one-shots.** Two shapes count:
//!      a. Rows with `delete_after_fire=1` whose inline tear-down was
//!         skipped (dispatch error path).
//!      b. Pre-0.103.0 spawn-style rows: agent-owned subscriptions on
//!         `agent.worker.completed` whose predicate keys on a unique
//!         `task_id`. Those never set the flag (it didn't exist yet)
//!         but the predicate can never match again once the completion
//!         event has fired, so they're equally dead.
//!    Anything in either shape older than 24h is safe to delete.
//!
//! 2. **Stuck-without-firing `agent.worker.completed` deliveries**
//!    that sat through a long uptime without ever matching. The
//!    boot orphan sweep in `gateway/builder.rs` handles the common
//!    "service restart abandoned the worker" case at boot, but
//!    misses workers that abandoned their parent while the service
//!    stayed up. Anything older than 7d with `last_fired_at IS NULL`
//!    gets marked `failed` so it stops occupying the active set.
//!
//! Branch 2 is intentionally silent — by the time a row is 7 days
//! old, the user has already moved on and a delayed "task abandoned"
//! toast would just be noise. The boot sweep keeps the loud path
//! for the common-case immediate-restart recovery.
//!
//! Cadence: hourly. The work scales with `event_subscriptions` row
//! count and is two indexed queries plus per-row updates.

use async_trait::async_trait;
use tracing::{info, warn};

use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

/// One-shot rows fired more than this long ago whose
/// `delete_after_fire=1` flag never got tripped (legacy + dispatch
/// errors) are considered safe to delete unconditionally.
const FIRED_TTL_SECS: i64 = 24 * 60 * 60;

/// `agent.worker.completed` delivery rows that sat unfired for this
/// long during a single uptime are treated as abandoned.
const ORPHAN_TTL_SECS: i64 = 7 * 24 * 60 * 60;

pub struct AutomationsCleanup {
    store: std::sync::Arc<crate::automations::AutomationsStore>,
}

impl AutomationsCleanup {
    pub fn new(store: std::sync::Arc<crate::automations::AutomationsStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl HeartbeatTask for AutomationsCleanup {
    fn name(&self) -> &'static str { "automations_cleanup" }

    async fn run(
        &self,
        _ctx:  &HeartbeatContext,
        _args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        let now = chrono::Utc::now().timestamp();
        let fired_cutoff  = now - FIRED_TTL_SECS;
        let orphan_cutoff = now - ORPHAN_TTL_SECS;

        // Branch 1: dead-after-fire one-shot rows.
        let dead = self.store.list_dead_after_fire_subscriptions(fired_cutoff)?;
        let mut deleted = 0usize;
        for sub in &dead {
            match self.store.delete_event_subscription(&sub.id) {
                Ok(true)  => deleted += 1,
                Ok(false) => {/* race: someone deleted it already */},
                Err(e)    => warn!("automations_cleanup: delete {} failed: {e}", sub.id),
            }
        }

        // Branch 2: stuck completion subs (silent — see module doc).
        let stuck = self.store.list_stuck_completion_subscriptions(orphan_cutoff)?;
        let mut abandoned = 0usize;
        for sub in &stuck {
            if let Err(e) = self.store.fail_event_subscription(
                &sub.id, now, "abandoned (no completion event observed)",
            ) {
                warn!("automations_cleanup: mark stuck sub {} failed: {e}", sub.id);
            } else {
                abandoned += 1;
            }
        }

        let summary = format!(
            "automations_cleanup: pruned {deleted} fired one-shot row(s), \
             marked {abandoned} stuck completion sub(s) abandoned",
        );
        if deleted > 0 || abandoned > 0 {
            info!("{summary}");
        }
        Ok(HeartbeatOutcome { summary })
    }
}
