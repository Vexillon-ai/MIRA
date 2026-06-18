// SPDX-License-Identifier: AGPL-3.0-or-later

// src/onboarding/preamble.rs
//! Build the per-user preamble that is layered onto the base system prompt
//! during normal (non-onboarding) chat.
//!
//! The preamble has two parts:
//!
//! 1. **Structured facts**: one or two lines derived from `user_profile`
//!    — preferred name, pronouns, timezone, contact hours, what to call
//!    MIRA. These are deterministic and small.
//!
//! 2. **Preferences**: the per-user `profile.md` included verbatim.
//!    Preferences like verbosity, autonomy, and off-limits topics must
//!    bias every turn, so they can't live in probabilistic memory.
//!
//! Target size: ≤500 tokens total. We truncate a very long `profile.md`
//! rather than risk blowing the context budget of small-context providers.
//!
//! A per-user cache ([`ProfilePreambleCache`]) keyed on
//! `user_profile.updated_at` + `profile.md` mtime means a normal chat turn
//! does one SQL fetch + one `statx` call and returns a cached string —
//! the cheap path for the common case where neither has changed.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

use crate::auth::{LocalAuthService, UserProfile};
use crate::onboarding::profile_file::{profile_md_path, read_profile_md};

/// Soft upper bound on the `profile.md` body included in the preamble.
/// Anything past this is dropped with a marker so the model sees the
/// truncation rather than silently missing context.
///
/// Roughly 400 tokens (≈1600 chars); combined with the structured-facts
/// block we stay under the ≤500 token budget from the plan.
const MAX_MD_BODY_CHARS: usize = 1600;

/// Compose the preamble string. Returns `None` when there is literally
/// nothing useful to inject — an unprofiled user chats with the unmodified
/// base persona.
pub fn build_profile_preamble(
    profile:    Option<&UserProfile>,
    profile_md: Option<&str>,
) -> Option<String> {
    let facts = structured_facts_line(profile);
    let md    = profile_md_body(profile_md);

    if facts.is_none() && md.is_none() {
        return None;
    }

    let mut out = String::with_capacity(256);
    out.push_str("## About this user\n\n");
    if let Some(line) = facts {
        out.push_str(&line);
        out.push_str("\n\n");
    }
    if let Some(body) = md {
        out.push_str(&body);
        if !out.ends_with('\n') { out.push('\n'); }
    }
    Some(out)
}

/// One compact line of structured facts: "You are talking to Alex (they/them).
/// Timezone: Australia/Sydney. Contact them between 480..1200
/// (minutes-from-midnight). They call you Milo."
fn structured_facts_line(profile: Option<&UserProfile>) -> Option<String> {
    let p = profile?;

    let mut line = String::new();
    let name = p.preferred_name.as_deref()
        .or(p.nickname.as_deref())
        .or(p.full_name.as_deref());

    match (name, p.pronouns.as_deref()) {
        (Some(n), Some(pr)) => { let _ = write!(line, "You are talking to {} ({}).", n, pr); }
        (Some(n), None)     => { let _ = write!(line, "You are talking to {}.", n); }
        (None, Some(pr))    => { let _ = write!(line, "The user's pronouns are {}.", pr); }
        (None, None)        => {}
    }

    if let Some(tz) = p.timezone.as_deref() {
        if !line.is_empty() { line.push(' '); }
        let _ = write!(line, "Timezone: {}.", tz);
    }
    if let (Some(s), Some(e)) = (p.contact_hours_start, p.contact_hours_end) {
        if !line.is_empty() { line.push(' '); }
        let _ = write!(line, "Contact hours: {}..{} (minutes from midnight).", s, e);
    }
    if let Some(agent_name) = p.agent_name.as_deref() {
        if !line.is_empty() { line.push(' '); }
        let _ = write!(line, "They call you {}.", agent_name);
    }

    if line.is_empty() { None } else { Some(line) }
}

/// Strip YAML frontmatter and the top-level `# Profile` heading from the
/// stored `profile.md`; only the section prose is useful to the model.
/// Returns `None` if the remaining body is empty (all sections blank).
fn profile_md_body(profile_md: Option<&str>) -> Option<String> {
    let raw = profile_md?;

    let trimmed = strip_frontmatter(raw);
    let trimmed = trimmed.trim_start();

    // Drop a leading "# Profile" heading — the `## About this user` header
    // we emit above already frames the block.
    let without_title = if trimmed.starts_with("# Profile") {
        match trimmed.find('\n') {
            Some(idx) => &trimmed[idx + 1..],
            None      => "",
        }
    } else {
        trimmed
    };

    // Drop sections whose body is empty/whitespace-only to avoid shipping
    // a wall of headers for a barely-populated profile.
    let compacted = compact_empty_sections(without_title);

    let compacted = compacted.trim();
    if compacted.is_empty() {
        return None;
    }

    if compacted.len() <= MAX_MD_BODY_CHARS {
        return Some(compacted.to_owned());
    }

    // Truncate at a line boundary and tag so the LLM knows there's more.
    let cutoff = compacted[..MAX_MD_BODY_CHARS]
        .rfind('\n')
        .unwrap_or(MAX_MD_BODY_CHARS);
    let mut truncated = compacted[..cutoff].to_owned();
    truncated.push_str("\n\n_(preferences truncated)_\n");
    Some(truncated)
}

/// Remove a leading `---\n...---\n` YAML frontmatter block if present.
fn strip_frontmatter(s: &str) -> &str {
    if !s.starts_with("---\n") && !s.starts_with("---\r\n") {
        return s;
    }
    let after = &s[4..];
    // Find the closing `---` on its own line.
    let mut offset = 0usize;
    for line in after.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return &after[offset + line.len()..];
        }
        offset += line.len();
    }
    s
}

/// Collapse `## Heading\n\n## NextHeading` (empty body) by dropping the
/// empty section entirely. Keeps the output from looking like a skeleton.
fn compact_empty_sections(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("## ") {
            // Collect the section body until the next `##` heading or EOF.
            let mut body_end = i + 1;
            while body_end < lines.len() && !lines[body_end].starts_with("## ") {
                body_end += 1;
            }
            let body_has_content = lines[i + 1..body_end].iter().any(|l| !l.trim().is_empty());
            if body_has_content {
                for l in &lines[i..body_end] {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            i = body_end;
        } else {
            out.push_str(line);
            out.push('\n');
            i += 1;
        }
    }
    out
}

// ── Cache ────────────────────────────────────────────────────────────────────

/// Per-user preamble cache. Keyed on `user_profile.updated_at` and the
/// `profile.md` file mtime — when either changes, the next resolve
/// rebuilds from scratch.
///
/// The staleness check is cheap (one SQL read of `updated_at`, one
/// `std::fs::metadata` call), so explicit invalidation isn't required;
/// profile writes just bump `updated_at` and this type picks it up on
/// the following turn.
#[derive(Default)]
pub struct ProfilePreambleCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
}

#[derive(Clone)]
struct CacheEntry {
    profile_updated_at: i64,
    md_mtime_secs:      Option<u64>,
    /// `None` means "we computed it and there was nothing to inject" —
    /// cached to avoid recomputing.
    preamble:           Option<String>,
}

impl ProfilePreambleCache {
    pub fn new() -> Self { Self::default() }

    /// Return the preamble for `user_id`, reusing the cached value when
    /// neither the DB row nor the `profile.md` file has changed. Returns
    /// `Ok(None)` when there's nothing to inject.
    pub fn resolve(
        &self,
        user_id:  &str,
        auth:     &LocalAuthService,
        data_dir: &Path,
    ) -> Option<String> {
        // Fetch lightweight staleness keys first. On error we log and
        // return None — better to skip injection for one turn than to
        // crash a chat request.
        let profile = match auth.get_profile(user_id) {
            Ok(p)  => p,
            Err(e) => {
                tracing::warn!("preamble cache: get_profile({}) failed: {}", user_id, e);
                return None;
            }
        };
        let md_path = profile_md_path(data_dir, user_id);
        let md_mtime_secs = std::fs::metadata(&md_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let updated_at = profile.as_ref().map(|p| p.updated_at).unwrap_or(0);

        // Fast path: cache hit with matching version keys.
        {
            let map = self.entries.lock().unwrap();
            if let Some(entry) = map.get(user_id) {
                if entry.profile_updated_at == updated_at && entry.md_mtime_secs == md_mtime_secs {
                    return entry.preamble.clone();
                }
            }
        }

        // Miss — rebuild.
        let md_body = read_profile_md(data_dir, user_id).ok().flatten();
        let preamble = build_profile_preamble(profile.as_ref(), md_body.as_deref());

        let mut map = self.entries.lock().unwrap();
        map.insert(user_id.to_owned(), CacheEntry {
            profile_updated_at: updated_at,
            md_mtime_secs,
            preamble:           preamble.clone(),
        });
        preamble
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn base_profile() -> UserProfile {
        UserProfile {
            user_id:         "u1".to_string(),
            preferred_name:  Some("Alex".to_string()),
            pronouns:        Some("they/them".to_string()),
            timezone:        Some("Australia/Sydney".to_string()),
            agent_name:      Some("Milo".to_string()),
            contact_hours_start: Some(480),
            contact_hours_end:   Some(1200),
            ..Default::default()
        }
    }

    #[test]
    fn empty_profile_and_no_md_returns_none() {
        assert!(build_profile_preamble(None, None).is_none());
    }

    #[test]
    fn structured_line_combines_name_and_pronouns_and_tz() {
        let p = base_profile();
        let line = structured_facts_line(Some(&p)).unwrap();
        assert!(line.contains("You are talking to Alex (they/them)."));
        assert!(line.contains("Timezone: Australia/Sydney."));
        assert!(line.contains("Contact hours: 480..1200"));
        assert!(line.contains("They call you Milo."));
    }

    #[test]
    fn name_fallback_order_is_preferred_then_nickname_then_full() {
        let p = UserProfile {
            full_name: Some("Alexandra Smith".to_string()),
            nickname:  Some("Lex".to_string()),
            ..Default::default()
        };
        assert!(structured_facts_line(Some(&p)).unwrap().starts_with("You are talking to Lex"));
    }

    #[test]
    fn pronouns_only_still_produces_a_sentence() {
        let p = UserProfile {
            pronouns: Some("she/her".to_string()),
            ..Default::default()
        };
        assert_eq!(structured_facts_line(Some(&p)).unwrap(), "The user's pronouns are she/her.");
    }

    #[test]
    fn frontmatter_and_title_are_stripped() {
        let md = "---\nuser_id: u1\nversion: 1\n---\n\n# Profile\n\n## Communication style\n\nBe terse.\n";
        let body = profile_md_body(Some(md)).unwrap();
        assert!(!body.contains("---"));
        assert!(!body.contains("# Profile"));
        assert!(body.contains("## Communication style"));
        assert!(body.contains("Be terse."));
    }

    #[test]
    fn empty_sections_are_dropped() {
        let md = "# Profile\n\n## Communication style\n\n## Autonomy\n\nAsk before acting.\n\n## Goals\n\n";
        let body = profile_md_body(Some(md)).unwrap();
        assert!(!body.contains("Communication style"));
        assert!(body.contains("## Autonomy"));
        assert!(body.contains("Ask before acting"));
        assert!(!body.contains("Goals")); // empty section dropped
    }

    #[test]
    fn md_all_empty_sections_returns_none() {
        let md = "---\nuser_id: u1\nversion: 1\n---\n\n# Profile\n\n## Communication style\n\n## Autonomy\n\n";
        assert!(profile_md_body(Some(md)).is_none());
    }

    #[test]
    fn long_md_is_truncated_with_marker() {
        let mut md = String::from("# Profile\n\n## Autonomy\n\n");
        // Pad well past the limit.
        for i in 0..200 { md.push_str(&format!("- line {}\n", i)); }
        let body = profile_md_body(Some(&md)).unwrap();
        assert!(body.len() <= MAX_MD_BODY_CHARS + 64);
        assert!(body.contains("(preferences truncated)"));
    }

    #[test]
    fn preamble_combines_facts_and_md_under_budget() {
        let p = base_profile();
        let md = "# Profile\n\n## Autonomy\n\nAsk before doing anything destructive.\n";
        let out = build_profile_preamble(Some(&p), Some(md)).unwrap();
        assert!(out.starts_with("## About this user"));
        assert!(out.contains("You are talking to Alex"));
        assert!(out.contains("Ask before doing anything destructive"));
        // Guard against the preamble ever drifting past the budget.
        assert!(out.len() < 2400, "preamble too large: {} chars", out.len());
    }

    #[test]
    fn preamble_returns_none_when_profile_empty_and_md_empty_body() {
        let md = "---\nuser_id: u1\n---\n\n# Profile\n\n## Goals\n\n";
        assert!(build_profile_preamble(None, Some(md)).is_none());
    }

    // ── Cache tests ──────────────────────────────────────────────────────

    use tempfile::TempDir;

    fn setup_cache_env() -> (TempDir, std::sync::Arc<LocalAuthService>, String) {
        let dir  = TempDir::new().unwrap();
        let auth = std::sync::Arc::new(
            LocalAuthService::new(
                &dir.path().join("auth.db"),
                "test-secret".to_owned(),
                7,
            ).unwrap()
        );
        let user = auth.create_user(crate::auth::NewUser {
            username:     "u1".to_owned(),
            password:     "password1".to_owned(),
            display_name: None,
            email:        None,
            role:         crate::auth::Role::User,
        }).unwrap();
        (dir, auth, user.id)
    }

    #[test]
    fn cache_returns_none_for_unprofiled_user() {
        let (dir, auth, user_id) = setup_cache_env();
        let cache = ProfilePreambleCache::new();
        let out = cache.resolve(&user_id, &auth, dir.path());
        assert!(out.is_none(), "unprofiled user should not yield a preamble");
    }

    #[test]
    fn cache_picks_up_profile_updates_via_updated_at() {
        let (dir, auth, user_id) = setup_cache_env();
        let cache = ProfilePreambleCache::new();

        // Initial: nothing to inject.
        assert!(cache.resolve(&user_id, &auth, dir.path()).is_none());

        // Write a field → the next resolve must rebuild.
        auth.upsert_profile_field(&user_id, "preferred_name", "Alex").unwrap();
        let first = cache.resolve(&user_id, &auth, dir.path()).unwrap();
        assert!(first.contains("You are talking to Alex"));

        // Second call with no change must return the same string (cache hit).
        let second = cache.resolve(&user_id, &auth, dir.path()).unwrap();
        assert_eq!(first, second);

        // Writing again advances `updated_at` and invalidates.
        // Sleep ≥1ms to guarantee the unix-ms timestamp actually increments
        // on systems where two successive writes can land in the same ms.
        std::thread::sleep(std::time::Duration::from_millis(2));
        auth.upsert_profile_field(&user_id, "timezone", "Australia/Sydney").unwrap();
        let third = cache.resolve(&user_id, &auth, dir.path()).unwrap();
        assert!(third.contains("Timezone: Australia/Sydney"));
    }

    #[test]
    fn cache_picks_up_profile_md_mtime_changes() {
        let (dir, auth, user_id) = setup_cache_env();
        let cache = ProfilePreambleCache::new();

        auth.upsert_profile_field(&user_id, "preferred_name", "Alex").unwrap();
        let first = cache.resolve(&user_id, &auth, dir.path()).unwrap();
        assert!(!first.contains("Ask before"));

        // Write a profile.md section, sleeping 1s so mtime measurably changes
        // (mtime granularity on some filesystems is ~1s).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        crate::onboarding::profile_file::write_profile_section(
            dir.path(), &user_id, "autonomy", "Ask before doing anything destructive.",
        ).unwrap();

        let second = cache.resolve(&user_id, &auth, dir.path()).unwrap();
        assert!(second.contains("Ask before doing anything destructive"));
    }
}
