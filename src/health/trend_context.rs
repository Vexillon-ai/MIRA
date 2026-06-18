// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/trend_context.rs
//! 0.109.0 — assemble a markdown-formatted "trend context" block for
//! the LLM analyst.
//!
//! When an admin clicks Analyze on a `system_health` watchdog incident,
//! this module enriches the bare incident with the detector's trajectory
//! over the last 24h plus the fingerprint's incident history. Lets the
//! LLM answer "is this getting worse?" and "did this start when X
//! happened?" without round-tripping back to grep the DB.

use std::sync::Arc;

use crate::automations::AutomationsStore;
use crate::health::store::HealthStore;
use crate::health::HealthSnapshot;

// Build a markdown block describing the recent history of one
// detector. Returns None when there's no useful history yet (fresh
// install, < 2h since first audit).
pub fn build_for_incident(
    health_store: &HealthStore,
    automations:  &AutomationsStore,
    detector_name: &str,
    fingerprint:   &str,
) -> Option<String> {
    // 1. Last 24h of snapshot summaries — what was this detector doing?
    let summaries = health_store.list_summaries_since(
        chrono::Utc::now().timestamp() - 24 * 3600,
    ).ok()?;
    if summaries.len() < 2 { return None; }

    // 2. Last 5 full snapshots — for sampling the detector's recent values.
    let recent_full = health_store.list_recent(5).ok()?;
    let detector_history: Vec<(i64, String, String, Option<f64>)> = recent_full
        .iter()
        .filter_map(|s: &HealthSnapshot| {
            s.reports.iter()
                .find(|r| r.name == detector_name)
                .map(|r| (s.taken_at, r.level.as_str().to_string(),
                          r.message.clone(), r.value))
        })
        .collect();

    // 3. Same-fingerprint incident count in last 7 days.
    let week_ago = chrono::Utc::now().timestamp() - 7 * 24 * 3600;
    let fp_count = automations
        .count_incidents_by_fingerprint_since(fingerprint, week_ago)
        .unwrap_or(0);

    // 4. Roll up the snapshot summaries into hourly buckets so the
    //  LLM sees a digestible trend, not 24 raw rows.
    let trend_bucket = bucket_summaries_hourly(&summaries);

    // ── Render markdown ─────────────────────────────────────────────
    let mut out = String::new();
    out.push_str("\n\n### Trend context (last 24h)\n\n");

    out.push_str(&format!(
        "**Same-fingerprint incidents in last 7 days:** {}\n\n",
        fp_count,
    ));

    if !detector_history.is_empty() {
        out.push_str(&format!(
            "**Recent readings of `{}`** (newest first):\n\n",
            detector_name,
        ));
        for (ts, level, msg, val) in &detector_history {
            let when = chrono::DateTime::<chrono::Utc>::from_timestamp(*ts, 0)
                .map(|d| d.format("%H:%M UTC").to_string())
                .unwrap_or_else(|| ts.to_string());
            let val_s = val.map(|v| format!("(value={v:.2}) ")).unwrap_or_default();
            out.push_str(&format!("- `{level:6}` {when} — {val_s}{msg}\n"));
        }
        out.push('\n');
    }

    if !trend_bucket.is_empty() {
        out.push_str("**Hourly snapshot rollup** (worst level + triggered-detector count per hour):\n\n");
        out.push_str("| Hour (UTC) | Worst | Triggered |\n");
        out.push_str("|---|---|---|\n");
        for row in &trend_bucket {
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                row.hour_label, row.worst_level, row.max_triggered,
            ));
        }
        out.push('\n');
    }

    // 0.110.0 — predictive analytics from the latest full snapshot,
    // when present.  attaches these on every audit.
    if let Some(latest) = health_store.latest().ok().flatten() {
        if let Some(rep) = latest.reports.iter().find(|r| r.name == detector_name) {
            if let Some(an) = rep.analytics.as_ref() {
                out.push_str(&super::analytics::render_for_prompt(an));
            }
        }
    }

    out.push_str(
        "**When answering, use this trend context to address:**\n\
         - Is this signal getting worse, steady, or recovering?\n\
         - When did the trip first appear in the last 24h?\n\
         - Did anything else trip near the same time? (look at the rollup)\n\
         - If there's an anomaly z-score or forecast, factor that in.\n\n",
    );

    Some(out)
}

#[derive(Debug, Clone)]
struct HourBucket {
    hour_label:    String,
    worst_level:   String,
    max_triggered: u64,
}

// Group summaries by hour-of-day. Within each hour, take the worst
// level seen and the max triggered count. Output is oldest-first so
// the table reads naturally.
fn bucket_summaries_hourly(summaries: &[crate::health::store::SnapshotSummary]) -> Vec<HourBucket> {
    use std::collections::BTreeMap;
    let mut by_hour: BTreeMap<i64, (String, u64)> = BTreeMap::new();
    for s in summaries {
        let hour_key = s.taken_at - (s.taken_at % 3600);
        let entry = by_hour.entry(hour_key).or_insert(("green".into(), 0));
        if level_rank(&s.worst_level) > level_rank(&entry.0) {
            entry.0 = s.worst_level.clone();
        }
        if s.triggered_signal_count > entry.1 {
            entry.1 = s.triggered_signal_count;
        }
    }
    by_hour.into_iter().map(|(ts, (lvl, n))| {
        let label = chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
            .map(|d| d.format("%m-%d %H:00").to_string())
            .unwrap_or_else(|| ts.to_string());
        HourBucket { hour_label: label, worst_level: lvl, max_triggered: n }
    }).collect()
}

fn level_rank(s: &str) -> u8 {
    match s { "red" => 2, "yellow" => 1, _ => 0 }
}

// Convenience wrapper used by the watchdog handler: returns the
// extra prompt text to append, or "" if no health-trend context
// applies (incident wasn't health-derived, or store unavailable).
pub fn enrich_prompt(
    health_store: Option<&Arc<HealthStore>>,
    automations:  &AutomationsStore,
    incident:     &crate::automations::WatchdogIncident,
) -> String {
    if incident.source != crate::health::collector::HEALTH_SOURCE {
        return String::new();
    }
    let Some(hs) = health_store else { return String::new(); };
    // The detector name lives in `module` as `health/<detector_name>`.
    let detector_name = incident.module.strip_prefix("health/").unwrap_or(&incident.module);
    build_for_incident(hs, automations, detector_name, &incident.fingerprint)
        .unwrap_or_default()
}

// 0.112.2 — render a "what you can actually do about this" footer
// for ANY watchdog incident (not just system_health). The earlier
// LLM analyses were missing easy answers like "delete the webhook
// via the dashboard" and instead suggested grep'ing files at made-up
// paths. This grounds the LLM in the actual surfaces the operator
// has at their disposal.
// // Always-shown footer + per-module specific hints from a static map.
// Returns "" when there's nothing meaningful to add.
pub fn render_remediation_hints(
    incident: &crate::automations::WatchdogIncident,
) -> String {
    let mut s = String::new();
    s.push_str("\n\n---\n**Available remediation paths (admin operator):**\n\n");
    s.push_str("Dashboard surfaces under `/health` (admin-only):\n");
    s.push_str("- **Status** — all detectors + analytics badges + `Run audit now` button\n");
    s.push_str("- **Incidents** — system-health incident history\n");
    s.push_str("- **Config** — per-detector policy (Off / Notify / Auto-cleanup), threshold overrides, time-bounded snooze (1h / 4h / 1d / 1w)\n");
    s.push_str("- **Custom SQL** — add/edit/delete admin-defined SQL detectors\n");
    s.push_str("- **Webhooks** — outbound notification destinations (delete a broken one here)\n");
    s.push_str("- **Artifacts** — subagent task outputs in `~/mira-artifacts/`\n");
    s.push_str("- **IP bans** — lift any active temp-ban\n\n");
    s.push_str("HTTP endpoints (when CLI/curl is faster than UI):\n");
    s.push_str("- `POST /api/health/run-now` — force the next audit\n");
    s.push_str("- `PUT /api/health/config` — set detector policy / snooze\n");
    s.push_str("- `DELETE /api/health/webhooks/{id}` — remove a broken webhook\n");
    s.push_str("- `POST /api/health/ip-bans/{ip}/lift` — unban an IP\n");
    s.push_str("- `DELETE /api/health/custom-detectors/{name}` — remove a misbehaving custom detector\n\n");

    // Per-module specifics. Match the broadest first then the more
    // specific overrides; keep entries terse.
    if let Some(hint) = module_specific_hint(&incident.module) {
        s.push_str(&format!("**For this signal specifically**: {hint}\n\n"));
    }
    s.push_str(
        "When recommending an action, prefer the dashboard path over \
        manual file edits — it's logged, auditable, and reversible. \
        Only suggest config-file edits when the dashboard genuinely \
        can't reach the setting.\n",
    );
    s
}

// Map a watchdog incident's `module` field to a specific remediation
// pointer. Modules either come from log lines (`mira::path::to::module`)
// or from system_health detectors (`health/<detector_name>`). Returns
// None when no specific advice applies — the always-shown footer
// covers the general case.
fn module_specific_hint(module: &str) -> Option<&'static str> {
    // System-health detector hints (slice 1+2+3b+4 + slice-5b actions).
    if let Some(detector) = module.strip_prefix("health/") {
        return Some(match detector {
            "auth.master_key_perms" =>
                "chmod 0600 auto-action wired — set policy to **Auto-cleanup** in the Config tab and the next audit will fix it.",
            "auth.master_key_present" =>
                "Restore from backup. There's no auto-action because this is unrecoverable from the running system; the master key is the only thing that decrypts the secrets vault.",
            "auth.failed_logins_per_ip_1h" =>
                "30-min temp-ban auto-action wired — set policy to **Auto-cleanup**. To lift an existing ban, use the IP bans tab.",
            "db.wal_size_mb" =>
                "`wal_checkpoint(TRUNCATE)` auto-action wired — set policy to **Auto-cleanup**.",
            "db.integrity_check" =>
                "Corruption is serious — back up the affected DB first, then run `sqlite3 <db> 'PRAGMA integrity_check; PRAGMA quick_check;'` for full diagnostics. No auto-action because corruption recovery needs human judgment.",
            "automations.subscriptions_stranded_completion" =>
                "Sweep auto-action wired — marks them failed so they stop occupying the active set. Set policy to **Auto-cleanup**.",
            "automations.scheduler_tick_lag_secs" =>
                "Scheduler thread is stuck. Restart MIRA to recover (`systemctl --user restart mira`). The boot-time orphan sweep will clean up afterwards.",
            "skills.bundled_drift" =>
                "`extract_or_refresh` auto-action wired — set policy to **Auto-cleanup** and the next audit will pull the newer bundled version onto disk.",
            "skills.dangling_secrets_count" =>
                "Purge auto-action wired — deletes secret rows for uninstalled skills. Set policy to **Auto-cleanup**.",
            "skills.broken_manifest_count" =>
                "Inspect each broken manifest's path (in the detector's payload) and either fix the `skill.toml` or remove the directory. No auto-action because we can't safely guess the user's intent.",
            "consistency.automations_for_deleted_users" =>
                "Sweep auto-action wired — deletes orphan rows. Set policy to **Auto-cleanup**.",
            "channel.signal.daemon_alive" =>
                "Restart auto-action wired — calls `ChannelManager::restart_account` on each dead Signal account. Set policy to **Auto-cleanup**. Or restart manually via the Channels page.",
            "channel.signal.no_received_24h" =>
                "Verify signal-cli is registered and reachable: `signal-cli -u <phone> receive --timeout 5`. Could also indicate a paired-device dropoff.",
            "watchdog.analysis_stuck_in_progress_30m" =>
                "Reset auto-action wired — flips stuck rows to `failed` so the Analyze button works again. Set policy to **Auto-cleanup**.",
            "watchdog.same_fingerprint_count_24h" =>
                "If the underlying error is expected noise, add a regex to `automations.watchdog.ignore_patterns` in mira_config.json. Otherwise treat the underlying signal as the real bug.",
            "process.restart_count_24h" =>
                "Check `systemctl --user status mira` for the restart cause. Frequent restarts often mean a panic on startup — see the boot-time logs for the panic stack.",
            "process.rss_mb" | "process.fd_count" | "process.thread_count" =>
                "Resource leak suspected. Spawn the agents/audit pages to see what's running long. No auto-action — operator should restart MIRA to clear if needed.",
            "llm.embedding.provider_unreachable" =>
                "When provider=internal, check ONNX runtime via `POST /api/admin/deps/onnxruntime/install`. For lmstudio/openai/etc., check the provider URL + API key in Settings → Providers.",
            "llm.error_rate_1h" | "llm.no_successful_call_2h" =>
                "Check the upstream provider directly (LM Studio / Ollama UI, OpenRouter status page, etc.). Provider-side outages don't have a MIRA-side remediation beyond switching providers.",
            "llm.cost_burn_24h_usd" | "llm.current_cost_burn_usd" =>
                "Inspect the agents page for what's running. To kill a runaway worker: `POST /api/agents/{id}/interrupt`. Lower per-task budgets in `automations.watchdog` config to cap future spend.",
            _ => return None,
        });
    }
    // Log-derived hints — keyed on the originating Rust module path.
    if module.starts_with("mira::health::webhooks") {
        return Some("Delete or edit the misbehaving webhook via the **Webhooks tab** in `/health`. The detector's `payload` includes the failing URL; if it's a stale test entry, just Delete the row.");
    }
    if module.starts_with("mira::providers::signal_cli") {
        return Some("Check the **Status tab** for `channel.signal.daemon_alive`. The signal-cli daemon may be dead — auto-restart is wired if you set that detector to **Auto-cleanup**.");
    }
    if module.starts_with("mira::providers::") {
        return Some("Check `llm.error_rate_1h` and `llm.no_successful_call_2h` in the **Status tab**. If the provider is genuinely down, switch to a fallback in Settings → Providers.");
    }
    if module.starts_with("mira::auth::") {
        return Some("Check `auth.failed_logins_per_ip_1h` in the **Status tab** and the **IP bans tab** for any active bans you may want to lift.");
    }
    if module.starts_with("mira::automations::") {
        return Some("Check the relevant `automations.*` detectors in the **Status tab**. The Automations page (sidebar) lets you pause/edit individual schedules.");
    }
    if module.starts_with("mira::memory::") {
        return Some("Check `memory.rollup_lag_hours` in the **Status tab**. The Memory page (sidebar) shows current vector index state.");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{DetectorReport, HealthLevel};

    #[test]
    fn level_rank_orders_correctly() {
        assert!(level_rank("red") > level_rank("yellow"));
        assert!(level_rank("yellow") > level_rank("green"));
    }

    #[test]
    fn bucket_collapses_multiple_summaries_to_worst_per_hour() {
        use crate::health::store::SnapshotSummary;
        let summaries = vec![
            SnapshotSummary { taken_at: 1000,    duration_ms: 1, triggered_signal_count: 1, worst_level: "yellow".into(), incident_id: None },
            SnapshotSummary { taken_at: 1001,    duration_ms: 1, triggered_signal_count: 3, worst_level: "red".into(),    incident_id: None },
            SnapshotSummary { taken_at: 5000,    duration_ms: 1, triggered_signal_count: 0, worst_level: "green".into(),  incident_id: None },
        ];
        let buckets = bucket_summaries_hourly(&summaries);
        assert_eq!(buckets.len(), 2);
        // Hour 0 should report the worst (red) and the max triggered (3).
        assert_eq!(buckets[0].worst_level, "red");
        assert_eq!(buckets[0].max_triggered, 3);
        assert_eq!(buckets[1].worst_level, "green");
    }

    #[test]
    fn remediation_hints_always_include_dashboard_paths() {
        let inc = crate::automations::WatchdogIncident {
            id: "x".into(), user_id: "u".into(), fingerprint: "f".into(),
            severity: "WARN".into(), source: "log:mira.log".into(),
            module: "mira::health::webhooks".into(), message: "x".into(),
            payload_json: "{}".into(), created_at: 0,
            analysis_status: "none".into(),
            analysis_started_at: None, analysis_completed_at: None,
            conversation_id: None, analysis_response: None,
        };
        let s = render_remediation_hints(&inc);
        assert!(s.contains("Webhooks tab"), "missing webhooks-specific hint");
        assert!(s.contains("/api/health/run-now"), "missing API endpoint hint");
        assert!(s.contains("Dashboard surfaces"), "missing always-shown header");
    }

    #[test]
    fn remediation_hints_pick_per_detector_advice() {
        let inc = crate::automations::WatchdogIncident {
            id: "x".into(), user_id: "u".into(), fingerprint: "f".into(),
            severity: "WARN".into(), source: "system_health".into(),
            module: "health/db.wal_size_mb".into(), message: "x".into(),
            payload_json: "{}".into(), created_at: 0,
            analysis_status: "none".into(),
            analysis_started_at: None, analysis_completed_at: None,
            conversation_id: None, analysis_response: None,
        };
        let s = render_remediation_hints(&inc);
        assert!(s.contains("wal_checkpoint"), "missing wal-specific hint");
        assert!(s.contains("Auto-cleanup"), "missing policy advice");
    }

    #[test]
    fn remediation_hints_unknown_module_still_returns_generic() {
        let inc = crate::automations::WatchdogIncident {
            id: "x".into(), user_id: "u".into(), fingerprint: "f".into(),
            severity: "WARN".into(), source: "log:mira.log".into(),
            module: "mira::some::random::module".into(), message: "x".into(),
            payload_json: "{}".into(), created_at: 0,
            analysis_status: "none".into(),
            analysis_started_at: None, analysis_completed_at: None,
            conversation_id: None, analysis_response: None,
        };
        let s = render_remediation_hints(&inc);
        // No "For this signal specifically" line, but the always-shown
        // footer is still there.
        assert!(!s.contains("For this signal specifically"));
        assert!(s.contains("Dashboard surfaces"));
    }

    #[test]
    fn build_for_incident_returns_none_with_too_few_snapshots() {
        let store = HealthStore::open_in_memory();
        // Just one snapshot — under the min-2 threshold.
        let snap = HealthSnapshot {
            taken_at: chrono::Utc::now().timestamp(),
            duration_ms: 1,
            reports: vec![DetectorReport {
                name: "test.x".into(), level: HealthLevel::Yellow,
                message: "x".into(), value: None,
                payload: serde_json::Value::Null, auto_action_eligible: false,
                analytics: None,
            }],
        };
        store.record(&snap, None).unwrap();
        // We can only test the no-history path without an AutomationsStore;
        // the AutomationsStore-dependent path is exercised live on test-prod.
        // Stub the automations store with a tempdir-backed open.
        let dir = tempfile::tempdir().unwrap();
        let auto = crate::automations::AutomationsStore::open(&dir.path().join("a.db")).unwrap();
        let out = build_for_incident(&store, &auto, "test.x", "fp");
        assert!(out.is_none(), "should return None with <2 snapshots");
    }
}
