// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/engagement.rs
//! LLM-based engagement classifier — labels a (user-msg, assistant-msg)
//! pair as engaged / brief / declined / distressed.
//!
//! Called as a fire-and-forget post-turn hook by `AgentCore` when
//! companion mode is active for the user. Failures are non-fatal —
//! we'd rather lose a label than break the conversation. The label
//! lands in [`engagement_log::EngagementLog`] where the scheduler
//! reads it to adjust cadence, and (in a follow-up) where wiki
//! routine-update extraction reads it to aggregate by hour-of-day.

use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use tracing::{debug, warn};

use crate::companion::engagement_log::{EngagementEntry, EngagementLabel, EngagementLog};
use crate::companion::safety::{ConcernSeverity, SafetyFloor};
use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions};

// Wall-clock cap for the assessor's LLM call. Generous — local
// reasoning models can burn a few seconds on internal thinking
// before they spit out a one-word label.
const ENGAGEMENT_TIMEOUT: Duration = Duration::from_secs(20);

// Skip labelling when the turn is shorter than this on either side
// too little signal, and the LLM call cost isn't worth it. The
// scheduler will fall back to default cadence for those turns,
// which is fine.
const MIN_TURN_CHARS_FOR_LABEL: usize = 6;

// Bind together everything the post-hook needs. Passed by `Arc`
// from the gateway so the spawn cost is just a clone.
#[derive(Clone)]
pub struct EngagementAssessor {
    pub provider: Arc<dyn ModelProvider>,
    pub log: Arc<EngagementLog>,
    // Safety floor. When the classifier returns
    // `Distressed`, we hand off to `safety.handle_distress` before
    // returning. `None` in builds where the safety floor isn't
    // wired — the label still lands in the engagement log, just
    // without escalation.
    pub safety: Option<SafetyFloor>,
}

// Fire the assessor in the background. Returns immediately; the
// classification + insert happen on the spawned task. Errors are
// logged at warn-level — the chat reply has already been delivered.
// // `user_tz` is the user's IANA timezone string (from `UserProfile`).
// `None` falls back to UTC for the `hour_of_day` / `day_of_week`
// columns; the wiki routines updater would prefer the user's tz.
pub fn spawn_post_hook(
    assessor: EngagementAssessor,
    user_id: String,
    conversation_id: Option<String>,
    turn_id: Option<String>,
    user_msg: String,
    assistant_msg: String,
    user_tz: Option<String>,
) {
    if user_msg.trim().chars().count() < MIN_TURN_CHARS_FOR_LABEL
        || assistant_msg.trim().chars().count() < MIN_TURN_CHARS_FOR_LABEL
    {
        debug!("companion engagement: turn too short to label, skipping");
        return;
    }

    tokio::spawn(async move {
        let (label, severity) = match classify(&assessor.provider, &user_msg, &assistant_msg).await {
            Some(c) => c,
            None => {
                debug!("companion engagement: classifier produced no label for '{user_id}'");
                return;
            }
        };

        let now = Utc::now();
        let tz: Tz = user_tz.as_deref()
            .and_then(|s| s.parse::<Tz>().ok())
            .unwrap_or(chrono_tz::UTC);
        let local = tz.from_utc_datetime(&now.naive_utc());

        let entry = EngagementEntry {
            user_id: user_id.clone(),
            conversation_id,
            turn_id,
            label,
            hour_of_day: local.hour() as u8,
            day_of_week: local.weekday().num_days_from_monday() as u8,
            created_at: now,
        };
        if let Err(e) = assessor.log.insert(&entry) {
            warn!("companion engagement: log insert failed for '{user_id}': {e}");
        } else {
            debug!(
                "companion engagement: '{user_id}' → {} (h={}, d={})",
                label.as_str(), entry.hour_of_day, entry.day_of_week,
            );
        }

        // if the label is Distressed, hand off to the
        // safety floor. The floor takes care of dedup, contact
        // resolution, delivery + audit.
        if matches!(label, EngagementLabel::Distressed) {
            if let Some(floor) = &assessor.safety {
                let summary = build_distress_summary(&user_msg, &assistant_msg);
                let sev = severity.unwrap_or_default();
                let _ = floor.handle_distress(&user_id, &summary, sev).await;
            } else {
                warn!(
                    "companion engagement: distressed signal for '{user_id}' \
                     but safety floor not wired"
                );
            }
        }
    });
}

// Build a short summary fed into the safety audit log. Caps both
// halves so a long transcript can't bloat the audit row. The
// safety floor's own `clip` truncates further; this just keeps the
// audit summary readable.
fn build_distress_summary(user_msg: &str, assistant_msg: &str) -> String {
    let u = truncate(user_msg, 200);
    let a = truncate(assistant_msg, 200);
    format!("user: {u} | assistant: {a}")
}

// Run the LLM classifier on one (user, assistant) pair. Returns the
// engagement label plus, when the label is `distressed`, a concern
// severity (`Acute` vs `Concerning`) parsed from the same response — so
// severity costs no extra LLM call. `None` on provider error, timeout,
// or unparseable output.
async fn classify(
    provider: &Arc<dyn ModelProvider>,
    user_msg: &str,
    assistant_msg: &str,
) -> Option<(EngagementLabel, Option<ConcernSeverity>)> {
    let messages = vec![
        ChatMessage::system(SYSTEM_PROMPT.to_string()),
        ChatMessage::user(build_user_prompt(user_msg, assistant_msg)),
    ];
    let opts = GenerationOptions {
        temperature: 0.0,
        max_tokens: Some(32),
        ..Default::default()
    };

    let response = match tokio::time::timeout(
        ENGAGEMENT_TIMEOUT,
        provider.generate(&messages, &opts),
    ).await {
        Ok(Ok(r))  => r.content,
        Ok(Err(e)) => { warn!("engagement classifier: provider failed: {e}"); return None; }
        Err(_)     => { warn!("engagement classifier: timed out"); return None; }
    };

    let label = parse_label(&response)?;
    let severity = matches!(label, EngagementLabel::Distressed)
        .then(|| parse_severity(&response));
    Some((label, severity.flatten()))
}

const SYSTEM_PROMPT: &str = "\
You are a turn-engagement classifier for a personal AI companion. \
Given a (user message, assistant reply) pair, output EXACTLY one \
of these labels, on a single line, with no surrounding text:\n\
\n\
- engaged   — the user is conversing meaningfully: asks follow-ups, \
              shares context, expresses emotion or opinion.\n\
- brief     — the user replied curtly (one or two words / very \
              short acknowledgements). Not negative; just terse.\n\
- declined  — the user explicitly wants the conversation to stop \
              now ('not now', 'leave me alone', 'I'm busy'). Be \
              conservative — only use this when the user clearly \
              wants the conversation to end.\n\
- distressed — the user signals serious distress (mentions of \
               self-harm, severe physical symptoms, acute \
               loneliness/hopelessness, etc.).\n\
\n\
When and ONLY when the label is `distressed`, add a severity after a \
colon:\n\
- `distressed:acute`  — imminent or serious: self-harm intent or \
                        methods, or acute physical symptoms (chest \
                        pain, can't breathe, a fall).\n\
- `distressed:concerning` — notable but not imminent: sadness, \
                        loneliness, hopelessness, a hard day.\n\
\n\
Output only the label (and severity if distressed). No reasoning, no \
explanations.\
";

fn build_user_prompt(user_msg: &str, assistant_msg: &str) -> String {
    format!(
        "User said:\n```\n{}\n```\n\nAssistant replied:\n```\n{}\n```\n\nLabel:",
        truncate(user_msg, 800),
        truncate(assistant_msg, 800),
    )
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars { return s.to_string(); }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

// Pick the first known label out of the response text. Lenient by
// design — local models love to wrap their output in reasoning
// before they land on the answer.
fn parse_label(raw: &str) -> Option<EngagementLabel> {
    let lower = raw.to_lowercase();
    // Match in priority order: distressed > declined > engaged > brief.
    // If the model says both "engaged" and "brief" we pick the more
    // serious signal, NOT the first one written.
    if lower.contains("distressed") { return Some(EngagementLabel::Distressed); }
    if lower.contains("declined")   { return Some(EngagementLabel::Declined); }
    if lower.contains("engaged")    { return Some(EngagementLabel::Engaged); }
    if lower.contains("brief")      { return Some(EngagementLabel::Brief); }
    None
}

// Pull the concern severity out of a `distressed:...` response. Defaults to
// `Concerning` when distress is present but no severity is stated — the safe,
// non-alarming default. Returns `None` only if the text mentions no distress
// at all (the caller already gates on the Distressed label).
fn parse_severity(raw: &str) -> Option<ConcernSeverity> {
    let lower = raw.to_lowercase();
    if !lower.contains("distress") { return None; }
    if lower.contains("acute") { Some(ConcernSeverity::Acute) }
    else { Some(ConcernSeverity::Concerning) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_label_handles_clean_output() {
        assert_eq!(parse_label("engaged"), Some(EngagementLabel::Engaged));
        assert_eq!(parse_label("brief"),   Some(EngagementLabel::Brief));
        assert_eq!(parse_label("declined"), Some(EngagementLabel::Declined));
        assert_eq!(parse_label("distressed"), Some(EngagementLabel::Distressed));
        assert_eq!(parse_label("distressed:acute"), Some(EngagementLabel::Distressed));
    }

    #[test]
    fn parse_severity_reads_distress_grade() {
        assert_eq!(parse_severity("distressed:acute"), Some(ConcernSeverity::Acute));
        assert_eq!(parse_severity("distressed:concerning"), Some(ConcernSeverity::Concerning));
        // distress with no grade defaults to the non-alarming Concerning.
        assert_eq!(parse_severity("distressed"), Some(ConcernSeverity::Concerning));
        // no distress mentioned → None (caller gates on the Distressed label).
        assert_eq!(parse_severity("engaged"), None);
    }

    #[test]
    fn parse_label_handles_capitalised_output() {
        assert_eq!(parse_label("Engaged"),    Some(EngagementLabel::Engaged));
        assert_eq!(parse_label("DECLINED"),   Some(EngagementLabel::Declined));
    }

    #[test]
    fn parse_label_handles_reasoning_preamble() {
        let raw = "<think>The user said 'not now' so this is...</think>\ndeclined";
        assert_eq!(parse_label(raw), Some(EngagementLabel::Declined));
    }

    #[test]
    fn parse_label_returns_none_on_garbage() {
        assert_eq!(parse_label(""), None);
        assert_eq!(parse_label("I don't know what to say"), None);
    }

    #[test]
    fn parse_label_priority_distressed_beats_others() {
        // If the model emits both labels (it shouldn't, but local
        // models can chatter), the more serious wins.
        assert_eq!(parse_label("the user seems engaged but distressed"),
                   Some(EngagementLabel::Distressed));
        assert_eq!(parse_label("brief reply, user has declined"),
                   Some(EngagementLabel::Declined));
    }

    #[test]
    fn build_user_prompt_truncates_long_messages() {
        let long = "x".repeat(2000);
        let out = build_user_prompt(&long, "ok");
        // The user-side block contains a truncated version with ellipsis.
        assert!(out.contains("…"));
        // Both sides are present.
        assert!(out.contains("User said"));
        assert!(out.contains("Assistant replied"));
    }
}
