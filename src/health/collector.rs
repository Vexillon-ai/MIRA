// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/collector.rs
//! Run every detector in the slice-1 registry, persist the snapshot,
//! and file watchdog incidents for non-green reports.
//!
//! The collector is the only thing that reaches across the health
//! subsystem boundary into the watchdog incident table. Detectors stay
//! ignorant of how their reports get delivered — they just produce
//! [`super::DetectorReport`] values.
//!
//! Dedup model: the watchdog itself dedups in-memory with a TTL. We
//! can't reuse that map (different process at first, and we file
//! incidents by direct DB insert rather than via `bus.emit`). Instead,
//! we ask the AutomationsStore "did we file an incident with this
//! fingerprint in the last DEDUP_WINDOW_SECS?" — if yes, log-only.

use std::sync::Arc;

use serde_json::json;
use tracing::{debug, info, warn};

use crate::automations::AutomationsStore;
use crate::automations::store::NewWatchdogIncident;
use crate::events::{Event, EventBus};
use crate::MiraError;

use super::actions::AutoAction;
use super::store::{HealthStore, ThresholdRow, CustomDetectorRow};
use super::{ActionPolicy, DetectorContext, DetectorReport, HealthLevel, HealthSnapshot};

/// How long to suppress a duplicate incident for the same detector
/// after the previous one was filed. 12h matches the practical cadence
/// users want — a sticky problem (e.g. master.key wrong perms) reminds
/// you twice a day rather than every hour.
const DEDUP_WINDOW_SECS: i64 = 12 * 60 * 60;

/// Source string written into `watchdog_incidents.source`. Lets the
/// analyze flow / future UI filter system-health incidents from
/// log/db-derived ones.
pub const HEALTH_SOURCE: &str = "system_health";

pub struct CollectorOutcome {
    pub snapshot:       HealthSnapshot,
    pub incidents_filed:    usize,
    pub auto_actions_run:   usize,
    pub dedup_skipped:      usize,
}

/// Run all detectors, return their reports, persist the snapshot, and
/// (when configured) file watchdog incidents. The result is a small
/// summary the heartbeat surfaces in its run-history row.
pub fn run_audit(
    detectors:        &[Box<dyn super::Detector>],
    ctx:              &DetectorContext,
    health_store:     &Arc<HealthStore>,
    automations:      Option<&Arc<AutomationsStore>>,
    notify_user_id:   Option<&str>,
    bus:              Option<&EventBus>,
    policy_lookup:    impl Fn(&str) -> ActionPolicy,
) -> Result<CollectorOutcome, MiraError> {
    let started = std::time::Instant::now();
    let now     = chrono::Utc::now().timestamp();
    let mut reports = Vec::with_capacity(detectors.len() + 4);

    // 0.110.0 — pull threshold overrides + custom detectors once per
    // audit. Cheap (small tables). Failures degrade to defaults.
    let thresholds: std::collections::HashMap<String, ThresholdRow> =
        health_store.list_thresholds().unwrap_or_default()
            .into_iter().map(|r| (r.detector_name.clone(), r)).collect();
    let custom_rows: Vec<CustomDetectorRow> =
        health_store.list_custom_detectors().unwrap_or_default();

    for d in detectors {
        let policy = policy_lookup(d.name());
        if matches!(policy, ActionPolicy::Disabled) {
            // Still record a Green entry so the snapshot is complete —
            // operators can see at a glance which detectors are muted.
            reports.push(super::DetectorReport::green(
                d.name(), "detector disabled by config".to_string(),
            ));
            continue;
        }
        // Best-effort panic isolation. NOTE: the release profile sets
        // `panic = "abort"`, which DEFEATS catch_unwind — a panicking detector
        // aborts the whole process regardless. (A nested `block_on` in the
        // telegram-reachable detector did exactly this: froze the Health page
        // and restart-looped MIRA.) So detectors MUST NOT panic; this guard
        // only helps in debug/test builds. Treat any detector panic as a bug.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.run(ctx)));
        let mut report = match result {
            Ok(r) => r,
            Err(_) => super::DetectorReport {
                name:    d.name().into(),
                level:   HealthLevel::Yellow,
                message: format!("detector `{}` panicked — see logs", d.name()),
                value:   None,
                payload: serde_json::Value::Null,
                auto_action_eligible: false,
                analytics: None,
            },
        };
        // 0.110.0 — apply per-detector threshold override if present
        // AND the detector reported a numeric value. Re-levels in
        // place; preserves message/payload/eligibility from the
        // detector itself (the override only changes severity).
        if let Some(t) = thresholds.get(d.name()) {
            if let Some(v) = report.value {
                let new_level = level_from_thresholds(v, t);
                if new_level != report.level {
                    debug!(
                        "threshold override re-leveled `{}` from {} to {} (value={v})",
                        d.name(), report.level.as_str(), new_level.as_str(),
                    );
                    report.level = new_level;
                }
            }
        }
        reports.push(report);
    }

    // 0.110.0 — run admin-defined custom SQL detectors. Each row's
    // SQL must produce a single numeric value; thresholds mirror
    // built-in detectors. SELECT-only enforced at evaluation time.
    for row in &custom_rows {
        if !row.enabled { continue; }
        let policy = policy_lookup(&row.name);
        if matches!(policy, ActionPolicy::Disabled) {
            reports.push(super::DetectorReport::green(
                row.name.clone(), "custom detector disabled by config".to_string(),
            ));
            continue;
        }
        let report = run_custom_detector(row, &ctx.data_dir);
        reports.push(report);
    }

    // 0.110.0 — slice 5c. Enrich each report with forecast / anomaly /
    // correlation data computed from snapshot history. Read history
    // once, share it across detectors. Correlation table is built once
    // per audit too (cross-detector overlap analysis).
    let history = health_store.list_recent(7 * 24).unwrap_or_default();
    let correlations = match automations {
        Some(a) => super::analytics::compute_correlations(a),
        None    => std::collections::HashMap::new(),
    };
    for r in reports.iter_mut() {
        let red_at = super::analytics::resolve_red_threshold(&r.name, &thresholds);
        let corr = correlations.get(&r.name).cloned().unwrap_or_default();
        super::analytics::enrich(r, &history, red_at, corr);
    }

    let snapshot = HealthSnapshot {
        taken_at:    now,
        duration_ms: started.elapsed().as_millis() as u64,
        reports,
    };

    // ── File incidents for non-green reports ─────────────────────────
    let mut incidents_filed   = 0usize;
    let mut auto_actions_run  = 0usize;
    let mut dedup_skipped     = 0usize;
    let mut first_incident_id: Option<String> = None;

    // 0.110.0 — outbound webhooks fire on every report whose level
    // matches the hook's filter (default = yellow+red). Fan-out runs
    // here so the snapshot delivery covers all reports including
    // dedup-suppressed ones.
    for r in &snapshot.reports {
        super::webhooks::fan_out(Arc::clone(health_store), r, now);
    }

    for r in &snapshot.reports {
        if matches!(r.level, HealthLevel::Green) { continue; }
        let fp = format!("health:{}", r.name);

        // Cross-restart dedup. Yellow uses the full window; Red uses
        // half so persistent bad states still nudge twice a day.
        let dedup_window = match r.level {
            HealthLevel::Red => DEDUP_WINDOW_SECS / 2,
            _                => DEDUP_WINDOW_SECS,
        };
        if let Some(store) = automations {
            match HealthStore::was_recently_filed(store, &fp, dedup_window, now) {
                Ok(true)  => { dedup_skipped += 1; continue; }
                Ok(false) => {}
                Err(e)    => warn!("system_audit: dedup check failed for {fp}: {e}"),
            }
        }

        // Optionally run the auto-action FIRST so the incident message
        // can include "auto-action ran: ok / failed". Only when policy
        // is AutoCleanup AND the detector flagged itself eligible.
        let policy = policy_lookup(&r.name);
        let mut action_summary: Option<String> = None;
        if matches!(policy, ActionPolicy::AutoCleanup) && r.auto_action_eligible {
            match super::actions::run_action_for(&r.name, ctx, automations) {
                Ok(AutoAction::Ran { summary }) => {
                    action_summary = Some(format!("auto-cleanup ran: {summary}"));
                    auto_actions_run += 1;
                }
                Ok(AutoAction::NotImplemented) => {
                    debug!("system_audit: no auto-action for `{}`", r.name);
                }
                Err(e) => {
                    action_summary = Some(format!("auto-cleanup failed: {e}"));
                    warn!("system_audit: auto-action for {} failed: {e}", r.name);
                }
            }
        }

        // Build incident payload. Echo the detector's structured payload
        // verbatim so the analyze flow has the original evidence.
        let payload = json!({
            "first_seen_at": now,
            "recent_count":  1u32,
            "level":         r.level.as_str(),
            "value":         r.value,
            "detector_payload": r.payload,
            "auto_action":   action_summary,
        });

        let message = match action_summary {
            Some(ref s) => format!("{}\n\n{s}", r.message),
            None        => r.message.clone(),
        };

        if let (Some(store), Some(uid)) = (automations, notify_user_id) {
            let new = NewWatchdogIncident {
                user_id:      uid.to_string(),
                fingerprint:  fp.clone(),
                severity:     r.level.as_watchdog_severity().into(),
                source:       HEALTH_SOURCE.into(),
                module:       format!("health/{}", r.name),
                message:      message.clone(),
                payload_json: payload.to_string(),
            };
            match store.create_watchdog_incident(new) {
                Ok(id) => {
                    incidents_filed += 1;
                    if first_incident_id.is_none() {
                        first_incident_id = Some(id.clone());
                    }
                    // Emit the same `watchdog.alert` event the existing
                    // routing subscription listens for — this is what
                    // delivers the message to the admin's Signal/web/etc.
                    if let Some(bus) = bus {
                        let analyze_link = format!("[🔍 Analyze with LLM](/incidents/{id})");
                        let severity_emoji = match r.level {
                            HealthLevel::Red    => "🚨",
                            HealthLevel::Yellow => "⚠️",
                            HealthLevel::Green  => "ℹ️",
                        };
                        let event_payload = json!({
                            "severity":       r.level.as_watchdog_severity(),
                            "severity_emoji": severity_emoji,
                            "source":         HEALTH_SOURCE,
                            "module":         format!("health/{}", r.name),
                            "message":        message,
                            "fingerprint":    fp,
                            "first_seen_at":  now,
                            "recent_count":   1u32,
                            "incident_id":    id,
                            "analyze_link":   analyze_link,
                        });
                        bus.emit(Event::new(
                            crate::events::names::WATCHDOG_ALERT, None, event_payload,
                        ));
                    }
                }
                Err(e) => warn!("system_audit: persist incident for {fp} failed: {e}"),
            }
        } else {
            debug!(
                "system_audit: would file incident for {fp} but no automations store / notify_user_id — \
                 logging only: {message}",
            );
        }
    }

    // Persist snapshot + prune retention.
    if let Err(e) = health_store.record(&snapshot, first_incident_id.as_deref()) {
        warn!("system_audit: snapshot persist failed: {e}");
    }
    if let Err(e) = health_store.prune_old(now) {
        debug!("system_audit: prune_old failed (non-fatal): {e}");
    }

    info!(
        "system_audit: {} detector(s), worst={}, triggered={}, filed={}, auto_actions={}, dedup_skipped={}",
        snapshot.reports.len(), snapshot.worst_level().as_str(),
        snapshot.triggered_count(), incidents_filed, auto_actions_run, dedup_skipped,
    );

    Ok(CollectorOutcome {
        snapshot,
        incidents_filed,
        auto_actions_run,
        dedup_skipped,
    })
}

/// 0.110.0 — apply override thresholds against a detector's raw value.
/// `direction="above"` means bigger = worse (the common case);
/// `"below"` flips it (e.g. for free-disk-MB).
pub(crate) fn level_from_thresholds(value: f64, t: &ThresholdRow) -> HealthLevel {
    let above = t.direction.as_str() != "below";
    let trips_red = match (above, t.red_at) {
        (true,  Some(r)) => value >= r,
        (false, Some(r)) => value <= r,
        _ => false,
    };
    if trips_red { return HealthLevel::Red; }
    let trips_yellow = match (above, t.yellow_at) {
        (true,  Some(y)) => value >= y,
        (false, Some(y)) => value <= y,
        _ => false,
    };
    if trips_yellow { HealthLevel::Yellow } else { HealthLevel::Green }
}

/// Run one admin-defined SQL detector. Opens `target_db` read-only,
/// runs `sql`, expects `(value REAL)` from a single row. Errors
/// degrade to Yellow so a typo in the SQL doesn't poison the snapshot.
pub(crate) fn run_custom_detector(
    row:       &CustomDetectorRow,
    data_dir:  &std::path::Path,
) -> DetectorReport {
    use rusqlite::{Connection, OpenFlags};
    let trimmed = row.sql.trim_start().to_ascii_lowercase();
    if !trimmed.starts_with("select") && !trimmed.starts_with("with") {
        return DetectorReport {
            name: row.name.clone(), level: HealthLevel::Yellow,
            message: "rejected: SQL must start with SELECT or WITH".into(),
            value: None,
            payload: json!({"sql": row.sql, "rejected_reason": "non-SELECT statement"}),
            auto_action_eligible: false,
            analytics: None,
        };
    }
    let db_path = data_dir.join(format!("{}.db", row.target_db));
    let conn = match Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c)  => c,
        Err(e) => return DetectorReport {
            name: row.name.clone(), level: HealthLevel::Yellow,
            message: format!("custom detector unavailable: open `{}` failed: {e}", row.target_db),
            value: None,
            payload: json!({"target_db": row.target_db}),
            auto_action_eligible: false,
            analytics: None,
        },
    };
    // 5s timeout via SQLite's busy_timeout. Doesn't interrupt a
    // genuinely-runaway query but caps lock contention.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let value: f64 = match conn.query_row(&row.sql, [], |r| r.get::<_, f64>(0)) {
        Ok(v)  => v,
        Err(e) => return DetectorReport {
            name: row.name.clone(), level: HealthLevel::Yellow,
            message: format!("custom detector query failed: {e}"),
            value: None,
            payload: json!({"sql": row.sql, "error": e.to_string()}),
            auto_action_eligible: false,
            analytics: None,
        },
    };
    let t = ThresholdRow {
        detector_name: row.name.clone(),
        yellow_at: row.yellow_at,
        red_at:    row.red_at,
        direction: row.direction.clone(),
        updated_at: row.updated_at, updated_by: row.updated_by.clone(),
    };
    let level = level_from_thresholds(value, &t);
    DetectorReport {
        name: row.name.clone(), level,
        message: format!("{} = {value}", row.name),
        value: Some(value),
        payload: json!({
            "value": value, "thresholds": {"yellow": row.yellow_at, "red": row.red_at, "direction": row.direction},
            "description": row.description, "target_db": row.target_db,
        }),
        auto_action_eligible: false,
        analytics: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::detectors::*;

    fn ctx_for(dir: &std::path::Path) -> DetectorContext {
        DetectorContext {
            data_dir: dir.to_path_buf(),
            automations: None,
            audit_store: None,
            embedding_provider_kind: "lmstudio".into(),
            mira_version: semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
            agent_registry: None,
            auth_db: None,
            log_path: None,
            channel_manager: None,
            secrets_store: None,
            channel_accounts: None,
            degradations: None,        }
    }

    #[test]
    fn audit_runs_all_detectors_and_records_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = HealthStore::open_in_memory();
        let ctx = ctx_for(dir.path());
        let detectors: Vec<Box<dyn super::super::Detector>> = vec![
            Box::new(MasterKeyPresentDetector),
            Box::new(EmbeddingProviderReachableDetector),
        ];
        let store_arc = Arc::new(store);
        let outcome = run_audit(
            &detectors, &ctx, &store_arc, None, None, None,
            |_| ActionPolicy::NotifyOnly,
        ).unwrap();
        assert_eq!(outcome.snapshot.reports.len(), 2);
        // master.key missing → Red; embedding skipped → Green.
        assert_eq!(outcome.snapshot.worst_level(), HealthLevel::Red);
        // No automations store wired → no incidents filed.
        assert_eq!(outcome.incidents_filed, 0);
        // Snapshot persisted.
        assert_eq!(store_arc.list_recent(5).unwrap().len(), 1);
    }

    #[test]
    fn disabled_policy_records_green_skip() {
        let dir = tempfile::tempdir().unwrap();
        let store = HealthStore::open_in_memory();
        let ctx = ctx_for(dir.path());
        let detectors: Vec<Box<dyn super::super::Detector>> = vec![
            Box::new(MasterKeyPresentDetector),
        ];
        let store_arc = Arc::new(store);
        let outcome = run_audit(
            &detectors, &ctx, &store_arc, None, None, None,
            |_| ActionPolicy::Disabled,
        ).unwrap();
        // Detector that would have been Red (master.key missing) is
        // now reported as Green because its policy = Disabled.
        assert_eq!(outcome.snapshot.worst_level(), HealthLevel::Green);
    }
}
