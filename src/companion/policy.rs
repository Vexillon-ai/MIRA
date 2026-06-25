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

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Timelike, Utc, Weekday};
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
    // True when the fuzzy/scheduled rhythm engine says a planned slot
    // is due *right now* (computed by the scheduler from
    // `plan_day_slots` in Fuzzy mode, or the configured `scheduled_times`
    // in Scheduled mode). The gates above (recent activity, min_gap,
    // daily cap, unanswered, quiet hours) remain guards layered on top
    // of this timing signal: even a due slot is suppressed if e.g. the
    // user just messaged. `false` short-circuits to a `not_due` skip.
    pub due_now: bool,
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

    // 4b. Rhythm gate — even when every frequency guard above is
    //  satisfied, only fire when the rhythm engine says a planned
    //  slot is actually due now. This is what turns the previously
    //  greedy "fire whenever allowed" behaviour into the friend-like
    //  band of varied sends. The scheduler computes `due_now`
    //  (Fuzzy: from `plan_day_slots`; Scheduled: from configured
    //  times) so `evaluate` stays pure.
    if !inputs.due_now {
        return Decision::Skip { reason: "not_due" };
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

// Resolve `now` (UTC) into the user's local calendar date + clock
// time, given an IANA tz name (UTC fallback). The scheduler uses
// this to ask "is one of today's planned slots due as of the local
// clock?" — kept here next to `parse_tz` so the tz-resolution rule
// (unknown/missing → UTC) is identical to `evaluate`'s.
pub fn local_now_parts(now: DateTime<Utc>, tz_name: Option<&str>) -> (NaiveDate, NaiveTime) {
    let tz = parse_tz(tz_name);
    let local = tz.from_utc_datetime(&now.naive_utc());
    (local.date_naive(), local.time())
}

// ── Fuzzy rhythm: day-slot planner ────────────────────────────────

// Stable FNV-1a 64-bit hash over an arbitrary byte slice. Same
// algorithm/constants as `jitter_for` so the variance "feel" is
// consistent; pulled out here so the planner can seed multiple
// derived values off one base hash. Pure + reproducible across
// builds (no rand, no clock).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// Derive a further 64-bit pseudo-random value from a base hash and a
// counter (slot index). Mixing in the counter via the same FNV step
// gives each slot its own well-distributed offset while staying a
// pure function of (base, idx).
fn mix(base: u64, idx: u64) -> u64 {
    let mut h = base ^ idx;
    h = h.wrapping_mul(0x100000001b3);
    h ^= h >> 29; // a cheap final avalanche so low bits aren't sticky
    h
}

// Minutes-since-local-midnight for a `NaiveTime`.
fn mins_of(t: NaiveTime) -> i64 {
    (t.hour() * 60 + t.minute()) as i64
}

// Build a `NaiveTime` from minutes-since-midnight, clamped to the
// valid 0..=1439 range so arithmetic can never panic.
fn time_from_mins(m: i64) -> NaiveTime {
    let m = m.clamp(0, 24 * 60 - 1);
    NaiveTime::from_hms_opt((m / 60) as u32, (m % 60) as u32, 0)
        .unwrap_or(NaiveTime::MIN)
}

// Fallback contactable window when quiet hours can't be parsed or
// would leave essentially no daytime gap. A sane "awake" span.
const FALLBACK_CONTACTABLE: (i64, i64) = (8 * 60, 22 * 60); // 08:00–22:00

// Compute the single contiguous contactable window (start,end) in
// minutes-since-midnight as a NON-wrapping span (start < end), given
// the user's quiet hours.
//
// v1 simplification: we model ONE contiguous contactable window. The
// quiet window is the complement; for a normal midnight-wrapping
// quiet span (e.g. 22:00–07:00) the complement is the natural daytime
// block (07:00–22:00). For a non-wrapping quiet span (e.g. 13:00–
// 15:00) the complement is two pieces — we keep only the LARGER one
// (here 15:00–13:00-next-day collapses to the bigger of 00:00–13:00
// vs 15:00–24:00). Multiple quiet windows are likewise reduced to the
// largest single contactable gap. If quiet covers ~all day, or can't
// be parsed, fall back to 08:00–22:00.
fn contactable_window(
    quiet_windows: &[(String, String)],
    default_quiet: (&str, &str),
) -> (i64, i64) {
    // Resolve the effective quiet windows: user's, else the default.
    let raw: Vec<(String, String)> = if quiet_windows.is_empty() {
        vec![(default_quiet.0.to_string(), default_quiet.1.to_string())]
    } else {
        quiet_windows.to_vec()
    };
    let parsed = parse_windows(&raw);
    if parsed.is_empty() {
        return FALLBACK_CONTACTABLE;
    }

    // Mark every minute of the day as quiet (true) or free (false),
    // honouring midnight-wrap. O(1440) — trivially cheap and dodges
    // all the fiddly interval-complement edge cases.
    let mut quiet = [false; 24 * 60];
    for (s, e) in &parsed {
        for (m, slot) in quiet.iter_mut().enumerate() {
            if in_window(time_from_mins(m as i64), *s, *e) {
                *slot = true;
            }
        }
    }

    // Find the largest contiguous run of free (non-quiet) minutes.
    // We treat the day as linear 00:00..24:00 (no wrap for the free
    // run) — the largest daytime block is what we want in practice,
    // and wrapping the free side would put a send across midnight,
    // which the quiet-hours gate would mostly reject anyway.
    let mut best_start = 0i64;
    let mut best_len = 0i64;
    let mut cur_start = 0i64;
    let mut cur_len = 0i64;
    for (m, &is_quiet) in quiet.iter().enumerate() {
        if is_quiet {
            cur_len = 0;
            cur_start = m as i64 + 1;
        } else {
            if cur_len == 0 {
                cur_start = m as i64;
            }
            cur_len += 1;
            if cur_len > best_len {
                best_len = cur_len;
                best_start = cur_start;
            }
        }
    }

    // If the day is (nearly) all quiet — less than min_gap's worth of
    // free time, here a conservative 60 minutes — fall back so we
    // never produce an empty or absurd window.
    if best_len < 60 {
        return FALLBACK_CONTACTABLE;
    }
    (best_start, best_start + best_len)
}

// Pure planner: place this user's proactive sends for `local_date`
// across their contactable window at varied, deterministic-but-
// non-descript times.
//
// Determinism: every random choice (target count + per-slot offset)
// is seeded off a stable FNV-1a hash of `(seed_user_id, local_date)`,
// mirroring `jitter_for`'s hashing style. No `rand`, no clock — the
// same (user, day) always yields the same plan, but it varies across
// users and across days.
//
// Returns SORTED local `NaiveTime`s, all inside the contactable
// window, spaced ≥ `min_gap_minutes` apart, with a count inside the
// `[min_per_day, max_per_day]` band (reduced only when the window is
// too narrow to hold the band at the required spacing).
pub fn plan_day_slots(
    seed_user_id: &str,
    local_date: NaiveDate,
    quiet_windows: &[(String, String)],
    default_quiet: (&str, &str),
    min_per_day: u32,
    max_per_day: u32,
    min_gap_minutes: i64,
) -> Vec<NaiveTime> {
    // Contactable span [start,end) in minutes-since-midnight.
    let (win_start, win_end) = contactable_window(quiet_windows, default_quiet);
    let win_minutes = (win_end - win_start).max(0);
    if win_minutes <= 0 {
        return Vec::new();
    }

    // Stable base hash over (user, date). The date string gives a
    // distinct seed per day; the user id distinguishes users.
    let mut seed = String::with_capacity(seed_user_id.len() + 12);
    seed.push_str(seed_user_id);
    seed.push('|');
    seed.push_str(&local_date.to_string());
    let base = fnv1a(seed.as_bytes());

    // Target count inside the band. `lo`/`hi` are normalised so a
    // mis-ordered (min > max) pair can't underflow.
    let lo = min_per_day.min(max_per_day);
    let hi = min_per_day.max(max_per_day);
    let span = (hi - lo + 1) as u64; // ≥ 1
    let mut target = lo + (base % span) as u32;

    // Fit to min_gap: the window must hold `target` sends each
    // ≥ min_gap apart. Picks are clamped to `[win_start, win_end-1]`,
    // so the usable span is `win_minutes - 1`; with N sends the
    // tightest packing spans (N-1)*gap, giving capacity
    // `(win_minutes-1)/gap + 1`. (Using `win_minutes` here would
    // over-count by one at the exact boundary and squeeze the last
    // gap below `min_gap`.)
    if min_gap_minutes > 0 {
        let usable = (win_minutes - 1).max(0);
        let capacity = (usable / min_gap_minutes + 1) as u32;
        if target > capacity {
            target = capacity;
        }
    }
    // Never drop below 1 when the window is non-empty and the user
    // asked for at least one send a day.
    if target == 0 && min_per_day >= 1 && win_minutes > 0 {
        target = 1;
    }
    if target == 0 {
        return Vec::new();
    }

    // Place `target` times: split the window into equal segments and
    // pick a pseudo-random offset inside each so times look organic
    // (not on the hour/half-hour). Then sweep left→right enforcing
    // ≥ min_gap spacing, pushing a too-close pick later (clamped to
    // the window end). Equal segmentation keeps them spread out
    // across the whole day rather than clustering.
    let seg = win_minutes / target as i64; // ≥ 0; segment width
    let mut picks: Vec<i64> = Vec::with_capacity(target as usize);
    for i in 0..target as i64 {
        let seg_start = win_start + i * seg;
        // Last segment absorbs the remainder so we use the full span.
        let seg_end = if i == target as i64 - 1 {
            win_end
        } else {
            seg_start + seg
        };
        let seg_width = (seg_end - seg_start).max(1);
        let r = mix(base, i as u64) % seg_width as u64;
        picks.push(seg_start + r as i64);
    }
    picks.sort_unstable();

    // Enforce spacing + window bounds in a single forward sweep.
    let mut out: Vec<i64> = Vec::with_capacity(picks.len());
    let mut prev: Option<i64> = None;
    for p in picks {
        let mut m = p.clamp(win_start, win_end - 1);
        if let Some(pv) = prev {
            if m - pv < min_gap_minutes {
                m = (pv + min_gap_minutes).min(win_end - 1);
            }
        }
        // If pushing forward collided with the window end and would
        // duplicate the previous pick, drop it rather than stack two
        // sends on the same minute.
        if let Some(pv) = prev {
            if m <= pv {
                continue;
            }
        }
        prev = Some(m);
        out.push(m);
    }

    out.into_iter().map(time_from_mins).collect()
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
            due_now: true, // all existing tests assume a slot is due
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

    // ── due_now rhythm gate ───────────────────────────────────────

    #[test]
    fn not_due_skips_even_when_all_gates_pass() {
        let mut i = inputs_default();
        i.due_now = false;
        let d = evaluate(&i);
        assert!(!d.is_fire());
        assert_eq!(d.reason(), "not_due");
    }

    #[test]
    fn due_now_default_still_fires() {
        // inputs_default sets due_now: true — the unchanged baseline.
        let d = evaluate(&inputs_default());
        assert!(d.is_fire());
    }

    // ── Fuzzy rhythm: plan_day_slots ──────────────────────────────

    const DEFAULT_QUIET: (&str, &str) = ("22:00", "07:00");

    // Helper: the contactable window in minutes for the default quiet
    // hours, used to assert membership. Default quiet 22:00–07:00 →
    // contactable 07:00–22:00 (420..1320).
    fn in_default_contactable(t: NaiveTime) -> bool {
        let m = mins_of(t);
        (7 * 60..22 * 60).contains(&m)
    }

    #[test]
    fn plan_count_within_band() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 24).unwrap();
        // Band [2,5], generous window, small gap → no capacity trim.
        for u in &["alice", "bob", "carol", "dave", "erin"] {
            let slots = plan_day_slots(u, date, &[], DEFAULT_QUIET, 2, 5, 30);
            assert!(
                (2..=5).contains(&(slots.len() as u32)),
                "user '{u}': count {} outside [2,5]", slots.len()
            );
        }
    }

    #[test]
    fn plan_respects_min_gap() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 24).unwrap();
        let slots = plan_day_slots("alice", date, &[], DEFAULT_QUIET, 4, 6, 90);
        for w in slots.windows(2) {
            let gap = mins_of(w[1]) - mins_of(w[0]);
            assert!(gap >= 90, "gap {gap} < 90 between {:?} and {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn plan_all_inside_contactable_and_outside_quiet() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 24).unwrap();
        let slots = plan_day_slots("alice", date, &[], DEFAULT_QUIET, 3, 5, 30);
        for t in &slots {
            assert!(in_default_contactable(*t), "slot {:?} outside 07:00–22:00", t);
        }
        // And explicitly: none fall in the quiet window 22:00–07:00.
        let q_start = NaiveTime::from_hms_opt(22, 0, 0).unwrap();
        let q_end = NaiveTime::from_hms_opt(7, 0, 0).unwrap();
        for t in &slots {
            assert!(!in_window(*t, q_start, q_end), "slot {:?} inside quiet", t);
        }
    }

    #[test]
    fn plan_is_deterministic_for_same_user_and_date() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 24).unwrap();
        let a = plan_day_slots("alice", date, &[], DEFAULT_QUIET, 2, 5, 60);
        let b = plan_day_slots("alice", date, &[], DEFAULT_QUIET, 2, 5, 60);
        assert_eq!(a, b, "same (user,date) must be reproducible");
    }

    #[test]
    fn plan_varies_across_dates_and_users() {
        // Probabilistic: we assert the *plans* aren't all identical
        // across a handful of days/users — vanishingly unlikely to
        // collide for a correct seed mix, and we only assert bounds.
        let d1 = NaiveDate::from_ymd_opt(2026, 6, 24).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 6, 25).unwrap();
        let d3 = NaiveDate::from_ymd_opt(2026, 6, 26).unwrap();
        let plans: Vec<Vec<NaiveTime>> = [d1, d2, d3]
            .iter()
            .map(|d| plan_day_slots("alice", *d, &[], DEFAULT_QUIET, 3, 3, 30))
            .collect();
        assert!(
            !(plans[0] == plans[1] && plans[1] == plans[2]),
            "plans identical across three days — seed not varying by date"
        );
        // Across users on one day.
        let pa = plan_day_slots("alice", d1, &[], DEFAULT_QUIET, 3, 3, 30);
        let pb = plan_day_slots("zelda", d1, &[], DEFAULT_QUIET, 3, 3, 30);
        let pc = plan_day_slots("mira-user-42", d1, &[], DEFAULT_QUIET, 3, 3, 30);
        assert!(
            !(pa == pb && pb == pc),
            "plans identical across three users — seed not varying by user"
        );
    }

    #[test]
    fn plan_capacity_trims_target_to_fit_gap() {
        // Window 07:00–22:00 = 900 min; gap 300 → capacity = 900/300+1
        // = 4. Asking for [8,8] must trim to ≤ 4.
        let date = NaiveDate::from_ymd_opt(2026, 6, 24).unwrap();
        let slots = plan_day_slots("alice", date, &[], DEFAULT_QUIET, 8, 8, 300);
        assert!(slots.len() <= 4, "expected ≤4 with 300m gap, got {}", slots.len());
        assert!(!slots.is_empty(), "should still place at least one");
        for w in slots.windows(2) {
            assert!(mins_of(w[1]) - mins_of(w[0]) >= 300);
        }
    }

    #[test]
    fn plan_never_below_one_when_min_at_least_one() {
        // Extreme gap larger than the whole window: capacity formula
        // still yields ≥1, and min_per_day=1 keeps us from dropping
        // to zero.
        let date = NaiveDate::from_ymd_opt(2026, 6, 24).unwrap();
        let slots = plan_day_slots("alice", date, &[], DEFAULT_QUIET, 1, 4, 100_000);
        assert_eq!(slots.len(), 1, "min_per_day=1 must place exactly one");
    }

    #[test]
    fn plan_handles_empty_quiet_uses_default() {
        // Empty quiet → default applied → still produces a plan.
        let date = NaiveDate::from_ymd_opt(2026, 6, 24).unwrap();
        let slots = plan_day_slots("alice", date, &[], DEFAULT_QUIET, 2, 4, 60);
        assert!(!slots.is_empty());
        for t in &slots {
            assert!(in_default_contactable(*t));
        }
    }

    #[test]
    fn plan_handles_odd_quiet_without_panic() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 24).unwrap();
        // Malformed windows → fall back to 08:00–22:00, never panic.
        let bogus = vec![("nope".to_string(), "also-nope".to_string())];
        let slots = plan_day_slots("alice", date, &bogus, DEFAULT_QUIET, 2, 4, 60);
        assert!(!slots.is_empty());
        for t in &slots {
            let m = mins_of(*t);
            assert!((8 * 60..22 * 60).contains(&m), "slot {:?} outside fallback", t);
        }

        // Quiet covering ~all day → fall back, still no panic.
        let allday = vec![("00:00".to_string(), "23:59".to_string())];
        let slots = plan_day_slots("alice", date, &allday, DEFAULT_QUIET, 2, 4, 60);
        assert!(!slots.is_empty(), "all-day quiet should fall back, not empty");

        // Non-wrapping mid-day quiet (13:00–15:00) → largest gap kept,
        // no slot inside the quiet block.
        let midday = vec![("13:00".to_string(), "15:00".to_string())];
        let slots = plan_day_slots("alice", date, &midday, DEFAULT_QUIET, 3, 5, 30);
        let q_start = NaiveTime::from_hms_opt(13, 0, 0).unwrap();
        let q_end = NaiveTime::from_hms_opt(15, 0, 0).unwrap();
        for t in &slots {
            assert!(!in_window(*t, q_start, q_end), "slot {:?} inside 13–15 quiet", t);
        }
    }

    #[test]
    fn local_now_parts_resolves_tz() {
        // 14:00 UTC → 00:00 (next-day boundary) in Sydney summer; just
        // assert it doesn't panic and the time is plausible.
        let now = Utc.with_ymd_and_hms(2026, 6, 24, 14, 0, 0).unwrap();
        let (date_utc, time_utc) = local_now_parts(now, Some("UTC"));
        assert_eq!(time_utc, NaiveTime::from_hms_opt(14, 0, 0).unwrap());
        assert_eq!(date_utc, NaiveDate::from_ymd_opt(2026, 6, 24).unwrap());
        // Unknown tz → UTC fallback, same result.
        let (_d, t) = local_now_parts(now, Some("Not/A/Zone"));
        assert_eq!(t, NaiveTime::from_hms_opt(14, 0, 0).unwrap());
    }
}
