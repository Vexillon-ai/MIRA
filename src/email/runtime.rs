// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/runtime.rs
//! Per-account poller registry (slice E1+E3, chunk 2).
//!
//! Holds the live `SharedStatus` for every enabled account so the
//! `/api/email/status` endpoint can return a UI-ready snapshot.
//! Spawned by the gateway at startup; the registry itself doesn't
//! reload — changes to accounts via CRUD take effect on next
//! restart, matching the MCP registry's behaviour.

use std::sync::Arc;

use tracing::warn;

use crate::agent::AgentCore;
use crate::email::audit::EmailAuditStore;
use crate::email::imap_poll::{self, EmailPollerStatus, SharedStatus};
use crate::email::quarantine::EmailQuarantineStore;
use crate::email::rate::InMemoryRateLimiter;
use crate::email::smtp::ReplyLoopCache;
use crate::email::store::EmailAccountStore;
use crate::history::HistoryStore;
use crate::web::LiveConfig;

pub struct EmailPollerRegistry {
    statuses: Vec<SharedStatus>,
    /// Held so `Drop` aborts the tasks on a gateway shutdown — not
    /// strictly necessary for correctness (tokio cancels on runtime
    /// drop) but makes intent explicit.
    _handles: Vec<tokio::task::JoinHandle<()>>,
    /// E2 — shared reply-loop cache. Exposed (via Arc clone) to
    /// the quarantine-approval handler so manual approvals go
    /// through the same loop guard as the live poller.
    pub loop_cache: Arc<ReplyLoopCache>,
}

impl EmailPollerRegistry {
    pub fn empty() -> Self {
        Self {
            statuses: Vec::new(),
            _handles: Vec::new(),
            loop_cache: Arc::new(ReplyLoopCache::new()),
        }
    }

    /// Spawn one poller task per enabled account in the store.
    /// Per-account spawn failures (bad row data) are logged at WARN;
    /// the rest of the registry comes up regardless.
    ///
    /// `history` + `agent` are threaded down to chunk 4's dispatch
    /// path so an accepted inbound email can turn into an actual
    /// MIRA conversation turn.
    pub fn start_all(
        store:      Arc<EmailAccountStore>,
        history:    Arc<HistoryStore>,
        agent:      Arc<AgentCore>,
        quarantine: Arc<EmailQuarantineStore>,
        audit:      Arc<EmailAuditStore>,
        live_cfg:   Arc<LiveConfig>,
    ) -> Self {
        let rows = match store.list_all_enabled() {
            Ok(r)  => r,
            Err(e) => {
                warn!("email: list_all_enabled failed: {e}");
                return Self::empty();
            }
        };

        // One rate-limiter instance shared across every poller task
        // — per-account / per-sender counters live here, so two
        // pollers can't accidentally let the same flood through by
        // each having their own bucket.
        let rate = Arc::new(InMemoryRateLimiter::new());
        // Same posture for the reply-loop cache: one shared instance
        // so the inbound dispatch path + the manual quarantine
        // approval path both consult the same (recipient, body)
        // history.
        let loop_cache = Arc::new(ReplyLoopCache::new());

        let mut statuses = Vec::new();
        let mut handles  = Vec::new();
        for row in rows {
            let (status, handle) = imap_poll::spawn_poller(
                row,
                Arc::clone(&store),
                Arc::clone(&history),
                Arc::clone(&agent),
                Arc::clone(&quarantine),
                Arc::clone(&audit),
                Arc::clone(&rate),
                Arc::clone(&loop_cache),
                Arc::clone(&live_cfg),
            );
            statuses.push(status);
            handles.push(handle);
        }

        Self { statuses, _handles: handles, loop_cache }
    }

    /// Snapshot used by `/api/email/status`. Awaits each per-account
    /// read-lock; cheap since the poller only write-locks at cycle
    /// boundaries.
    pub async fn snapshot_for_user(&self, user_id: &str) -> Vec<EmailPollerStatus> {
        let mut out = Vec::new();
        for s in &self.statuses {
            let snap = s.read().await.clone();
            if snap.owner_user_id == user_id {
                out.push(snap);
            }
        }
        out
    }
}
