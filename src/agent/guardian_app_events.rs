// SPDX-License-Identifier: AGPL-3.0-or-later

//! Guardian ⇄ apps-framework bridge (Phase 2): the Guardian's **monitoring**
//! subscription to the shared event bus.
//!
//! Per the two-actor split, apps interact with **MIRA** (tools, UI, benign `info`
//! events feed MIRA's automations layer), while the **Guardian** only watches for
//! *problems*. This subscriber embodies that: it reads every event but acts **only**
//! on app-domain events whose severity marks them an *issue* (`warn`/`error`/
//! `critical`) — logging them and updating the Guardian's telemetry
//! ([`WatchStatus`]), surfaced read-only at `GET /api/guardian/status`.
//!
//! Slice 1 only observes + counts app issues. Real triage (LLM assessment, dedup,
//! action) is a later Phase-2 slice — this is the seam it plugs into.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::agent::core::AgentCore;
use crate::agent::guardian::{self, GuardianMode, GuardianTier};
use crate::agent::guardian_actions::{GuardianActionStatus, GuardianActionStore};
use crate::config::MiraConfig;
use crate::events::{names, Event, EventBus};
use crate::notifications::{Notification, NotificationBus, NotificationKind};

/// Don't re-triage the same (app, event, severity) more often than this — an app
/// can emit the same issue repeatedly; the LLM turn is expensive.
const APP_ISSUE_DEBOUNCE_SECS: i64 = 600;

/// Guardian deps needed to run a real triage turn on an app issue and deliver the
/// alert (the same handles the health-audit `spawn_watch_loop` uses). `None`
/// (minimal/test builds) → the monitor still logs + counts issues but runs no
/// LLM turn.
pub struct AppTriageDeps {
    pub agent:            Arc<AgentCore>,
    pub notifications:    Arc<NotificationBus>,
    pub config:           Arc<MiraConfig>,
    pub notify_user_id:   Option<String>,
    pub guardian_actions: Option<Arc<GuardianActionStore>>,
}

/// Spawn the Guardian's app-event monitoring loop. Subscribes to the bus and,
/// for each app-domain **issue** event, logs + bumps the app-issue telemetry
/// and — when `deps` are wired and `guardian.mode != off` — runs a real triage
/// turn (LLM assessment → operator alert, and in `active` mode a proposed bounded
/// fix), deduped per (app, event, severity). Wire this before the server accepts
/// traffic (a broadcast subscription only sees events emitted *after*
/// `subscribe()`).
pub fn spawn_app_event_triage(
    bus:  Arc<EventBus>,
    deps: Option<AppTriageDeps>,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        // Event-driven dedup: fingerprint → last-triage unix ts.
        let mut last_triage: HashMap<String, i64> = HashMap::new();
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    // Guardian's lens: only app-domain events that are *issues*.
                    // Benign `info` app events (and non-app events) are MIRA's
                    // concern, not the Guardian's — skip them.
                    if ev.domain.is_none() || !ev.severity_is_issue() {
                        continue;
                    }
                    let app = ev.entity.as_deref().unwrap_or("unknown app").to_string();
                    let sev = ev.severity.as_deref().unwrap_or("issue").to_string();
                    info!("MIRA-Guardian: app {sev} '{}' from {app}", ev.name);

                    {
                        let mut st = guardian::watch_status().write().await;
                        st.app_issues_total = st.app_issues_total.saturating_add(1);
                        st.last_app_issue_at = Some(ev.at);
                        st.last_app_issue_summary = Some(format!("{} from {app}", ev.name));
                    }

                    // Real triage (Slice 3): only with deps + guardian not Off.
                    let Some(deps) = deps.as_ref() else { continue };
                    let gmode = guardian::mode(&deps.config);
                    if gmode == GuardianMode::Off {
                        continue;
                    }
                    // Debounce the same issue; prune stale entries.
                    let now = chrono::Utc::now().timestamp();
                    let fp = format!("{app}|{}|{sev}", ev.name);
                    last_triage.retain(|_, t| now - *t < APP_ISSUE_DEBOUNCE_SECS);
                    if last_triage.get(&fp).is_some_and(|&t| now - t < APP_ISSUE_DEBOUNCE_SECS) {
                        debug!("guardian app-issue triage: debounced {fp}");
                        continue;
                    }
                    last_triage.insert(fp, now);

                    if let Err(e) = triage_app_issue(deps, &bus, &ev, &app, &sev, gmode).await {
                        warn!("guardian app-issue triage failed: {e}");
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    warn!("guardian app-event monitor: lagged, dropped {n} event(s)");
                }
                Err(RecvError::Closed) => {
                    debug!("guardian app-event monitor: bus closed, exiting");
                    break;
                }
            }
        }
    })
}

/// Severity → tier: `error`/`critical` is real triage (stronger model + full
/// charter); `warn` is routine (light model).
fn tier_for_severity(sev: &str) -> GuardianTier {
    match sev {
        "error" | "critical" => GuardianTier::Triage,
        _ => GuardianTier::Routine,
    }
}

/// Build the triage task prompt for an app issue. Mirrors the health-audit
/// prompt; the `active` flag appends the same "you MAY propose ONE bounded fix"
/// clause the watch loop uses. Payload is bounded so a chatty app can't blow the
/// prompt.
fn app_issue_task(app: &str, event: &str, sev: &str, payload: &serde_json::Value, active: bool) -> String {
    let payload = {
        let s = payload.to_string();
        if s.len() > 500 { format!("{}…", s.chars().take(500).collect::<String>()) } else { s }
    };
    let mut task = format!(
        "An installed app reported a problem. app={app}, event={event:?}, severity={sev}, payload={payload}. \
         Call guardian_inspect (what=\"all\") for context, then write a 2-3 sentence operator alert: what the \
         app is reporting, the most likely cause, and the single most useful next action. Be specific and \
         concise; begin with 'MIRA-Guardian:'.",
    );
    if active {
        task.push_str(
            " If exactly ONE bounded fix is clearly warranted (rerun_audit / restart_bridge / \
             requeue_automation / trim_logs), you MAY propose it with guardian_propose_action — it is \
             recorded PENDING for operator approval and does NOT run now. Otherwise just alert.");
    }
    task
}

/// Run one real triage turn for an app issue and deliver the alert on the same
/// three rails as the health-audit loop (web/push notification, the
/// `watchdog.alert` event, and the operator's messaging channel). In `active`
/// mode the turn may record a PENDING proposal, which we surface in the alert.
async fn triage_app_issue(
    deps:  &AppTriageDeps,
    bus:   &Arc<EventBus>,
    ev:    &Event,
    app:   &str,
    sev:   &str,
    gmode: GuardianMode,
) -> Result<(), crate::MiraError> {
    let tier = tier_for_severity(sev);
    let task = app_issue_task(app, &ev.name, sev, &ev.payload, gmode == GuardianMode::Active);
    let uid = deps.notify_user_id.clone().unwrap_or_else(|| "system".to_string());
    let turn_start = chrono::Utc::now().timestamp();
    let text = deps.agent.run_guardian_turn(&uid, &task, tier).await?;
    let mut alert = text.trim().to_string();
    if alert.is_empty() {
        return Ok(());
    }

    // Surface any proposal this turn recorded (append to the alert).
    if let Some(store) = deps.guardian_actions.as_ref() {
        if let Ok(pend) = store.list(Some(GuardianActionStatus::Pending), 20) {
            if let Some(a) = pend.into_iter().find(|a| a.created_at >= turn_start) {
                alert.push_str(&format!(
                    "\n\n(Proposed fix pending your approval: {}{})",
                    a.kind.as_str(),
                    a.target.as_deref().map(|t| format!(" {t}")).unwrap_or_default(),
                ));
            }
        }
    }

    // Rail 1 — web/push notification.
    deps.notifications.send(Notification {
        kind:            NotificationKind::GuardianAlert,
        conversation_id: None,
        channel:         Some("web".to_string()),
        user_id:         deps.notify_user_id.clone(),
        message:         Some(alert.clone()),
        category:        None,
    });
    // Rail 2 — the watchdog.alert event rail.
    bus.emit(Event::new(
        names::WATCHDOG_ALERT,
        deps.notify_user_id.clone(),
        serde_json::json!({ "source": "app_issue", "app": app, "event": ev.name, "severity": sev }),
    ));
    // Rail 3 — the operator's last messaging channel.
    if let (Some(recipient), Some(disp)) =
        (deps.notify_user_id.as_deref(), deps.agent.companion_dispatcher())
    {
        let _ = disp.deliver_to_user(recipient, &alert).await;
    }

    // Telemetry: count this as a delivered alert too.
    {
        let mut st = guardian::watch_status().write().await;
        st.last_alert_at = Some(turn_start);
        st.last_alert_summary = Some(alert.chars().take(160).collect());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use serde_json::json;

    // Wait (bounded) for the detached subscriber to have processed emitted
    // events, i.e. for the app-issue counter to reach `want`. Broadcast
    // delivery + the counter bump happen on the spawned task, so we can't read
    // synchronously right after `emit`.
    async fn wait_for_count(want: u64) -> u64 {
        for _ in 0..200 {
            let got = crate::agent::guardian::watch_status().read().await.app_issues_total;
            if got >= want { return got; }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        crate::agent::guardian::watch_status().read().await.app_issues_total
    }

    // Two-actor routing (the slice's core contract): the Guardian's monitor
    // acts ONLY on app-domain *issue* events. A benign `info` app event and a
    // non-app event must not move its counter; a `warn`+ app event must.
    #[tokio::test]
    async fn guardian_counts_only_app_domain_issue_events() {
        let bus = Arc::new(EventBus::new());
        // No triage deps → telemetry-only path (Slice 1 behaviour), which is
        // exactly the two-actor routing this test asserts.
        let handle = spawn_app_event_triage(Arc::clone(&bus), None);

        let base = crate::agent::guardian::watch_status().read().await.app_issues_total;

        // 1) benign info app event — Guardian ignores it (MIRA's layer owns it).
        bus.emit(Event::new_app("app.demo.hello", None, "demo", "info", "com.mira.demo-hello", json!({})));
        // 2) a non-app event (no domain) — also ignored.
        bus.emit(Event::new("some.system.tick", None, json!({})));
        // Give the subscriber a beat; the counter must still be at baseline.
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        assert_eq!(
            crate::agent::guardian::watch_status().read().await.app_issues_total, base,
            "info + non-app events must not move the Guardian's app-issue counter",
        );

        // 3) an issue-severity app event — Guardian counts it.
        bus.emit(Event::new_app("app.demo.issue", None, "demo", "warn", "com.mira.demo-hello", json!({})));
        let after = wait_for_count(base + 1).await;
        assert_eq!(after, base + 1, "a warn app event must increment the Guardian's app-issue counter");

        let st = crate::agent::guardian::watch_status().read().await;
        assert_eq!(st.last_app_issue_summary.as_deref(), Some("app.demo.issue from com.mira.demo-hello"));
        assert!(st.last_app_issue_at.is_some());
        drop(st);

        handle.abort();
    }

    #[test]
    fn severity_maps_to_tier() {
        assert_eq!(tier_for_severity("warn"), GuardianTier::Routine);
        assert_eq!(tier_for_severity("warning"), GuardianTier::Routine);
        assert_eq!(tier_for_severity("error"), GuardianTier::Triage);
        assert_eq!(tier_for_severity("critical"), GuardianTier::Triage);
    }

    #[test]
    fn task_prompt_includes_issue_and_only_proposes_in_active() {
        let payload = serde_json::json!({ "code": 500 });
        let routine = app_issue_task("com.mira.demo", "app.demo.issue", "warn", &payload, false);
        assert!(routine.contains("com.mira.demo"));
        assert!(routine.contains("app.demo.issue"));
        assert!(routine.contains("severity=warn"));
        assert!(routine.contains("MIRA-Guardian:"));
        // Routine (monitor / non-active) must NOT invite a proposal.
        assert!(!routine.contains("guardian_propose_action"));
        // Active mode appends the propose clause.
        let active = app_issue_task("com.mira.demo", "app.demo.issue", "error", &payload, true);
        assert!(active.contains("guardian_propose_action"));
    }
}
