// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/mod.rs
//! Self-monitoring subsystem.
//!
//! The hourly `system_audit` heartbeat (`automations::heartbeats::system_audit`)
//! runs every detector in [`detectors`], turns the results into a
//! [`HealthSnapshot`], persists the snapshot, and files watchdog incidents
//! for any signal at Yellow or Red severity.
//!
//! Architectural choice — no LLM in the hot path. Detectors are
//! deterministic SQL / FS / process probes with hard thresholds. The
//! existing watchdog → analyze pipeline (slice W3) handles the
//! human-readable diagnosis when the user clicks Analyze on an incident.
//! Hour-by-hour ticks should be near-free; only genuinely anomalous
//! states pay the LLM cost.

pub mod actions;
pub mod analytics;
pub mod boot;
pub mod collector;
pub mod db_paths;
pub mod degradation;
pub mod detectors;
pub mod process;
pub mod store;
pub mod trend_context;
pub mod webhooks;

use serde::{Deserialize, Serialize};

/// Severity for a detector reading.
///
/// - **Green** — within normal thresholds; nothing to do.
/// - **Yellow** — degraded, worth flagging but not an emergency.
/// - **Red** — broken, needs attention now.
///
/// The collector files a watchdog incident for any non-green report
/// (severity propagated to the incident row); auto-cleanup actions run
/// only on detectors whose policy is set to `auto_cleanup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthLevel {
    Green,
    Yellow,
    Red,
}

impl HealthLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Green  => "green",
            Self::Yellow => "yellow",
            Self::Red    => "red",
        }
    }

    /// Watchdog incident severity string. Yellow → WARN, Red → ERROR;
    /// Green never files an incident so this case is unreachable
    /// during normal operation but defaults to INFO for safety.
    pub fn as_watchdog_severity(self) -> &'static str {
        match self {
            Self::Red    => "ERROR",
            Self::Yellow => "WARN",
            Self::Green  => "INFO",
        }
    }
}

/// What to do when a detector reports non-green. Disabled is the
/// per-signal mute switch — the detector still runs (so the snapshot
/// is complete) but no incident is filed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionPolicy {
    Disabled,
    NotifyOnly,
    AutoCleanup,
}

impl Default for ActionPolicy {
    fn default() -> Self { Self::NotifyOnly }
}

/// One detector's output for a single audit run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectorReport {
    pub name:    String,
    pub level:   HealthLevel,
    pub message: String,
    /// Numeric reading the detector compared to its threshold. None for
    /// boolean detectors (e.g. master-key-missing).
    pub value:   Option<f64>,
    /// Detector-specific extra context — e.g. list of stuck subscription
    /// IDs, free disk MB, broken skill paths. Echoed verbatim into the
    /// watchdog incident's payload_json so the analyze flow has the
    /// raw evidence without re-querying.
    pub payload: serde_json::Value,
    /// True when the detector itself decided this trip is severe enough
    /// to warrant the auto-action (if the user's policy permits it).
    /// Yellow detectors generally leave this false; Red usually true.
    pub auto_action_eligible: bool,
    /// 0.110.0 — analytics enrichment, attached by the collector after
    /// the detector returns. None when there's no history to compute
    /// against (fresh install, < N snapshots) or the detector has no
    /// numeric value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analytics: Option<DetectorAnalytics>,
}

/// 0.110.0 — slice-5c enrichment computed from snapshot history.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DetectorAnalytics {
    /// Forecast — `Some((projected_value, hours))` when the linear
    /// trend over the last 24h projects the value to cross the red
    /// threshold within `hours`. None when no forecast can be made
    /// (flat trend, no threshold known, etc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast_red_in_hours: Option<f64>,
    /// Z-score of the current value vs the last 7d of values for this
    /// detector. None when stddev is zero or sample size < 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anomaly_z: Option<f64>,
    /// Detectors that tripped within ±10 min of this one ≥3 times
    /// over the last 7 days. Helps the LLM analyst spot causal chains.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub correlated_detectors: Vec<String>,
}

impl DetectorReport {
    pub fn green(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: HealthLevel::Green,
            message: message.into(),
            value: None,
            payload: serde_json::Value::Null,
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

/// One full audit run — what happened, when, how long it took.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub taken_at:    i64,
    pub duration_ms: u64,
    pub reports:     Vec<DetectorReport>,
}

impl HealthSnapshot {
    /// Worst severity across all reports — Green if everything's fine.
    pub fn worst_level(&self) -> HealthLevel {
        self.reports.iter().map(|r| r.level)
            .fold(HealthLevel::Green, |a, b| match (a, b) {
                (HealthLevel::Red,    _) | (_, HealthLevel::Red)    => HealthLevel::Red,
                (HealthLevel::Yellow, _) | (_, HealthLevel::Yellow) => HealthLevel::Yellow,
                _ => HealthLevel::Green,
            })
    }

    pub fn triggered_count(&self) -> usize {
        self.reports.iter().filter(|r| !matches!(r.level, HealthLevel::Green)).count()
    }
}

/// Side-channel deps the detectors pull from. Add a field only when
/// something genuinely needs it — every new field touches the heartbeat
/// constructor.
pub struct DetectorContext {
    pub data_dir:                std::path::PathBuf,
    pub automations:             Option<std::sync::Arc<crate::automations::AutomationsStore>>,
    pub audit_store:             Option<std::sync::Arc<crate::agent::AuditStore>>,
    /// Provider kind from `config.embedding.provider` — the embedding
    /// detector skips when this is anything other than `"internal"`
    /// (other providers don't need ONNX runtime locally).
    pub embedding_provider_kind: String,
    pub mira_version:            semver::Version,
    // ── 0.106.0 (slice 2) additions ──────────────────────────────────
    /// Live agent registry — used by the workers_running / stuck /
    /// over-budget detectors. None in tests / minimal builds.
    pub agent_registry:          Option<std::sync::Arc<crate::agent::AgentRegistry>>,
    /// AuthDb — used by the failed_login + jwt + ban-state detectors.
    pub auth_db:                 Option<std::sync::Arc<crate::auth::AuthDb>>,
    /// Resolved path to the live log file. Used by log-scan detectors
    /// (max_rounds_hit, duplicate_tool_call, jwt_validation_failures).
    /// None in tests; detectors degrade to Yellow when missing.
    pub log_path:                Option<std::path::PathBuf>,

    // ── 0.108.0 (slice 3b) additions ─────────────────────────────────
    /// Live channel manager — used by `channel.signal.daemon_alive` and
    /// the restart auto-action. Wrapped in tokio RwLock to match the
    /// rest of the codebase; the detector takes a brief write lock for
    /// the `is_running` probe. None in tests.
    pub channel_manager: Option<std::sync::Arc<tokio::sync::RwLock<crate::gateway::channel_manager::ChannelManager>>>,
    /// Encrypted skill secrets vault — used by
    /// `skills.dangling_secrets_count` to enumerate skill_ids and by
    /// the sweep auto-action to delete orphaned rows. None when the
    /// open-failure path was hit at boot.
    pub secrets_store: Option<std::sync::Arc<crate::skills::SecretsStore>>,

    // ── 0.110.0 (slice 5d) additions ─────────────────────────────────
    /// Channel-account store. Used by the Telegram reachability probe
    /// to enumerate enabled accounts and pull their bot tokens.
    pub channel_accounts: Option<std::sync::Arc<crate::channel_accounts::ChannelAccountStore>>,

    /// Live subsystem-fallback tracker — read by `subsystem.degraded` to
    /// surface silent fallbacks (TTS/STT/embeddings/reasoning). None in tests.
    pub degradations: Option<std::sync::Arc<crate::health::degradation::DegradationTracker>>,
}

/// Detector trait. Sync because every slice-1 detector is a fast SQLite
/// read or filesystem stat — running them on the heartbeat thread is
/// fine. If a future detector needs HTTP probes, switch to async-trait.
pub trait Detector: Send + Sync {
    /// Stable identifier — also used as the dedup key when filing
    /// watchdog incidents. Format: `<domain>.<thing>` e.g.
    /// `automations.subscriptions_stranded_completion`.
    fn name(&self) -> &'static str;

    /// Whether this detector can run on the current platform. Detectors
    /// whose prerequisites don't exist here (e.g. the `/proc`-based
    /// process detectors on a non-Linux host) return `false`; the audit
    /// then records them as Green "not applicable on this platform"
    /// instead of running them and reporting a scary "unavailable"
    /// Yellow. Defaults to `true` — only platform-bound detectors
    /// override it.
    fn is_applicable(&self) -> bool { true }

    /// Run one probe. Detector errors are reported as Yellow (the
    /// detector itself is broken, not necessarily MIRA) so an
    /// individual probe failure never crashes the whole audit.
    fn run(&self, ctx: &DetectorContext) -> DetectorReport;
}
