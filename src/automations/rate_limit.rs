// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/rate_limit.rs
//! Per-user, per-channel sliding-window rate limiter for `channel_message`
//! actions.
//!
//! Why this exists: both users and the agent can author automations, and a
//! mistake on either side (a 1-second cron, a feedback loop between two
//! schedules) can spam an external channel. The limiter is a hard cap that
//! catches runaway emission in seconds without blocking legitimate use
//! cases like a 1-minute reminder.
//!
//! Sliding window per `(user_id, channel)` pair: each successful dispatch
//! pushes a timestamp; a new dispatch is allowed only if the count of
//! timestamps within the past 60 s is below the configured cap. A cap of
//! `0` means the channel is unlimited.
//!
//! All state is in-memory. Restart-on-leak is acceptable: the worst case is
//! a brief grace period after a process restart, not a permanent bypass.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// One sliding-window bucket per `(user_id, channel)`.
pub struct ChannelRateLimiter {
    /// Channel → max messages per 60 s window. The special key `*` is the
    /// fallback. A value of `0` disables the limit.
    limits: HashMap<String, u32>,
    /// `(user_id, channel)` → timestamps (unix seconds) of recent dispatches
    /// inside the active window. Bounded by the per-channel cap, so memory
    /// is `O(active users × channels × cap)`.
    buckets: Mutex<HashMap<(String, String), VecDeque<i64>>>,
}

/// What the limiter tells the caller. `Allowed` carries the new bucket
/// size for diagnostics; `Denied` carries the cap and the seconds until
/// the oldest sample falls out of the window so the caller can surface a
/// useful error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateDecision {
    Allowed { in_window: u32, cap: u32 },
    Denied  { cap: u32, retry_after_secs: i64 },
}

impl ChannelRateLimiter {
    pub fn new(limits: HashMap<String, u32>) -> Self {
        Self { limits, buckets: Mutex::new(HashMap::new()) }
    }

    /// Resolve the cap for `channel`, falling back to the `*` entry. A
    /// missing fallback is treated as "no limit configured" — the caller
    /// gets `None` and should let the dispatch through.
    fn cap_for(&self, channel: &str) -> Option<u32> {
        self.limits.get(channel).copied()
            .or_else(|| self.limits.get("*").copied())
    }

    /// Check the limit for `(user_id, channel)` at `now` (unix seconds),
    /// and on Allowed record the timestamp. Single call so the check and
    /// the bookkeeping share one lock acquisition.
    pub fn check_and_record(
        &self,
        user_id: &str,
        channel: &str,
        now:     i64,
    ) -> RateDecision {
        let cap = match self.cap_for(channel) {
            Some(0) | None => {
                // No limit (explicit 0, or no entry + no fallback).
                return RateDecision::Allowed { in_window: 0, cap: 0 };
            }
            Some(c) => c,
        };

        let key = (user_id.to_string(), channel.to_string());
        let window_start = now - 60;

        let mut buckets = self.buckets.lock().expect("rate-limit mutex poisoned");
        let q = buckets.entry(key).or_default();

        // Drop samples older than the window before checking.
        while q.front().is_some_and(|t| *t <= window_start) {
            q.pop_front();
        }

        if (q.len() as u32) >= cap {
            // Oldest sample dictates how long until a slot frees up.
            let oldest = *q.front().unwrap_or(&now);
            let retry_after_secs = (oldest + 60 - now).max(1);
            return RateDecision::Denied { cap, retry_after_secs };
        }

        q.push_back(now);
        RateDecision::Allowed { in_window: q.len() as u32, cap }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(pairs: &[(&str, u32)]) -> HashMap<String, u32> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn allows_until_cap_then_denies() {
        let lim = ChannelRateLimiter::new(limits(&[("signal", 3)]));
        for i in 0..3 {
            assert!(matches!(
                lim.check_and_record("u1", "signal", 100 + i),
                RateDecision::Allowed { .. }
            ));
        }
        assert!(matches!(
            lim.check_and_record("u1", "signal", 103),
            RateDecision::Denied { cap: 3, .. }
        ));
    }

    #[test]
    fn slides_window_so_old_samples_drop_out() {
        let lim = ChannelRateLimiter::new(limits(&[("signal", 2)]));
        lim.check_and_record("u1", "signal", 100);
        lim.check_and_record("u1", "signal", 110);
        // 60s after the first sample, the bucket has 1 sample left, so a
        // new one fits.
        assert!(matches!(
            lim.check_and_record("u1", "signal", 161),
            RateDecision::Allowed { in_window: 2, cap: 2 }
        ));
    }

    #[test]
    fn isolates_users() {
        let lim = ChannelRateLimiter::new(limits(&[("signal", 1)]));
        assert!(matches!(
            lim.check_and_record("u1", "signal", 100),
            RateDecision::Allowed { .. }
        ));
        assert!(matches!(
            lim.check_and_record("u2", "signal", 100),
            RateDecision::Allowed { .. }
        ));
        assert!(matches!(
            lim.check_and_record("u1", "signal", 101),
            RateDecision::Denied { .. }
        ));
    }

    #[test]
    fn isolates_channels() {
        let lim = ChannelRateLimiter::new(limits(&[("signal", 1), ("telegram", 1)]));
        assert!(matches!(
            lim.check_and_record("u1", "signal", 100),
            RateDecision::Allowed { .. }
        ));
        assert!(matches!(
            lim.check_and_record("u1", "telegram", 100),
            RateDecision::Allowed { .. }
        ));
    }

    #[test]
    fn zero_cap_is_unlimited() {
        let lim = ChannelRateLimiter::new(limits(&[("web", 0)]));
        for i in 0..1000 {
            assert!(matches!(
                lim.check_and_record("u1", "web", 100 + i),
                RateDecision::Allowed { cap: 0, .. }
            ));
        }
    }

    #[test]
    fn unknown_channel_falls_back_to_star() {
        let lim = ChannelRateLimiter::new(limits(&[("*", 2)]));
        assert!(matches!(
            lim.check_and_record("u1", "discord", 100),
            RateDecision::Allowed { .. }
        ));
        assert!(matches!(
            lim.check_and_record("u1", "discord", 101),
            RateDecision::Allowed { .. }
        ));
        assert!(matches!(
            lim.check_and_record("u1", "discord", 102),
            RateDecision::Denied { cap: 2, .. }
        ));
    }

    #[test]
    fn no_config_means_no_limit() {
        let lim = ChannelRateLimiter::new(limits(&[]));
        for i in 0..50 {
            assert!(matches!(
                lim.check_and_record("u1", "anything", 100 + i),
                RateDecision::Allowed { .. }
            ));
        }
    }
}
