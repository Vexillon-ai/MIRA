// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/memory_hook.rs
//! Memory integration hooks for [`AgentCore`].
//!
//! # Pre-generation hook
//! Queries the memory system for context relevant to the current user input
//! and injects it as an ephemeral prefix into the system prompt. The stored
//! system prompt is never mutated.
//!
//! # Post-generation hook
//! Fires a background task (fire-and-forget) after every completed turn to
//! extract and persist new memories from the conversation.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tracing::{debug, warn};

use crate::auth::LocalAuthService;
use crate::memory::MemorySystem;


// ─────────────────────────────────────────────────────────────────────────────

/// A compact, human-relative age for a recalled fact ("today", "yesterday",
/// "3 weeks ago"), so the model can tell a fresh fact from a stale one. Derived
/// from the fact's `created_at` (when MIRA recorded it), not its content.
fn relative_age(created_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let days = (now - created_at).num_days();
    match days {
        i64::MIN..=0 => "today".to_string(),
        1            => "yesterday".to_string(),
        2..=6        => format!("{days} days ago"),
        7..=13       => "a week ago".to_string(),
        14..=59      => format!("{} weeks ago", days / 7),
        60..=364     => format!("{} months ago", days / 30),
        _            => "over a year ago".to_string(),
    }
}

/// Format the per-turn "current date & time" anchor the model sees so it has a
/// reference for "now" (otherwise it must guess or call the `now` tool, and
/// treats stale recalled facts as current). Resolves to the given timezone;
/// falls back to UTC when the tz is absent or unparseable. Pure for testing;
/// callers pass `Utc::now()`.
fn format_time_anchor(now: DateTime<Utc>, tz_name: Option<&str>) -> String {
    if let Some(name) = tz_name {
        if let Ok(tz) = name.parse::<chrono_tz::Tz>() {
            let local = now.with_timezone(&tz);
            return format!(
                "[Current date & time: {} ({name}).]\n",
                local.format("%A %-d %B %Y, %-I:%M %p"),
            );
        }
    }
    format!("[Current date & time: {} (UTC).]\n", now.format("%A %-d %B %Y, %H:%M"))
}

/// Build the "now" anchor block for a turn, resolving the user's profile
/// timezone. Empty only if there is genuinely no clock (there always is) — it's
/// unconditional so every real turn knows what "now" is.
pub fn time_context_hook(auth: Option<&Arc<LocalAuthService>>, user_id: &str) -> String {
    let tz = auth
        .and_then(|a| a.get_profile(user_id).ok().flatten())
        .and_then(|p| p.timezone);
    format!("\n\n{}", format_time_anchor(Utc::now(), tz.as_deref()))
}

// ─────────────────────────────────────────────────────────────────────────────

/// Query the memory system and return a formatted context block to prepend to
/// the system prompt for this turn. Returns an empty string when no relevant
/// memories are found or memory is unavailable.
///
/// `group_ids` is the caller's resolved group membership; pass `&[]` when auth
/// isn't available. Memories surfaced here are reinforced automatically.
pub async fn pre_hook(
    memory:    &Arc<MemorySystem>,
    user_id:   &str,
    group_ids: &[String],
    input:     &str,
    top_k:     usize,
) -> String {
    // Knowledge-graph context (empty unless `memory.graph.enabled`): the
    // complete edge set for entities the question names, so counting/totalling
    // sees exact membership. Additive to flat retrieval — the A/B is the flag.
    let graph_lines = memory.graph_context_for_query(input, user_id, 60);

    match memory.search_visible_for_context(input, user_id, group_ids, top_k).await {
        Ok(items) if items.is_empty() && graph_lines.is_empty() => String::new(),
        Ok(items) => {
            debug!(
                "Memory pre-hook: {} relevant items + {} graph facts for user '{}'",
                items.len(), graph_lines.len(), user_id,
            );
            // Assertive framing: small models otherwise treat recalled facts
            // as optional flavour and deflect ("I'd need to check your
            // records…") even when the answer is right there — which tanked
            // aggregation / multi-session questions. Tell the model these ARE
            // its own knowledge and to answer directly, while still allowing
            // honest abstention when the needed fact isn't present.
            let mut block = String::from(
                "\n\n[What you already know about this user, recalled from your own memory of past \
                 conversations — each line is tagged with when you recorded it. Treat these as facts you \
                 personally hold (do NOT say you lack access, cannot recall, or need to check records), \
                 but reconcile them against the current date & time you've been given this turn: durable \
                 facts (their name, home, \
                 relationships, ongoing preferences) hold regardless of age, while time-bound ones \
                 (plans, trips, appointments, or a \"current\"/\"this week\" state) may already be in the \
                 PAST — an old \"tomorrow\" or \"next Friday\" has most likely already happened. Don't \
                 assume a dated plan is still upcoming or a past state still holds; if it matters and \
                 you're unsure, ask rather than assert. When the user asks you to count, total, sum, or \
                 list, do it directly from these facts. Only if the specific fact needed genuinely isn't \
                 listed here should you say you don't have that information.]\n"
            );
            // Stamp each fact with its recorded age so the model can tell fresh
            // from stale. No second similarity gate — `semantic_search` already
            // filtered at the configurable `memory.similarity_threshold`, so a
            // redundant hardcoded floor only re-dropped low-but-relevant facts
            // (which starved aggregation / multi-session questions).
            let now = Utc::now();
            for item in items {
                block.push_str(&format!(
                    "• {} (recorded {})\n",
                    item.content, relative_age(item.created_at, now),
                ));
            }
            // Graph facts are the *complete* set for the entities the question
            // names — flag them as authoritative for counting so the model
            // totals from this list, not the scattered facts above.
            if !graph_lines.is_empty() {
                block.push_str(
                    "\n[Complete records for what you asked about — use THIS list (it is the \
                     full set) for any counting, totalling, or listing:]\n"
                );
                for line in &graph_lines {
                    block.push_str(&format!("• {}\n", line));
                }
            }
            block
        }
        Err(e) => {
            // Non-fatal — memory unavailable doesn't stop the conversation.
            warn!("Memory pre-hook failed (non-fatal): {}", e);
            String::new()
        }
    }
}

/// Build a short identity block describing who the agent is talking to and
/// what the user calls the agent. Prepended to the system prompt for every
/// turn so cross-channel chats (Signal, Telegram) carry the same identity
/// the user established during onboarding on the web UI.
///
/// Returns an empty string when no auth service is wired, the user has no
/// profile row, or none of the surfaced fields are populated. Only the
/// fields the user actually filled in show up — we never invent defaults
/// like "they/them" that the user didn't choose.
pub fn profile_hook(
    auth:    Option<&Arc<LocalAuthService>>,
    user_id: &str,
) -> String {
    let Some(auth)    = auth                                     else { return String::new() };
    let Ok(Some(p))   = auth.get_profile(user_id)                else { return String::new() };

    let name = p.preferred_name.as_deref()
        .or(p.full_name.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let pronouns = p.pronouns.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let timezone = p.timezone.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let agent    = p.agent_name.as_deref().map(str::trim).filter(|s| !s.is_empty());

    if name.is_none() && pronouns.is_none() && timezone.is_none() && agent.is_none() {
        return String::new();
    }

    let mut block = String::from("\n\n[Identity for this turn:]\n");
    if let Some(n) = name {
        let mut who = format!("• You are talking to {n}");
        if let Some(pr) = pronouns { who.push_str(&format!(" ({pr})")); }
        if let Some(tz) = timezone { who.push_str(&format!(", in {tz}")); }
        who.push_str(".\n");
        block.push_str(&who);
    } else if let Some(tz) = timezone {
        block.push_str(&format!("• User timezone: {tz}.\n"));
    }
    if let Some(a) = agent {
        block.push_str(&format!(
            "• They call you {a} — answer to that name and use it when referring to yourself.\n",
        ));
    }
    block
}

/// Spawn a background task to extract and store new memories from the
/// just-completed conversation turn. Returns immediately; errors are logged.
pub fn post_hook(
    memory:    Arc<MemorySystem>,
    user_id:   String,
    channel:   String,
    user_msg:  String,
    assistant: String,
) {
    tokio::spawn(async move {
        let combined = format!("User: {}\n\nAssistant: {}", user_msg, assistant);
        match memory.auto_extract_and_store(&combined, &user_id, &channel).await {
            Ok(n) if n > 0 => debug!("Memory post-hook: stored {} memories for '{}'", n, user_id),
            Ok(_) => {},
            Err(e) => warn!("Memory post-hook failed (non-fatal): {}", e),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{LocalAuthService, NewUser, Role};
    use tempfile::tempdir;

    #[test]
    fn relative_age_buckets() {
        let now = Utc::now();
        let ago = |d: i64| now - chrono::Duration::days(d);
        assert_eq!(relative_age(now, now), "today");
        assert_eq!(relative_age(ago(1), now), "yesterday");
        assert_eq!(relative_age(ago(3), now), "3 days ago");
        assert_eq!(relative_age(ago(9), now), "a week ago");
        assert_eq!(relative_age(ago(28), now), "4 weeks ago");
        assert_eq!(relative_age(ago(90), now), "3 months ago");
        assert_eq!(relative_age(ago(800), now), "over a year ago");
        // A clock skew (future created_at) must not panic or go negative.
        assert_eq!(relative_age(now + chrono::Duration::days(2), now), "today");
    }

    #[test]
    fn time_anchor_resolves_timezone_and_falls_back_to_utc() {
        // A fixed instant: 2026-08-11 09:30 UTC.
        let now = DateTime::parse_from_rfc3339("2026-08-11T09:30:00Z")
            .unwrap().with_timezone(&Utc);

        let mel = format_time_anchor(now, Some("Australia/Melbourne"));
        assert!(mel.contains("Australia/Melbourne"), "got: {mel}");
        // Melbourne is UTC+10 in August (no DST) → 19:30 local, same date.
        assert!(mel.contains("11 August 2026"), "got: {mel}");
        assert!(mel.contains("7:30 PM"), "got: {mel}");

        // Unknown/blank tz → UTC, never panics.
        let utc = format_time_anchor(now, None);
        assert!(utc.contains("(UTC)"), "got: {utc}");
        assert!(utc.contains("09:30"), "got: {utc}");
        assert!(format_time_anchor(now, Some("Not/AZone")).contains("(UTC)"));
    }

    #[test]
    fn default_context_top_k_is_positive() {
        // top_k is now config-driven (memory.context_top_k); ensure the default
        // is a sensible, aggregation-friendly value.
        assert!(crate::config::MemoryConfig::default().context_top_k >= 5);
    }


    /// Mirror of the `auth.db` setup the gateway does at startup so the
    /// hook tests can populate a real `user_profile` row.
    fn auth_with_user() -> (tempfile::TempDir, Arc<LocalAuthService>, String) {
        let dir = tempdir().unwrap();
        let auth = LocalAuthService::new(
            &dir.path().join("auth.db"),
            "secret-jwt-key-for-tests".to_string(),
            7,
        ).unwrap();
        let user = auth.create_user(NewUser {
            username:     "alex".to_string(),
            display_name: Some("Alex".to_string()),
            email:        None,
            password:     "irrelevant".to_string(),
            role:         Role::User,
        }).unwrap();
        (dir, Arc::new(auth), user.id)
    }

    #[test]
    fn profile_hook_returns_empty_when_no_profile() {
        let (_dir, auth, user_id) = auth_with_user();
        // No profile fields populated yet.
        assert!(profile_hook(Some(&auth), &user_id).is_empty());
    }

    #[test]
    fn profile_hook_returns_empty_when_no_auth() {
        // Some test paths construct an AgentCore without auth installed —
        // the hook must not panic or fabricate identity in that case.
        assert!(profile_hook(None, "any-user").is_empty());
    }

    #[test]
    fn profile_hook_includes_agent_name_and_preferred_name() {
        let (_dir, auth, user_id) = auth_with_user();
        auth.upsert_profile_field(&user_id, "preferred_name", "Tarek").unwrap();
        auth.upsert_profile_field(&user_id, "pronouns",       "he/him").unwrap();
        auth.upsert_profile_field(&user_id, "timezone",       "Australia/Melbourne").unwrap();
        auth.upsert_profile_field(&user_id, "agent_name",     "Athena").unwrap();

        let block = profile_hook(Some(&auth), &user_id);
        assert!(block.contains("[Identity for this turn:]"));
        assert!(block.contains("You are talking to Tarek (he/him), in Australia/Melbourne."));
        assert!(block.contains("They call you Athena"));
    }

    #[test]
    fn profile_hook_falls_back_to_full_name_when_preferred_missing() {
        let (_dir, auth, user_id) = auth_with_user();
        auth.upsert_profile_field(&user_id, "full_name", "Tarek El Diab").unwrap();
        let block = profile_hook(Some(&auth), &user_id);
        assert!(block.contains("You are talking to Tarek El Diab"), "got: {block}");
    }
}
