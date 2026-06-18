// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/system_audit.rs
//! Heartbeat: hourly self-audit.
//!
//! Wraps [`crate::health::collector::run_audit`] so the existing
//! heartbeat dispatcher can fire it on the seeded `heartbeat.system_audit`
//! schedule. Cadence: hourly at :15 (offset from `heartbeat.automations_cleanup`
//! at :30 so the two never contend for the database).

use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;

use crate::automations::AutomationsStore;
use crate::health::{
    collector,
    detectors,
    store::HealthStore,
    ActionPolicy,
    DetectorContext,
};
use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

pub const SYSTEM_AUDIT_TASK_NAME: &str = "system_audit";

/// Pulled together at registry construction time. Detectors are
/// recreated each fire (cheap — no state) but the stores and the
/// notify_user_id are stable for the heartbeat's lifetime.
pub struct SystemAudit {
    health_store:    Arc<HealthStore>,
    automations:     Arc<AutomationsStore>,
    audit_store:     Option<Arc<crate::agent::AuditStore>>,
    /// 0.106.0 — slice 2 deps.
    agent_registry:  Option<Arc<crate::agent::AgentRegistry>>,
    auth_db:         Option<Arc<crate::auth::AuthDb>>,
    log_path:        Option<std::path::PathBuf>,
    /// 0.108.0 — slice 3b deps. ChannelManager powers the Signal
    /// daemon-alive detector + restart action; SecretsStore powers
    /// dangling-secrets detection + sweep.
    channel_manager: Option<Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>>,
    secrets_store:   Option<Arc<crate::skills::SecretsStore>>,
    /// 0.110.0 — slice 5d. ChannelAccountStore powers the Telegram
    /// reachability + signal-received detectors.
    channel_accounts: Option<Arc<crate::channel_accounts::ChannelAccountStore>>,
    /// Live subsystem-fallback tracker (read by `subsystem.degraded`).
    degradations:    Option<Arc<crate::health::degradation::DegradationTracker>>,
    embedding_provider_kind: String,
    notify_user_id:  Option<String>,
}

impl SystemAudit {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        health_store:            Arc<HealthStore>,
        automations:             Arc<AutomationsStore>,
        audit_store:             Option<Arc<crate::agent::AuditStore>>,
        agent_registry:          Option<Arc<crate::agent::AgentRegistry>>,
        auth_db:                 Option<Arc<crate::auth::AuthDb>>,
        log_path:                Option<std::path::PathBuf>,
        channel_manager:         Option<Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>>,
        secrets_store:           Option<Arc<crate::skills::SecretsStore>>,
        channel_accounts:        Option<Arc<crate::channel_accounts::ChannelAccountStore>>,
        degradations:            Option<Arc<crate::health::degradation::DegradationTracker>>,
        embedding_provider_kind: String,
        notify_user_id:          Option<String>,
    ) -> Self {
        Self {
            health_store, automations, audit_store,
            agent_registry, auth_db, log_path,
            channel_manager, secrets_store, channel_accounts,
            degradations, embedding_provider_kind, notify_user_id,
        }
    }
}

#[async_trait]
impl HeartbeatTask for SystemAudit {
    fn name(&self) -> &'static str { SYSTEM_AUDIT_TASK_NAME }

    async fn run(
        &self,
        ctx:   &HeartbeatContext,
        _args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        let mira_version = match semver::Version::parse(env!("CARGO_PKG_VERSION")) {
            Ok(v)  => v,
            Err(e) => {
                warn!("system_audit: failed to parse mira version: {e}");
                semver::Version::new(0, 0, 0)
            }
        };
        let det_ctx = DetectorContext {
            data_dir:     ctx.data_dir.clone(),
            automations:  Some(Arc::clone(&self.automations)),
            audit_store:  self.audit_store.clone(),
            embedding_provider_kind: self.embedding_provider_kind.clone(),
            mira_version,
            agent_registry: self.agent_registry.clone(),
            auth_db:        self.auth_db.clone(),
            log_path:       self.log_path.clone(),
            channel_manager: self.channel_manager.clone(),
            secrets_store:   self.secrets_store.clone(),
            channel_accounts: self.channel_accounts.clone(),
            degradations:    self.degradations.clone(),
        };
        let detectors = detectors::default_registry();

        // 0.109.0 — sweep expired snoozes first. Cheap conditional
        // UPDATE; restores the user's declared policy automatically
        // for any detector whose snooze window has passed.
        if let Err(e) = self.health_store.clear_expired_snoozes() {
            warn!("system_audit: clear_expired_snoozes failed: {e}");
        }

        // 0.107.0 — policy comes from `health_signal_config` (admin can
        // override per-detector via the dashboard). Detectors absent
        // from the table fall back to NotifyOnly (the slice 1+2
        // default). 0.109.0 — when `snooze_until` is in the future, the
        // declared policy is overridden to `Disabled` for this fire.
        let overrides = self.health_store.list_signal_configs().unwrap_or_else(|e| {
            warn!("system_audit: signal-config read failed (defaulting all to notify_only): {e}");
            Vec::new()
        });
        let now = chrono::Utc::now().timestamp();
        let policy_for: std::collections::HashMap<String, ActionPolicy> = overrides
            .into_iter()
            .filter_map(|r| {
                let snoozed = r.snooze_until.map(|t| t > now).unwrap_or(false);
                let policy = if snoozed {
                    ActionPolicy::Disabled
                } else {
                    match r.policy.as_str() {
                        "disabled"     => ActionPolicy::Disabled,
                        "notify_only"  => ActionPolicy::NotifyOnly,
                        "auto_cleanup" => ActionPolicy::AutoCleanup,
                        _              => return None,
                    }
                };
                Some((r.detector_name, policy))
            })
            .collect();

        let outcome = collector::run_audit(
            &detectors,
            &det_ctx,
            &self.health_store,
            Some(&self.automations),
            self.notify_user_id.as_deref(),
            ctx.event_bus.as_deref(),
            |name| policy_for.get(name).copied().unwrap_or(ActionPolicy::NotifyOnly),
        )?;
        // 0.110.0 — opportunistic ledger pruning so the cost-burn
        // detector keeps running fast.
        if let Err(e) = self.health_store.prune_old_charges(chrono::Utc::now().timestamp()) {
            warn!("system_audit: prune_old_charges failed (non-fatal): {e}");
        }

        let summary = format!(
            "system_audit: detectors={}, worst={}, triggered={}, incidents_filed={}, dedup_skipped={}",
            outcome.snapshot.reports.len(),
            outcome.snapshot.worst_level().as_str(),
            outcome.snapshot.triggered_count(),
            outcome.incidents_filed,
            outcome.dedup_skipped,
        );
        Ok(HeartbeatOutcome { summary })
    }
}
