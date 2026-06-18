// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/routing.rs
//! Reasoning-model auto-routing (roadmap #13).
//!
//! Decides whether a turn is "hard" enough to route to a stronger reasoning
//! model instead of the default. **Slice A** is the heuristic stage: cheap,
//! deterministic signals with zero added latency/cost. A later slice adds a
//! classifier fallback for ambiguous turns (the "hybrid" design) and a
//! thinking-effort bump on the routed provider.

/// Outcome of the routing decision, with a short reason for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    pub route_up: bool,
    pub reason:   &'static str,
}

impl RouteDecision {
    fn up(reason: &'static str) -> Self { Self { route_up: true, reason } }
    fn keep(reason: &'static str) -> Self { Self { route_up: false, reason } }
}

/// Three-way triage for the **hybrid** router (Slice C):
///   * `Up` — a heuristic signal fired; route up immediately (no model call).
///   * `Down` — clearly trivial; keep the default (no model call).
///   * `Ambiguous` — no signal but not obviously trivial; consult the cheap
///     classifier (the only case that spends a model call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Triage {
    Up(&'static str),
    Down(&'static str),
    Ambiguous(&'static str),
}

/// Below this many characters with no hard signal, a turn is treated as
/// clearly trivial ("hi", "thanks", "what time is it") — not worth a
/// classifier call. Between this and `min_chars` is the ambiguous band.
const TRIVIAL_CHARS: usize = 80;

/// Substrings that strongly signal a multi-step / reasoning task. Matched
/// case-insensitively; deliberately stems (e.g. `analy` covers analyse /
/// analyze / analysis) so we don't enumerate every inflection.
const HARD_KEYWORDS: &[&str] = &[
    "step by step", "step-by-step", "think hard", "think carefully", "reason carefully",
    "prove", "derive", "analy", "debug", "root cause", "trade-off", "tradeoff",
    "optimi", "algorithm", "complexity", "explain why", "explain how",
    "walk me through", "plan out", "design a", "architect", "refactor", "diagnose",
];

/// Heuristic stage of the router (no model calls). `Up` when a signal fires,
/// `Down` when clearly trivial, `Ambiguous` otherwise (the classifier's job).
pub fn triage(input: &str, min_chars: usize) -> Triage {
    let trimmed = input.trim();
    let lower = trimmed.to_lowercase();

    // ── Hard signals → route up immediately ──
    if trimmed.contains("```") {
        return Triage::Up("code block");
    }
    if HARD_KEYWORDS.iter().any(|k| lower.contains(k)) {
        return Triage::Up("reasoning keyword");
    }
    if trimmed.chars().count() >= min_chars {
        return Triage::Up("long input");
    }
    if trimmed.matches('?').count() >= 3 {
        return Triage::Up("multi-question");
    }
    if looks_mathy(trimmed) {
        return Triage::Up("math content");
    }

    // ── No signal: trivial vs ambiguous ──
    if trimmed.chars().count() < TRIVIAL_CHARS {
        Triage::Down("trivial")
    } else {
        Triage::Ambiguous("no signal, mid-length")
    }
}

/// Heuristic-only decision (Slice A/B path + tests): route up only on a hard
/// signal; ambiguous/trivial both keep the default. The hybrid path uses
/// [`triage`] directly so it can consult the classifier on `Ambiguous`.
pub fn classify_heuristic(input: &str, min_chars: usize) -> RouteDecision {
    match triage(input, min_chars) {
        Triage::Up(r) => RouteDecision::up(r),
        Triage::Down(r) | Triage::Ambiguous(r) => RouteDecision::keep(r),
    }
}

/// Classifier fallback (Slice C) for `Ambiguous` turns: ask a cheap model
/// whether the turn needs careful multi-step reasoning. Returns `false`
/// (don't route up) on any error, so an unavailable classifier never blocks
/// the turn or silently inflates cost.
pub async fn classify_ambiguous(
    provider: &std::sync::Arc<dyn crate::providers::ModelProvider>,
    input:    &str,
) -> bool {
    use crate::types::{ChatMessage, GenerationOptions};

    // Keep the input bounded — the classifier only needs the gist.
    let snippet: String = input.chars().take(1200).collect();
    let prompt = format!(
        "Classify the user's message. Does answering it WELL require careful, \
         multi-step reasoning (planning, math, code, analysis, or synthesising \
         several facts) — as opposed to a quick, direct answer or chit-chat?\n\n\
         Message: {snippet}\n\n\
         Reply with exactly `yes` (needs reasoning) or `no` (quick answer)."
    );
    let messages = [ChatMessage::user(prompt)];
    let opts = GenerationOptions { temperature: 0.0, max_tokens: Some(4), ..Default::default() };
    match provider.generate(&messages, &opts).await {
        Ok(resp) => {
            let lower = resp.content.trim().to_ascii_lowercase();
            lower.split(|c: char| !c.is_ascii_alphabetic()).any(|t| t == "yes")
        }
        Err(_) => false,
    }
}

/// Cheap "is this mathematical?" check: at least a few math operators sitting
/// near digits. Catches "solve 3x^2 + 2x - 5 = 0" without firing on prose.
fn looks_mathy(s: &str) -> bool {
    let has_digit = s.chars().any(|c| c.is_ascii_digit());
    if !has_digit {
        return false;
    }
    let ops = s.chars().filter(|c| matches!(c, '=' | '^' | '√' | '∫' | '∑' | '×' | '÷')).count()
        + s.matches("**").count();
    ops >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: usize = 600;

    #[test]
    fn simple_turns_stay() {
        assert!(!classify_heuristic("hey, what's up?", MIN).route_up);
        assert!(!classify_heuristic("remind me to call mum", MIN).route_up);
        assert!(!classify_heuristic("what time is it in Tokyo?", MIN).route_up);
    }

    #[test]
    fn code_blocks_route_up() {
        let d = classify_heuristic("fix this:\n```rust\nfn x(){}\n```", MIN);
        assert!(d.route_up);
        assert_eq!(d.reason, "code block");
    }

    #[test]
    fn reasoning_keywords_route_up() {
        assert!(classify_heuristic("walk me through how DNS resolution works", MIN).route_up);
        assert!(classify_heuristic("debug why my service keeps restarting", MIN).route_up);
        assert!(classify_heuristic("prove that sqrt(2) is irrational", MIN).route_up);
    }

    #[test]
    fn long_input_routes_up() {
        let long = "context: ".to_string() + &"word ".repeat(200);
        assert!(classify_heuristic(&long, MIN).route_up);
    }

    #[test]
    fn math_routes_up() {
        let d = classify_heuristic("solve 3x^2 + 2x - 5 = 0 for x", MIN);
        assert!(d.route_up, "got {:?}", d);
        assert_eq!(d.reason, "math content");
    }

    #[test]
    fn plain_number_does_not_trip_math() {
        assert!(!classify_heuristic("set a timer for 5 minutes", MIN).route_up);
    }

    #[test]
    fn triage_three_way() {
        // Hard signal → Up.
        assert!(matches!(triage("debug my crash", MIN), Triage::Up(_)));
        // Short + no signal → Down (trivial, no classifier call).
        assert!(matches!(triage("hi there", MIN), Triage::Down(_)));
        // Mid-length prose, no signal → Ambiguous (consult classifier).
        let mid = "I've been thinking about whether to take the new job offer or stay where I am for now";
        assert!(matches!(triage(mid, MIN), Triage::Ambiguous(_)), "len={}", mid.len());
    }
}
