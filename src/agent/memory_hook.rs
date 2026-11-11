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
/// Structured outcome of the memory pre-hook: the rendered context block plus
/// which providers fed the turn and which were unavailable (for the UI pills /
/// badge via `StreamEvent::MemoryContext`).
#[derive(Debug, Default, Clone)]
pub struct MemoryContextResult {
    pub block: String,
    pub served: Vec<String>,
    pub unavailable: Vec<String>,
}

/// Back-compat wrapper returning just the rendered block.
pub async fn pre_hook(
    memory:    &Arc<MemorySystem>,
    user_id:   &str,
    group_ids: &[String],
    input:     &str,
    top_k:     usize,
) -> String {
    pre_hook_detailed(memory, user_id, group_ids, input, top_k).await.block
}

/// As [`pre_hook`], but also reports which providers served / were unavailable.
pub async fn pre_hook_detailed(
    memory:    &Arc<MemorySystem>,
    user_id:   &str,
    group_ids: &[String],
    input:     &str,
    top_k:     usize,
) -> MemoryContextResult {
    // B1b: recall through the provider seam. Today that's the single built-in
    // provider; the router is where multiple providers (files, external HTTP, MCP) will
    // merge. The router keeps the three statuses distinct — crucially, a store
    // that is DOWN surfaces as an explicit "unavailable" block here rather than
    // collapsing into an empty result the model reads as "no such memory" (the
    // old `Err(_) => String::new()` bug). A.5's dropped graph-completeness claim
    // is now the earned `can_assert_complete` capability, enforced in the router.
    use crate::memory::provider::MemoryProvider;
    use crate::memory::provider::builtin::BuiltinMemoryProvider;
    use crate::memory::router::MemoryRouter;

    let provider: Arc<dyn MemoryProvider> = Arc::new(BuiltinMemoryProvider::new(Arc::clone(memory)));
    let router = MemoryRouter::single(provider);
    // This convenience wrapper keeps the explicit unavailable notice (the voice
    // suppression is applied only on the core path that knows the channel) and
    // the builtin-only store, which keys on the UUID — no username/email needed.
    pre_hook_via_router(&router, user_id, group_ids, input, top_k, true, None, None).await
}

/// Recall through an explicit router (the configured provider set, B2d). This
/// is what the turn path uses; [`pre_hook_detailed`] is the builtin-only
/// convenience for callers without a configured set (tests, the guardian).
pub async fn pre_hook_via_router(
    router:    &crate::memory::router::MemoryRouter,
    user_id:   &str,
    group_ids: &[String],
    input:     &str,
    top_k:     usize,
    // When false, a down provider is NOT announced in the
    // injected context (voice turns that aren't about memory just answer). The
    // `unavailable` list is still reported for the UI pills regardless.
    include_unavailable: bool,
    // The account's username + email, so a provider configured with
    // `user_id_field = username|email` can send that instead of the UUID.
    username: Option<&str>,
    email: Option<&str>,
) -> MemoryContextResult {
    use crate::memory::provider::MemoryQuery;
    let mut q = MemoryQuery::new(user_id, input, top_k);
    q.group_ids = group_ids.to_vec();
    q.username = username.map(str::to_string);
    q.email = email.map(str::to_string);
    // Budget 0 here: the wiki+memory window cap is applied downstream by
    // `cap_rag_context` (core.rs). Per-source co-budget lands with the UI work.
    let recalled = router.recall(&q, 0).await;
    let served = recalled.served.clone();
    let unavailable: Vec<String> = recalled.unavailable.iter().map(|(s, _)| s.clone()).collect();
    if recalled.is_empty() {
        return MemoryContextResult { block: String::new(), served, unavailable };
    }
    debug!(
        "Memory pre-hook: {} facts, {} unavailable for user '{}'",
        recalled.facts.len(), recalled.unavailable.len(), user_id,
    );
    MemoryContextResult { block: render_recalled(&recalled, include_unavailable), served, unavailable }
}

// A.7: softened, trust-tiered framing for recalled notes. Drops the old "facts
// you personally hold / don't hedge" certainty claim; keeps per-line ages,
// reconcile-against-now, and permission to aggregate; frames a missing fact as
// "not recorded", never "false".
const RECALLED_NOTES_HEADER: &str =
    "\n\n[Recalled from your memory of past conversations with this user — each line is tagged \
     with when you recorded it. This material is useful but may be incomplete or outdated, so \
     treat it as your own notes rather than as verified fact, and reconcile it against the \
     current date & time you've been given this turn: durable facts (their name, home, \
     relationships, ongoing preferences) usually still hold regardless of age, while time-bound \
     ones (plans, trips, appointments, or a \"current\"/\"this week\" state) may already be in \
     the PAST — an old \"tomorrow\" or \"next Friday\" has most likely already happened. Don't \
     assume a dated plan is still upcoming or a past state still holds. You may use these notes \
     to answer directly, including to count, total, sum or list; where a note looks stale, \
     self-contradictory or simply wrong, say so or check with the user rather than asserting it. \
     If the fact you need isn't here, say you don't have it — a missing note means you haven't \
     recorded it, not that it is false.]\n";

// Verified facts (a citations-capable provider's attested rows) are presented as
// established — the tier the softened header deliberately doesn't claim.
const RECALLED_VERIFIED_HEADER: &str =
    "\n[Established facts from your records, with their source — you can rely on these, though \
     still flag any that look stale or contradictory:]\n";

// Facts from an EXTERNAL memory source that the source does NOT attest as
// verified (the memory SPI's `reported` / `unverified` trust levels — e.g. a
// row the source found in some material but hasn't confirmed). These are
// neither the user's own notes nor established fact: they are attributed,
// unverified findings from another store. Rule: render such an item as the
// source passage it is — an attributed quote — never as an asserted fact, and
// never drop it (a subject held only at this level must not read as "no
// memory"). Each line carries the source's own trust label.
const RECALLED_SOURCED_HEADER: &str =
    "\n[Found in an external memory source you can consult — attributed, and NOT verified. Each \
     line shows its source and that source's own trust label. Treat these as the source's \
     findings, not your own notes and not established fact: attribute them to the source and do \
     not assert them as confirmed. A line labelled \"found in source, not verified\" is a source \
     PASSAGE about the subject, so quote or paraphrase it as such rather than stating it as a \
     fact. Still surface what's here — never say you have no information about the subject when a \
     source passage is present, just be clear about what it is:]\n";

/// Render a [`RecalledMemory`](crate::memory::router::RecalledMemory) into the
/// context block. Notes vs verified facts are tiered (A.7); an unavailable
/// provider gets an explicit block so the model never reads "down" as "absent".
fn render_recalled(r: &crate::memory::router::RecalledMemory, include_unavailable: bool) -> String {
    use crate::memory::provider::{Speaker, Trust};
    let now = Utc::now();
    let mut block = String::new();

    // Three presentation tiers (memory SPI trust levels + A.7):
    //   • verified — a citations-capable provider's attested rows (SPI `verified`).
    //   • sourced  — an external provider's UNverified findings (SPI `reported` /
    //                `unverified`): attributed, never asserted, never dropped.
    //   • notes    — the user's own unverified notes (builtin store).
    let mut verified = Vec::new();
    let mut sourced  = Vec::new();
    let mut notes    = Vec::new();
    for f in &r.facts {
        if matches!(f.trust, Trust::Verified) { verified.push(f); }
        // `reported`/`unverified` from an external source are attributed findings,
        // never the user's own notes.
        else if matches!(f.trust, Trust::Reported) || matches!(f.speaker, Speaker::External) { sourced.push(f); }
        else { notes.push(f); }
    }

    if !verified.is_empty() {
        block.push_str(RECALLED_VERIFIED_HEADER);
        for f in &verified {
            let src = if f.source_id.is_empty() { String::new() }
                      else { format!(" [{}]", f.source_id) };
            block.push_str(&format!("• {}{}{}\n", f.content, src, age_suffix(f, now)));
        }
    }
    if !sourced.is_empty() {
        block.push_str(RECALLED_SOURCED_HEADER);
        for f in &sourced {
            let src = if f.source_id.is_empty() { String::new() }
                      else { format!(" [{}]", f.source_id) };
            block.push_str(&format!(
                "• \"{}\"{} ({}){}\n", f.content, src, sourced_label(f), age_suffix(f, now)));
        }
    }
    if !notes.is_empty() {
        block.push_str(RECALLED_NOTES_HEADER);
        for f in &notes {
            block.push_str(&format!("• {}{}\n", f.content, age_suffix(f, now)));
        }
    }
    // Earned completeness — only reached when a provider with
    // `can_assert_complete` set it (the router strips it otherwise).
    if r.has_complete_set() {
        block.push_str(
            "\n[The list above is the COMPLETE set for what you asked about — use it directly \
             for any counting, totalling, or listing.]\n",
        );
    }
    // A down/warming provider: say so explicitly, never let it read as "absent"
    // — UNLESS the caller suppressed it (a voice reply should
    // just answer when memory is down, not announce it, unless asked about memory).
    if include_unavailable {
        for (source, reason) in &r.unavailable {
            block.push_str(&format!(
                "\n[Your {source} memory store is unavailable this turn ({reason}). Do not conclude \
                 the user has no such information — say the store could not be reached.]\n",
            ));
        }
        // A WITHHELD result: the source HAS related material but didn't return it
        // for this query (e.g. a near/ambiguous match). NOT "no such memory" — tell
        // the model to refine or ask, and surface the source's reason.
        for (source, reason) in &r.withheld {
            block.push_str(&format!(
                "\n[Your {source} memory store has related information it did NOT return for this \
                 query ({reason}). This is NOT \"no such memory\" — a result was withheld; help the \
                 user narrow it down or ask a more specific question rather than saying nothing is \
                 recorded.]\n",
            ));
        }
    }
    block
}

/// Does the user's message look like it's about memory itself (recall, remember,
/// forget, "do you know …")? Used to decide whether a voice turn should surface
/// a memory-unavailable notice.
pub fn mentions_memory(input: &str) -> bool {
    let l = input.to_ascii_lowercase();
    ["remember", "memory", "recall", "forget", "do you know", "did i tell", "what do you know"]
        .iter().any(|kw| l.contains(kw))
}

/// " (recorded 3 weeks ago)" when the fact carries a record time, else "".
fn age_suffix(f: &crate::memory::provider::MemoryFact, now: DateTime<Utc>) -> String {
    match f.recorded_at {
        Some(t) => format!(" (recorded {})", relative_age(t, now)),
        None => String::new(),
    }
}

/// The human trust label to show beside a sourced (external, unverified) fact.
/// Prefers the provider's own `trust_label` (the memory SPI's optional per-fact
/// label, which `wire_fact` folds into `provider_meta`); otherwise falls back to
/// the generic SPI trust level.
fn sourced_label(f: &crate::memory::provider::MemoryFact) -> String {
    use crate::memory::provider::Trust;
    let meta = &f.provider_meta;
    // A free-text label from the source wins (shown verbatim, per the SPI).
    if let Some(l) = meta.get("trust_label").and_then(|v| v.as_str()) {
        let l = l.trim();
        if !l.is_empty() { return l.to_string(); }
    }
    // Otherwise fall back to the typed SPI trust level.
    match f.trust {
        Trust::Reported => "reported by the source".into(),
        _               => "unverified".into(),
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
    // Categories the user has opted into (empty = allow all).
    allowed_categories: Vec<String>,
    // The user explicitly asked to remember something this turn.
    explicit:  bool,
) {
    // A.6: feed ONLY the user turn to the heuristic extractor. Its patterns are
    // user-phrased ("my name is…", "i live in…"), so concatenating the assistant
    // turn made it emit "User lives in/is from X" from the assistant's own text.
    // The assistant turn is intentionally unused here.
    let _ = assistant;
    tokio::spawn(async move {
        match memory.auto_extract_and_store(&user_msg, &user_id, &channel, &allowed_categories, explicit).await {
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

    use crate::memory::provider::{MemoryFact, Speaker, Trust};
    use crate::memory::router::RecalledMemory;

    fn note(content: &str) -> MemoryFact {
        MemoryFact {
            id: None, content: content.into(), score: Some(0.9), recorded_at: Some(Utc::now()),
            valid_at: None, source_id: "builtin".into(), provenance: None, confidence: None,
            speaker: Speaker::User, trust: Trust::Unverified, complete_set: false,
            provider_meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn render_unavailable_is_explicit_never_empty() {
        // The key Part-B property on the live path: a down store renders a
        // "could not be reached" block, not an empty string the model reads as
        // "the user has no such memory".
        let r = RecalledMemory {
            facts: vec![],
            unavailable: vec![("ext-http".into(), "connection refused".into())],
            withheld: vec![],
            served: vec![],
        };
        let block = render_recalled(&r, true);
        assert!(!block.is_empty());
        assert!(block.contains("ext-http") && block.contains("unavailable"));
        assert!(block.contains("Do not conclude"));
    }

    // A voice turn that isn't about memory suppresses the
    // "store unavailable" notice (just answers); an about-memory turn keeps it.
    #[test]
    fn render_suppresses_unavailable_notice_when_requested() {
        let r = RecalledMemory {
            facts: vec![], unavailable: vec![("ext-http".into(), "connection refused".into())],
            withheld: vec![],
            served: vec![],
        };
        let suppressed = render_recalled(&r, false);
        assert!(suppressed.is_empty(), "no unavailable notice when suppressed");
        let shown = render_recalled(&r, true);
        assert!(shown.contains("unavailable"), "notice shown when included");
    }

    #[test]
    fn mentions_memory_detects_memory_questions() {
        assert!(mentions_memory("what do you remember about my trip?"));
        assert!(mentions_memory("do you know my dog's name"));
        assert!(mentions_memory("forget what I said"));
        assert!(!mentions_memory("what's the weather today"));
    }

    #[test]
    fn render_notes_use_the_softened_header_with_ages() {
        let r = RecalledMemory { facts: vec![note("User likes tea")], unavailable: vec![], withheld: vec![], served: vec!["builtin".into()] };
        let block = render_recalled(&r, true);
        assert!(block.contains("User likes tea"));
        assert!(block.contains("treat it as your own notes"), "softened header");
        assert!(block.contains("(recorded"), "per-line age stamp");
        assert!(!block.contains("facts you personally hold"), "old assertive wording gone");
    }

    #[test]
    fn render_verified_facts_use_the_established_header() {
        let mut f = note("User works at Anthropic");
        f.trust = Trust::Verified;
        f.source_id = "ext-http".into();
        let r = RecalledMemory { facts: vec![f], unavailable: vec![], withheld: vec![], served: vec!["ext-http".into()] };
        let block = render_recalled(&r, true);
        assert!(block.contains("Established facts"), "verified tier header");
        assert!(block.contains("[ext-http]"), "source labelled");
    }

    // An external, unverified fact carrying an SPI trust level + label.
    fn sourced(content: &str, trust_level: &str, label: &str) -> MemoryFact {
        let mut f = note(content);
        f.speaker = Speaker::External;
        f.trust = match trust_level {
            "verified" => Trust::Verified,
            "reported" => Trust::Reported,
            "unknown"  => Trust::Unknown,
            _          => Trust::Unverified,
        };
        f.source_id = "ext-http".into();
        f.provider_meta = serde_json::json!({ "trust": trust_level, "trust_label": label });
        f
    }

    #[test]
    fn render_sourced_passage_is_attributed_never_asserted_never_dropped() {
        // An unverified source passage must appear as an attributed quote, not as
        // established fact, and must not vanish (the "appears as no memory" case).
        let f = sourced(
            "Jordan Vale appeared in the 1994 film.",
            "unverified", "found in source, not verified");
        let r = RecalledMemory { facts: vec![f], unavailable: vec![], withheld: vec![], served: vec!["ext-http".into()] };
        let block = render_recalled(&r, true);
        assert!(block.contains("external memory source"), "sourced-tier header present");
        assert!(block.contains("Jordan Vale appeared"), "passage not dropped");
        assert!(block.contains("found in source, not verified"), "source's own trust label shown");
        assert!(block.contains("[ext-http]"), "attributed to the source");
        // Never promoted into the established/verified tier.
        assert!(!block.contains("Established facts"), "an unverified source fact is not established");
        // Not framed as the user's own note.
        assert!(!block.contains("treat it as your own notes"), "not a personal note");
    }

    #[test]
    fn render_sourced_reported_label() {
        let f = sourced("Subject directed a 1980 film.", "reported", "externally confirmed");
        let r = RecalledMemory { facts: vec![f], unavailable: vec![], withheld: vec![], served: vec!["ext-http".into()] };
        let block = render_recalled(&r, true);
        assert!(block.contains("external memory source"), "sourced-tier header");
        assert!(block.contains("externally confirmed"), "the source's label");
        assert!(!block.contains("Established facts"), "reported is not the verified tier");
    }

    #[test]
    fn render_tiers_coexist_and_builtin_stays_a_note() {
        // verified + unverified-source + builtin note → three blocks.
        let mut attested = note("User's employer is on record.");
        attested.trust = Trust::Verified;
        attested.source_id = "ext-http".into();
        let srcd = sourced("A passage about the subject.", "unverified", "found in source, not verified");
        let own = note("User likes tea");
        let r = RecalledMemory {
            facts: vec![attested, srcd, own], unavailable: vec![], withheld: vec![],
            served: vec!["ext-http".into(), "builtin".into()],
        };
        let block = render_recalled(&r, true);
        assert!(block.contains("Established facts"), "verified block");
        assert!(block.contains("external memory source"), "sourced block");
        assert!(block.contains("treat it as your own notes"), "notes block");
        assert!(block.contains("User likes tea") && block.contains("A passage about the subject."));
    }

    #[test]
    fn render_withheld_surfaces_the_reason_not_absence() {
        // A withheld result (near/ambiguous match) must render WITH its reason and
        // never read as "nothing recorded".
        let r = RecalledMemory {
            facts: vec![], unavailable: vec![],
            withheld: vec![("ext-http".into(), "a near match exists; be more specific".into())],
            served: vec![],
        };
        let block = render_recalled(&r, true);
        assert!(block.contains("did NOT return"), "withheld framing");
        assert!(block.contains("be more specific"), "the source's reason is surfaced");
        assert!(block.contains("NOT \"no such memory\""), "not read as absence");
        // Suppressed on a non-memory voice turn, like the unavailable notice.
        assert!(render_recalled(&r, false).is_empty(), "suppressed when include_unavailable=false");
    }

    #[test]
    fn sourced_label_falls_back_to_level_then_generic() {
        // No trust_label → fall back to the typed SPI trust level on the fact.
        let mut f = sourced("x", "reported", "");
        f.provider_meta = serde_json::Value::Null;
        assert_eq!(f.trust, Trust::Reported);
        assert_eq!(sourced_label(&f), "reported by the source");
        // Neither label nor `reported` level → generic.
        let mut u = sourced("x", "unverified", "");
        u.provider_meta = serde_json::Value::Null;
        assert_eq!(sourced_label(&u), "unverified");
    }

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
        auth.upsert_profile_field(&user_id, "agent_name",     "Nova").unwrap();

        let block = profile_hook(Some(&auth), &user_id);
        assert!(block.contains("[Identity for this turn:]"));
        assert!(block.contains("You are talking to Tarek (he/him), in Australia/Melbourne."));
        assert!(block.contains("They call you Nova"));
    }

    #[test]
    fn profile_hook_falls_back_to_full_name_when_preferred_missing() {
        let (_dir, auth, user_id) = auth_with_user();
        auth.upsert_profile_field(&user_id, "full_name", "Tarek El Diab").unwrap();
        let block = profile_hook(Some(&auth), &user_id);
        assert!(block.contains("You are talking to Tarek El Diab"), "got: {block}");
    }
}
