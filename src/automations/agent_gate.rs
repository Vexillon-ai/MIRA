// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/agent_gate.rs
//! create-time gating for automations rows.
//!
//! Three knobs the gate enforces against the user's `AutomationsConfig`:
//! 1. **Quota** — per-user cap on schedules / webhooks / event-subscriptions.
//!    `User` and `Agent` owners share the bucket; `System` rows bypass.
//! 2. **Rationale** — when `agent_rationale_required` is on, agent-owned
//!    rows must include a non-empty `rationale` so the user can audit
//!    what the agent thought it was doing.
//! 3. **Approval mode** — when `agent_creates_pending` is on, agent-owned
//!    rows land in `pending_approval` instead of `active`, awaiting an
//!    explicit user decision before they can fire.
//!
//! Used by both the HTTP create handlers (when admin/UI creates rows) and
//! the agent-facing `automations.*` tools. Centralising here keeps the two
//! create paths from drifting.

use crate::config::AutomationsConfig;
use crate::MiraError;

use super::store::AutomationsStore;
use super::types::{AutomationStatus, OwnerKind, ScheduleStatus};

// Errors a gate check can produce. Caller decides 4xx vs 5xx mapping.
#[derive(Debug, Clone)]
pub enum GateError {
    QuotaExceeded { kind: &'static str, limit: usize, current: usize },
    RationaleRequired,
    Storage(String),
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QuotaExceeded { kind, limit, current } => write!(
                f, "quota exceeded for {kind}: {current}/{limit} reached"
            ),
            Self::RationaleRequired => write!(
                f, "rationale is required for agent-authored automations"
            ),
            Self::Storage(e) => write!(f, "storage: {e}"),
        }
    }
}

impl std::error::Error for GateError {}

impl From<MiraError> for GateError {
    fn from(e: MiraError) -> Self { Self::Storage(e.to_string()) }
}

// What kind of row the caller is creating. Used to pick the right quota
// bucket and human-readable error string.
#[derive(Debug, Clone, Copy)]
pub enum RowKind {
    Schedule,
    Webhook,
    EventSubscription,
}

impl RowKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Schedule          => "schedules",
            Self::Webhook           => "webhooks",
            Self::EventSubscription => "event_subscriptions",
        }
    }
}

// Check rationale + quota; return the status the row should be created with.
// `None` means "leave caller's default" (typically Active); `Some(status)`
// is the gate's override (PendingApproval for agent-owned in approval mode).
// // `system`-owned rows skip every check — they're seeded by the platform.
pub fn gate_create_schedule(
    store:      &AutomationsStore,
    config:     &AutomationsConfig,
    user_id:    &str,
    owner:      OwnerKind,
    rationale:  Option<&str>,
) -> Result<Option<ScheduleStatus>, GateError> {
    if matches!(owner, OwnerKind::System) { return Ok(None); }

    if matches!(owner, OwnerKind::Agent)
        && config.agent_rationale_required
        && rationale.map(str::trim).map(|s| s.is_empty()).unwrap_or(true)
    {
        return Err(GateError::RationaleRequired);
    }

    let current = store.count_schedules_for_user(user_id)?;
    let limit   = config.quota_per_user.schedules;
    if current >= limit {
        return Err(GateError::QuotaExceeded {
            kind: RowKind::Schedule.as_str(),
            limit,
            current,
        });
    }

    if matches!(owner, OwnerKind::Agent) && config.agent_creates_pending {
        Ok(Some(ScheduleStatus::PendingApproval))
    } else {
        Ok(None)
    }
}

pub fn gate_create_webhook(
    store:      &AutomationsStore,
    config:     &AutomationsConfig,
    user_id:    &str,
    owner:      OwnerKind,
    rationale:  Option<&str>,
) -> Result<Option<AutomationStatus>, GateError> {
    if matches!(owner, OwnerKind::System) { return Ok(None); }

    if matches!(owner, OwnerKind::Agent)
        && config.agent_rationale_required
        && rationale.map(str::trim).map(|s| s.is_empty()).unwrap_or(true)
    {
        return Err(GateError::RationaleRequired);
    }

    let current = store.count_webhooks_for_user(user_id)?;
    let limit   = config.quota_per_user.webhooks;
    if current >= limit {
        return Err(GateError::QuotaExceeded {
            kind: RowKind::Webhook.as_str(),
            limit,
            current,
        });
    }

    if matches!(owner, OwnerKind::Agent) && config.agent_creates_pending {
        Ok(Some(AutomationStatus::PendingApproval))
    } else {
        Ok(None)
    }
}

pub fn gate_create_event_subscription(
    store:      &AutomationsStore,
    config:     &AutomationsConfig,
    user_id:    &str,
    owner:      OwnerKind,
    rationale:  Option<&str>,
) -> Result<Option<AutomationStatus>, GateError> {
    if matches!(owner, OwnerKind::System) { return Ok(None); }

    if matches!(owner, OwnerKind::Agent)
        && config.agent_rationale_required
        && rationale.map(str::trim).map(|s| s.is_empty()).unwrap_or(true)
    {
        return Err(GateError::RationaleRequired);
    }

    let current = store.count_event_subscriptions_for_user(user_id)?;
    let limit   = config.quota_per_user.event_subscriptions;
    if current >= limit {
        return Err(GateError::QuotaExceeded {
            kind: RowKind::EventSubscription.as_str(),
            limit,
            current,
        });
    }

    if matches!(owner, OwnerKind::Agent) && config.agent_creates_pending {
        Ok(Some(AutomationStatus::PendingApproval))
    } else {
        Ok(None)
    }
}
