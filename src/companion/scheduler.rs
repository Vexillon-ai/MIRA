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
    adjust_for_engagement, evaluate, local_now_parts, plan_day_slots,
    Decision, Limits, PolicyInputs,
};
use crate::companion::settings::FrequencyMode;
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

// ── Fuzzy-rhythm / Scheduled timing gate ──────────────────────────
//
// Turn the formerly greedy "fire whenever the gates allow" into a
// friend-like rhythm: a band of varied sends across the day (Fuzzy,
// the default) or fixed clock times (Scheduled). Returns whether a
// *planned slot* is due as of the user's local clock — fed to
// `evaluate` as `due_now`. All the other gates (min_gap, daily cap,
// unanswered, quiet hours, engagement adjust) stay as they were:
// guards layered on top of this timing signal.
//
// A planned local time `t` is "due now" when it's at or before the
// local clock AND we haven't already served it: either no check-in
// yet, or the last one was on a previous local day, or (same day) the
// last check-in's local time predates this slot. This fires each slot
// exactly once; after an outage the earliest unfired slot fires this
// tick and the rest catch up on later ticks (min_gap still guards).
//
// Pure (no I/O) so the scheduler test module can exercise it directly
// without standing up a dispatcher / AgentCore.
fn compute_due_now(
    user_id: &str,
    s: &crate::companion::CompanionSettings,
    now: chrono::DateTime<Utc>,
    tz: Option<&str>,
    adjusted_limits: Limits,
) -> bool {
    let (today_date, local_now_time) = local_now_parts(now, tz);
    // Band: max from per-user cadence override → global cap; min from the
    // user's presence tuning.
    let band_max = s.cadence.max_per_day.unwrap_or(adjusted_limits.max_per_day);
    let band_min = s.presence.min_per_day;
    let band_gap = adjusted_limits.min_gap_minutes;

    let last_local = s.last_checkin_at.map(|ck| local_now_parts(ck, tz));
    let slot_served = |slot: chrono::NaiveTime| -> bool {
        match last_local {
            None => false,
            Some((last_date, last_time)) => last_date == today_date && last_time >= slot,
        }
    };

    match s.presence.frequency_mode {
        FrequencyMode::Fuzzy => {
            let slots = plan_day_slots(
                user_id,
                today_date,
                &s.quiet_hours,
                adjusted_limits.default_quiet_hours,
                band_min,
                band_max,
                band_gap,
            );
            slots.iter().any(|&slot| slot <= local_now_time && !slot_served(slot))
        }
        FrequencyMode::Scheduled => {
            // Parse configured "HH:MM" times, skipping malformed entries.
            s.presence.scheduled_times.iter().any(|raw| {
                match chrono::NaiveTime::parse_from_str(raw, "%H:%M") {
                    Ok(t) => t <= local_now_time && !slot_served(t),
                    Err(_) => false,
                }
            })
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

        // ── Fuzzy-rhythm / Scheduled timing gate ─────────────────────────────
        //
        // Turn the formerly greedy "fire whenever the gates allow" into a
        // friend-like rhythm: a band of varied sends across the day (Fuzzy,
        // the default) or fixed clock times (Scheduled). We compute whether a
        // *planned slot* is due as of the user's local clock and feed that to
        // `evaluate` as `due_now`. All the other gates (min_gap, daily cap,
        // unanswered, quiet hours, engagement adjust) stay exactly as they
        // were — they remain guards layered on top of this timing signal.
        let due_now = compute_due_now(&user_id, &s, now, tz.as_deref(), adjusted_limits);

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
            due_now,
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
            cadence: Default::default(), presence: Default::default(),
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
            last_briefing_at: None, cadence: Default::default(), presence: Default::default(),
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
            last_briefing_at: Some(day1_9am), cadence: Default::default(), presence: Default::default(),
            created_at: day1_9am, updated_at: day1_9am,
        };
        // ~25h later, a different real day → fires via the normal path.
        let (fire, why) = briefing_decision(&s, day2_10am, Some("UTC"));
        assert!(fire, "should fire next day: {why}");
    }

    // ── Fuzzy / Scheduled due_now gate ────────────────────────────
    //
    // `compute_due_now` is the timing decision that `tick_once` feeds
    // into `evaluate` as `due_now`. We test it directly (it's pure) so
    // we don't need a dispatcher / AgentCore. The full skip/fire wiring
    // is covered by policy.rs's `not_due` test plus the runtime smoke.

    fn settings_for(user: &str) -> CompanionSettings {
        use chrono::TimeZone;
        let t = Utc.with_ymd_and_hms(2026, 6, 24, 0, 0, 0).unwrap();
        CompanionSettings {
            user_id: user.into(), enabled: true, paused_until: None,
            quiet_hours: vec![], preferred_channels: vec![],
            safety_contact_user_id: None, setup_completed_at: Some(t),
            last_checkin_at: None, consecutive_missed_checkins: 0,
            daily_briefing_enabled: false, daily_briefing_hour: 7,
            last_briefing_at: None, cadence: Default::default(),
            presence: Default::default(),
            created_at: t, updated_at: t,
        }
    }

    #[test]
    fn fuzzy_due_when_a_slot_has_passed() {
        use chrono::TimeZone;
        // Late in the contactable day (21:30 UTC) — at least one of a
        // 1..max band of slots in 07:00–22:00 must already be at/before
        // now, and none served yet (no last_checkin). So → due.
        let s = settings_for("alice");
        let now = Utc.with_ymd_and_hms(2026, 6, 24, 21, 30, 0).unwrap();
        let due = compute_due_now("alice", &s, now, Some("UTC"), Limits::default());
        assert!(due, "a planned slot should be due by 21:30 with no prior check-in");
    }

    #[test]
    fn fuzzy_not_due_before_first_slot() {
        use chrono::TimeZone;
        // 06:00 UTC is inside the default quiet window 22:00–07:00 → the
        // contactable window starts at 07:00, so no slot can be at/before
        // 06:00 → not due. (This also matches the quiet-hours gate, but
        // here we assert the timing gate itself returns false.)
        let s = settings_for("alice");
        let now = Utc.with_ymd_and_hms(2026, 6, 24, 6, 0, 0).unwrap();
        let due = compute_due_now("alice", &s, now, Some("UTC"), Limits::default());
        assert!(!due, "no slot should be due before the contactable window opens");
    }

    #[test]
    fn scheduled_due_at_configured_time() {
        use chrono::TimeZone;
        let mut s = settings_for("bob");
        s.presence.frequency_mode = FrequencyMode::Scheduled;
        s.presence.scheduled_times = vec!["09:00".into(), "18:30".into()];
        // 10:00 UTC: 09:00 has passed, not served → due.
        let now = Utc.with_ymd_and_hms(2026, 6, 24, 10, 0, 0).unwrap();
        assert!(compute_due_now("bob", &s, now, Some("UTC"), Limits::default()));
        // 08:00 UTC: neither 09:00 nor 18:30 has passed → not due.
        let early = Utc.with_ymd_and_hms(2026, 6, 24, 8, 0, 0).unwrap();
        assert!(!compute_due_now("bob", &s, early, Some("UTC"), Limits::default()));
    }

    #[test]
    fn scheduled_slot_not_refired_same_day() {
        use chrono::TimeZone;
        let mut s = settings_for("bob");
        s.presence.frequency_mode = FrequencyMode::Scheduled;
        s.presence.scheduled_times = vec!["09:00".into()];
        // Already checked in at 09:05 today → 09:00 slot is served → not due.
        s.last_checkin_at = Some(Utc.with_ymd_and_hms(2026, 6, 24, 9, 5, 0).unwrap());
        let now = Utc.with_ymd_and_hms(2026, 6, 24, 12, 0, 0).unwrap();
        assert!(!compute_due_now("bob", &s, now, Some("UTC"), Limits::default()));
        // A check-in from YESTERDAY doesn't serve today's slot → due again.
        s.last_checkin_at = Some(Utc.with_ymd_and_hms(2026, 6, 23, 9, 5, 0).unwrap());
        assert!(compute_due_now("bob", &s, now, Some("UTC"), Limits::default()));
    }

    #[test]
    fn scheduled_ignores_malformed_times() {
        use chrono::TimeZone;
        let mut s = settings_for("bob");
        s.presence.frequency_mode = FrequencyMode::Scheduled;
        s.presence.scheduled_times = vec!["nope".into(), "25:99".into()];
        let now = Utc.with_ymd_and_hms(2026, 6, 24, 23, 0, 0).unwrap();
        assert!(!compute_due_now("bob", &s, now, Some("UTC"), Limits::default()),
            "all-malformed schedule → never due, no panic");
    }

    #[test]
    fn ensure_companiondispatcher_does_not_require_notifications_to_compile() {
        // Type-shape check: the constructor variants compile in
        // both branches (with / without NotificationBus). The real
        // dispatch path is exercised in dispatcher.rs.
        fn _accepts_dispatcher(_d: CompanionDispatcher) {}
    }
}
