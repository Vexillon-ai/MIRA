// SPDX-License-Identifier: AGPL-3.0-or-later

// src/guardian_sentinel.rs
//! **MIRA-Guardian liveness sentinel** — the out-of-process watchdog
//! (`mira guardian-watch`).
//!
//! The co-resident Guardian watch loop runs *inside* the MIRA process, so the
//! one failure it cannot catch is MIRA itself going down — it shares MIRA's
//! fate. This sentinel is a **separate, supervised process** whose only job is
//! to confirm MIRA is alive and, if it isn't, raise a **direct** alarm to the
//! household — without routing through the (down) MIRA.
//!
//! It probes MIRA's unauthenticated `/health` endpoint on an interval. **Any**
//! HTTP response — even a 5xx (provider-down but process-up) — counts as alive;
//! only a transport error (connection refused / timeout) counts as down. After
//! `down_after_failures` consecutive misses it declares MIRA down and delivers a
//! web-push alarm, opened **cold** from the shared data dir (`web_push_vapid.key`
//! + `web_push.db`) so it works with MIRA fully offline. On recovery it clears
//! the alarm. Observe-and-alarm only in this increment — it never restarts or
//! mutates anything (see design-docs/guardian-separate-process.md).
//!
//! The failure threshold is what makes a normal restart *not* alarm: MIRA comes
//! back within one probe window, so the miss count never reaches the threshold.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info, warn};

use crate::config::MiraConfig;
use crate::MiraError;

/// What the sentinel state machine decides after one observation. Pure — the
/// I/O (push, audit, log) happens in the loop based on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelAction {
    /// Nothing changed worth acting on.
    Nothing,
    /// MIRA just crossed from up → down (miss count reached the threshold).
    RaiseAlarm,
    /// MIRA just came back after an alarm was raised.
    Recover,
}

/// Rolling liveness state. `observe` is a pure transition so it's unit-tested
/// without any network or clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SentinelState {
    consecutive_failures: u32,
    in_alarm:             bool,
}

impl SentinelState {
    /// Fold one probe result into the state and report the edge action.
    /// - alive → reset the miss counter; if we were alarming, `Recover` once.
    /// - down  → bump the miss counter; when it first reaches `down_after`
    ///   (and we're not already alarming), `RaiseAlarm` once. No repeat alarms
    ///   while it stays down.
    pub fn observe(&mut self, alive: bool, down_after: u32) -> SentinelAction {
        let down_after = down_after.max(1);
        if alive {
            self.consecutive_failures = 0;
            if self.in_alarm {
                self.in_alarm = false;
                return SentinelAction::Recover;
            }
            SentinelAction::Nothing
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            if self.consecutive_failures >= down_after && !self.in_alarm {
                self.in_alarm = true;
                return SentinelAction::RaiseAlarm;
            }
            SentinelAction::Nothing
        }
    }

    /// Consecutive failed probes so far (for logging).
    pub fn misses(&self) -> u32 { self.consecutive_failures }
}

/// The liveness URL to probe: the explicit `guardian.process.probe_url` if set,
/// else `http://127.0.0.1:<server.port>/health` (the unauthenticated readiness
/// route). Loopback, not `server.host`, since the sentinel is co-located and a
/// `0.0.0.0` bind isn't a dialable address.
pub fn probe_url(config: &MiraConfig) -> String {
    config.guardian.process.probe_url.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("http://127.0.0.1:{}/health", config.server.port))
}

/// One liveness probe. ANY HTTP response (incl. 5xx) means the process is up;
/// only a transport error (connection refused / timeout / DNS) means down.
async fn probe(client: &reqwest::Client, url: &str) -> bool {
    client.get(url).send().await.is_ok()
}

/// Deliver the direct out-of-process alarm/notice via web push (cold from the
/// shared data dir). Best-effort: a missing push target or store just logs.
async fn push_notice(
    webpush:  Option<&crate::notifications::web_push::WebPushService>,
    user_id:  Option<&str>,
    message:  &str,
) {
    let Some(uid) = user_id.map(str::trim).filter(|s| !s.is_empty()) else {
        warn!("guardian-watch: no guardian.process.notify_user_id set — logged only, no push sent");
        return;
    };
    let Some(wp) = webpush else {
        warn!("guardian-watch: web-push unavailable (no keys/subscriptions in data dir) — logged only");
        return;
    };
    let notif = crate::notifications::Notification {
        kind:            crate::notifications::NotificationKind::GuardianAlert,
        conversation_id: None,
        channel:         None,
        user_id:         Some(uid.to_string()),
        message:         Some(message.to_string()),
        category:        None,
    };
    match wp.send_to_user(uid, &notif.to_envelope()).await {
        Ok(n)  => info!("guardian-watch: alarm delivered to {n} device(s) for '{uid}'"),
        Err(e) => error!("guardian-watch: web-push delivery failed for '{uid}': {e}"),
    }
}

/// The base alarm message on MIRA going down. [`down_message`] appends the
/// last-known health summary when one is available.
const DOWN_MSG: &str =
    "MIRA appears to be down — the assistant isn't responding. Someone may need to check the server.";
/// The reassurance message on recovery.
const RECOVER_MSG: &str = "MIRA is back up — the assistant is responding again.";

/// Humanize an age in seconds for alarm text: `2h` / `45m` / `30s`.
fn humanize_age(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 { format!("{}h", s / 3600) }
    else if s >= 60 { format!("{}m", s / 60) }
    else { format!("{s}s") }
}

/// A one-line summary of MIRA's last-known health for the down-alarm, or `None`
/// when there's no snapshot. Pure — unit-tested. When non-green, lists up to 4
/// triggered detectors (sorted, `+N more` beyond that).
fn health_summary(snap: Option<&crate::health::HealthSnapshot>, now: i64) -> Option<String> {
    let snap = snap?;
    let ago = humanize_age(now - snap.taken_at);
    if matches!(snap.worst_level(), crate::health::HealthLevel::Green) {
        return Some(format!("Its last health check {ago} ago was all-green."));
    }
    let mut det: Vec<&str> = snap.reports.iter()
        .filter(|r| !matches!(r.level, crate::health::HealthLevel::Green))
        .map(|r| r.name.as_str())
        .collect();
    det.sort_unstable();
    let shown = det.len().min(4);
    let more  = det.len() - shown;
    let list  = det[..shown].join(", ");
    let tail  = if more > 0 { format!(" (+{more} more)") } else { String::new() };
    Some(format!("Its last health check {ago} ago showed {:?}: {list}{tail}.", snap.worst_level()))
}

/// The MIRA-down alarm text, optionally enriched with the last-known health.
fn down_message(health: Option<&str>) -> String {
    match health {
        Some(h) => format!("{DOWN_MSG} {h}"),
        None    => DOWN_MSG.to_string(),
    }
}

/// Run the sentinel loop. Blocks (a long-running process) until the task is
/// cancelled / the process exits. A no-op (returns immediately) when
/// `guardian.process.enabled` is false.
pub async fn run(config: Arc<MiraConfig>) -> Result<(), MiraError> {
    let pc = &config.guardian.process;
    if !pc.enabled {
        // Park instead of exiting. This process is meant to run under a
        // supervisor with `Restart=always`, so an immediate `exit(0)` would spin
        // the unit in a tight restart loop. Idle quietly instead; the operator
        // enables `guardian.process.enabled` and restarts this unit to begin
        // watching. (A manual `mira guardian-watch` here just idles — Ctrl-C to
        // stop.)
        warn!("guardian-watch: guardian.process.enabled is false — idling (enable it and restart this unit to begin watching)");
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
    let data_dir   = config.data_dir_path();
    let url        = probe_url(&config);
    let interval   = Duration::from_secs(pc.probe_interval_secs.max(5));
    let down_after = pc.down_after_failures.max(1);
    let notify_uid = pc.notify_user_id.clone();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| MiraError::ConfigError(format!("guardian-watch: http client: {e}")))?;

    // Cold web-push service, opened from the shared data dir — no running MIRA
    // needed. Best-effort: if the keys/DB aren't there yet, alarms log only.
    let webpush = crate::notifications::web_push::WebPushService::open(
        &data_dir,
        &crate::notifications::web_push::service_path(&data_dir),
        None,
    ).map_err(|e| warn!("guardian-watch: web-push unavailable: {e}")).ok();

    // Read-only handle on MIRA's health snapshots, so a MIRA-down alarm can
    // carry the last-known health state ("… last check 2h ago showed Red: …").
    // Best-effort: absent/locked DB just means a plainer alarm. (2a — the
    // out-of-process health-read plumbing the future triage turn builds on.)
    let health = crate::health::store::HealthStore::open(&data_dir.join("health.db"))
        .map_err(|e| warn!("guardian-watch: health store unavailable (alarms won't include last-known health): {e}"))
        .ok();

    info!(
        "guardian-watch: probing {url} every {}s; MIRA declared down after {down_after} consecutive misses; \
         push target = {}",
        interval.as_secs(),
        notify_uid.as_deref().filter(|s| !s.is_empty()).unwrap_or("(none — logs only)"),
    );

    let mut state  = SentinelState::default();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let alive = probe(&client, &url).await;
        match state.observe(alive, down_after) {
            SentinelAction::RaiseAlarm => {
                // Enrich the alarm with MIRA's last-known health, if we can read it.
                let now  = chrono::Utc::now().timestamp();
                let hsum = health.as_ref()
                    .and_then(|h| h.latest().ok().flatten())
                    .and_then(|snap| health_summary(Some(&snap), now));
                let msg = down_message(hsum.as_deref());
                error!("guardian-watch: MIRA DOWN ({} failed probes of {url}) — {msg}", state.misses());
                push_notice(webpush.as_ref(), notify_uid.as_deref(), &msg).await;
                // Audit the down-alarm. Safe to write the shared HMAC chain here:
                // MIRA is confirmed down, so it isn't concurrently writing.
                audit_down(&data_dir, &url, state.misses(), hsum.as_deref());
            }
            SentinelAction::Recover => {
                info!("guardian-watch: MIRA recovered — responding again at {url}");
                push_notice(webpush.as_ref(), notify_uid.as_deref(), RECOVER_MSG).await;
                // No audit write on recovery: MIRA is back up and writing the
                // chain itself; a concurrent sentinel write could interleave.
            }
            SentinelAction::Nothing => {
                if !alive {
                    warn!("guardian-watch: probe miss {}/{down_after} for {url}", state.misses());
                }
            }
        }
    }
}

/// Append a tamper-evident audit record for a down-alarm. Best-effort + isolated
/// so an audit failure never stops the watch. Only called when MIRA is down
/// (single-writer, so the HMAC chain stays ordered).
fn audit_down(data_dir: &Path, url: &str, misses: u32, health: Option<&str>) {
    let Ok(store) = crate::agent::audit::AuditStore::open(&data_dir.join("agent_audit.db")) else {
        return;
    };
    let mut detail = format!("{misses} consecutive failed probes of {url}");
    if let Some(h) = health {
        detail.push_str(" — ");
        detail.push_str(h);
    }
    let _ = store.record(
        crate::agent::audit::guardian_agent_id(),
        None,
        crate::agent::audit::AuditEvent::GuardianAction {
            action_id:   format!("sentinel-down-{url}"),
            action_kind: "sentinel_liveness".to_string(),
            decision:    "mira_down".to_string(),
            detail:      Some(detail),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_alarms_once_then_recovers_once() {
        let mut s = SentinelState::default();
        // Below threshold — no alarm.
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        // Third miss crosses the threshold → alarm exactly once.
        assert_eq!(s.observe(false, 3), SentinelAction::RaiseAlarm);
        // Still down → no repeat alarms.
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        // Back up → recover exactly once.
        assert_eq!(s.observe(true, 3), SentinelAction::Recover);
        assert_eq!(s.observe(true, 3), SentinelAction::Nothing);
    }

    #[test]
    fn brief_blip_under_threshold_never_alarms() {
        // A normal restart: two misses then recovery, threshold 3 → no alarm,
        // no recovery edge (we never entered the alarm state).
        let mut s = SentinelState::default();
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        assert_eq!(s.observe(false, 3), SentinelAction::Nothing);
        assert_eq!(s.observe(true, 3),  SentinelAction::Nothing);
        assert_eq!(s.misses(), 0);
    }

    #[test]
    fn threshold_of_one_alarms_immediately() {
        let mut s = SentinelState::default();
        assert_eq!(s.observe(false, 1), SentinelAction::RaiseAlarm);
        assert_eq!(s.observe(true, 1),  SentinelAction::Recover);
    }

    fn rep(name: &str, level: crate::health::HealthLevel) -> crate::health::DetectorReport {
        crate::health::DetectorReport {
            name: name.to_string(), level, message: String::new(), value: None,
            payload: serde_json::Value::Null, auto_action_eligible: false, analytics: None,
        }
    }

    #[test]
    fn health_summary_lists_triggered_detectors_worst_first() {
        use crate::health::{HealthSnapshot, HealthLevel, DetectorReport};
        let snap = HealthSnapshot {
            taken_at: 1000, duration_ms: 0,
            reports: vec![
                DetectorReport::green("proc.ok", "fine"),
                rep("db.integrity", HealthLevel::Red),
                rep("disk.free", HealthLevel::Yellow),
            ],
        };
        let s = health_summary(Some(&snap), 1000 + 7200).unwrap(); // 2h later
        assert!(s.contains("2h ago"), "{s}");
        assert!(s.contains("Red"), "worst level shown: {s}");
        assert!(s.contains("db.integrity") && s.contains("disk.free"));
        assert!(!s.contains("proc.ok"), "green detectors omitted: {s}");
    }

    #[test]
    fn health_summary_all_green_and_none() {
        use crate::health::{HealthSnapshot, DetectorReport};
        let snap = HealthSnapshot {
            taken_at: 500, duration_ms: 0,
            reports: vec![DetectorReport::green("a", "")],
        };
        let s = health_summary(Some(&snap), 500 + 90).unwrap(); // 90s → "1m"
        assert!(s.contains("all-green"), "{s}");
        assert!(s.contains("1m ago"), "{s}");
        assert_eq!(health_summary(None, 0), None);
    }

    #[test]
    fn humanize_age_units() {
        assert_eq!(humanize_age(30), "30s");
        assert_eq!(humanize_age(90), "1m");
        assert_eq!(humanize_age(7200), "2h");
        assert_eq!(humanize_age(-5), "0s");
    }

    #[test]
    fn down_message_enriches_when_health_present() {
        assert!(down_message(None).starts_with("MIRA appears to be down"));
        assert_eq!(down_message(None), DOWN_MSG);
        let m = down_message(Some("Its last health check 2h ago showed Red: db.integrity."));
        assert!(m.contains("MIRA appears to be down") && m.contains("Red: db.integrity"));
    }

    #[test]
    fn probe_url_defaults_to_loopback_health() {
        let mut c = MiraConfig::default();
        c.server.port = 8087;
        assert_eq!(probe_url(&c), "http://127.0.0.1:8087/health");
        // Explicit override wins; blank is ignored.
        c.guardian.process.probe_url = Some("   ".into());
        assert_eq!(probe_url(&c), "http://127.0.0.1:8087/health");
        c.guardian.process.probe_url = Some("http://mira.local:9000/healthz".into());
        assert_eq!(probe_url(&c), "http://mira.local:9000/healthz");
    }
}
