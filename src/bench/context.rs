// SPDX-License-Identifier: AGPL-3.0-or-later

// src/bench/context.rs
//! `mira bench context` — measurement-only baseline for the context-compaction
//! work (see design-docs/context-compaction.md §3). Generates synthetic
//! multi-turn conversations, applies MIRA's CURRENT windowing
//! (`agent.max_context_turns`), and reports how many tokens the window actually
//! sends, how much of the model's context window that uses, and how many turns
//! are dropped. **No API spend** — this is the "before" picture the later phases
//! (token-aware budget, compaction) are measured against.

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::context_budget::{context_budget, history_fit, safe_eviction};
use crate::agent::tokens::estimate_messages;
use crate::config::MiraConfig;
use crate::types::ChatMessage;
use crate::MiraError;

/// CLI-supplied options for a context benchmark run.
#[derive(Debug, Clone)]
pub struct ContextBenchOptions {
    /// Conversation lengths (in turns) to measure.
    pub turns: Vec<usize>,
    /// Average characters per message (≈ 4 chars/token).
    pub avg_msg_chars: usize,
    /// Model context window (tokens) used to compute utilisation.
    pub context_length: usize,
    /// Fixed system-prompt + memory overhead estimate (tokens) added to every
    /// assembled prompt, so utilisation reflects more than just history.
    pub system_tokens: usize,
    /// Output reservation (tokens) subtracted from the window for the Phase-1
    /// budget column. `0` = use `agent.max_response_tokens` from config.
    pub max_response_tokens: usize,
    /// Safety margin (tokens) held back in the Phase-1 budget column.
    pub safety_margin: usize,
    /// Optional CSV output path.
    pub out: Option<PathBuf>,
}

struct Row {
    turns:          usize,
    total_tokens:   usize,
    // Current fixed-turn window.
    window_tokens:  usize,
    util_pct:       f64,
    turns_kept:     usize,
    // Phase-1 token-aware budget.
    budget_tokens:  usize,
    budget_util:    f64,
    budget_kept:    usize,
}

/// One synthetic message of ~`chars` characters of filler prose.
fn synth(is_user: bool, chars: usize, i: usize) -> ChatMessage {
    let filler = "lorem ipsum dolor sit amet consectetur ".repeat(chars / 39 + 1);
    let body: String = format!("[turn {i}] {filler}").chars().take(chars.max(8)).collect();
    if is_user { ChatMessage::user(body) } else { ChatMessage::assistant(body) }
}

/// Run the measurement-only context baseline. Async to match the other bench
/// entry points, though this phase does no I/O.
pub async fn run_context_bench(
    opts:   ContextBenchOptions,
    config: Arc<MiraConfig>,
) -> Result<(), MiraError> {
    let max_ctx_turns = config.agent.max_context_turns;   // current window (turns)
    let window_msgs   = max_ctx_turns * 2;
    let max_resp = if opts.max_response_tokens == 0 {
        config.agent.max_response_tokens as usize
    } else {
        opts.max_response_tokens
    };
    let budget = context_budget(opts.context_length, max_resp, opts.safety_margin);

    println!("bench context — fixed window vs token-aware budget (no API spend)");
    println!("  current: max_context_turns = {max_ctx_turns}  (window = {window_msgs} messages)");
    println!("  budget : context {} − response {} − margin {} = {} tok",
             opts.context_length, max_resp, opts.safety_margin, budget);
    println!("  avg_msg_chars = {}   system+memory est = {} tok\n",
             opts.avg_msg_chars, opts.system_tokens);

    let mut rows = Vec::new();
    for &l in &opts.turns {
        // Full conversation: one user + one assistant message per turn.
        let mut msgs = Vec::with_capacity(l * 2);
        for i in 0..l {
            msgs.push(synth(true,  opts.avg_msg_chars, i));
            msgs.push(synth(false, opts.avg_msg_chars, i));
        }
        let total_tokens = opts.system_tokens + estimate_messages(&msgs);

        // Current: keep only the last `window_msgs` messages.
        let skip          = msgs.len().saturating_sub(window_msgs);
        let window_tokens = opts.system_tokens + estimate_messages(&msgs[skip..]);
        let turns_kept    = (msgs.len() - skip) / 2;
        let util_pct      = 100.0 * window_tokens as f64 / opts.context_length as f64;

        // Phase 1: fill the token budget, newest-first.
        let fit           = history_fit(&msgs, opts.system_tokens, budget);
        let bskip         = msgs.len() - fit;
        let budget_tokens = opts.system_tokens + estimate_messages(&msgs[bskip..]);
        let budget_kept   = fit / 2;
        let budget_util   = 100.0 * budget_tokens as f64 / opts.context_length as f64;

        rows.push(Row {
            turns: l, total_tokens, window_tokens, util_pct, turns_kept,
            budget_tokens, budget_util, budget_kept,
        });
    }

    println!("{:>6}  {:>10}  │ {:>10} {:>6} {:>8}  │ {:>10} {:>6} {:>8}",
             "turns", "total_tok", "win_tok", "util%", "win_turn", "bud_tok", "util%", "bud_turn");
    for r in &rows {
        println!("{:>6}  {:>10}  │ {:>10} {:>5.1}% {:>4}/{:<3} │ {:>10} {:>5.1}% {:>4}/{:<3}",
                 r.turns, r.total_tokens,
                 r.window_tokens, r.util_pct, r.turns_kept, r.turns,
                 r.budget_tokens, r.budget_util, r.budget_kept, r.turns);
    }
    if let Some(worst) = rows.iter().max_by_key(|r| r.turns) {
        let gain = if worst.turns_kept > 0 {
            worst.budget_kept as f64 / worst.turns_kept as f64
        } else { 0.0 };
        println!(
            "\nReading: at {} turns the fixed window keeps {} turns ({:.1}% of a {}-tok window); \
             the token budget keeps {} turns ({:.1}%) — {:.1}× more history in the same window, \
             no compaction yet. Phase 2 will compact what still doesn't fit.",
            worst.turns, worst.turns_kept, worst.util_pct, opts.context_length,
            worst.budget_kept, worst.budget_util, gain,
        );
    }

    // Phase-3 write-before-compaction coverage check. Simulate the compaction
    // watermark advancing as the conversation overflows — including a stalled
    // round where a rewrite fails and the watermark does NOT advance — and
    // assert the guardrail never evicts a message the summary hasn't captured.
    // This is the deterministic stand-in for "were the facts written before the
    // turns were dropped?": no turn is ever evicted ahead of its capture.
    {
        let longest = *opts.turns.iter().max().unwrap_or(&0);
        let total_msgs = longest * 2;
        let mut covered = 0usize;          // watermark: oldest msgs captured
        let mut worst_uncaptured = 0usize; // max messages held back in any round
        let mut violations = 0usize;
        // Walk the conversation growing two messages per turn; every few turns
        // the budget wants to evict more, and one in five capture attempts
        // "fails" (watermark stalls) to exercise the guardrail.
        for turn in 1..=longest {
            let msgs_so_far = turn * 2;
            // The budget wants to keep only the last `window` messages.
            let window = (config.agent.max_context_turns * 2).max(2);
            let requested_skip = msgs_so_far.saturating_sub(window);
            // Capture advances the watermark toward the request, except on a
            // simulated failure round.
            let capture_fails = turn % 5 == 0;
            if !capture_fails {
                covered = requested_skip;
            }
            let evicted = safe_eviction(requested_skip, covered);
            if evicted > covered { violations += 1; }
            worst_uncaptured = worst_uncaptured.max(requested_skip - evicted);
        }
        let _ = total_msgs;
        println!(
            "\nWrite-before-compaction guardrail: {} — max {} message(s) held verbatim during a \
             stalled compaction round (never dropped); evicted-beyond-capture violations: {}.",
            if violations == 0 { "PASS" } else { "FAIL" },
            worst_uncaptured, violations,
        );
    }

    if let Some(path) = &opts.out {
        let mut csv = String::from(
            "turns,total_tokens,window_tokens,window_util_pct,window_turns,budget_tokens,budget_util_pct,budget_turns\n");
        for r in &rows {
            csv.push_str(&format!("{},{},{},{:.2},{},{},{:.2},{}\n",
                r.turns, r.total_tokens, r.window_tokens, r.util_pct, r.turns_kept,
                r.budget_tokens, r.budget_util, r.budget_kept));
        }
        std::fs::write(path, csv)
            .map_err(|e| MiraError::ConfigError(format!("write bench csv {}: {e}", path.display())))?;
        println!("\nWrote {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn baseline_runs_and_windows() {
        let cfg = Arc::new(MiraConfig::default()); // max_context_turns default = 20
        let opts = ContextBenchOptions {
            turns: vec![10, 300],
            avg_msg_chars: 320,
            context_length: 128_000,
            system_tokens: 1500,
            max_response_tokens: 16_384,
            safety_margin: 1024,
            out: None,
        };
        // Smoke: it runs without panicking / erroring.
        run_context_bench(opts, cfg).await.unwrap();
    }

    #[test]
    fn windowing_drops_old_turns_beyond_budget() {
        // Sanity on the core windowing math the runner uses.
        let window_turns = 20usize;
        let l = 300usize;
        let msgs = l * 2;
        let kept = msgs.min(window_turns * 2);
        assert_eq!(kept / 2, 20);
        assert_eq!(l - kept / 2, 280); // 280 turns dropped at 300
    }
}
