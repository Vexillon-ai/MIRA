// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/context_budget.rs
//! Phase 1 — token-aware context budgeting (see design-docs/context-compaction.md
//! §5). Replaces the fixed `max_context_turns` window with a real token budget
//! derived from the model's context length, filling the window efficiently
//! instead of sending a near-constant slice regardless of what fits.
//!
//! Pure logic (unit-tested); the agent path and the `bench context` harness both
//! call [`history_fit`] so the measured gain reflects the real selection.

use crate::agent::tokens::estimate_message;
use crate::types::ChatMessage;

/// Tokens available for *everything we send* (system + memory + summary +
/// history), given the model's context window and the output reservation.
/// Saturating so a tiny/zero context never underflows.
pub fn context_budget(context_length: usize, max_response_tokens: usize, safety_margin: usize) -> usize {
    context_length
        .saturating_sub(max_response_tokens)
        .saturating_sub(safety_margin)
}

/// How many of the most-recent `history` messages fit, newest-first, given the
/// `fixed_tokens` already committed (system + memory + summary) and the total
/// `budget`. Returns the count to KEEP from the tail (so the caller slices
/// `history[len - kept ..]`). Always tries to keep at least the last message if
/// any budget remains for it.
pub fn history_fit(history: &[ChatMessage], fixed_tokens: usize, budget: usize) -> usize {
    let mut used = fixed_tokens;
    let mut kept = 0usize;
    for m in history.iter().rev() {
        let cost = estimate_message(m);
        if used.saturating_add(cost) > budget {
            break;
        }
        used += cost;
        kept += 1;
    }
    kept
}

/// Phase-3 write-before-compaction guardrail. Given the eviction the budget
/// *wants* (`requested_skip` oldest messages) and how many of the oldest
/// messages compaction has actually captured (`covered`), returns how many may
/// safely be evicted this turn. It never exceeds `covered`, so a failed or
/// partial compaction holds the uncaptured turns verbatim (retried next turn)
/// rather than dropping them — nothing is lost that wasn't first written to the
/// rolling summary (and, independently, the memory store). Both the agent path
/// and the harness call this so the invariant is enforced in one place.
pub fn safe_eviction(requested_skip: usize, covered: usize) -> usize {
    requested_skip.min(covered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    fn convo(n: usize, chars: usize) -> Vec<ChatMessage> {
        (0..n)
            .flat_map(|i| {
                let body: String = std::iter::repeat('x').take(chars).collect();
                [ChatMessage::user(format!("{i}:{body}")), ChatMessage::assistant(body)]
            })
            .collect()
    }

    #[test]
    fn budget_reserves_output_and_margin() {
        assert_eq!(context_budget(128_000, 16_384, 1024), 128_000 - 16_384 - 1024);
        assert_eq!(context_budget(1000, 2000, 100), 0); // saturating, no underflow
    }

    #[test]
    fn fits_more_when_budget_is_large() {
        let h = convo(300, 300);            // 600 messages
        let big = history_fit(&h, 1500, 100_000);
        let small = history_fit(&h, 1500, 5_000);
        assert!(big > small, "big={big} small={small}");
        assert!(big <= h.len());
    }

    #[test]
    fn budget_beats_fixed_window_on_long_convo() {
        // The whole point: at a generous budget we keep far more than the old
        // fixed 20-turn (40-message) window.
        let h = convo(300, 200);
        let kept = history_fit(&h, 1500, 110_000);
        assert!(kept > 40, "budget kept only {kept} messages — no better than the fixed window");
    }

    #[test]
    fn zero_budget_keeps_nothing() {
        let h = convo(10, 100);
        assert_eq!(history_fit(&h, 999_999, 1000), 0);
    }

    #[test]
    fn safe_eviction_never_exceeds_capture() {
        // Compaction succeeded: everything requested was captured → evict all.
        assert_eq!(safe_eviction(40, 40), 40);
        // Compaction lagged/failed: only 24 captured → evict at most 24, hold
        // the uncaptured 16 verbatim. The write-before-compaction invariant.
        assert_eq!(safe_eviction(40, 24), 24);
        // Nothing captured yet (first compaction down) → evict nothing.
        assert_eq!(safe_eviction(40, 0), 0);
        // Watermark ahead of request (rehydrated shorter buffer) → clamp to request.
        assert_eq!(safe_eviction(10, 40), 10);
    }

    #[test]
    fn no_evicted_message_is_ever_uncaptured() {
        // Sweep requested-vs-covered pairs; the evicted region [0, eff) must
        // always lie entirely within the captured region [0, covered).
        for requested in 0..=50 {
            for covered in 0..=50 {
                let eff = safe_eviction(requested, covered);
                assert!(eff <= covered, "evicted {eff} beyond captured {covered}");
                assert!(eff <= requested);
            }
        }
    }
}
