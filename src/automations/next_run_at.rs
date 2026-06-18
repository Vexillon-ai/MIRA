// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/next_run_at.rs
//! Compute the next fire time for a schedule.
//!
//! Three trigger kinds, one entry point. All times are unix seconds (UTC)
//! in storage; cron interpretation respects the schedule's IANA timezone
//! (DST included — `chrono-tz` handles the spring-forward / fall-back
//! disambiguation, and `cron`'s `Schedule::after` skips invalid local
//! times automatically).

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use std::str::FromStr;

use super::types::TriggerSpec;
use crate::MiraError;

/// Compute the next fire time for the given trigger.
///
/// **OneOff semantics**: always returns `Some(at)` — past timestamps mean
/// "fire immediately". The worker claims any row with `next_run_at <= now`,
/// runs it, then flips `status` to `expired` and clears `next_run_at`. This
/// way a one-off scheduled "for 09:00" still fires if the worker was
/// asleep until 09:30.
pub fn next_run_at(
    spec:       &TriggerSpec,
    timezone:   &str,
    after_unix: i64,
) -> Result<Option<i64>, MiraError> {
    match spec {
        TriggerSpec::OneOff { at } => Ok(Some(*at)),
        TriggerSpec::Interval { every_secs } => {
            if *every_secs == 0 {
                return Err(MiraError::ConfigError(
                    "interval schedule: every_secs must be > 0".into()
                ));
            }
            // Anchor relative to the caller's reference. Caller passes
            // `last_run_at.unwrap_or(created_at)` so the first fire happens
            // one period after creation.
            Ok(Some(after_unix.saturating_add(*every_secs as i64)))
        }
        TriggerSpec::Cron { expr } => {
            let tz: Tz = timezone.parse()
                .map_err(|_| MiraError::ConfigError(
                    format!("unknown IANA timezone: {timezone}")
                ))?;
            // The `cron` crate uses Quartz cron (6-7 fields:
            // `sec min hour dom mon dow [year]`), but most operators —
            // and most LLMs — type standard 5-field Unix cron
            // (`min hour dom mon dow`). Normalise here: a 5-field
            // expression gets an implicit `0 ` (seconds=0) prepended,
            // a 6-field expression goes through unchanged. Anything
            // else is rejected with a hint.
            let normalised = normalise_cron(expr)?;
            let cron_schedule = cron::Schedule::from_str(&normalised)
                .map_err(|e| MiraError::ConfigError(
                    format!("invalid cron expression {expr:?} \
                             (normalised to {normalised:?}): {e}")
                ))?;
            let after_utc: DateTime<Utc> = match Utc.timestamp_opt(after_unix, 0) {
                chrono::offset::LocalResult::Single(dt) => dt,
                _ => return Err(MiraError::ConfigError(
                    "after_unix is out of range for chrono".into()
                )),
            };
            let after_local = after_utc.with_timezone(&tz);
            // `Schedule::after` returns iterator of next fire times in the
            // given tz; first one is strictly after `after_local`.
            let next_local = cron_schedule.after(&after_local).next();
            Ok(next_local.map(|t| t.with_timezone(&Utc).timestamp()))
        }
    }
}

/// Promote a standard 5-field Unix cron to the 6-field Quartz form
/// the `cron` crate expects. 6 and 7 field expressions pass through.
/// Anything else is rejected with a hint at the right shape.
fn normalise_cron(expr: &str) -> Result<String, MiraError> {
    let trimmed = expr.trim();
    let field_count = trimmed.split_whitespace().count();
    match field_count {
        5 => {
            // Treat as `min hour dom mon dow` and prepend seconds=0.
            // Operators rarely want sub-minute precision and even more
            // rarely type a seconds field; defaulting to 0 matches
            // the standard-cron contract.
            Ok(format!("0 {trimmed}"))
        }
        6 | 7 => Ok(trimmed.to_string()),
        n => Err(MiraError::ConfigError(format!(
            "cron expression {expr:?} has {n} fields; expected 5 \
             (standard Unix cron `min hour dom mon dow`) or 6 \
             (Quartz with seconds: `sec min hour dom mon dow`)"
        ))),
    }
}

/// Compute the next N fire times. Used by the cron-preview API.
pub fn next_n_runs(
    spec:       &TriggerSpec,
    timezone:   &str,
    after_unix: i64,
    n:          usize,
) -> Result<Vec<i64>, MiraError> {
    let mut out = Vec::with_capacity(n);
    let mut cursor = after_unix;
    for _ in 0..n {
        match next_run_at(spec, timezone, cursor)? {
            Some(t) => { out.push(t); cursor = t; }
            None    => break,
        }
    }
    Ok(out)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn unix(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap().timestamp()
    }

    #[test]
    fn cron_normaliser_promotes_5_field_to_6_field() {
        // Standard Unix cron — works.
        assert_eq!(normalise_cron("0 9 * * *").unwrap(),  "0 0 9 * * *");
        assert_eq!(normalise_cron("30 9 * * *").unwrap(), "0 30 9 * * *");
        assert_eq!(normalise_cron("  0 9 * * 1-5  ").unwrap(), "0 0 9 * * 1-5");
        // Quartz 6-field already — passes through.
        assert_eq!(normalise_cron("0 30 9 * * *").unwrap(), "0 30 9 * * *");
        assert_eq!(normalise_cron("0 0 9 * * MON-FRI").unwrap(), "0 0 9 * * MON-FRI");
        // 7-field with year — passes through.
        assert_eq!(normalise_cron("0 0 9 * * * 2026").unwrap(), "0 0 9 * * * 2026");
        // Wrong arity — rejected with a clear message.
        assert!(normalise_cron("9 *").is_err());
        assert!(normalise_cron("").is_err());
    }

    #[test]
    fn cron_trigger_accepts_standard_5_field_for_9am_daily() {
        // The exact pattern Tarek's model tried that previously failed.
        let spec = TriggerSpec::Cron { expr: "0 9 * * *".into() };
        let now = unix(2026, 5, 17, 0, 0); // midnight UTC
        let next = next_run_at(&spec, "UTC", now).unwrap().unwrap();
        // 09:00 same day in UTC.
        assert_eq!(next, unix(2026, 5, 17, 9, 0));
    }

    #[test]
    fn one_off_future_returns_at() {
        let spec = TriggerSpec::OneOff { at: 2_000 };
        assert_eq!(next_run_at(&spec, "UTC", 1_000).unwrap(), Some(2_000));
    }

    #[test]
    fn one_off_past_still_returns_at_so_worker_fires_immediately() {
        // Past one-offs are due "now"; the worker claims them on the next
        // tick, runs the action, then expires the row.
        let spec = TriggerSpec::OneOff { at: 1_000 };
        assert_eq!(next_run_at(&spec, "UTC", 2_000).unwrap(), Some(1_000));
    }

    #[test]
    fn one_off_at_now_returns_at() {
        let spec = TriggerSpec::OneOff { at: 1_000 };
        assert_eq!(next_run_at(&spec, "UTC", 1_000).unwrap(), Some(1_000));
    }

    #[test]
    fn interval_advances_by_period() {
        let spec = TriggerSpec::Interval { every_secs: 60 };
        assert_eq!(next_run_at(&spec, "UTC", 1_000).unwrap(), Some(1_060));
    }

    #[test]
    fn interval_zero_is_error() {
        let spec = TriggerSpec::Interval { every_secs: 0 };
        assert!(next_run_at(&spec, "UTC", 0).is_err());
    }

    #[test]
    fn cron_daily_9am_utc() {
        // "every day at 09:00:00 UTC" — 6-field cron with seconds.
        let spec = TriggerSpec::Cron { expr: "0 0 9 * * *".into() };
        // After 2026-04-29 08:00:00 UTC, next fire is the same day 09:00.
        let after = unix(2026, 4, 29, 8, 0);
        let want  = unix(2026, 4, 29, 9, 0);
        assert_eq!(next_run_at(&spec, "UTC", after).unwrap(), Some(want));
        // After 09:00 the next fire is the following day.
        let after = unix(2026, 4, 29, 9, 0);
        let want  = unix(2026, 4, 30, 9, 0);
        assert_eq!(next_run_at(&spec, "UTC", after).unwrap(), Some(want));
    }

    #[test]
    fn cron_respects_timezone() {
        // 09:00 in America/New_York during DST (EDT, UTC-4) = 13:00 UTC.
        let spec = TriggerSpec::Cron { expr: "0 0 9 * * *".into() };
        // 2026-07-01 12:00 UTC — before today's NY 09:00 fire.
        let after = unix(2026, 7, 1, 12, 0);
        let want  = unix(2026, 7, 1, 13, 0);
        assert_eq!(next_run_at(&spec, "America/New_York", after).unwrap(), Some(want));
    }

    #[test]
    fn cron_dst_spring_forward_skips_missing_hour() {
        // US DST starts 2026-03-08, clocks jump 02:00 → 03:00 in NY.
        // A daily 02:30 schedule has no valid local time on that day; the
        // cron crate should skip it and fire the next valid day.
        let spec = TriggerSpec::Cron { expr: "0 30 2 * * *".into() };
        // Cursor placed late on Mar 7 NY (= early Mar 8 UTC).
        let after = unix(2026, 3, 8, 1, 0); // 01:00 UTC = 20:00 EST Mar 7
        // Expected: skip 02:30 EST Mar 8 (it doesn't exist) → fire 02:30 EDT
        // Mar 9 (= 06:30 UTC).
        let want = unix(2026, 3, 9, 6, 30);
        assert_eq!(next_run_at(&spec, "America/New_York", after).unwrap(), Some(want));
    }

    #[test]
    fn next_n_runs_returns_three_for_daily_cron() {
        let spec = TriggerSpec::Cron { expr: "0 0 9 * * *".into() };
        let after = unix(2026, 4, 29, 0, 0);
        let runs  = next_n_runs(&spec, "UTC", after, 3).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0], unix(2026, 4, 29, 9, 0));
        assert_eq!(runs[1], unix(2026, 4, 30, 9, 0));
        assert_eq!(runs[2], unix(2026, 5,  1, 9, 0));
    }

    #[test]
    fn invalid_cron_is_error() {
        let spec = TriggerSpec::Cron { expr: "not-a-cron".into() };
        assert!(next_run_at(&spec, "UTC", 0).is_err());
    }

    #[test]
    fn invalid_timezone_is_error() {
        let spec = TriggerSpec::Cron { expr: "0 0 9 * * *".into() };
        assert!(next_run_at(&spec, "Mars/Olympus", 0).is_err());
    }
}
