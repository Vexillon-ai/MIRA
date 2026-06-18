// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/mod.rs
//! System-owned heartbeat handlers.
//!
//! shipped `log_cleanup` and `tmp_cleanup`;  fills in the
//! remaining six (`memory_janitor`, `conversation_rollup`,
//! `embedding_refresh`, `oauth_token_refresh`, `onboarding_nudge`,
//! `weekly_reflection`). Some of those six are deliberately
//! report-and-tick: their substantive behaviour depends on subsystems that
//! land in dedicated phases (memory consolidation, OAuth token store,
//! onboarding reminders). The handlers exist now so the dispatcher routes
//! cleanly and the seeded rows show up in run history; expanding each
//! handler is a one-file change.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::MiraError;

use super::store::AutomationsStore;
use super::types::{Action, NewSchedule, OwnerKind, TriggerSpec};

pub mod automations_cleanup;
pub mod conversation_rollup;
pub mod embedding_refresh;
pub mod health_weekly_digest;
pub mod log_cleanup;
pub mod memory_janitor;
pub mod oauth_token_refresh;
pub mod onboarding_nudge;
pub mod system_audit;
pub mod tmp_cleanup;
pub mod watchdog;
pub mod weekly_reflection;

// Result a heartbeat hands back to the dispatcher. The `summary` is
// stored on the run row so the user can see what happened.
#[derive(Debug, Default)]
pub struct HeartbeatOutcome {
    pub summary: String,
}

#[async_trait]
pub trait HeartbeatTask: Send + Sync {
    // Stable identifier referenced from `Action::Internal { task: … }`.
    fn name(&self) -> &'static str;

    async fn run(
        &self,
        ctx:  &HeartbeatContext,
        args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError>;
}

// Side-channel dependencies handlers can pull from. Kept tiny — every
// new field forces  callers to rebuild, so add only what's needed.
pub struct HeartbeatContext {
    pub data_dir:  PathBuf,
    /// handlers that detect anomalous user state
    // (idle conversations, memory pressure, stale onboarding) emit an
    // event here so user automations can react. Optional: in tests and
    // builds without an event bus, emission is silently skipped.
    pub event_bus: Option<Arc<crate::events::EventBus>>,
}

pub struct HeartbeatRegistry {
    by_name: HashMap<&'static str, Arc<dyn HeartbeatTask>>,
}

impl HeartbeatRegistry {
    pub fn new() -> Self {
        let mut r = Self { by_name: HashMap::new() };
        r.register(Arc::new(log_cleanup::LogCleanup));
        r.register(Arc::new(tmp_cleanup::TmpCleanup));
        r.register(Arc::new(memory_janitor::MemoryJanitor));
        r.register(Arc::new(conversation_rollup::ConversationRollup));
        r.register(Arc::new(embedding_refresh::EmbeddingRefresh));
        r.register(Arc::new(oauth_token_refresh::OauthTokenRefresh));
        r.register(Arc::new(onboarding_nudge::OnboardingNudge));
        r.register(Arc::new(weekly_reflection::WeeklyReflection));
        r
    }

    // Registry seeded with everything in [`new`] plus the slice-W1
    // watchdog. Caller passes `enabled=false` config to skip
    // registration entirely (keeps the seeded schedule from finding
    // a handler and erroring per fire). When enabled, the watchdog
    // is constructed once and its `Arc` shared with the dispatcher.
    // W3 — the optional `incidents_store` arg wires incident
    // persistence so emitted alerts include a stable id the
    // "Analyze with LLM" link can reference.
    pub fn with_watchdog(
        watchdog_cfg:    crate::config::WatchdogConfig,
        data_dir:        PathBuf,
        log_file:        PathBuf,
        incidents_store: Option<Arc<super::store::AutomationsStore>>,
    ) -> Self {
        let mut r = Self::new();
        if watchdog_cfg.enabled {
            let mut wd = watchdog::Watchdog::new(watchdog_cfg, data_dir, log_file);
            if let Some(s) = incidents_store.as_ref() {
                wd = wd.with_incident_store(Arc::clone(s));
            }
            r.register(Arc::new(wd));
        }
        // Register the periodic cleanup whenever the store is
        // available — it's cheap and pairs with the seeded
        // `heartbeat.automations_cleanup` schedule from
        // [`seed_defaults`]. Without the store handle we couldn't
        // run the queries, so skip silently in that minimal-build
        // case (the schedule will still tick but fail-fast on
        // missing handler — same behaviour as the disabled
        // watchdog path).
        if let Some(s) = incidents_store {
            r.register(Arc::new(automations_cleanup::AutomationsCleanup::new(s)));
        }
        r
    }

    fn register(&mut self, task: Arc<dyn HeartbeatTask>) {
        self.by_name.insert(task.name(), task);
    }

    // 0.109.0 — register the weekly digest. Same dependency surface
    // as the system_audit heartbeat but only needs automations +
    // health_store + the notify recipient. Cron: Sunday 18:30.
    pub fn register_weekly_digest(
        &mut self,
        health_store:    Arc<crate::health::store::HealthStore>,
        automations:     Arc<super::store::AutomationsStore>,
        notify_user_id:  Option<String>,
    ) {
        self.register(Arc::new(health_weekly_digest::HealthWeeklyDigest::new(
            automations, health_store, notify_user_id,
        )));
    }

    // Register the slice-1+2+3b self-monitoring heartbeat. Called
    // from the gateway builder after the watchdog and stores are
    // wired so the system_audit can reach into automations +
    // agent_audit + the dedicated health DB + the live agent registry
    // + the auth DB + the channel manager + the secrets vault.
    // `notify_user_id` is reused from the watchdog config — health
    // doesn't introduce a separate knob.
    #[allow(clippy::too_many_arguments)]
    pub fn register_system_audit(
        &mut self,
        health_store:            Arc<crate::health::store::HealthStore>,
        automations:             Arc<super::store::AutomationsStore>,
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
    ) {
        self.register(Arc::new(system_audit::SystemAudit::new(
            health_store, automations, audit_store,
            agent_registry, auth_db, log_path,
            channel_manager, secrets_store, channel_accounts,
            degradations, embedding_provider_kind, notify_user_id,
        )));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn HeartbeatTask>> {
        self.by_name.get(name).cloned()
    }
}

impl Default for HeartbeatRegistry {
    fn default() -> Self { Self::new() }
}

// ── Default seeds ────────────────────────────────────────────────────────────

// Default system heartbeats inserted on first boot. Cadences match the
// design doc; user can pause individually from Settings → Automations.
// // `user_id` for system rows is the literal string `"system"` — these
// rows aren't owned by any real user. The dispatcher routes their
// actions through the system context (no per-user prompt firing).
pub fn seed_defaults(store: &AutomationsStore) -> Result<(), MiraError> {
    // Cron syntax: `sec min hour day-of-month month day-of-week`. Quartz
    // semantics — when `day-of-week` is specified, `day-of-month` must be
    // `?` (and vice-versa) because the two fields conflict.
    let seeds: &[(&str, &str, TriggerSpec, &str)] = &[
        // (name, internal task, trigger, description)
        (
            "heartbeat.log_cleanup",
            "log_cleanup",
            TriggerSpec::Cron { expr: "0 0 4 ? * SUN".into() }, // Sunday 04:00
            "Trim and rotate old MIRA logs.",
        ),
        (
            "heartbeat.tmp_cleanup",
            "tmp_cleanup",
            TriggerSpec::Cron { expr: "0 15 4 * * *".into() }, // Daily 04:15
            "Remove stale per-call sandbox scratch dirs.",
        ),
        (
            "heartbeat.memory_janitor",
            "memory_janitor",
            TriggerSpec::Cron { expr: "0 0 3 * * *".into() },  // Daily 03:00
            "Dedup, decay, and promote memory entries.",
        ),
        (
            "heartbeat.conversation_rollup",
            "conversation_rollup",
            TriggerSpec::Cron { expr: "0 15 3 * * *".into() }, // Daily 03:15
            "Summarise idle conversations into memory and archive.",
        ),
        (
            "heartbeat.embedding_refresh",
            "embedding_refresh",
            TriggerSpec::Cron { expr: "0 0 5 ? * SUN".into() },  // Sunday 05:00
            "Refresh embeddings against the current model.",
        ),
        (
            "heartbeat.oauth_token_refresh",
            "oauth_token_refresh",
            TriggerSpec::Cron { expr: "0 0 * * * *".into() },  // Hourly
            "Refresh OAuth tokens before expiry.",
        ),
        (
            "heartbeat.onboarding_nudge",
            "onboarding_nudge",
            TriggerSpec::Cron { expr: "0 0 9 * * *".into() },  // Daily 09:00
            "Nudge users with stale onboarding groups.",
        ),
        (
            "heartbeat.weekly_reflection",
            "weekly_reflection",
            TriggerSpec::Cron { expr: "0 0 18 ? * SUN".into() }, // Sunday 18:00
            "Review the past week's interactions and surface patterns.",
        ),
        (
            "heartbeat.automations_cleanup",
            "automations_cleanup",
            TriggerSpec::Cron { expr: "0 30 * * * *".into() },  // Every hour at :30
            "Prune dead agent-owned event subscriptions (one-shot deliveries that already fired, stuck completion subs).",
        ),
        (
            "heartbeat.system_audit",
            "system_audit",
            TriggerSpec::Cron { expr: "0 15 * * * *".into() },  // Every hour at :15
            "Self-audit: run health detectors and file watchdog incidents for stranded subs, master-key issues, broken skills, etc.",
        ),
        (
            "heartbeat.health_weekly_digest",
            "health_weekly_digest",
            TriggerSpec::Cron { expr: "0 30 18 ? * SUN".into() },  // Sunday 18:30
            "Weekly INFO-severity digest of system health: top detectors, noisy fingerprints, audit-pass rate.",
        ),
    ];

    for (name, task, trigger, desc) in seeds {
        let action = Action::Internal {
            task: (*task).to_string(),
            args: serde_json::Value::Null,
        };
        let res = store.ensure_system_schedule(NewSchedule {
            user_id:     "system".to_string(),
            owner_kind:  OwnerKind::System,
            name:        (*name).to_string(),
            description: Some((*desc).to_string()),
            rationale:   None,
            trigger:     trigger.clone(),
            timezone:    "UTC".to_string(),
            quiet_hours: None,
            action,
            expires_at:  None,
            status:      None,
        });
        if let Err(e) = res {
            warn!("failed to seed heartbeat {name}: {e}");
        }
    }
    Ok(())
}

// Seed the watchdog's interval-based schedule. Idempotent — calls
// `ensure_system_schedule` so re-runs don't duplicate the row.
// // When `cfg.enabled == false` the `"watchdog"` task handler isn't registered,
// so any existing seeded row would fire every tick and fail with "unknown
// internal task: watchdog" until it exhausts to `status=failed` — leaving a
// zombie schedule *and* tripping the `watchdog.last_log_offset_lag_secs` health
// detector Red. Instead, **pause** the seeded row on disable so it never fires;
// re-enabling reseeds it (and the operator resets it active). The detector
// reads the schedule status, so a paused/absent watchdog is reported green.
pub fn seed_watchdog_schedule(
    store: &AutomationsStore,
    cfg:   &crate::config::WatchdogConfig,
) -> Result<(), MiraError> {
    if !cfg.enabled {
        match store.pause_system_schedule_by_name("heartbeat.watchdog") {
            Ok(true)  => info!("watchdog disabled — paused stale heartbeat.watchdog schedule"),
            Ok(false) => {} // absent or already paused — nothing to do
            Err(e)    => warn!("watchdog disabled — failed to pause heartbeat.watchdog: {e}"),
        }
        return Ok(());
    }
    let action = Action::Internal {
        task: super::heartbeats::watchdog::WATCHDOG_TASK_NAME.to_string(),
        args: serde_json::Value::Null,
    };
    let trigger = TriggerSpec::Interval { every_secs: cfg.interval_secs };
    let res = store.ensure_system_schedule(NewSchedule {
        user_id:     "system".to_string(),
        owner_kind:  OwnerKind::System,
        name:        "heartbeat.watchdog".to_string(),
        description: Some(format!(
            "Tail logs every {}s, alert on {} or higher.",
            cfg.interval_secs, cfg.severity_threshold,
        )),
        rationale:   None,
        trigger,
        timezone:    "UTC".to_string(),
        quiet_hours: None,
        action,
        expires_at:  None,
        status:      None,
    });
    if let Err(e) = res {
        warn!("failed to seed heartbeat.watchdog: {e}");
    }
    Ok(())
}

// Seed the user-scoped event_subscription that turns
// `watchdog.alert` events into `ChannelMessage` deliveries. Owner
// is the configured `notify_user_id` so the dispatcher can resolve
// their channel address. Skipped when no recipient is configured —
// alerts still emit on the bus (visible in run history) but no
// channel routing happens. Idempotent via stable name.
pub fn seed_watchdog_subscription(
    store: &AutomationsStore,
    cfg:   &crate::config::WatchdogConfig,
) -> Result<(), MiraError> {
    if !cfg.enabled { return Ok(()); }
    let Some(user_id) = cfg.notify_user_id.as_ref().filter(|s| !s.is_empty()) else {
        debug!("watchdog: notify_user_id unset — alert events will fire but no auto-route is seeded");
        return Ok(());
    };
    let text_template = "{{payload.severity_emoji}} MIRA watchdog · {{payload.severity}} · `{{payload.module}}`\n\n\
                        {{payload.message}}\n\n\
                        {{payload.analyze_link}}\n\n\
                        _seen {{payload.recent_count}}× recently · {{payload.fingerprint}}_".to_string();
    let action = super::types::Action::ChannelMessage {
        channel:         cfg.channel.clone(),
        to:              None,
        conversation_id: None,
        text_template,
    };
    let new = super::types::NewEventSubscription {
        user_id:    user_id.to_string(),
        owner_kind: OwnerKind::System,
        name:       "watchdog.alert delivery".to_string(),
        description: Some(format!("Auto-route watchdog.alert events to {}", cfg.channel)),
        rationale:   Some("Seeded by automations.watchdog config".to_string()),
        event_name: crate::events::names::WATCHDOG_ALERT.to_string(),
        predicate:  None,
        action,
        expires_at: None,
        status:     Some(super::types::AutomationStatus::Active),
        // System-seeded routing rule — fires repeatedly and must
        // persist.
        delete_after_fire: false,
    };
    // ensure_*-flavoured upsert by stable name.
    if let Err(e) = store.ensure_system_event_subscription(new) {
        warn!("failed to seed watchdog.alert subscription: {e}");
    }
    Ok(())
}
