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

/// The alarm message on MIRA going down.
const DOWN_MSG: &str =
    "MIRA appears to be down — the assistant isn't responding. Someone may need to check the server.";
/// The reassurance message on recovery.
const RECOVER_MSG: &str = "MIRA is back up — the assistant is responding again.";

/// Run the sentinel loop. Blocks (a long-running process) until the task is
/// cancelled / the process exits. A no-op (returns immediately) when
/// `guardian.process.enabled` is false.
pub async fn run(config: Arc<MiraConfig>) -> Result<(), MiraError> {
    let pc = &config.guardian.process;
    if !pc.enabled {
        warn!("guardian-watch: guardian.process.enabled is false — nothing to watch; exiting");
        return Ok(());
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
                error!("guardian-watch: MIRA DOWN — {} consecutive failed probes of {url}", state.misses());
                push_notice(webpush.as_ref(), notify_uid.as_deref(), DOWN_MSG).await;
                // Audit the down-alarm. Safe to write the shared HMAC chain here:
                // MIRA is confirmed down, so it isn't concurrently writing.
                audit_down(&data_dir, &url, state.misses());
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
fn audit_down(data_dir: &Path, url: &str, misses: u32) {
    let Ok(store) = crate::agent::audit::AuditStore::open(&data_dir.join("agent_audit.db")) else {
        return;
    };
    let _ = store.record(
        crate::agent::audit::guardian_agent_id(),
        None,
        crate::agent::audit::AuditEvent::GuardianAction {
            action_id:   format!("sentinel-down-{url}"),
            action_kind: "sentinel_liveness".to_string(),
            decision:    "mira_down".to_string(),
            detail:      Some(format!("{misses} consecutive failed probes of {url}")),
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
