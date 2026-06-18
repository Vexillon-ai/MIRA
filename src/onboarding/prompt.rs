// SPDX-License-Identifier: AGPL-3.0-or-later

// src/onboarding/prompt.rs
//! Build the system prompt used during `mode=onboarding` conversations.
//!
//! The prompt steers the LLM to run the onboarding flow: ask warm, one-to-two
//! questions per turn; extract multiple answers when volunteered; call the
//! onboarding tools (`record_profile`, `skip_topic`, `mark_group_complete`,
//! `complete_onboarding`, `resolve_timezone`) after each extraction; never
//! re-ask a completed or skipped key.
//!
//! Target size: ~1500 tokens, well under any provider's context budget.

use std::collections::HashSet;
use std::fmt::Write as _;

use serde_json::Value;

use crate::auth::UserProfile;
use crate::onboarding::{OnboardingSchema, WriteTarget};

/// Build the full onboarding system prompt. `base_persona` is the agent's
/// normal persona (typically `AgentCore::system_prompt`); onboarding
/// instructions layer on top so the user still feels they're chatting with
/// the same assistant, just in an onboarding context.
///
/// `progress_json` is the raw string stored in `user_profile.onboarding_progress`
/// — parsed leniently, so a corrupted/missing blob just means "fresh start".
pub fn build_onboarding_prompt(
    base_persona:  &str,
    schema:        &OnboardingSchema,
    progress_json: Option<&str>,
    profile:       Option<&UserProfile>,
) -> String {
    let progress = progress_json
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let completed: HashSet<String> = progress.get("completed_groups")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let skipped_keys: HashSet<String> = progress.get("skipped_keys")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let mut out = String::with_capacity(2048);

    // ── Base persona ────────────────────────────────────────────────────────
    out.push_str(base_persona.trim_end());
    out.push_str("\n\n");

    // ── Flow instructions ───────────────────────────────────────────────────
    out.push_str("\
## Onboarding mode

You are running the user's first-time onboarding. Your job is to get to know \
them well enough to personalize future interactions. Be warm, concise, and \
curious — this is a conversation, not a form.

Rules:
- Ask one or two questions per turn. Never dump the whole list.
- If the user volunteers extra information, extract it — don't make them \
  repeat themselves on the next turn.
- After extracting any answer, record it with `record_profile`. One tool \
  call per answer. Call it *before* your next conversational reply, so the \
  user never sees a stale repeat-ask.
- If the user wants to skip a question, call `skip_topic` and move on \
  without protest.
- When every question in a group is answered or skipped, call \
  `mark_group_complete` with that group's id *immediately*, before asking \
  the first question of the next group. This keeps the user's progress \
  bar in sync; the server also auto-advances as a safety net, but your \
  explicit call is the authoritative signal.
- Never re-ask a key that appears in Completed groups or Skipped keys below.
- When all required groups are done, call `complete_onboarding` and give a \
  short, friendly closing message — don't keep asking optional questions \
  after the user signals they're done.
- For timezone questions, call `resolve_timezone` first with the city they \
  name (argument name: `location_text`); use the returned IANA zone as the \
  value for `record_profile`.
- Questions are grouped for flow, but the user can answer out of order. Go \
  with the conversation; use the group list as guidance, not a script.
- Anything you record via `memory.seed` questions (work, hobbies) is \
  written into MIRA's long-term memory and will be searchable in future \
  conversations — mention this casually so the user knows their answers \
  feed MIRA's memory rather than just filling a form.

");

    // ── Progress snapshot ───────────────────────────────────────────────────
    write!(out, "## Progress\n\n").ok();
    if completed.is_empty() {
        out.push_str("Completed groups: (none yet)\n");
    } else {
        let mut names: Vec<&String> = completed.iter().collect();
        names.sort();
        let list = names.into_iter().cloned().collect::<Vec<_>>().join(", ");
        writeln!(out, "Completed groups: {}", list).ok();
    }
    if skipped_keys.is_empty() {
        out.push_str("Skipped keys: (none)\n");
    } else {
        let mut names: Vec<&String> = skipped_keys.iter().collect();
        names.sort();
        let list = names.into_iter().cloned().collect::<Vec<_>>().join(", ");
        writeln!(out, "Skipped keys: {}", list).ok();
    }
    out.push('\n');

    // ── Next group to focus on ──────────────────────────────────────────────
    let next_group = schema.groups.iter().find(|g| !completed.contains(&g.id));
    match next_group {
        Some(g) => {
            writeln!(out, "## Current focus: `{}` — {}", g.id, g.label).ok();
            if g.optional {
                out.push_str("(This group is optional — fine to skip the whole thing if the user isn't keen.)\n");
            }
            out.push('\n');
            for q in &g.questions {
                if skipped_keys.contains(&q.key) { continue; }
                write!(out, "- `{}`", q.key).ok();
                if let Some(target) = &q.writes_to {
                    write!(out, " → {}", describe_target(target)).ok();
                }
                if !q.required {
                    out.push_str(" (optional)");
                }
                if let Some(hint) = &q.prompt_hint {
                    write!(out, ": {}", hint).ok();
                }
                if let Some(opts) = &q.options {
                    write!(out, " — one of: {}", opts.join(", ")).ok();
                }
                if let Some(tool) = &q.helper_tool {
                    write!(out, " [helper: `{}`]", tool).ok();
                }
                out.push('\n');
            }
            out.push('\n');
        }
        None => {
            out.push_str("## All groups complete\n\n\
All required groups are marked complete. Call `complete_onboarding` now \
and thank the user.\n\n");
        }
    }

    // ── Remaining groups (short summary) ────────────────────────────────────
    let remaining: Vec<_> = schema.groups.iter()
        .filter(|g| !completed.contains(&g.id))
        .filter(|g| next_group.map_or(true, |n| n.id != g.id))
        .collect();
    if !remaining.is_empty() {
        out.push_str("## Upcoming groups\n\n");
        for g in remaining {
            let opt = if g.optional { " *(optional)*" } else { "" };
            writeln!(out, "- `{}`: {}{}", g.id, g.label, opt).ok();
        }
        out.push('\n');
    }

    // ── Already-known profile fields (don't re-ask) ─────────────────────────
    if let Some(profile) = profile {
        let known = known_profile_lines(profile);
        if !known.is_empty() {
            out.push_str("## Already known about the user\n\n");
            for line in known {
                writeln!(out, "- {}", line).ok();
            }
            out.push_str("\nDo not re-ask these unless the user asks to correct them.\n\n");
        }
    }

    out
}

fn describe_target(t: &WriteTarget) -> String {
    match t {
        WriteTarget::User(c)         => format!("user.{}", c),
        WriteTarget::UserProfile(c)  => format!("user_profile.{}", c),
        WriteTarget::ProfileMd(s)    => format!("profile_md.{}", s),
        WriteTarget::MemorySeed      => "memory.seed".to_owned(),
    }
}

fn known_profile_lines(p: &UserProfile) -> Vec<String> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<String>, label: &str, v: Option<&str>| {
        if let Some(s) = v { if !s.is_empty() { out.push(format!("{}: {}", label, s)); } }
    };
    push(&mut out, "full_name",      p.full_name.as_deref());
    push(&mut out, "preferred_name", p.preferred_name.as_deref());
    push(&mut out, "nickname",       p.nickname.as_deref());
    push(&mut out, "pronouns",       p.pronouns.as_deref());
    push(&mut out, "birth_date",     p.birth_date.as_deref());
    push(&mut out, "timezone",       p.timezone.as_deref());
    push(&mut out, "locale",         p.locale.as_deref());
    push(&mut out, "agent_name",     p.agent_name.as_deref());
    if let Some(h) = p.height_cm { out.push(format!("height_cm: {}", h)); }
    if let Some(w) = p.weight_kg { out.push(format!("weight_kg: {}", w)); }
    if let (Some(s), Some(e)) = (p.contact_hours_start, p.contact_hours_end) {
        out.push(format!("contact_hours: {}..{} (minutes-from-midnight)", s, e));
    }
    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> &'static str {
        "You are MIRA, a helpful assistant."
    }

    #[test]
    fn fresh_user_prompt_points_at_first_group() {
        let schema = OnboardingSchema::bundled().unwrap();
        let prompt = build_onboarding_prompt(base(), &schema, None, None);

        assert!(prompt.starts_with("You are MIRA"));
        assert!(prompt.contains("## Onboarding mode"));
        assert!(prompt.contains("record_profile"));
        assert!(prompt.contains("Completed groups: (none yet)"));
        // First bundled group is `name` — should be the current focus.
        assert!(prompt.contains("## Current focus: `name`"));
        assert!(prompt.contains("preferred_name"));
    }

    #[test]
    fn mid_flow_prompt_reflects_progress_and_hides_done_groups() {
        let schema = OnboardingSchema::bundled().unwrap();
        let progress = r#"{
            "completed_groups": ["name", "location_time"],
            "skipped_keys": ["weight_kg"]
        }"#;
        let prompt = build_onboarding_prompt(base(), &schema, Some(progress), None);

        assert!(prompt.contains("Completed groups: location_time, name"));
        assert!(prompt.contains("Skipped keys: weight_kg"));
        // Next non-completed group is `personal`.
        assert!(prompt.contains("## Current focus: `personal`"));
        // Skipped key must not appear as a question line.
        let lines: Vec<&str> = prompt.lines().collect();
        let found_weight = lines.iter().any(|l| l.trim_start().starts_with("- `weight_kg`"));
        assert!(!found_weight, "skipped key should be omitted from current-focus list");
    }

    #[test]
    fn all_done_prompt_tells_llm_to_complete() {
        let schema = OnboardingSchema::bundled().unwrap();
        let completed: Vec<String> = schema.groups.iter().map(|g| g.id.clone()).collect();
        let progress = serde_json::json!({
            "completed_groups": completed,
            "skipped_keys":    [],
        }).to_string();
        let prompt = build_onboarding_prompt(base(), &schema, Some(&progress), None);
        assert!(prompt.contains("All groups complete"));
        assert!(prompt.contains("complete_onboarding"));
    }

    #[test]
    fn known_profile_suppresses_reask_hint() {
        let schema = OnboardingSchema::bundled().unwrap();
        let profile = UserProfile {
            user_id:        "u1".to_string(),
            preferred_name: Some("Alex".to_string()),
            timezone:       Some("Australia/Sydney".to_string()),
            ..Default::default()
        };
        let prompt = build_onboarding_prompt(base(), &schema, None, Some(&profile));
        assert!(prompt.contains("Already known about the user"));
        assert!(prompt.contains("preferred_name: Alex"));
        assert!(prompt.contains("timezone: Australia/Sydney"));
    }

    #[test]
    fn prompt_stays_under_soft_limit() {
        let schema = OnboardingSchema::bundled().unwrap();
        let prompt = build_onboarding_prompt(base(), &schema, None, None);
        // Very rough heuristic: 1 token ≈ 4 chars. Target < 1500 tokens → ~6000 chars.
        assert!(prompt.len() < 8000, "onboarding prompt grew too large: {} chars", prompt.len());
    }

    #[test]
    fn bad_progress_json_falls_back_to_empty() {
        let schema = OnboardingSchema::bundled().unwrap();
        let prompt = build_onboarding_prompt(base(), &schema, Some("not json"), None);
        // Lenient: treat as empty progress, still valid.
        assert!(prompt.contains("Completed groups: (none yet)"));
    }
}
