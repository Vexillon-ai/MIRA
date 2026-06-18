// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/webhooks.rs
//! 0.110.0 — outbound webhook fan-out for non-green health reports.
//!
//! Each row in `health_webhooks` is a configured destination. When the
//! collector files an incident, it also POSTs a JSON body to each
//! enabled hook whose `levels_csv` includes the report's level.
//! HMAC-SHA256 signs the body with the row's `secret` so the receiver
//! can verify integrity.
//!
//! Fan-out runs in a tokio::spawn so a slow/dead webhook doesn't block
//! the audit. Each delivery's outcome (HTTP status or transport error)
//! is recorded back to the row via `record_webhook_fire` for the
//! dashboard's Webhooks tab.

use std::sync::Arc;

use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use tracing::warn;

use super::store::{HealthStore, WebhookRow};
use super::DetectorReport;

type HmacSha256 = Hmac<Sha256>;

/// JSON body POSTed to every enabled webhook. Stable shape so
/// receivers can parse it without depending on internal types.
#[derive(Debug, Serialize)]
pub struct WebhookEvent<'a> {
    pub schema:        &'a str,
    pub kind:          &'a str,           // "system_health.incident"
    pub detector_name: &'a str,
    pub level:         &'a str,           // "green" | "yellow" | "red"
    pub message:       &'a str,
    pub value:         Option<f64>,
    pub payload:       &'a serde_json::Value,
    pub fired_at:      i64,
    pub mira_version:  &'a str,
}

/// Fan-out one report to every enabled webhook whose level filter
/// matches. Returns immediately; deliveries spawn into background
/// tasks. Best-effort by design.
pub fn fan_out(
    store:        Arc<HealthStore>,
    report:       &DetectorReport,
    fired_at:     i64,
) {
    // Snapshot the destinations under the lock then drop it before
    // touching the network.
    let hooks = match store.list_webhooks() {
        Ok(rows) => rows,
        Err(e)   => { warn!("webhook fan-out: list failed: {e}"); return; }
    };
    let level_str = report.level.as_str().to_string();
    let body = WebhookEvent {
        schema:        "https://mira.dev/schema/webhook/health/v1",
        kind:          "system_health.incident",
        detector_name: &report.name,
        level:         &level_str,
        message:       &report.message,
        value:         report.value,
        payload:       &report.payload,
        fired_at,
        mira_version:  env!("CARGO_PKG_VERSION"),
    };
    let body_json = match serde_json::to_string(&body) {
        Ok(s)  => s,
        Err(e) => { warn!("webhook fan-out: serialize failed: {e}"); return; }
    };
    let body_arc = Arc::new(body_json);

    for hook in hooks {
        if !hook.enabled { continue; }
        if !level_matches(&hook.levels_csv, &level_str) { continue; }
        spawn_delivery(Arc::clone(&store), hook, Arc::clone(&body_arc));
    }
}

fn level_matches(levels_csv: &Option<String>, level: &str) -> bool {
    // None / empty = default = (yellow, red). The detector's "green"
    // status doesn't fire by default — admins opt in by adding "green"
    // to the CSV (e.g. for a green-pulse health monitor).
    let csv = match levels_csv.as_deref() {
        None | Some("") => return matches!(level, "yellow" | "red"),
        Some(s) => s,
    };
    csv.split(',').map(|s| s.trim()).any(|s| s == level)
}

/// Spawn one delivery. Records the outcome (status code or error)
/// back into the row.
fn spawn_delivery(
    store:    Arc<HealthStore>,
    hook:     WebhookRow,
    body:     Arc<String>,
) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
        {
            Ok(c)  => c,
            Err(e) => {
                let _ = store.record_webhook_fire(&hook.id, None, Some(&e.to_string()));
                return;
            }
        };

        let mut req = client.post(&hook.url)
            .header("Content-Type", "application/json")
            .header("User-Agent", concat!("mira-health-webhook/", env!("CARGO_PKG_VERSION")))
            .body(body.as_str().to_owned());

        // HMAC-SHA256 over the raw body. Header name follows the
        // GitHub-style convention so receivers can plug into existing
        // webhook libs.
        if let Some(secret) = hook.secret.as_deref().filter(|s| !s.is_empty()) {
            if let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) {
                mac.update(body.as_bytes());
                let sig = hex::encode(mac.finalize().into_bytes());
                req = req.header("X-Mira-Signature", format!("sha256={sig}"));
            }
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16() as i64;
                let err: Option<String> = if !resp.status().is_success() {
                    resp.text().await.ok().map(|t| t.chars().take(200).collect())
                } else {
                    None
                };
                let _ = store.record_webhook_fire(&hook.id, Some(status), err.as_deref());
            }
            Err(e) => {
                warn!("webhook {}: POST failed: {e}", hook.id);
                let _ = store.record_webhook_fire(&hook.id, None, Some(&e.to_string()));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_matches_default_is_yellow_red() {
        assert!(level_matches(&None, "yellow"));
        assert!(level_matches(&None, "red"));
        assert!(!level_matches(&None, "green"));
        assert!(level_matches(&Some(String::new()), "yellow"));
    }

    #[test]
    fn level_matches_csv_filter() {
        let csv = Some("green,red".to_string());
        assert!(level_matches(&csv, "red"));
        assert!(level_matches(&csv, "green"));
        assert!(!level_matches(&csv, "yellow"));
    }
}
