// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/policy.rs
//! Pure decision logic for "should the scheduler fire a check-in for
//! this user *right now*?".
//!
//! All inputs come in as plain data (no I/O, no DB calls), so the
//! function is straight-line and unit-testable. The scheduler
//! (`scheduler.rs`) does the I/O — fetches settings + recent activity
//! + last check-in time + the user's timezone, then asks `evaluate`
//! for a decision.
//!
//! Per the locked design (proposal §11.5): learned + rule-based +
//! deliberately variant. v1 has the rule-based half; the learning
//! half lands in  when the engagement assessor is in place.
//! Variance comes from the `jitter` and "windows shouldn't be on the
//! dot" semantics applied here.

use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::companion::engagement_log::EngagementTally;

// Result of a single policy evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    // Fire a check-in now.
    Fire {
        // One-word reason, useful for logs / tests
        // (`"window"`, `"first_run"`).
        reason: &'static str,
    },
    // Don't fire. `reason` is a short human-readable string the
    // scheduler logs at debug-level — not surfaced to the user.
    Skip {
        reason: &'static str,
    },
}

impl Decision {
    pub fn is_fire(&self) -> bool { matches!(self, Decision::Fire { .. }) }
    pub fn reason(&self) -> &'static str {
        match self {
            Decision::Fire { reason } => reason,
            Decision::Skip { reason } => reason,
        }
    }
}

// Inputs to the policy. Pass-by-value because the scheduler's call
// path doesn't need to keep these alive longer than the decision.
#[derive(Debug, Clone)]
pub struct PolicyInputs {
    // Quiet-hours windows. Each is `("HH:MM", "HH:MM")` interpreted in
    // the user's timezone. A window that wraps midnight
    // (e.g. `("22:00", "06:30")`) is supported.
    pub quiet_hours: Vec<(String, String)>,
    // "Now" in UTC. The scheduler typically passes `Utc::now()` but
    // tests pass deterministic values.
    pub now: DateTime<Utc>,
    // Last time the user sent a message in any channel (any
    // conversation). `None` = no prior activity recorded.
    pub last_user_activity_at: Option<DateTime<Utc>>,
    // Last time the companion fired a check-in for this user. `None`
    // = never fired.
    pub last_checkin_at: Option<DateTime<Utc>>,
    // User's IANA timezone (e.g. `"Australia/Sydney"`). Falls back to
    // UTC if missing/unrecognised — quiet hours still parse, just
    // against UTC.
    pub user_tz: Option<String>,
    // Caller-supplied limits. Default bounds in the design proposal
    // land here. Letting the scheduler pass them in means we don't
    // hard-code them.
    pub limits: Limits,
    // Number of check-ins fired for this user since the start of
    // "today" (in the user's tz). Pre-counted by the scheduler so
    // the policy stays a pure function.
    pub checkins_today: u32,
    // Consecutive check-ins fired since the user last replied. Reset to
    // 0 on any user message. Drives the unanswered-cap gate so
    // stops talking into the void when the user isn't responding.
    pub consecutive_missed_checkins: u32,
    // Window the scheduler ticks at. The "variant cadence" jitter
    // adds randomness within ±jitter_seconds of the natural fire
    // time, capped at half this value so we can't drift past the
    // next tick.
    pub tick_interval_secs: u64,
}

// Configurable bounds. Defaults match `design-docs/companion/design-proposal.md`.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    // Minimum minutes between consecutive check-ins. Hard floor.
    pub min_gap_minutes: i64,
    // Maximum check-ins per day (user-local day).
    pub max_per_day: u32,
    // Maximum consecutive check-ins to send without a user reply before
    // pausing proactive check-ins (the counter resets on any user
    // message, so they auto-resume on the next reply). `0` disables the
    // cap. Configurable via `companion.max_unanswered_checkins`.
    pub max_unanswered_checkins: u32,
    // If the user has spoken in the last N minutes, don't fire —
    // no one wants a check-in on top of a real conversation.
    pub skip_if_recent_user_activity_minutes: i64,
    // Jitter applied within a tick, in seconds. The scheduler may
    // use it to delay the fire-trigger so check-ins don't land on
    // the same second every tick.
    pub jitter_seconds: i64,
    // Default quiet hours used when the user hasn't set any.
    // `("22:00", "07:00")` in design proposal — keeps  from
    // firing at 3am on day one before the user has configured
    // anything.
    pub default_quiet_hours: (&'static str, &'static str),
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min_gap_minutes: 90,
            max_per_day: 6,
            max_unanswered_checkins: 3,
            skip_if_recent_user_activity_minutes: 60,
            jitter_seconds: 600, // 10 minutes
            default_quiet_hours: ("22:00", "07:00"),
        }
    }
}

// The core decision function. Pure: no I/O, no clock reads beyond
// what's already in `inputs.now`.
// // Order of evaluation matters — the first failing check decides. We
// check cheap-and-final reasons before expensive ones:
// // 1. **Recent activity** (cheap) → Skip.
// 2. **Min-gap** (cheap, frequency floor).
// 3. **Daily cap** (cheap, frequency ceiling).
// 4. **Quiet hours** (slightly more work — tz + window math).
// 5. Otherwise → Fire.
pub fn evaluate(inputs: &PolicyInputs) -> Decision {
    // 1. Recent activity
    if let Some(last_act) = inputs.last_user_activity_at {
        let mins_since = (inputs.now - last_act).num_minutes();
        if mins_since >= 0 && mins_since < inputs.limits.skip_if_recent_user_activity_minutes {
            return Decision::Skip { reason: "recent_user_activity" };
        }
    }

    // 2. Min-gap since the last check-in
    if let Some(last_ck) = inputs.last_checkin_at {
        let mins_since = (inputs.now - last_ck).num_minutes();
        if mins_since >= 0 && mins_since < inputs.limits.min_gap_minutes {
            return Decision::Skip { reason: "min_gap" };
        }
    }

    // 3. Daily cap
    if inputs.checkins_today >= inputs.limits.max_per_day {
        return Decision::Skip { reason: "daily_cap" };
    }

    // 3b. Unanswered cap — stop sending once the user has ignored
    // `max_unanswered_checkins` in a row. The counter resets on any user
    // message, so check-ins auto-resume the moment they reply. This is the
    // "don't talk into the void" guard; `0` disables it.
    if inputs.limits.max_unanswered_checkins > 0
        && inputs.consecutive_missed_checkins >= inputs.limits.max_unanswered_checkins
    {
        return Decision::Skip { reason: "unanswered_cap" };
    }

    // 4. Quiet hours — interpret the windows in the user's tz; fall
    //  back to UTC if the tz name doesn't parse. When the user
    //  hasn't configured any quiet hours, use the default so we
    //  never fire at 3am.
    let tz = parse_tz(inputs.user_tz.as_deref());
    let local_now = tz.from_utc_datetime(&inputs.now.naive_utc()).time();

    let windows: Vec<(NaiveTime, NaiveTime)> = if inputs.quiet_hours.is_empty() {
        // Apply the configured-by-default window so a fresh user
        // doesn't get woken at 03:00 before they've set anything.
        parse_windows(&[(
            inputs.limits.default_quiet_hours.0.to_string(),
            inputs.limits.default_quiet_hours.1.to_string(),
        )])
    } else {
        parse_windows(&inputs.quiet_hours)
    };

    for (start, end) in &windows {
        if in_window(local_now, *start, *end) {
            return Decision::Skip { reason: "quiet_hours" };
        }
    }

    // 5. Clear to fire. If this is the user's first-ever check-in
    //  note that in the reason so the log is informative.
    let reason = if inputs.last_checkin_at.is_none() { "first_run" } else { "window" };
    Decision::Fire { reason }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_tz(name: Option<&str>) -> Tz {
    name.and_then(|n| n.parse::<Tz>().ok()).unwrap_or(chrono_tz::UTC)
}

fn parse_windows(raw: &[(String, String)]) -> Vec<(NaiveTime, NaiveTime)> {
    raw.iter()
        .filter_map(|(a, b)| {
            let s = NaiveTime::parse_from_str(a, "%H:%M").ok()?;
            let e = NaiveTime::parse_from_str(b, "%H:%M").ok()?;
            Some((s, e))
        })
        .collect()
}

// Is `t` inside `[start, end)`? Supports wrap-around windows
// (`22:00`–`06:30` means 22:00 through 06:29:59).
fn in_window(t: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start <= end {
        t >= start && t < end
    } else {
        // Wraps midnight: "before end" OR "at-or-after start".
        t >= start || t < end
    }
}

// Count check-ins fired "today" in the user's tz. Pulled out as a
// helper so the scheduler can compute it deterministically from
// stored `last_checkin_at` timestamps. (For  we don't yet
// keep a per-day count; the scheduler approximates by checking
// whether the LAST checkin was on the same local-day.  may
// promote this to a real counter if needed.)
pub fn same_local_day(a: DateTime<Utc>, b: DateTime<Utc>, tz_name: Option<&str>) -> bool {
    let tz = parse_tz(tz_name);
    let la = tz.from_utc_datetime(&a.naive_utc());
    let lb = tz.from_utc_datetime(&b.naive_utc());
    la.date_naive() == lb.date_naive()
}

// Compute a jittered delay (seconds) before actually firing a
// scheduled check-in. The scheduler can `tokio::time::sleep` this
// amount to scatter the fire times so they don't always land on the
// tick boundary. Pseudo-random but seeded off `user_id + now` so
// it's reproducible per-user-per-tick (handy for tests).
pub fn jitter_for(user_id: &str, now: DateTime<Utc>, jitter_seconds: i64) -> Duration {
    if jitter_seconds <= 0 { return Duration::seconds(0); }
    // Simple FNV-1a 64-bit hash; cheap and stable across builds.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in user_id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let bucket = (now.timestamp() / 60) as u64; // minute bucket
    h ^= bucket;
    h = h.wrapping_mul(0x100000001b3);
    let secs = (h % (jitter_seconds as u64 * 2 + 1)) as i64 - jitter_seconds;
    Duration::seconds(secs.abs())  // forward jitter only; pure rule-based avoids overshooting next tick
}

// Day-of-week as a string for log/UI. Not used by `evaluate` itself
// exposed because future slices' engagement window-picking will
// want it.
pub fn local_weekday(now: DateTime<Utc>, tz_name: Option<&str>) -> Weekday {
    let tz = parse_tz(tz_name);
    tz.from_utc_datetime(&now.naive_utc()).weekday()
}

// ── Cadence adjustment ────────────────────────────────────────────

// Bounds for the adjusted min_gap. Floors at the baseline so a
// briefly-disengaged stretch can never make us spammier; ceiling
// caps how long we'll back off (12 hours = at least 1 check-in/day).
const ADJ_MIN_GAP_FLOOR_MINUTES: i64 = 90;
const ADJ_MIN_GAP_CEILING_MINUTES: i64 = 12 * 60;

// Adjust the policy [`Limits`] based on recent engagement.
// // Pure function — the scheduler computes the tally from the
// engagement log and passes it in. Currently we adjust only
// `min_gap_minutes`:
// // - `disengaged_fraction < 0.4` → no change (engaged enough).
// - `0.4 ≤ disengaged_fraction < 0.7` → 1.5× the baseline gap.
// - `disengaged_fraction ≥ 0.7` → 3× the baseline gap.
// // The thresholds aren't sacred — they encode the intuition that a
// few brief replies in a row shouldn't drastically change cadence,
// but a sustained pattern of brevity / declines should slow us
// down. Bounded by `ADJ_MIN_GAP_CEILING_MINUTES` so we can't fall
// asleep entirely.
// // Returns the adjusted `Limits` plus a one-word reason string for
// log lines: `"baseline"`, `"slow_1_5x"`, `"slow_3x"`. The reason
// is `&'static str` so the scheduler can log it without
// allocations.
// `min_samples`: only apply adjustment when at least this many
// labelled turns are in the tally. Below the floor we don't have
// a signal to act on yet — keep the baseline.
pub fn adjust_for_engagement(
    base: Limits,
    tally: EngagementTally,
    min_samples: u32,
) -> (Limits, &'static str) {
    if tally.total() < min_samples {
        return (base, "baseline");
    }
    let frac = tally.disengaged_fraction();
    let (multiplier, reason): (f32, &'static str) = if frac >= 0.7 {
        (3.0, "slow_3x")
    } else if frac >= 0.4 {
        (1.5, "slow_1_5x")
    } else {
        (1.0, "baseline")
    };

    let adjusted_gap = ((base.min_gap_minutes as f32 * multiplier) as i64)
        .max(ADJ_MIN_GAP_FLOOR_MINUTES)
        .min(ADJ_MIN_GAP_CEILING_MINUTES);

    let out = Limits { min_gap_minutes: adjusted_gap, ..base };
    (out, reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn inputs_default() -> PolicyInputs {
        PolicyInputs {
            quiet_hours: vec![],
            now: Utc.with_ymd_and_hms(2026, 5, 14, 14, 0, 0).unwrap(), // 2pm UTC, Wed
            last_user_activity_at: None,
            last_checkin_at: None,
            user_tz: Some("UTC".to_string()),
            limits: Limits::default(),
            checkins_today: 0,
            consecutive_missed_checkins: 0,
            tick_interval_secs: 60,
        }
    }

    #[test]
    fn skips_when_unanswered_cap_reached() {
        let mut i = inputs_default();
        // 3 consecutive unanswered (default cap = 3) → pause.
        i.consecutive_missed_checkins = 3;
        let d = evaluate(&i);
        assert!(!d.is_fire());
        assert_eq!(d.reason(), "unanswered_cap");
    }

    #[test]
    fn fires_below_unanswered_cap() {
        let mut i = inputs_default();
        i.consecutive_missed_checkins = 2; // below the cap of 3
        assert!(evaluate(&i).is_fire());
    }

    #[test]
    fn unanswered_cap_zero_disables_the_gate() {
        let mut i = inputs_default();
        i.limits.max_unanswered_checkins = 0; // disabled
        i.consecutive_missed_checkins = 99;
        assert!(evaluate(&i).is_fire(), "cap=0 must not gate");
    }

    #[test]
    fn fires_when_no_constraints_match() {
        let d = evaluate(&inputs_default());
        assert!(d.is_fire());
        assert_eq!(d.reason(), "first_run");
    }

    #[test]
    fn marks_subsequent_fires_as_window_not_first_run() {
        let mut i = inputs_default();
        i.last_checkin_at = Some(i.now - Duration::hours(3));
        let d = evaluate(&i);
        assert!(d.is_fire());
        assert_eq!(d.reason(), "window");
    }

    #[test]
    fn skips_when_user_active_recently() {
        let mut i = inputs_default();
        i.last_user_activity_at = Some(i.now - Duration::minutes(10));
        let d = evaluate(&i);
        assert!(!d.is_fire());
        assert_eq!(d.reason(), "recent_user_activity");
    }

    #[test]
    fn fires_when_user_activity_is_old_enough() {
        let mut i = inputs_default();
        i.last_user_activity_at = Some(i.now - Duration::hours(2));
        assert!(evaluate(&i).is_fire());
    }

    #[test]
    fn skips_when_within_min_gap_since_last_checkin() {
        let mut i = inputs_default();
        i.last_checkin_at = Some(i.now - Duration::minutes(30)); // < 90
        assert_eq!(evaluate(&i).reason(), "min_gap");
    }

    #[test]
    fn skips_when_daily_cap_reached() {
        let mut i = inputs_default();
        i.checkins_today = i.limits.max_per_day;
        assert_eq!(evaluate(&i).reason(), "daily_cap");
    }

    #[test]
    fn skips_during_default_quiet_hours_when_user_set_none() {
        // 3am UTC → 03:00, within default 22:00–07:00
        let mut i = inputs_default();
        i.now = Utc.with_ymd_and_hms(2026, 5, 14, 3, 0, 0).unwrap();
        assert_eq!(evaluate(&i).reason(), "quiet_hours");
    }

    #[test]
    fn skips_during_user_configured_quiet_hours() {
        let mut i = inputs_default();
        i.quiet_hours = vec![("13:00".into(), "15:00".into())]; // covers 14:00
        assert_eq!(evaluate(&i).reason(), "quiet_hours");
    }

    #[test]
    fn user_quiet_hours_override_default() {
        // User explicitly says no quiet hours mid-afternoon — but
        // default would still cover 03:00 if we fell through. Here,
        // user set quiet hours that DON'T cover 14:00 → fire allowed.
        let mut i = inputs_default();
        i.quiet_hours = vec![("22:00".into(), "06:00".into())]; // doesn't include 14:00
        assert!(evaluate(&i).is_fire());
    }

    #[test]
    fn wrap_around_quiet_window_is_respected() {
        let mut i = inputs_default();
        i.quiet_hours = vec![("22:00".into(), "06:30".into())];
        i.now = Utc.with_ymd_and_hms(2026, 5, 14, 23, 30, 0).unwrap();
        assert_eq!(evaluate(&i).reason(), "quiet_hours");
        i.now = Utc.with_ymd_and_hms(2026, 5, 14, 5, 0, 0).unwrap();
        assert_eq!(evaluate(&i).reason(), "quiet_hours");
        i.now = Utc.with_ymd_and_hms(2026, 5, 14, 7, 0, 0).unwrap();
        assert!(evaluate(&i).is_fire(), "07:00 should be outside 22-06:30");
    }

    #[test]
    fn quiet_hours_interpreted_in_user_tz() {
        // 14:00 UTC is 00:00 (midnight) in Australia/Sydney (UTC+10/+11).
        // A user with default quiet hours (22-07) in Sydney should be
        // skipped at this moment.
        let mut i = inputs_default();
        i.user_tz = Some("Australia/Sydney".to_string());
        // No quiet_hours configured → falls back to default 22-07.
        assert_eq!(evaluate(&i).reason(), "quiet_hours");
    }

    #[test]
    fn invalid_tz_falls_back_to_utc() {
        let mut i = inputs_default();
        i.user_tz = Some("Not/A/Zone".to_string());
        // Default quiet hours 22-07 in UTC; 14:00 is fine.
        assert!(evaluate(&i).is_fire());
    }

    #[test]
    fn malformed_quiet_window_is_silently_ignored() {
        let mut i = inputs_default();
        i.quiet_hours = vec![("bogus".into(), "also-bogus".into())];
        // Should fall through to "fire" — invalid windows are dropped.
        assert!(evaluate(&i).is_fire());
    }

    #[test]
    fn ordering_recent_activity_beats_quiet_hours() {
        // Both apply; we report the first one for log clarity.
        let mut i = inputs_default();
        i.now = Utc.with_ymd_and_hms(2026, 5, 14, 3, 0, 0).unwrap(); // quiet hours
        i.last_user_activity_at = Some(i.now - Duration::minutes(5)); // recent
        assert_eq!(evaluate(&i).reason(), "recent_user_activity");
    }

    #[test]
    fn ordering_recent_activity_beats_min_gap() {
        let mut i = inputs_default();
        i.last_user_activity_at = Some(i.now - Duration::minutes(5));
        i.last_checkin_at = Some(i.now - Duration::minutes(15));
        assert_eq!(evaluate(&i).reason(), "recent_user_activity");
    }

    #[test]
    fn ordering_min_gap_beats_daily_cap() {
        let mut i = inputs_default();
        i.last_checkin_at = Some(i.now - Duration::minutes(15));
        i.checkins_today = i.limits.max_per_day;
        assert_eq!(evaluate(&i).reason(), "min_gap");
    }

    #[test]
    fn same_local_day_is_tz_aware() {
        // Two timestamps that are on different UTC days but the same
        // Sydney day: 23:00 UTC May 13 → 09:00 Sydney May 14;
        // 01:00 UTC May 14 → 11:00 Sydney May 14. Same Sydney day,
        // different UTC days.
        let a = Utc.with_ymd_and_hms(2026, 5, 13, 23, 0, 0).unwrap();
        let b = Utc.with_ymd_and_hms(2026, 5, 14, 1, 0, 0).unwrap();
        assert!(same_local_day(a, b, Some("Australia/Sydney")));
        assert!(!same_local_day(a, b, Some("UTC")),
            "different UTC days — should be different");

        // And the converse: two timestamps on the same UTC day but
        // different Sydney days. 13:00 UTC May 13 = 23:00 Sydney May 13;
        // 15:00 UTC May 13 = 01:00 Sydney May 14. Same UTC day,
        // different Sydney days.
        let c = Utc.with_ymd_and_hms(2026, 5, 13, 13, 0, 0).unwrap();
        let d = Utc.with_ymd_and_hms(2026, 5, 13, 15, 0, 0).unwrap();
        assert!(same_local_day(c, d, Some("UTC")));
        assert!(!same_local_day(c, d, Some("Australia/Sydney")),
            "crosses Sydney midnight — different local days");
    }

    #[test]
    fn jitter_is_deterministic_per_user_and_minute() {
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 14, 0, 0).unwrap();
        let j1 = jitter_for("alice", now, 600);
        let j2 = jitter_for("alice", now, 600);
        assert_eq!(j1, j2, "same user + same minute → same jitter");
        let j3 = jitter_for("bob", now, 600);
        // Almost certainly differs, but not guaranteed; just check bounds.
        assert!(j1.num_seconds() <= 600);
        assert!(j3.num_seconds() <= 600);
    }

    #[test]
    fn jitter_zero_yields_zero() {
        let now = Utc::now();
        let j = jitter_for("alice", now, 0);
        assert_eq!(j.num_seconds(), 0);
    }

    // ── Cadence adjustment ────────────────────────────────────────

    #[test]
    fn cadence_unchanged_below_min_samples() {
        let base = Limits::default();
        let tally = EngagementTally { engaged: 1, brief: 0, declined: 0, distressed: 0 };
        let (out, reason) = adjust_for_engagement(base, tally, 5);
        assert_eq!(out.min_gap_minutes, base.min_gap_minutes);
        assert_eq!(reason, "baseline");
    }

    #[test]
    fn cadence_unchanged_when_engaged() {
        let base = Limits::default();
        // 5 engaged, 0 disengaged → fraction 0 → baseline.
        let tally = EngagementTally { engaged: 5, brief: 0, declined: 0, distressed: 0 };
        let (out, reason) = adjust_for_engagement(base, tally, 3);
        assert_eq!(out.min_gap_minutes, base.min_gap_minutes);
        assert_eq!(reason, "baseline");
    }

    #[test]
    fn cadence_slows_15x_at_mid_disengagement() {
        let base = Limits::default();
        // 2 engaged, 3 brief → fraction 0.6 → 1.5x.
        let tally = EngagementTally { engaged: 2, brief: 3, declined: 0, distressed: 0 };
        let (out, reason) = adjust_for_engagement(base, tally, 3);
        assert_eq!(out.min_gap_minutes, (base.min_gap_minutes as f32 * 1.5) as i64);
        assert_eq!(reason, "slow_1_5x");
    }

    #[test]
    fn cadence_slows_3x_at_high_disengagement() {
        let base = Limits::default();
        // 1 engaged, 9 disengaged → fraction 0.9 → 3x.
        let tally = EngagementTally { engaged: 1, brief: 5, declined: 4, distressed: 0 };
        let (out, reason) = adjust_for_engagement(base, tally, 3);
        // 90 * 3 = 270, well under 12h ceiling
        assert_eq!(out.min_gap_minutes, 270);
        assert_eq!(reason, "slow_3x");
    }

    #[test]
    fn cadence_respects_ceiling() {
        let base = Limits { min_gap_minutes: 600, ..Limits::default() };
        // 0/10 engaged → fraction 1.0 → would be 1800 min (30h),
        // capped at 720 (12h).
        let tally = EngagementTally { engaged: 0, brief: 10, declined: 0, distressed: 0 };
        let (out, _reason) = adjust_for_engagement(base, tally, 3);
        assert_eq!(out.min_gap_minutes, 12 * 60);
    }

    #[test]
    fn cadence_respects_floor() {
        // A baseline below the configured floor (someone overrode it)
        // gets nudged back up by the adjuster — the floor protects
        // against accidental too-spammy cadence.
        let base = Limits { min_gap_minutes: 30, ..Limits::default() };
        let tally = EngagementTally { engaged: 5, ..Default::default() };
        let (out, _reason) = adjust_for_engagement(base, tally, 3);
        assert_eq!(out.min_gap_minutes, 90);
    }

    #[test]
    fn jitter_capped_to_configured_max() {
        let now = Utc::now();
        for u in &["a", "b", "c", "d", "e", "f", "g", "h"] {
            let j = jitter_for(u, now, 60);
            assert!(j.num_seconds() <= 60, "user '{u}' jitter {:?} > 60s", j);
        }
    }
}
