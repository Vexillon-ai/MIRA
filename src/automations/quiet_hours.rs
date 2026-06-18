// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/quiet_hours.rs
//! Quiet-hours window computation.
//!
//! A schedule may carry a [`QuietHours`] window during which user-visible
//! actions (`Prompt`, `ChannelMessage`) must not fire. The window is
//! expressed in the schedule's local timezone as `HH:MM` start/end strings.
//!
//! # Semantics
//! - Half-open: `[start, end)`. End-equals-start means "always quiet" (the
//!   schedule is effectively muted) — uncommon but well-defined.
//! - Overnight windows where `end < start` (e.g. `22:00`–`07:00`) are
//!   interpreted as crossing midnight in local time.
//!
//! Worker behaviour: `is_quiet(...) == true` → action is skipped, and
//! `next_quiet_end(...)` is used to push `next_run_at` forward to one minute
//! past the window's end so the row gets re-fired immediately when quiet
//! lifts.

use chrono::{DateTime, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

use super::types::QuietHours;

/// Parse `HH:MM` (24-hour). Returns `None` for malformed input — the worker
/// treats an unparseable window as "no quiet hours" so we never silently
/// gate an entire schedule because of a typo.
fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let mut parts = s.split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() { return None; }
    NaiveTime::from_hms_opt(h, m, 0)
}

/// `true` iff the unix-second `at` (interpreted in `tz`) falls inside the
/// quiet window. Returns `false` on any parse failure so a malformed config
/// can't permanently silence a schedule.
pub fn is_quiet(qh: &QuietHours, tz: &str, at_unix: i64) -> bool {
    let Some(tz_parsed) = tz.parse::<Tz>().ok() else { return false; };
    let Some(start) = parse_hhmm(&qh.start) else { return false; };
    let Some(end)   = parse_hhmm(&qh.end)   else { return false; };

    let local: DateTime<Tz> = match Utc.timestamp_opt(at_unix, 0) {
        chrono::offset::LocalResult::Single(t) => t.with_timezone(&tz_parsed),
        _ => return false,
    };
    let now_t = NaiveTime::from_hms_opt(local.hour(), local.minute(), local.second())
        .unwrap_or(NaiveTime::MIN);

    if start == end {
        // Treat as "always quiet" — unusual but unambiguous.
        true
    } else if start < end {
        now_t >= start && now_t < end
    } else {
        // Overnight window: in-window if at-or-after start OR before end.
        now_t >= start || now_t < end
    }
}

/// Given that `at_unix` is inside the window, return the unix-second at which
/// the window ends (i.e., the first non-quiet second). Used to bump
/// `next_run_at` past the window so the worker fires the schedule as soon as
/// quiet lifts.
///
/// Returns `None` if the window can't be parsed; the caller falls back to
/// the normal `next_run_at` calculation.
pub fn quiet_end_after(qh: &QuietHours, tz: &str, at_unix: i64) -> Option<i64> {
    let tz_parsed: Tz = tz.parse().ok()?;
    let end = parse_hhmm(&qh.end)?;
    let start = parse_hhmm(&qh.start)?;

    let local = match Utc.timestamp_opt(at_unix, 0) {
        chrono::offset::LocalResult::Single(t) => t.with_timezone(&tz_parsed),
        _ => return None,
    };

    // Compose end-of-window for "today" in local time.
    let today_end = local.date_naive().and_time(end);
    // For overnight windows where end < start: if `now` is *after* start
    // (i.e. evening side), the relevant end is *tomorrow's* end. Otherwise
    // (morning side, before today's end) it's today's end.
    let target_naive = if start > end {
        let now_t = NaiveTime::from_hms_opt(local.hour(), local.minute(), local.second())
            .unwrap_or(NaiveTime::MIN);
        if now_t >= start {
            local.date_naive().succ_opt()?.and_time(end)
        } else {
            today_end
        }
    } else {
        today_end
    };

    // Localize, handle DST disambiguation by picking the "earliest" valid.
    let local_dt = match tz_parsed.from_local_datetime(&target_naive) {
        chrono::LocalResult::Single(t)        => t,
        chrono::LocalResult::Ambiguous(t, _)  => t,
        chrono::LocalResult::None             => return None,
    };
    Some(local_dt.with_timezone(&Utc).timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn unix_in_tz(tz: &str, y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        let tz: Tz = tz.parse().unwrap();
        tz.with_ymd_and_hms(y, m, d, h, min, 0).unwrap().with_timezone(&Utc).timestamp()
    }

    #[test]
    fn daytime_window_inside() {
        let qh = QuietHours { start: "09:00".into(), end: "17:00".into() };
        let at = unix_in_tz("UTC", 2026, 5, 1, 12, 0);
        assert!(is_quiet(&qh, "UTC", at));
    }

    #[test]
    fn daytime_window_outside() {
        let qh = QuietHours { start: "09:00".into(), end: "17:00".into() };
        let at = unix_in_tz("UTC", 2026, 5, 1, 8, 0);
        assert!(!is_quiet(&qh, "UTC", at));
        let at = unix_in_tz("UTC", 2026, 5, 1, 17, 0);
        assert!(!is_quiet(&qh, "UTC", at), "end is exclusive");
    }

    #[test]
    fn overnight_window_evening_side() {
        let qh = QuietHours { start: "22:00".into(), end: "07:00".into() };
        let at = unix_in_tz("UTC", 2026, 5, 1, 23, 30);
        assert!(is_quiet(&qh, "UTC", at));
    }

    #[test]
    fn overnight_window_morning_side() {
        let qh = QuietHours { start: "22:00".into(), end: "07:00".into() };
        let at = unix_in_tz("UTC", 2026, 5, 2, 3, 0);
        assert!(is_quiet(&qh, "UTC", at));
    }

    #[test]
    fn overnight_window_outside() {
        let qh = QuietHours { start: "22:00".into(), end: "07:00".into() };
        let at = unix_in_tz("UTC", 2026, 5, 1, 12, 0);
        assert!(!is_quiet(&qh, "UTC", at));
    }

    #[test]
    fn malformed_window_returns_false() {
        let qh = QuietHours { start: "nope".into(), end: "17:00".into() };
        assert!(!is_quiet(&qh, "UTC", 0));
    }

    #[test]
    fn quiet_end_after_daytime() {
        let qh = QuietHours { start: "09:00".into(), end: "17:00".into() };
        let at  = unix_in_tz("UTC", 2026, 5, 1, 12, 0);
        let out = quiet_end_after(&qh, "UTC", at).unwrap();
        let want = unix_in_tz("UTC", 2026, 5, 1, 17, 0);
        assert_eq!(out, want);
    }

    #[test]
    fn quiet_end_after_overnight_evening_to_next_morning() {
        // 23:00 → next 07:00, which is the *next* calendar day's end.
        let qh = QuietHours { start: "22:00".into(), end: "07:00".into() };
        let at  = unix_in_tz("UTC", 2026, 5, 1, 23, 0);
        let out = quiet_end_after(&qh, "UTC", at).unwrap();
        let want = unix_in_tz("UTC", 2026, 5, 2, 7, 0);
        assert_eq!(out, want);
    }

    #[test]
    fn quiet_end_after_overnight_morning_side_uses_today() {
        let qh = QuietHours { start: "22:00".into(), end: "07:00".into() };
        let at  = unix_in_tz("UTC", 2026, 5, 2, 3, 0);
        let out = quiet_end_after(&qh, "UTC", at).unwrap();
        let want = unix_in_tz("UTC", 2026, 5, 2, 7, 0);
        assert_eq!(out, want);
    }
}
