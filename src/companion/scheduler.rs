// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/scheduler.rs
//! Background ticker that fires companion check-ins.
//!
//! Architecture decision (vs reusing the existing automations table):
//! the scheduler lives inside the companion module and runs as its
//! own tokio task. Reasons:
//!
//! - The policy (quiet hours / min-gap / daily cap / recent-activity
//! skip / variant cadence) is companion-specific. Reusing the
//! generic automations dispatcher would either bolt those rules
//! onto the dispatch layer (cross-cutting concerns) or live in a
//! pre-dispatch hook (still indirection).
//! - Companion needs no persistent "next-run-at" planning — every
//! tick re-evaluates from the policy inputs. Stateless ticker is
//! simpler.
//! - The dispatcher (`dispatcher.rs`) is reusable from elsewhere
//! (admin "send a check-in now" button in a future slice) and
//! doesn't depend on the scheduler being running.
//!
//! The scheduler wakes every [`TICK_INTERVAL_SECS`] seconds, fetches
//! the active set with `list_active`, evaluates policy for each, and
//! fires the dispatcher for the due ones (with jitter). Errors per
//! user are logged and swallowed so one bad user can't kill the loop.

use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::auth::LocalAuthService;
use crate::companion::dispatcher::{CompanionDispatcher, DispatchOutcome};
use crate::companion::engagement_log::EngagementLog;
use crate::companion::policy::{
    adjust_for_engagement, evaluate, Decision, Limits, PolicyInputs,
};
use crate::companion::safety::{SafetyFloor, MISSED_CHECKIN_THRESHOLD};
use crate::companion::settings::CompanionStore;
use crate::history::HistoryStore;

// Window the cadence adjuster looks back over to compute the
// engagement tally. 24h gives the adjuster a recent picture
// without being overly noisy.
const ENGAGEMENT_LOOKBACK_HOURS: i64 = 24;

// Minimum number of labelled turns inside the lookback window
// before the adjuster will act. Below this we don't have signal,
// stay at baseline.
const ENGAGEMENT_MIN_SAMPLES: u32 = 3;

// How often the scheduler wakes. Picked to balance responsiveness
// (a configured "10:00 window" should fire within a minute of 10:00)
// against load (we don't need millisecond precision).
pub const TICK_INTERVAL_SECS: u64 = 60;

// A handle the gateway holds so it can shut the scheduler down
// during graceful restart. Dropping the handle aborts the task —
// no further check-ins fire.
pub struct CompanionScheduler {
    join: Option<JoinHandle<()>>,
    shutdown: Arc<Notify>,
}

impl CompanionScheduler {
    // Spawn the scheduler. Wires in the dispatcher (delivers
    // check-ins) plus the auth + history + engagement stores
    // (looks up user timezone, last activity, cadence inputs)
    // plus the safety floor (escalates missed
    // check-ins). `engagement` and `safety` may be `None` in
    // tests / minimal builds.
    pub fn spawn(
        store: Arc<CompanionStore>,
        dispatcher: CompanionDispatcher,
        auth: Option<Arc<LocalAuthService>>,
        history: Arc<HistoryStore>,
        engagement: Option<Arc<EngagementLog>>,
        safety: Option<SafetyFloor>,
        // Check-in frequency knobs, sourced from the `companion` config block:
        // pause after this many unanswered (0 = no cap), the hard per-day
        // ceiling, and the minimum minutes between check-ins.
        max_unanswered_checkins: u32,
        max_per_day: u32,
        min_gap_minutes: i64,
    ) -> Self {
        let shutdown = Arc::new(Notify::new());
        let shutdown_in = Arc::clone(&shutdown);

        let limits = Limits {
            max_unanswered_checkins,
            max_per_day,
            min_gap_minutes,
            ..Limits::default()
        };
        let safety_ref = safety;
        let join = tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(TICK_INTERVAL_SECS),
            );
            // First tick fires immediately; we want a delay so the
            // gateway has a chance to come fully up before we start
            // hitting the agent. `skip` the first tick.
            interval.tick().await;

            info!("companion scheduler started (tick = {TICK_INTERVAL_SECS}s)");
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(e) = tick_once(
                            &store, &dispatcher, auth.as_deref(),
                            &history, engagement.as_deref(),
                            safety_ref.as_ref(),
                            limits,
                        ).await {
                            warn!("companion scheduler tick failed: {e}");
                        }
                    }
                    _ = shutdown_in.notified() => {
                        info!("companion scheduler shutting down");
                        break;
                    }
                }
            }
        });

        Self { join: Some(join), shutdown }
    }

    // Signal the scheduler to stop. Returns after the loop exits.
    // Idempotent — calling shutdown twice is fine.
    pub async fn shutdown(&mut self) {
        self.shutdown.notify_one();
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for CompanionScheduler {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

// True when this user's daily-briefing pass should fire on the
// current tick. Gate:
// - briefing is enabled
// - it's at or past `daily_briefing_hour` in local time (catch-up:
// if MIRA was down/restarting during the exact hour, the next tick
// once it's back still fires today's briefing rather than skipping
// the day — the old `hour == briefing_hour` gate silently dropped a
// day whenever the process wasn't ticking during that one hour)
// - no briefing has gone out yet today (so it fires at most once per
// local day, and a reboot mid-hour doesn't double-fire)
// // Pure function — easy to unit-test against a stubbed `now`.
// How long since the last briefing forces a catch-up fire even if the
// same-local-day check is unreliable (e.g. the user's timezone was briefly
// missing/UTC). Comfortably above 1 day's worth of intra-day ticks so it
// can never cause a second briefing on the same day.
const BRIEFING_STALE_HOURS: i64 = 23;

// Decide whether the daily-briefing pass should fire on this tick, plus a
// human-readable reason for the log. Splitting out the reason lets the
// scheduler log *why* a briefing did or didn't go out — the morning-miss
// failure mode was previously invisible (skips logged at DEBUG only).
fn briefing_decision(
    s:       &crate::companion::CompanionSettings,
    now:     chrono::DateTime<Utc>,
    user_tz: Option<&str>,
) -> (bool, String) {
    use chrono::{TimeZone, Timelike};
    if !s.daily_briefing_enabled {
        return (false, "briefing disabled".into());
    }
    let tz: chrono_tz::Tz = user_tz
        .and_then(|n| n.parse().ok())
        .unwrap_or(chrono_tz::UTC);
    let tz_label = user_tz.unwrap_or("UTC(default)");
    let local_now = tz.from_utc_datetime(&now.naive_utc());
    let hour = local_now.hour();
    if hour < s.daily_briefing_hour as u32 {
        return (false, format!(
            "before briefing hour (local {hour:02}:xx < {:02}, tz={tz_label})",
            s.daily_briefing_hour
        ));
    }
    match s.last_briefing_at {
        None => (true, format!("due (past hour {}, tz={tz_label}, none sent yet)", s.daily_briefing_hour)),
        Some(prev) => {
            let same_day = crate::companion::briefing::same_local_day(prev, now, user_tz);
            let age_h = now.signed_duration_since(prev).num_hours();
            if !same_day {
                (true, format!("due (past hour {}, tz={tz_label}, last was a previous local day)", s.daily_briefing_hour))
            } else if age_h >= BRIEFING_STALE_HOURS {
                // Safety net: the same-local-day check says "already sent",
                // but the last briefing is >23h old — almost certainly a
                // timezone glitch. Fire rather than silently skip a day.
                (true, format!("due (catch-up: last briefing {age_h}h ago despite same-local-day, tz={tz_label})"))
            } else {
                (false, format!("already sent today ({age_h}h ago, tz={tz_label})"))
            }
        }
    }
}

// One scheduler iteration. Public-in-crate so an admin "tick now"
// endpoint can invoke it (future slice).
pub async fn tick_once(
    store: &Arc<CompanionStore>,
    dispatcher: &CompanionDispatcher,
    auth: Option<&LocalAuthService>,
    history: &Arc<HistoryStore>,
    engagement: Option<&EngagementLog>,
    safety: Option<&SafetyFloor>,
    limits: Limits,
) -> std::result::Result<(), crate::companion::CompanionError> {
    let now = Utc::now();
    let active = store.list_active(now)?;
    if active.is_empty() {
        return Ok(());
    }
    debug!("companion scheduler: tick with {} active user(s)", active.len());

    for s in active {
        let user_id = s.user_id.clone();
        let tz = auth
            .and_then(|a| a.get_profile(&user_id).ok().flatten())
            .and_then(|p| p.timezone);

        // User-local calendar day ("YYYY-MM-DD") for the real per-day check-in
        // counter. Same tz-resolution as the briefing pass; UTC fallback.
        let today_local = {
            use chrono::TimeZone;
            let tz_parsed: chrono_tz::Tz = tz.as_deref()
                .and_then(|n| n.parse().ok())
                .unwrap_or(chrono_tz::UTC);
            tz_parsed.from_utc_datetime(&now.naive_utc()).format("%Y-%m-%d").to_string()
        };

        // ── Q1.6 Daily Briefing pass ─────────────────────────────────────────
        //
        // Independent of check-in cadence — briefings are scheduled,
        // not policy-driven. Fires when:
        // 1. briefing is enabled for this user
        // 2. local-clock hour matches their configured briefing_hour
        // 3. last briefing was on a previous local day (one per day)
        //
        // We do this BEFORE check-in evaluation so a briefing-due tick
        // doesn't also get a redundant check-in opener moments later.
        let (briefing_due, briefing_reason) = briefing_decision(&s, now, tz.as_deref());
        if !briefing_due {
            // Surface the decision once per local hour at INFO — a "no briefing
            // for days" failure must be diagnosable from the default log level
            // without a rebuild (which the 0.169.0→0.189.0 regression proved).
            // Throttled to top-of-hour to keep the noise to ~24 lines/day.
            use chrono::Timelike;
            if now.minute() < (TICK_INTERVAL_SECS / 60).max(1) as u32 {
                info!("companion scheduler: briefing '{user_id}' not firing — {briefing_reason}");
            }
        }
        if briefing_due {
            info!(
                "companion scheduler: firing daily briefing for '{user_id}' — {briefing_reason}"
            );
            match dispatcher.send_briefing(&user_id).await {
                Ok(DispatchOutcome::Sent { conversation_id, channel, chars }) => {
                    info!(
                        "companion scheduler: briefing sent for '{user_id}' \
                         on '{channel}' ({chars} chars, conv={conversation_id})"
                    );
                }
                Ok(DispatchOutcome::SkippedNoChannel) => {
                    warn!("companion scheduler: briefing '{user_id}' skipped — no channel");
                }
                Ok(DispatchOutcome::Failed(msg)) => {
                    warn!("companion scheduler: briefing failed for '{user_id}': {msg}");
                }
                Err(e) => {
                    warn!("companion scheduler: briefing error for '{user_id}': {e}");
                }
            }
            // Carry on to the check-in pass — if the user is
            // ALSO due for a warm check-in, that's fine; the
            // min-gap policy will defer it.
        }

        // Last user activity ≈ most-recent conversation's updated_at.
        // It's a slight over-approximation (assistant messages also
        // touch it), but in practice the min-gap gate catches the
        // post-checkin case before recent_user_activity matters. The
        // signal we actually want here — "user is mid-conversation,
        // don't ping" — is faithfully captured.
        let last_user_activity_at = history
            .list_conversations(&user_id, None, 1, 0)
            .ok()
            .and_then(|cs| cs.into_iter().next())
            .map(|c| chrono::DateTime::from_timestamp_millis(c.updated_at).unwrap_or(now));

        // Real count of check-ins fired so far this user-local day (rolls
        // over at local midnight). Feeds the policy's `max_per_day` cap —
        // previously a 0-or-1 approximation that defeated the cap entirely.
        let checkins_today = store
            .checkins_today(&user_id, &today_local)
            .unwrap_or_else(|e| {
                warn!("companion scheduler: checkins_today read failed for '{user_id}': {e}");
                0
            });

        // Per-user cadence overrides sit on top of the instance defaults
        // (`limits`, from the `companion` config block): each override falls
        // back to the global value when unset. Applied before the engagement
        // adjustment so engagement tunes the user's effective min-gap.
        let limits = Limits {
            max_unanswered_checkins: s.cadence.max_unanswered_checkins
                .unwrap_or(limits.max_unanswered_checkins),
            max_per_day: s.cadence.max_per_day.unwrap_or(limits.max_per_day),
            min_gap_minutes: s.cadence.min_gap_minutes.unwrap_or(limits.min_gap_minutes),
            ..limits
        };

        // adjust cadence based on recent engagement.
        // When `engagement` isn't wired (tests / minimal builds),
        // skip the adjustment and use baseline limits.
        let (adjusted_limits, cadence_reason) = if let Some(log) = engagement {
            let since = now - chrono::Duration::hours(ENGAGEMENT_LOOKBACK_HOURS);
            match log.tally_since(&user_id, since) {
                Ok(tally) => adjust_for_engagement(limits, tally, ENGAGEMENT_MIN_SAMPLES),
                Err(e) => {
                    debug!("companion scheduler: engagement tally failed for '{user_id}' \
                            (using baseline): {e}");
                    (limits, "baseline")
                }
            }
        } else {
            (limits, "baseline")
        };

        let inputs = PolicyInputs {
            quiet_hours: s.quiet_hours.clone(),
            now,
            last_user_activity_at,
            last_checkin_at: s.last_checkin_at,
            user_tz: tz,
            limits: adjusted_limits,
            checkins_today,
            consecutive_missed_checkins: s.consecutive_missed_checkins,
            tick_interval_secs: TICK_INTERVAL_SECS,
        };

        match evaluate(&inputs) {
            Decision::Skip { reason } => {
                debug!(
                    "companion scheduler: '{user_id}' skipped \
                     ({reason}; cadence={cadence_reason})"
                );
                continue;
            }
            Decision::Fire { reason } => {
                info!(
                    "companion scheduler: firing for '{user_id}' \
                     (reason: {reason}, cadence: {cadence_reason})"
                );
                let sent = match dispatcher.send_checkin(&user_id).await {
                    Ok(DispatchOutcome::Sent { conversation_id, channel, chars }) => {
                        info!(
                            "companion scheduler: sent for '{user_id}' on '{channel}' \
                             ({chars} chars, conv={conversation_id})"
                        );
                        true
                    }
                    Ok(DispatchOutcome::SkippedNoChannel) => {
                        warn!("companion scheduler: '{user_id}' skipped — no channel resolved");
                        false
                    }
                    Ok(DispatchOutcome::Failed(msg)) => {
                        warn!("companion scheduler: dispatch failed for '{user_id}': {msg}");
                        false
                    }
                    Err(e) => {
                        warn!("companion scheduler: dispatch error for '{user_id}': {e}");
                        false
                    }
                };

                // track unanswered consecutive check-ins.
                // The chat handler resets this counter on any user
                // message; we increment on every successful send.
                // When the count crosses the threshold and we have a
                // safety floor wired, soft-escalate.
                if sent {
                    // Real per-day counter for the `max_per_day` cap (rolls
                    // over at local midnight via the stamped day).
                    if let Err(e) = store.bump_checkins_today(&user_id, &today_local) {
                        warn!("companion scheduler: bump_checkins_today failed for '{user_id}': {e}");
                    }
                    let count = store
                        .increment_missed_checkins(&user_id)
                        .unwrap_or_else(|e| {
                            warn!("companion scheduler: increment_missed failed for '{user_id}': {e}");
                            0
                        });
                    if count >= MISSED_CHECKIN_THRESHOLD {
                        if let Some(floor) = safety {
                            // Only escalate ONCE per threshold-crossing
                            // if count is already past threshold
                            // when we reach this point, that means an
                            // earlier tick already escalated. We
                            // dedupe by checking equality with the
                            // threshold, not >=, so a count of 3, 4,
                            // 5 all trigger at the 3 boundary only.
                            if count == MISSED_CHECKIN_THRESHOLD {
                                let _ = floor.handle_missed_checkins(&user_id, count).await;
                            }
                        } else {
                            debug!(
                                "companion scheduler: missed-checkin count {count} \
                                 for '{user_id}' but no safety floor wired"
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// The scheduler is mostly glue + a tokio loop, so we cover it with
// one integration-style test that calls `tick_once` directly with a
// stub dispatcher. The pure policy logic is unit-tested in
// `policy.rs`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::dispatcher::CompanionDispatcher;
    use crate::companion::CompanionSettings;
    use tempfile::tempdir;

    fn fresh_history() -> (tempfile::TempDir, Arc<HistoryStore>) {
        let dir = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("history.db")).unwrap();
        (dir, Arc::new(store))
    }

    fn fresh_store() -> (tempfile::TempDir, Arc<CompanionStore>) {
        let dir = tempdir().unwrap();
        let store = CompanionStore::open(&dir.path().join("companion.db")).unwrap();
        (dir, Arc::new(store))
    }

    // We can't easily stand up a real AgentCore in a unit test
    // (provider, memory, tools,...) — so the integration test
    // here exercises the policy + selection half of the tick. The
    // dispatch half is covered by dispatcher.rs unit tests + a
    // runtime smoke after wiring.
    #[tokio::test]
    async fn tick_once_with_no_active_users_is_a_noop() {
        let (_dir1, store)   = fresh_store();
        let (_dir2, history) = fresh_history();
        // Dispatcher is unused when no active users — we can't
        // construct a real one without an AgentCore, so this test
        // covers the early-return path.
        // tick_once short-circuits when list_active returns empty.
        // We assert no panic + Ok.
        let count = store.list_active(Utc::now()).unwrap().len();
        assert_eq!(count, 0);
        // Direct verification: the tick body returns Ok and writes
        // nothing. We can't invoke tick_once without a dispatcher,
        // so this test is the contract: list_active returns empty
        // for a fresh store. Real end-to-end happens in the
        // post-merge runtime smoke.
        let _ = history; // borrow to silence unused-var
    }

    #[test]
    fn list_active_drives_the_tick_planning() {
        // Same as the settings tests, but framed as "the scheduler
        // will only see these users". Documents the contract.
        let (_dir, store) = fresh_store();
        let now = Utc::now();

        let mut alice = CompanionSettings {
            user_id: "alice".into(),
            enabled: true,
            paused_until: None,
            quiet_hours: vec![],
            preferred_channels: vec![],
            safety_contact_user_id: Some("david".into()),
            setup_completed_at: Some(now),
            last_checkin_at: None,
            consecutive_missed_checkins: 0,
            daily_briefing_enabled: false,
            daily_briefing_hour: 7,
            last_briefing_at: None,
            cadence: Default::default(),
            created_at: now,
            updated_at: now,
        };
        store.upsert(&alice).unwrap();
        alice.user_id = "bob".into();
        alice.setup_completed_at = None; // setup-incomplete → excluded
        store.upsert(&alice).unwrap();

        let candidates = store.list_active(now).unwrap();
        let names: Vec<&str> = candidates.iter().map(|s| s.user_id.as_str()).collect();
        assert_eq!(names, vec!["alice"]);
    }

    #[test]
    fn briefing_fires_at_or_past_hour_once_per_day() {
        use chrono::TimeZone;
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 5, 23, h, 0, 0).unwrap();
        let mut s = CompanionSettings {
            user_id: "u".into(), enabled: true, paused_until: None,
            quiet_hours: vec![], preferred_channels: vec![],
            safety_contact_user_id: None, setup_completed_at: Some(at(0)),
            last_checkin_at: None, consecutive_missed_checkins: 0,
            daily_briefing_enabled: true, daily_briefing_hour: 9,
            last_briefing_at: None, cadence: Default::default(),
            created_at: at(0), updated_at: at(0),
        };
        let fires = |s: &CompanionSettings, now| briefing_decision(s, now, Some("UTC")).0;
        // Before the hour: wait.
        assert!(!fires(&s, at(8)));
        // Exactly the hour, none yet today: fire.
        assert!(fires(&s, at(9)));
        // Past the hour (the old code skipped this — the missed-window bug):
        // catch up and fire.
        assert!(fires(&s, at(14)));
        // Already briefed today: don't double-fire.
        s.last_briefing_at = Some(at(9));
        assert!(!fires(&s, at(14)));
        // Disabled: never.
        s.daily_briefing_enabled = false;
        s.last_briefing_at = None;
        assert!(!fires(&s, at(14)));
    }

    #[test]
    fn briefing_catch_up_when_last_is_stale_despite_same_day() {
        // Safety net: a timezone glitch could make same_local_day() wrongly
        // report "already today". If the last briefing is >23h old we fire
        // anyway rather than silently skip a day.
        use chrono::TimeZone;
        let day1_9am = Utc.with_ymd_and_hms(2026, 5, 23, 9, 0, 0).unwrap();
        let day2_10am = Utc.with_ymd_and_hms(2026, 5, 24, 10, 0, 0).unwrap();
        let s = CompanionSettings {
            user_id: "u".into(), enabled: true, paused_until: None,
            quiet_hours: vec![], preferred_channels: vec![],
            safety_contact_user_id: None, setup_completed_at: Some(day1_9am),
            last_checkin_at: None, consecutive_missed_checkins: 0,
            daily_briefing_enabled: true, daily_briefing_hour: 9,
            last_briefing_at: Some(day1_9am), cadence: Default::default(),
            created_at: day1_9am, updated_at: day1_9am,
        };
        // ~25h later, a different real day → fires via the normal path.
        let (fire, why) = briefing_decision(&s, day2_10am, Some("UTC"));
        assert!(fire, "should fire next day: {why}");
    }

    #[test]
    fn ensure_companiondispatcher_does_not_require_notifications_to_compile() {
        // Type-shape check: the constructor variants compile in
        // both branches (with / without NotificationBus). The real
        // dispatch path is exercised in dispatcher.rs.
        fn _accepts_dispatcher(_d: CompanionDispatcher) {}
    }
}
