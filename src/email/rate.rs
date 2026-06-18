// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/rate.rs
//! Per-sender + per-account inbound rate limits (slice E1+E3, chunk 6).
//!
//! Sliding-window in-memory counters keyed by `(account, sender)`
//! and `account`. Each `check_and_record_*` call:
//!
//!   1. Drops timestamps older than the window from the front of the
//!      deque (oldest-first ordering makes this O(stale) per call).
//!   2. If the remaining count is below the limit, pushes `now` and
//!      returns `true` (the message is allowed and counted).
//!   3. Otherwise returns `false` — the security pipeline emits
//!      `Verdict::Drop` and the audit row records why.
//!
//! `limit == 0` means "no limit"; the check returns `true` without
//! touching the deque. Operators disable a particular dimension by
//! setting the per-account override to 0.
//!
//! Persistence: none. A flood that survives a restart loses its
//! counter — defense-in-depth, not an SLA. The whole map fits in
//! a single `Mutex<HashMap<...>>` because the call frequency is
//! one-per-inbound-email, far below contention thresholds.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::email::security::InboundRateLimiter;

const PER_SENDER_WINDOW: Duration = Duration::from_secs(60 * 60);          // 1 hour
const PER_ACCOUNT_WINDOW: Duration = Duration::from_secs(60 * 60 * 24);    // 1 day

pub struct InMemoryRateLimiter {
    // Keyed by (account_id, sender_lowercased). Lowercased so case
    // differences in From-headers don't let a flooder slip through.
    sender: Mutex<HashMap<(String, String), VecDeque<Instant>>>,
    // Keyed by account_id.
    account: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl InMemoryRateLimiter {
    pub fn new() -> Self {
        Self {
            sender:  Mutex::new(HashMap::new()),
            account: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryRateLimiter {
    fn default() -> Self { Self::new() }
}

impl InboundRateLimiter for InMemoryRateLimiter {
    fn check_and_record_sender(
        &self,
        account_id:     &str,
        sender:         &str,
        limit_per_hour: u32,
    ) -> bool {
        if limit_per_hour == 0 { return true; }
        let now = Instant::now();
        let key = (account_id.to_string(), sender.to_ascii_lowercase());
        let mut map = self.sender.lock().unwrap();
        let dq = map.entry(key).or_default();
        prune(dq, now, PER_SENDER_WINDOW);
        if (dq.len() as u32) >= limit_per_hour { return false; }
        dq.push_back(now);
        true
    }

    fn check_and_record_account(
        &self,
        account_id:    &str,
        limit_per_day: u32,
    ) -> bool {
        if limit_per_day == 0 { return true; }
        let now = Instant::now();
        let mut map = self.account.lock().unwrap();
        let dq = map.entry(account_id.to_string()).or_default();
        prune(dq, now, PER_ACCOUNT_WINDOW);
        if (dq.len() as u32) >= limit_per_day { return false; }
        dq.push_back(now);
        true
    }
}

fn prune(dq: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while let Some(&front) = dq.front() {
        if now.duration_since(front) > window {
            dq.pop_front();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_zero_means_unlimited() {
        let rl = InMemoryRateLimiter::new();
        for _ in 0..1000 {
            assert!(rl.check_and_record_sender("a", "x@y.com", 0));
            assert!(rl.check_and_record_account("a", 0));
        }
    }

    #[test]
    fn sender_limit_blocks_after_quota() {
        let rl = InMemoryRateLimiter::new();
        for _ in 0..3 {
            assert!(rl.check_and_record_sender("a", "x@y.com", 3));
        }
        assert!(!rl.check_and_record_sender("a", "x@y.com", 3));
        // Different sender still has its own bucket.
        assert!(rl.check_and_record_sender("a", "other@y.com", 3));
        // Different account too.
        assert!(rl.check_and_record_sender("b", "x@y.com", 3));
    }

    #[test]
    fn account_limit_blocks_after_quota() {
        let rl = InMemoryRateLimiter::new();
        for _ in 0..5 {
            assert!(rl.check_and_record_account("a", 5));
        }
        assert!(!rl.check_and_record_account("a", 5));
        assert!(rl.check_and_record_account("b", 5));
    }

    #[test]
    fn case_insensitive_sender_match() {
        let rl = InMemoryRateLimiter::new();
        rl.check_and_record_sender("a", "Foo@BAR.com", 1);
        assert!(!rl.check_and_record_sender("a", "foo@bar.com", 1),
                "different casing of the same address must hit the same bucket");
    }
}
