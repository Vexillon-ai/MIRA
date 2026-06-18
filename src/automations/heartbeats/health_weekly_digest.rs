// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/health_weekly_digest.rs
//! 0.109.0 — Sunday-evening digest of the past 7 days of system health.
//!
//! Files an INFO-severity watchdog incident (so it routes via the same
//! `watchdog.alert delivery` subscription as everything else) summarising:
//!   - top detectors by incident count
//!   - currently-noisy fingerprints (collapsed dedup)
//!   - auto-actions that fired
//!
//! The digest is intentionally INFO-level so admins can mute it
//! per-channel via the Watchdog config without losing access — it's
//! a regular check-in, not an emergency.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::automations::store::NewWatchdogIncident;
use crate::automations::AutomationsStore;
use crate::events::{Event, EventBus};
use crate::health::store::HealthStore;
use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

pub const DIGEST_TASK_NAME: &str = "health_weekly_digest";

pub struct HealthWeeklyDigest {
    automations:     Arc<AutomationsStore>,
    health_store:    Arc<HealthStore>,
    notify_user_id:  Option<String>,
}

impl HealthWeeklyDigest {
    pub fn new(
        automations:    Arc<AutomationsStore>,
        health_store:   Arc<HealthStore>,
        notify_user_id: Option<String>,
    ) -> Self {
        Self { automations, health_store, notify_user_id }
    }
}

#[async_trait]
impl HeartbeatTask for HealthWeeklyDigest {
    fn name(&self) -> &'static str { DIGEST_TASK_NAME }

    async fn run(
        &self,
        ctx:   &HeartbeatContext,
        _args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        let now = chrono::Utc::now().timestamp();
        let week_ago = now - 7 * 24 * 3600;

        // Pull the last 7 days of system_health incidents and group by
        // detector + fingerprint.
        let incidents = self.automations
            .list_health_incidents_since(week_ago)
            .unwrap_or_else(|e| {
                warn!("weekly digest: incidents fetch failed (continuing with empty set): {e}");
                Vec::new()
            });
        let summaries = self.health_store
            .list_summaries_since(week_ago)
            .unwrap_or_else(|e| {
                warn!("weekly digest: summaries fetch failed: {e}");
                Vec::new()
            });

        // ── Roll up: incidents per detector ──────────────────────────
        use std::collections::HashMap;
        let mut by_detector: HashMap<String, usize> = HashMap::new();
        let mut by_fingerprint: HashMap<String, (usize, String)> = HashMap::new();
        for inc in &incidents {
            *by_detector.entry(inc.module.replace("health/", "")).or_insert(0) += 1;
            let entry = by_fingerprint.entry(inc.fingerprint.clone())
                .or_insert((0, inc.message.clone()));
            entry.0 += 1;
        }
        let mut top_detectors: Vec<(String, usize)> = by_detector.into_iter().collect();
        top_detectors.sort_by(|a, b| b.1.cmp(&a.1));
        top_detectors.truncate(5);
        let mut noisy_fps: Vec<(String, usize, String)> = by_fingerprint.into_iter()
            .map(|(fp, (n, msg))| (fp, n, msg))
            .filter(|t| t.1 >= 3)
            .collect();
        noisy_fps.sort_by(|a, b| b.1.cmp(&a.1));
        noisy_fps.truncate(5);

        // ── Roll up: snapshot summary stats ──────────────────────────
        let total_audits = summaries.len();
        let red_audits   = summaries.iter().filter(|s| s.worst_level == "red").count();
        let yellow_audits = summaries.iter().filter(|s| s.worst_level == "yellow").count();

        // ── Render the digest body ───────────────────────────────────
        let mut md = String::new();
        md.push_str(&format!(
            "## Weekly health digest — {}\n\n",
            chrono::DateTime::<chrono::Utc>::from_timestamp(now, 0)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| now.to_string()),
        ));
        md.push_str(&format!(
            "**Audits in last 7 days:** {total_audits} \
             ({red_audits} red, {yellow_audits} yellow, {} green)\n\n",
            total_audits.saturating_sub(red_audits).saturating_sub(yellow_audits),
        ));
        md.push_str(&format!("**Incidents filed:** {}\n\n", incidents.len()));

        if !top_detectors.is_empty() {
            md.push_str("**Top detectors by incident count:**\n");
            for (name, n) in &top_detectors {
                md.push_str(&format!("- `{name}` — {n}\n"));
            }
            md.push('\n');
        }

        if !noisy_fps.is_empty() {
            md.push_str("**Noisy fingerprints (≥3 incidents this week):**\n");
            for (fp, n, msg) in &noisy_fps {
                let preview: String = msg.chars().take(80).collect();
                md.push_str(&format!("- `{fp}` × {n} — {preview}…\n"));
            }
            md.push_str("\nIf any of these are expected noise, add an `ignore_patterns` regex to the watchdog config or set the detector to **Off** in the dashboard.\n\n");
        }

        if incidents.is_empty() {
            md.push_str("Nothing tripped this week. Either MIRA is genuinely healthy, or some detector got muted by accident — check the Config tab.\n");
        }

        // ── File as an INFO incident ─────────────────────────────────
        let Some(user_id) = self.notify_user_id.as_deref() else {
            return Ok(HeartbeatOutcome { summary: format!(
                "weekly digest computed but no notify_user_id — skipped persist (incidents={}, audits={total_audits})",
                incidents.len(),
            )});
        };
        let fingerprint = format!("health:weekly_digest:{}", now / (7 * 24 * 3600));
        let payload = serde_json::json!({
            "audit_count":      total_audits,
            "red_audits":       red_audits,
            "yellow_audits":    yellow_audits,
            "incident_count":   incidents.len(),
            "top_detectors":    top_detectors,
            "noisy_fingerprints": noisy_fps,
            "first_seen_at":    now,
            "recent_count":     1u32,
        });
        let new = NewWatchdogIncident {
            user_id:      user_id.to_string(),
            fingerprint:  fingerprint.clone(),
            severity:     "INFO".into(),
            source:       "system_health_digest".into(),
            module:       "health/weekly_digest".into(),
            message:      md.clone(),
            payload_json: payload.to_string(),
        };
        let incident_id = self.automations
            .create_watchdog_incident(new)
            .map_err(|e| { warn!("weekly digest persist failed: {e}"); e })?;

        // Emit through the existing watchdog.alert delivery sub.
        if let Some(bus) = ctx.event_bus.as_ref() {
            emit_digest_alert(bus, &incident_id, &fingerprint, &md, now);
        }

        info!(
            "weekly digest filed: incident={incident_id}, {} incident(s) summarised, {total_audits} audit(s)",
            incidents.len(),
        );
        Ok(HeartbeatOutcome { summary: format!(
            "weekly digest filed (incident={incident_id}, {} incident(s) over 7 days)",
            incidents.len(),
        )})
    }
}

fn emit_digest_alert(
    bus: &EventBus,
    incident_id: &str,
    fingerprint: &str,
    body: &str,
    now: i64,
) {
    let analyze_link = format!("[🔍 Analyze with LLM](/incidents/{incident_id})");
    let payload = serde_json::json!({
        "severity":       "INFO",
        "severity_emoji": "📊",
        "source":         "system_health_digest",
        "module":         "health/weekly_digest",
        // Send only the first line as the channel-routed message body —
        // the full digest stays in the incident, where the user can
        // open + read it in full.
        "message":        body.lines().next().unwrap_or("Weekly health digest").to_string(),
        "fingerprint":    fingerprint,
        "first_seen_at":  now,
        "recent_count":   1u32,
        "incident_id":    incident_id,
        "analyze_link":   analyze_link,
    });
    bus.emit(Event::new(crate::events::names::WATCHDOG_ALERT, None, payload));
}
