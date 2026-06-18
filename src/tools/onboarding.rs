// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/onboarding.rs
//! System-tier tools the onboarding flow uses to capture profile data.
//!
//! These tools never appear in user-facing palettes — their `visibility()`
//! returns `ToolVisibility::system("onboarding")` so [`ToolRegistry::list_for_flow`]
//! only exposes them when a conversation is in `mode = "onboarding"`.
//!
//! ## Context injection
//!
//! Onboarding tools need the caller's `user_id` (and, for book-keeping, the
//! active `conversation_id`) but do not trust the LLM to supply them. The
//! chat handler — when it dispatches a tool call on an onboarding
//! conversation — stamps both into the args object before execution.
//! This keeps the model API surface small while preventing an LLM from
//! accidentally writing to another user's profile.
//!
//! The stamp is a pair of reserved keys:
//!
//! ```json
//! { "_user_id": "...", "_conversation_id": "...", ... other tool args }
//! ```
//!
//! Missing `_user_id` is an error — the tool refuses to run.
//!
//! ## Shared services
//!
//! All five tools need the same handful of Arc'd dependencies. They're
//! bundled into [`OnboardingServices`] once at registration time so each
//! tool struct stays lightweight (one `Arc` clone).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::auth::LocalAuthService;
use crate::history::HistoryStore;
use crate::memory::{Category, MemorySource, MemorySystem, Scope};
use crate::onboarding::{write_profile_section, OnboardingSchema, WriteTarget};
use crate::tools::{Tier, Tool, ToolArgs, ToolResult, ToolVisibility};
use crate::wiki::{Provenance, WikiOp, WikiPath, WikiRegistry};
use crate::MiraError;
use tracing::{debug, warn};

// ── Shared services ──────────────────────────────────────────────────────────

/// Dependencies every onboarding tool needs. Constructed once by the gateway
/// and cloned into each tool struct.
pub struct OnboardingServices {
    pub auth:     Arc<LocalAuthService>,
    pub history:  Arc<HistoryStore>,
    pub memory:   Arc<MemorySystem>,
    pub schema:   Arc<OnboardingSchema>,
    pub data_dir: PathBuf,
    /// Optional wiki bridge — when wired, onboarding-captured values
    /// are mirrored into the user's wiki `profile.md` so the wiki
    /// reflects what onboarding learned. `None` in test/minimal builds
    /// without a wiki registry; the bridge is best-effort and never
    /// fails the underlying onboarding write.
    pub wiki:     Option<Arc<WikiRegistry>>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn require_user_id(args: &Value) -> Result<String, MiraError> {
    args.get("_user_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| MiraError::ToolError(
            "onboarding tool called without _user_id (chat handler must inject)".to_string()
        ))
}

/// Convert a JSON value to the string form we store in text columns. `null`
/// comes back as `None`; scalars (string/number/bool) stringify; anything else
/// is rejected so the LLM can retry.
fn value_to_string(v: &Value) -> Result<Option<String>, MiraError> {
    match v {
        Value::Null         => Ok(None),
        Value::String(s)    => Ok(Some(s.clone())),
        Value::Number(n)    => Ok(Some(n.to_string())),
        Value::Bool(b)      => Ok(Some(b.to_string())),
        _                   => Err(MiraError::ToolError(format!(
            "value must be a scalar (string/number/bool/null), got: {}", v
        ))),
    }
}

/// Extract an `Option<i64>` from a JSON value. Accepts `null`, an integer,
/// or a numeric string.
fn value_to_i64(v: &Value) -> Result<Option<i64>, MiraError> {
    match v {
        Value::Null        => Ok(None),
        Value::Number(n)   => n.as_i64().map(Some).ok_or_else(|| {
            MiraError::ToolError(format!("numeric value does not fit i64: {}", n))
        }),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s)   => s.trim().parse::<i64>().map(Some).map_err(|e| {
            MiraError::ToolError(format!("could not parse '{}' as integer: {}", s, e))
        }),
        _                  => Err(MiraError::ToolError(format!(
            "expected integer or numeric string, got: {}", v
        ))),
    }
}

// ── Progress JSON book-keeping ───────────────────────────────────────────────

/// Read-modify-write the `onboarding_progress` blob. The blob is opaque to
/// the DB layer, so we treat a missing/invalid blob as an empty progress
/// object — the LLM shouldn't be blocked by stale state.
fn update_progress<F>(auth: &LocalAuthService, user_id: &str, f: F) -> Result<Value, MiraError>
where
    F: FnOnce(&mut Value),
{
    let existing = auth.get_profile(user_id)?
        .and_then(|p| p.onboarding_progress)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));

    let mut progress = if existing.is_object() { existing } else { json!({}) };
    {
        let obj = progress.as_object_mut().expect("ensured object above");
        obj.entry("completed_groups").or_insert_with(|| json!([]));
        obj.entry("skipped_keys").or_insert_with(|| json!([]));
        obj.entry("answered_keys").or_insert_with(|| json!([]));
        // Subset of `completed_groups`: groups the LLM *explicitly* marked
        // complete (vs swept along by jump-ahead). Used by
        // `complete_onboarding` to enforce the silent-skip guard —
        // jump-ahead advances the UI liberally but can't satisfy the
        // finalization check on its own.
        obj.entry("explicitly_completed_groups").or_insert_with(|| json!([]));
    }
    f(&mut progress);

    let serialized = serde_json::to_string(&progress)
        .map_err(|e| MiraError::ToolError(format!("progress JSON serialize: {}", e)))?;
    auth.set_onboarding_progress(user_id, &serialized)?;
    Ok(progress)
}

fn push_unique(arr: &mut Value, v: &str) {
    if let Some(list) = arr.as_array_mut() {
        let exists = list.iter().any(|x| x.as_str() == Some(v));
        if !exists {
            list.push(Value::String(v.to_string()));
        }
    }
}

fn string_set(obj: &serde_json::Map<String, Value>, key: &str) -> HashSet<String> {
    obj.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// Record a tool-side progress delta and, as a safety net, auto-advance any
/// group whose questions are all either answered or skipped.
///
/// Small/local models sometimes forget to call `mark_group_complete` between
/// groups, leaving the UI progress bar stuck mid-flow. The LLM still owns
/// the happy path (it may call the tool explicitly), but this helper
/// guarantees the blob reflects reality after every record/skip: the UI
/// doesn't rely on model discipline for something the server can compute.
///
/// Also performs a **jump-ahead auto-complete** (see
/// [`auto_advance_passed_groups`]) for any earlier group the LLM has clearly
/// moved past — either optional (silent skip), or required-with-activity
/// (model handled at least one key, then moved on without marking complete).
fn record_and_autoadvance(
    auth:         &LocalAuthService,
    schema:       &OnboardingSchema,
    user_id:      &str,
    answered_key: Option<&str>,
    skipped_key:  Option<&str>,
) -> Result<(), MiraError> {
    update_progress(auth, user_id, |p| {
        let Some(obj) = p.as_object_mut() else { return };

        if let Some(k) = answered_key {
            if let Some(arr) = obj.get_mut("answered_keys") { push_unique(arr, k); }
        }
        if let Some(k) = skipped_key {
            if let Some(arr) = obj.get_mut("skipped_keys") { push_unique(arr, k); }
        }

        let answered = string_set(obj, "answered_keys");
        let skipped  = string_set(obj, "skipped_keys");

        for g in &schema.groups {
            let all_handled = g.questions.iter()
                .all(|q| answered.contains(&q.key) || skipped.contains(&q.key));
            if all_handled {
                if let Some(arr) = obj.get_mut("completed_groups") {
                    push_unique(arr, &g.id);
                }
            }
        }

        let latest_key = answered_key.or(skipped_key);
        if let Some(k) = latest_key {
            if let Some(idx) = schema.groups.iter()
                .position(|g| g.questions.iter().any(|q| q.key == k))
            {
                auto_advance_passed_groups(obj, schema, idx);
            }
        }
    })?;
    Ok(())
}

/// Reasons `finalize_onboarding` refuses to stamp the user as done. Carrying
/// the list of untouched groups lets the caller render an actionable error
/// (LLM path quotes them back to the model; HTTP path returns them in the
/// response body).
#[derive(Debug)]
pub enum FinalizeError {
    /// At least one required group has no activity and wasn't explicitly
    /// completed. The `Vec` lists the offending group ids in schema order.
    UntouchedRequiredGroups(Vec<String>),
    /// DB / storage layer failure bubbling up from the auth service.
    Storage(MiraError),
}

impl From<MiraError> for FinalizeError {
    fn from(e: MiraError) -> Self { FinalizeError::Storage(e) }
}

/// Shared finalization path: advance all groups, run the silent-skip guard,
/// then stamp `onboarded_at` and store the optional summary. Used by:
/// - `CompleteOnboardingTool` (LLM-invoked, with summary)
/// - `POST /api/onboarding/finalize` (user-invoked backstop when the model
///   forgets to call the tool)
/// - `try_auto_finalize` (server-side self-heal after tool activity brings
///   every required group across the guard)
///
/// Idempotent: if `onboarded_at` is already set the guard is skipped and
/// the call is a no-op success.
pub fn finalize_onboarding(
    auth:    &LocalAuthService,
    schema:  &OnboardingSchema,
    user_id: &str,
    summary: Option<&str>,
) -> Result<(), FinalizeError> {
    // Idempotence: already onboarded → nothing to do.
    if let Some(p) = auth.get_profile(user_id)? {
        if p.onboarded_at.is_some() {
            return Ok(());
        }
    }

    // Silent-skip guard — run BEFORE any mutation. `try_auto_finalize`
    // fires on every record/skip/mark, so a premature jump-ahead here would
    // advance `completed_groups` past groups that still have a live question
    // pending, breaking the progress-bar semantics.
    //
    // The guard reads activity + explicit marks only, neither of which is
    // touched by jump-ahead, so reordering is safe.
    let progress = auth.get_profile(user_id)?
        .and_then(|p| p.onboarding_progress)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));
    let obj = progress.as_object();
    let answered: HashSet<String> = obj.map(|m| string_set(m, "answered_keys")).unwrap_or_default();
    let skipped:  HashSet<String> = obj.map(|m| string_set(m, "skipped_keys")).unwrap_or_default();
    let explicit: HashSet<String> = obj.map(|m| string_set(m, "explicitly_completed_groups")).unwrap_or_default();

    let untouched: Vec<String> = schema.groups.iter()
        .filter(|g| !g.optional)
        .filter(|g| {
            let has_activity = g.questions.iter()
                .any(|q| answered.contains(&q.key) || skipped.contains(&q.key));
            !has_activity && !explicit.contains(&g.id)
        })
        .map(|g| g.id.clone())
        .collect();

    if !untouched.is_empty() {
        return Err(FinalizeError::UntouchedRequiredGroups(untouched));
    }

    // Guard passed — sweep every group into `completed_groups`, clear the
    // live conversation pointer, stash the summary, stamp onboarded.
    update_progress(auth, user_id, |p| {
        let Some(obj) = p.as_object_mut() else { return };
        auto_advance_passed_groups(obj, schema, schema.groups.len());
        obj.remove("active_conversation_id");
        if let Some(s) = summary {
            obj.insert("summary".to_string(), Value::String(s.to_owned()));
        }
    })?;
    auth.mark_onboarded(user_id)?;
    Ok(())
}

/// Best-effort auto-finalize: if every required group has activity or an
/// explicit mark, stamp `onboarded_at` and release the active conversation.
/// Swallows the "untouched groups" error path silently — this runs after
/// every record/skip/mark so it mustn't explode when the flow is mid-stream.
/// Storage errors still propagate (they'd indicate a real DB problem).
///
/// Returns `Ok(true)` when it actually flipped state, so callers / tests
/// can assert when finalization happens.
fn try_auto_finalize(
    auth:    &LocalAuthService,
    schema:  &OnboardingSchema,
    user_id: &str,
) -> Result<bool, MiraError> {
    match finalize_onboarding(auth, schema, user_id, None) {
        Ok(()) => {
            // `Ok` from a no-op-when-already-onboarded path is still "fine",
            // but we only want to report flips. Re-check the profile to see
            // if we *just* set it vs it was already set.
            let onboarded = auth.get_profile(user_id)?
                .and_then(|p| p.onboarded_at)
                .is_some();
            Ok(onboarded)
        }
        Err(FinalizeError::UntouchedRequiredGroups(_)) => Ok(false),
        Err(FinalizeError::Storage(e))                 => Err(e),
    }
}

/// Auto-complete every earlier group the LLM has moved past. Shared between
/// `record_and_autoadvance` (triggered on record/skip), `MarkGroupCompleteTool`
/// (triggered on explicit completion), and `CompleteOnboardingTool` (triggered
/// on finalization).
///
/// The rule is simple by design: **any forward motion advances the bar for
/// every prior group, regardless of whether their questions were handled.**
/// Reasoning-distilled local models routinely narrate through a group
/// conversationally without firing `record_profile` / `skip_topic`, which
/// would strand the progress bar behind the conversation. Treating forward
/// tool activity as the authoritative "moved on" signal keeps the UI
/// honest with what the user sees in chat.
///
/// The silent-skip guard isn't here — it lives in
/// [`CompleteOnboardingTool::execute`], which refuses to finalize a required
/// group that shows **no** activity in `answered_keys`/`skipped_keys` and
/// was never explicitly marked complete. That keeps the progress bar
/// liberal while still preventing a runaway model from declaring onboarding
/// done without touching a whole topic.
fn auto_advance_passed_groups(
    obj:              &mut serde_json::Map<String, Value>,
    schema:           &OnboardingSchema,
    latest_group_idx: usize,
) {
    for g in schema.groups.iter().take(latest_group_idx) {
        if let Some(arr) = obj.get_mut("completed_groups") {
            push_unique(arr, &g.id);
        }
    }
}

// ── record_profile ───────────────────────────────────────────────────────────

pub struct RecordProfileTool {
    services: Arc<OnboardingServices>,
}

impl RecordProfileTool {
    pub fn new(services: Arc<OnboardingServices>) -> Self { Self { services } }
}

#[async_trait]
impl Tool for RecordProfileTool {
    fn name(&self) -> &str { "record_profile" }
    fn description(&self) -> &str {
        "Record an onboarding answer. `key` must match a question in the onboarding schema; \
         the target storage is derived from the schema's `writes_to`."
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::system("onboarding") }
    fn tier(&self) -> Tier { Tier::System }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["key", "value"],
            "properties": {
                "key":   { "type": "string",  "description": "Onboarding question key (e.g. preferred_name)." },
                "value": {                    "description": "Raw answer. Type depends on the target column." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = require_user_id(&args)?;
        let key = args.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
            MiraError::ToolError("record_profile: missing `key`".to_string())
        })?.to_owned();
        let value = args.get("value").cloned().unwrap_or(Value::Null);

        let (_group, question) = self.services.schema.question(&key).ok_or_else(|| {
            MiraError::ToolError(format!("record_profile: unknown question key '{}'", key))
        })?;

        let target = question.writes_to.as_ref().ok_or_else(|| {
            MiraError::ToolError(format!(
                "record_profile: question '{}' has no writes_to target", key
            ))
        })?;

        let svc = &self.services;
        match target {
            WriteTarget::User(col) => {
                let s = value_to_string(&value)?.ok_or_else(|| {
                    MiraError::ToolError(format!("record_profile: '{}' requires non-null value", key))
                })?;
                // `avatar` is the only allowed users-column today.
                if col == "avatar" {
                    svc.auth.set_avatar(&user_id, Some(&s))?;
                } else {
                    return Err(MiraError::ToolError(format!(
                        "record_profile: users.{} not supported", col
                    )));
                }
            }
            WriteTarget::UserProfile(col) => {
                // Virtual fan-out — split "HH:MM-HH:MM" (or "start-end" minutes)
                // into two integer columns.
                if col == "contact_hours_start_end" {
                    let (start, end) = parse_contact_hours(&value)?;
                    svc.auth.upsert_profile_field(&user_id, "contact_hours_start", start)?;
                    svc.auth.upsert_profile_field(&user_id, "contact_hours_end",   end)?;
                } else if is_integer_profile_col(col) {
                    let n = value_to_i64(&value)?;
                    svc.auth.upsert_profile_field(&user_id, static_profile_col(col)?, n)?;
                } else {
                    let s = value_to_string(&value)?;
                    svc.auth.upsert_profile_field(&user_id, static_profile_col(col)?, s)?;
                }
            }
            WriteTarget::ProfileMd(section) => {
                let body = value_to_string(&value)?.unwrap_or_default();
                write_profile_section(&svc.data_dir, &user_id, section, &body)
                    .map_err(|e| MiraError::ToolError(format!("profile.md write: {}", e)))?;
            }
            WriteTarget::MemorySeed => {
                let text = value_to_string(&value)?.ok_or_else(|| {
                    MiraError::ToolError(format!("record_profile: '{}' requires non-null text", key))
                })?;
                let tags = vec!["onboarding".to_string(), key.clone()];
                // Scope to the user so reset/cleanup can target just their
                // seeds — a shared MemorySystem serves all users.
                svc.memory.store_scoped(
                    text,
                    seed_category_for(&key),
                    tags,
                    Some(MemorySource::Imported("onboarding".to_string())),
                    Scope::User,
                    Some(&user_id),
                    &user_id,
                    &[user_id.clone()],
                    None, None, None,
                ).await?;
            }
        }

        // Best-effort mirror of onboarding-captured data into the
        // user's wiki profile.md. Auth DB / legacy profile.md remain
        // the source of truth; wiki is a readable reflection.
        sync_onboarding_to_wiki(svc, &user_id, target, &value).await;

        record_and_autoadvance(&svc.auth, &svc.schema, &user_id, Some(&key), None)?;
        try_auto_finalize(&svc.auth, &svc.schema, &user_id)?;
        Ok(ToolResult::success(format!("recorded {}", key)))
    }
}

// ── Onboarding -> wiki bridge ────────────────────────────────────────────────

/// Mirror a just-applied onboarding write into the user's wiki
/// `profile.md`. Best-effort: any failure logs a warning and returns —
/// the underlying onboarding write has already succeeded and is the
/// source of truth.
///
/// Mapping:
/// - `WriteTarget::UserProfile(_)` → re-render the "Personal details"
///   section from the post-write `user_profile` row.
/// - `WriteTarget::ProfileMd(section)` → mirror the just-written
///   section body straight to the wiki page (same heading name).
/// - `WriteTarget::MemorySeed` → render the seed under
///   "About me" so hobbies / work summary / goals surface alongside
///   the structured fields.
/// - `WriteTarget::User(_)` → no-op (avatar is the only field there).
async fn sync_onboarding_to_wiki(
    svc:    &OnboardingServices,
    user_id: &str,
    target:  &WriteTarget,
    value:   &Value,
) {
    let Some(wiki_reg) = svc.wiki.as_ref() else {
        debug!("onboarding->wiki: registry not wired, skipping");
        return;
    };
    let wiki = match wiki_reg.for_user(user_id) {
        Ok(w) => w,
        Err(e) => { warn!("onboarding->wiki: for_user('{user_id}') failed: {e}"); return; }
    };
    let path = match WikiPath::parse("profile.md") {
        Ok(p) => p,
        Err(e) => { warn!("onboarding->wiki: WikiPath parse failed: {e}"); return; }
    };
    let provenance = Provenance {
        source: "onboarding".into(),
        turn_id: None,
        conversation_id: None,
        actor: "onboarding".into(),
    };

    match target {
        WriteTarget::User(_) => {} // avatar — not mirrored
        WriteTarget::UserProfile(_) => {
            let profile = match svc.auth.get_profile(user_id) {
                Ok(Some(p)) => p,
                Ok(None) => return, // row vanished — nothing to render
                Err(e) => { warn!("onboarding->wiki: get_profile failed: {e}"); return; }
            };
            let body = render_personal_details(&profile);
            if body.trim().is_empty() { return; }
            if let Err(e) = wiki.submit_and_apply(
                WikiOp::UpdateSection {
                    path,
                    section: "Personal details".into(),
                    body,
                },
                provenance,
            ) {
                warn!("onboarding->wiki: personal-details update failed: {e}");
            }
        }
        WriteTarget::ProfileMd(section_key) => {
            // The legacy section keys and the wiki ## headings are
            // mapped by PROFILE_SECTIONS; the heading is what the wiki
            // already uses.
            let heading = match crate::onboarding::profile_md_heading(section_key) {
                Some(h) => h.to_string(),
                None => return, // unknown section — skip rather than create odd headings
            };
            let body = value_to_string(value).ok().flatten().unwrap_or_default();
            if let Err(e) = wiki.submit_and_apply(
                WikiOp::UpdateSection { path, section: heading, body },
                provenance,
            ) {
                warn!("onboarding->wiki: section '{section_key}' update failed: {e}");
            }
        }
        WriteTarget::MemorySeed => {
            // Render under "About me" as a single bullet, appended.
            // Append (not replace) so successive seeds accumulate.
            let Some(text) = value_to_string(value).ok().flatten() else { return; };
            let trimmed = text.trim();
            if trimmed.is_empty() { return; }
            let body = format!("- {trimmed}\n");
            if let Err(e) = wiki.submit_and_apply(
                WikiOp::AppendSection {
                    path,
                    section: "About me".into(),
                    body,
                },
                provenance,
            ) {
                warn!("onboarding->wiki: about-me append failed: {e}");
            }
        }
    }
}

/// Render the structured `user_profile` columns into a markdown body
/// suitable for the "## Personal details" section of `profile.md`.
/// Fields are emitted as a bullet list, skipping anything null/empty.
fn render_personal_details(p: &crate::auth::UserProfile) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut push = |label: &str, value: String| {
        let v = value.trim().to_string();
        if !v.is_empty() {
            lines.push(format!("- **{label}:** {v}"));
        }
    };
    if let Some(s) = &p.full_name      { push("Full name",      s.clone()); }
    if let Some(s) = &p.preferred_name { push("Preferred name", s.clone()); }
    if let Some(s) = &p.nickname       { push("Nickname",       s.clone()); }
    if let Some(s) = &p.pronouns       { push("Pronouns",       s.clone()); }
    if let Some(s) = &p.birth_date     { push("Birth date",     s.clone()); }
    if let Some(n) = p.height_cm       { push("Height",         format!("{n} cm")); }
    if let Some(n) = p.weight_kg       { push("Weight",         format!("{n} kg")); }
    if let Some(s) = &p.eye_color      { push("Eye colour",     s.clone()); }
    if let Some(s) = &p.hair_color     { push("Hair colour",    s.clone()); }
    if let Some(s) = &p.timezone       { push("Timezone",       s.clone()); }
    if let Some(s) = &p.locale         { push("Locale",         s.clone()); }
    if let (Some(start), Some(end)) = (p.contact_hours_start, p.contact_hours_end) {
        let fmt = |m: i64| {
            let m = m.rem_euclid(24 * 60);
            format!("{:02}:{:02}", m / 60, m % 60)
        };
        push("Contact hours", format!("{}–{}", fmt(start), fmt(end)));
    }
    if let Some(s) = &p.agent_name     { push("Name for the assistant", s.clone()); }
    lines.join("\n")
}

/// Summary of a backfill run — what got mirrored to the wiki.
#[derive(Debug, Default, Clone)]
pub struct RebuildSummary {
    pub personal_details:   bool,
    pub sections:           Vec<String>,
    pub about_me_seed_count: usize,
}

/// One-shot replay that reconstructs a user's wiki `profile.md` from
/// their current onboarding state. Idempotent — uses `UpdateSection`
/// (replace-or-create) for every emitted section, so re-running is
/// safe and won't append duplicates. Returns a summary of what was
/// written so callers (CLI / admin tools) can report progress.
///
/// Useful for users who completed onboarding before the bridge was
/// wired in, or after a wiki reset.
pub fn rebuild_wiki_profile(
    auth:     &LocalAuthService,
    memory:   &MemorySystem,
    wiki_reg: &WikiRegistry,
    data_dir: &std::path::Path,
    user_id:  &str,
) -> Result<RebuildSummary, MiraError> {
    let wiki = wiki_reg.for_user(user_id)
        .map_err(|e| MiraError::ToolError(format!("wiki for_user: {e}")))?;
    let path = WikiPath::parse("profile.md")
        .map_err(|e| MiraError::ToolError(format!("WikiPath parse: {e}")))?;
    let prov = Provenance {
        source: "onboarding-backfill".into(),
        turn_id: None,
        conversation_id: None,
        actor: "onboarding".into(),
    };
    let mut summary = RebuildSummary::default();

    // 1. Personal details from user_profile columns.
    if let Some(profile) = auth.get_profile(user_id)? {
        let body = render_personal_details(&profile);
        if !body.trim().is_empty() {
            wiki.submit_and_apply(
                WikiOp::UpdateSection {
                    path: path.clone(),
                    section: "Personal details".into(),
                    body,
                },
                prov.clone(),
            ).map_err(|e| MiraError::ToolError(format!("wiki write personal details: {e}")))?;
            summary.personal_details = true;
        }
    }

    // 2. Legacy profile.md sections (Communication style / Goals / etc.).
    let legacy = crate::onboarding::read_profile_md(data_dir, user_id)
        .map_err(|e| MiraError::ToolError(format!("read profile.md: {e}")))?
        .unwrap_or_default();
    for (key, heading) in crate::onboarding::PROFILE_SECTIONS {
        // Treat the legacy file as the source of truth — only mirror
        // non-empty sections; leave any wiki edits the user made by
        // hand to stand for sections that have no legacy content yet.
        let Some(body) = crate::wiki::page::read_section(&legacy, heading) else { continue; };
        if body.trim().is_empty() { continue; }
        wiki.submit_and_apply(
            WikiOp::UpdateSection {
                path: path.clone(),
                section: heading.to_string(),
                body,
            },
            prov.clone(),
        ).map_err(|e| MiraError::ToolError(format!("wiki write '{key}': {e}")))?;
        summary.sections.push(heading.to_string());
    }

    // 3. About me — onboarding-tagged memory seeds, rendered as bullets.
    // Use a broad search query and filter on the "onboarding" tag client-
    // side. There's no dedicated tag-index API on MemorySystem; the search
    // path returns the memories we need plus a few extras to filter.
    let seeds = memory
        .search_visible("onboarding", user_id, &[])
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.tags.iter().any(|t| t == "onboarding"))
        .collect::<Vec<_>>();
    if !seeds.is_empty() {
        let body = seeds.iter()
            .map(|m| format!("- {}", m.content.trim()))
            .collect::<Vec<_>>()
            .join("\n");
        wiki.submit_and_apply(
            WikiOp::UpdateSection {
                path: path.clone(),
                section: "About me".into(),
                body,
            },
            prov,
        ).map_err(|e| MiraError::ToolError(format!("wiki write about me: {e}")))?;
        summary.about_me_seed_count = seeds.len();
    }

    Ok(summary)
}

// The profile column list is mirrored in `src/onboarding/schema.rs`. We route
// the raw &str back to a static &'static str here so `upsert_profile_field`
// can accept it — the DB layer trusts callers not to pass user input.
fn static_profile_col(col: &str) -> Result<&'static str, MiraError> {
    match col {
        "full_name"           => Ok("full_name"),
        "preferred_name"      => Ok("preferred_name"),
        "nickname"            => Ok("nickname"),
        "pronouns"            => Ok("pronouns"),
        "birth_date"          => Ok("birth_date"),
        "height_cm"           => Ok("height_cm"),
        "weight_kg"           => Ok("weight_kg"),
        "eye_color"           => Ok("eye_color"),
        "hair_color"          => Ok("hair_color"),
        "timezone"            => Ok("timezone"),
        "locale"              => Ok("locale"),
        "agent_name"          => Ok("agent_name"),
        "contact_hours_start" => Ok("contact_hours_start"),
        "contact_hours_end"   => Ok("contact_hours_end"),
        other => Err(MiraError::ToolError(format!(
            "record_profile: unsupported user_profile column '{}'", other
        ))),
    }
}

fn is_integer_profile_col(col: &str) -> bool {
    matches!(col, "height_cm" | "weight_kg" | "contact_hours_start" | "contact_hours_end")
}

/// Accept either `"HH:MM-HH:MM"` or `{"start": "HH:MM", "end": "HH:MM"}` or
/// `{"start": int, "end": int}`. Returns minutes-from-midnight in the user's
/// local timezone, matching the `contact_hours_start/end` column semantics.
fn parse_contact_hours(v: &Value) -> Result<(i64, i64), MiraError> {
    fn parse_hhmm(s: &str) -> Result<i64, MiraError> {
        let (h, m) = s.split_once(':').ok_or_else(|| {
            MiraError::ToolError(format!("expected HH:MM, got '{}'", s))
        })?;
        let h: i64 = h.trim().parse().map_err(|_| {
            MiraError::ToolError(format!("bad hour in '{}'", s))
        })?;
        let m: i64 = m.trim().parse().map_err(|_| {
            MiraError::ToolError(format!("bad minute in '{}'", s))
        })?;
        if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
            return Err(MiraError::ToolError(format!("time out of range: '{}'", s)));
        }
        Ok(h * 60 + m)
    }

    match v {
        Value::String(s) => {
            let (a, b) = s.split_once('-').ok_or_else(|| MiraError::ToolError(
                format!("expected 'HH:MM-HH:MM', got '{}'", s)
            ))?;
            Ok((parse_hhmm(a.trim())?, parse_hhmm(b.trim())?))
        }
        Value::Object(map) => {
            let start = map.get("start").ok_or_else(|| {
                MiraError::ToolError("contact_hours object missing `start`".to_string())
            })?;
            let end = map.get("end").ok_or_else(|| {
                MiraError::ToolError("contact_hours object missing `end`".to_string())
            })?;
            let start_min = match start {
                Value::String(s) => parse_hhmm(s)?,
                Value::Number(n) => n.as_i64().ok_or_else(|| {
                    MiraError::ToolError("start is not an integer".to_string())
                })?,
                _ => return Err(MiraError::ToolError("start must be string or number".to_string())),
            };
            let end_min = match end {
                Value::String(s) => parse_hhmm(s)?,
                Value::Number(n) => n.as_i64().ok_or_else(|| {
                    MiraError::ToolError("end is not an integer".to_string())
                })?,
                _ => return Err(MiraError::ToolError("end must be string or number".to_string())),
            };
            Ok((start_min, end_min))
        }
        _ => Err(MiraError::ToolError(format!(
            "contact_hours value must be 'HH:MM-HH:MM' or {{start,end}}, got: {}", v
        ))),
    }
}

/// Map memory-seed question keys to memory categories. Falls back to `Fact`
/// so an unmapped `memory.seed` question still stores *something* sensible.
fn seed_category_for(key: &str) -> Category {
    match key {
        "work_summary" => Category::Fact,
        "hobbies"      => Category::Preference,
        "top_goals"    => Category::Project,
        _              => Category::Fact,
    }
}

// ── skip_topic ───────────────────────────────────────────────────────────────

pub struct SkipTopicTool {
    services: Arc<OnboardingServices>,
}

impl SkipTopicTool {
    pub fn new(services: Arc<OnboardingServices>) -> Self { Self { services } }
}

#[async_trait]
impl Tool for SkipTopicTool {
    fn name(&self) -> &str { "skip_topic" }
    fn description(&self) -> &str {
        "Mark an onboarding question as skipped. The user will not be asked again \
         unless they restart the group."
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::system("onboarding") }
    fn tier(&self) -> Tier { Tier::System }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["key"],
            "properties": {
                "key":    { "type": "string",  "description": "Question key being skipped." },
                "reason": { "type": "string",  "description": "Optional reason — stored only in logs." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = require_user_id(&args)?;
        let key = args.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
            MiraError::ToolError("skip_topic: missing `key`".to_string())
        })?.to_owned();

        // Validate that the key exists in the schema — prevents accidental
        // typos from accumulating garbage in progress JSON.
        if self.services.schema.question(&key).is_none() {
            return Err(MiraError::ToolError(format!(
                "skip_topic: unknown question key '{}'", key
            )));
        }

        record_and_autoadvance(&self.services.auth, &self.services.schema, &user_id, None, Some(&key))?;
        try_auto_finalize(&self.services.auth, &self.services.schema, &user_id)?;
        Ok(ToolResult::success(format!("skipped {}", key)))
    }
}

// ── mark_group_complete ──────────────────────────────────────────────────────

pub struct MarkGroupCompleteTool {
    services: Arc<OnboardingServices>,
}

impl MarkGroupCompleteTool {
    pub fn new(services: Arc<OnboardingServices>) -> Self { Self { services } }
}

#[async_trait]
impl Tool for MarkGroupCompleteTool {
    fn name(&self) -> &str { "mark_group_complete" }
    fn description(&self) -> &str {
        "Record that an onboarding group is fully handled (answered or skipped)."
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::system("onboarding") }
    fn tier(&self) -> Tier { Tier::System }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["group_id"],
            "properties": {
                "group_id": { "type": "string", "description": "Onboarding group id." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = require_user_id(&args)?;
        let group_id = args.get("group_id").and_then(|v| v.as_str()).ok_or_else(|| {
            MiraError::ToolError("mark_group_complete: missing `group_id`".to_string())
        })?.to_owned();

        if self.services.schema.group(&group_id).is_none() {
            return Err(MiraError::ToolError(format!(
                "mark_group_complete: unknown group id '{}'", group_id
            )));
        }

        let schema = Arc::clone(&self.services.schema);
        update_progress(&self.services.auth, &user_id, |p| {
            let Some(obj) = p.as_object_mut() else { return };
            if let Some(arr) = obj.get_mut("completed_groups") {
                push_unique(arr, &group_id);
            }
            if let Some(arr) = obj.get_mut("explicitly_completed_groups") {
                push_unique(arr, &group_id);
            }
            if let Some(idx) = schema.groups.iter().position(|g| g.id == group_id) {
                auto_advance_passed_groups(obj, &schema, idx);
            }
        })?;
        try_auto_finalize(&self.services.auth, &schema, &user_id)?;
        Ok(ToolResult::success(format!("completed group {}", group_id)))
    }
}

// ── complete_onboarding ──────────────────────────────────────────────────────

pub struct CompleteOnboardingTool {
    services: Arc<OnboardingServices>,
}

impl CompleteOnboardingTool {
    pub fn new(services: Arc<OnboardingServices>) -> Self { Self { services } }
}

#[async_trait]
impl Tool for CompleteOnboardingTool {
    fn name(&self) -> &str { "complete_onboarding" }
    fn description(&self) -> &str {
        "Finalize onboarding: validate required groups are done-or-skipped, stamp \
         the user as onboarded, and release the active onboarding conversation."
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::system("onboarding") }
    fn tier(&self) -> Tier { Tier::System }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": { "type": "string", "description": "Optional human summary, stored in progress." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = require_user_id(&args)?;
        let summary = args.get("summary").and_then(|v| v.as_str());

        match finalize_onboarding(&self.services.auth, &self.services.schema, &user_id, summary) {
            Ok(()) => Ok(ToolResult::success("onboarding complete")),
            Err(FinalizeError::UntouchedRequiredGroups(groups)) => {
                Ok(ToolResult::failure(format!(
                    "required groups not yet covered: {}. Ask at least one question \
                     in each, or call mark_group_complete if you've handled it.",
                    groups.join(", ")
                )))
            }
            Err(FinalizeError::Storage(e)) => Err(e),
        }
    }
}

// ── resolve_timezone ─────────────────────────────────────────────────────────

pub struct ResolveTimezoneTool;

impl ResolveTimezoneTool {
    pub fn new() -> Self { Self }
}

impl Default for ResolveTimezoneTool {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl Tool for ResolveTimezoneTool {
    fn name(&self) -> &str { "resolve_timezone" }
    fn description(&self) -> &str {
        "Map a human location string (city, country, or free-form text) to an IANA \
         timezone. Returns `{iana, confidence}` or `null` when unresolved."
    }
    fn visibility(&self) -> ToolVisibility { ToolVisibility::system("onboarding") }
    fn tier(&self) -> Tier { Tier::System }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["location_text"],
            "properties": {
                "location_text": {
                    "type": "string",
                    "description": "City, country, or free-form location. Also accepted: `city`, `location`, `place`."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        // Small/local models often drift from the exact arg name. Accept a
        // handful of common aliases so a Qwen/Hermes misname doesn't dead-end
        // the flow.
        let loc = ["location_text", "city", "location", "place", "text"]
            .iter()
            .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
            .unwrap_or("");
        let normalized = loc.trim().to_lowercase();
        if normalized.is_empty() {
            return Ok(ToolResult::success(json!({ "iana": null, "confidence": 0.0 }).to_string()));
        }

        // Exact hit first, then substring (looser match, lower confidence).
        let exact = TZ_CITIES.iter()
            .find(|(city, _)| *city == normalized);
        let (iana, confidence) = if let Some((_, zone)) = exact {
            (Some(*zone), 0.95)
        } else if let Some((_, zone)) = TZ_CITIES.iter().find(|(city, _)| normalized.contains(city)) {
            (Some(*zone), 0.6)
        } else {
            (None, 0.0)
        };

        Ok(ToolResult::success(json!({
            "iana": iana,
            "confidence": confidence,
        }).to_string()))
    }
}

/// Bundled city → IANA map. Kept small on purpose; the LLM should prompt
/// again when confidence is low rather than guessing. Extend as users hit
/// misses — not aiming for exhaustive coverage.
const TZ_CITIES: &[(&str, &str)] = &[
    ("sydney",       "Australia/Sydney"),
    ("melbourne",    "Australia/Melbourne"),
    ("brisbane",     "Australia/Brisbane"),
    ("perth",        "Australia/Perth"),
    ("adelaide",     "Australia/Adelaide"),
    ("auckland",     "Pacific/Auckland"),
    ("wellington",   "Pacific/Auckland"),
    ("london",       "Europe/London"),
    ("dublin",       "Europe/Dublin"),
    ("paris",        "Europe/Paris"),
    ("berlin",       "Europe/Berlin"),
    ("amsterdam",    "Europe/Amsterdam"),
    ("madrid",       "Europe/Madrid"),
    ("rome",         "Europe/Rome"),
    ("athens",       "Europe/Athens"),
    ("stockholm",    "Europe/Stockholm"),
    ("warsaw",       "Europe/Warsaw"),
    ("moscow",       "Europe/Moscow"),
    ("istanbul",     "Europe/Istanbul"),
    ("dubai",        "Asia/Dubai"),
    ("riyadh",       "Asia/Riyadh"),
    ("tehran",       "Asia/Tehran"),
    ("karachi",      "Asia/Karachi"),
    ("mumbai",       "Asia/Kolkata"),
    ("delhi",        "Asia/Kolkata"),
    ("bangalore",    "Asia/Kolkata"),
    ("kolkata",      "Asia/Kolkata"),
    ("dhaka",        "Asia/Dhaka"),
    ("bangkok",      "Asia/Bangkok"),
    ("singapore",    "Asia/Singapore"),
    ("jakarta",      "Asia/Jakarta"),
    ("manila",       "Asia/Manila"),
    ("hong kong",    "Asia/Hong_Kong"),
    ("hongkong",     "Asia/Hong_Kong"),
    ("taipei",       "Asia/Taipei"),
    ("shanghai",     "Asia/Shanghai"),
    ("beijing",      "Asia/Shanghai"),
    ("tokyo",        "Asia/Tokyo"),
    ("seoul",        "Asia/Seoul"),
    ("cairo",        "Africa/Cairo"),
    ("lagos",        "Africa/Lagos"),
    ("nairobi",      "Africa/Nairobi"),
    ("johannesburg", "Africa/Johannesburg"),
    ("new york",     "America/New_York"),
    ("boston",       "America/New_York"),
    ("miami",        "America/New_York"),
    ("chicago",      "America/Chicago"),
    ("dallas",       "America/Chicago"),
    ("houston",      "America/Chicago"),
    ("denver",       "America/Denver"),
    ("phoenix",      "America/Phoenix"),
    ("los angeles",  "America/Los_Angeles"),
    ("san francisco","America/Los_Angeles"),
    ("seattle",      "America/Los_Angeles"),
    ("vancouver",    "America/Vancouver"),
    ("toronto",      "America/Toronto"),
    ("mexico city",  "America/Mexico_City"),
    ("buenos aires", "America/Argentina/Buenos_Aires"),
    ("sao paulo",    "America/Sao_Paulo"),
    ("são paulo",    "America/Sao_Paulo"),
    ("honolulu",     "Pacific/Honolulu"),
    ("anchorage",    "America/Anchorage"),
];

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::LocalAuthService;
    use crate::auth::models::{NewUser, Role};
    use crate::history::HistoryStore;
    use tempfile::TempDir;

    async fn build_services() -> (TempDir, String, Arc<OnboardingServices>) {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();

        let auth = Arc::new(LocalAuthService::new(
            &data_dir.join("auth.db"),
            "test-secret".to_string(),
            7,
        ).unwrap());
        let user = auth.create_user(NewUser {
            username:     "tester".to_string(),
            display_name: None,
            email:        None,
            password:     "hunter22".to_string(),
            role:         Role::User,
        }).unwrap();

        let history = Arc::new(HistoryStore::open(&data_dir.join("history.db")).unwrap());
        let memory  = Arc::new(MemorySystem::new_keyword_only(data_dir.join("memory.db")).unwrap());
        let schema  = Arc::new(OnboardingSchema::bundled().unwrap());

        let services = Arc::new(OnboardingServices {
            auth, history, memory, schema, data_dir,
            wiki: None,
        });
        (dir, user.id, services)
    }

    async fn build_services_with_wiki() -> (TempDir, String, Arc<OnboardingServices>) {
        use crate::wiki::WikiRegistry;
        let (dir, uid, services) = build_services().await;
        let wiki_reg = Arc::new(WikiRegistry::new(services.data_dir.clone()));
        // Force the per-user wiki to be created so for_user(uid) succeeds.
        wiki_reg.for_user(&uid).expect("wiki for_user");
        let services = Arc::new(OnboardingServices {
            auth:     Arc::clone(&services.auth),
            history:  Arc::clone(&services.history),
            memory:   Arc::clone(&services.memory),
            schema:   Arc::clone(&services.schema),
            data_dir: services.data_dir.clone(),
            wiki:     Some(wiki_reg),
        });
        (dir, uid, services)
    }

    #[tokio::test]
    async fn record_profile_writes_user_profile_column() {
        let (_dir, uid, svc) = build_services().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        let out  = tool.execute(json!({
            "_user_id": uid,
            "key": "preferred_name",
            "value": "Alex",
        })).await.unwrap();
        assert!(out.success, "{:?}", out);
        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        assert_eq!(p.preferred_name.as_deref(), Some("Alex"));
    }

    #[tokio::test]
    async fn record_profile_writes_profile_md_section() {
        let (_dir, uid, svc) = build_services().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid,
            "key": "top_goals",
            "value": "- ship onboarding\n- write tests",
        })).await.unwrap();

        let body = crate::onboarding::read_profile_md(&svc.data_dir, &uid)
            .unwrap().unwrap();
        assert!(body.contains("## Goals"));
        assert!(body.contains("ship onboarding"));
    }

    #[tokio::test]
    async fn record_profile_stores_memory_seed() {
        let (_dir, uid, svc) = build_services().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid,
            "key": "hobbies",
            "value": "bouldering and bad puns",
        })).await.unwrap();

        // Visibility-aware search — seeds are stored at scope=user so the
        // legacy default-user `search` won't find them.
        let hits = svc.memory.search_visible("bouldering", &uid, &[]).unwrap();
        assert!(!hits.is_empty(), "expected memory with seed content");
        assert_eq!(hits[0].scope_id.as_deref(), Some(uid.as_str()));
    }

    #[test]
    fn seed_category_maps_known_keys() {
        assert!(matches!(seed_category_for("work_summary"), Category::Fact));
        assert!(matches!(seed_category_for("hobbies"),      Category::Preference));
        assert!(matches!(seed_category_for("top_goals"),    Category::Project));
        // Unknown keys fall back to Fact.
        assert!(matches!(seed_category_for("nope"),         Category::Fact));
    }

    #[tokio::test]
    async fn record_profile_seed_is_user_scoped() {
        let (_dir, uid, svc) = build_services().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid,
            "key": "work_summary",
            "value": "staff eng on the gateway team",
        })).await.unwrap();

        let hits = svc.memory.search_visible("gateway", &uid, &[]).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].scope_id.as_deref(), Some(uid.as_str()));
        assert_eq!(hits[0].category, Category::Fact);
        assert!(matches!(
            hits[0].source,
            Some(MemorySource::Imported(ref s)) if s == "onboarding"
        ));
    }

    #[tokio::test]
    async fn record_profile_fans_out_contact_hours() {
        let (_dir, uid, svc) = build_services().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid,
            "key": "contact_hours",
            "value": "09:00-17:30",
        })).await.unwrap();

        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        assert_eq!(p.contact_hours_start, Some(9 * 60));
        assert_eq!(p.contact_hours_end,   Some(17 * 60 + 30));
    }

    #[tokio::test]
    async fn record_profile_rejects_unknown_key() {
        let (_dir, uid, svc) = build_services().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        let err = tool.execute(json!({
            "_user_id": uid,
            "key": "does_not_exist",
            "value": "x",
        })).await.unwrap_err();
        assert!(err.to_string().contains("unknown question key"));
    }

    #[tokio::test]
    async fn record_profile_rejects_missing_user_id() {
        let (_dir, _uid, svc) = build_services().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        let err = tool.execute(json!({
            "key": "preferred_name",
            "value": "Alex",
        })).await.unwrap_err();
        assert!(err.to_string().contains("_user_id"));
    }

    #[tokio::test]
    async fn record_and_skip_auto_advance_group_when_all_handled() {
        // `location_time` has two questions: timezone + contact_hours.
        // Answer one, skip the other, and the group should auto-complete
        // without an explicit mark_group_complete call.
        let (_dir, uid, svc) = build_services().await;
        let record = RecordProfileTool::new(Arc::clone(&svc));
        let skip   = SkipTopicTool::new(Arc::clone(&svc));

        record.execute(json!({
            "_user_id": uid,
            "key":      "timezone",
            "value":    "Australia/Sydney",
        })).await.unwrap();

        // Group not yet complete — one question remains.
        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        let progress: Value = serde_json::from_str(p.onboarding_progress.as_deref().unwrap()).unwrap();
        let done: Vec<&str> = progress["completed_groups"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(!done.contains(&"location_time"));

        skip.execute(json!({ "_user_id": uid, "key": "contact_hours" })).await.unwrap();

        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        let progress: Value = serde_json::from_str(p.onboarding_progress.as_deref().unwrap()).unwrap();
        let done: Vec<&str> = progress["completed_groups"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(done.contains(&"location_time"),
            "expected location_time to auto-advance, got completed={:?}", done);
    }

    #[tokio::test]
    async fn optional_group_auto_completes_when_llm_jumps_ahead() {
        // `personal` is optional; if the LLM skips it and starts recording
        // into a later group (`work_hobbies` here), the progress bar would
        // stick at step 3 without jump-ahead. Verify it auto-completes.
        let (_dir, uid, svc) = build_services().await;

        // Prime realistic state: required groups 1 & 2 already complete.
        let mark = MarkGroupCompleteTool::new(Arc::clone(&svc));
        mark.execute(json!({ "_user_id": uid, "group_id": "name"          })).await.unwrap();
        mark.execute(json!({ "_user_id": uid, "group_id": "location_time" })).await.unwrap();

        // LLM jumps straight to a work_hobbies key without touching personal.
        let record = RecordProfileTool::new(Arc::clone(&svc));
        record.execute(json!({
            "_user_id": uid,
            "key":      "work_summary",
            "value":    "staff eng, works on the gateway",
        })).await.unwrap();

        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        let progress: Value = serde_json::from_str(p.onboarding_progress.as_deref().unwrap()).unwrap();
        let done: Vec<&str> = progress["completed_groups"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(done.contains(&"personal"),
            "expected optional `personal` to auto-complete on jump-ahead, got {:?}", done);
    }

    #[tokio::test]
    async fn mark_group_complete_also_jumps_optionals_ahead() {
        let (_dir, uid, svc) = build_services().await;
        let mark = MarkGroupCompleteTool::new(Arc::clone(&svc));
        mark.execute(json!({ "_user_id": uid, "group_id": "name"          })).await.unwrap();
        mark.execute(json!({ "_user_id": uid, "group_id": "location_time" })).await.unwrap();

        // LLM explicitly completes a later group without ever touching personal.
        mark.execute(json!({ "_user_id": uid, "group_id": "work_hobbies" })).await.unwrap();

        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        let progress: Value = serde_json::from_str(p.onboarding_progress.as_deref().unwrap()).unwrap();
        let done: Vec<&str> = progress["completed_groups"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(done.contains(&"personal"),
            "expected optional `personal` to auto-complete when mark_group_complete skips past it, got {:?}", done);
    }

    #[tokio::test]
    async fn required_group_auto_completes_on_jump_ahead_if_any_activity() {
        // `work_hobbies` is required with two questions (work_summary, hobbies).
        // LLM records only work_summary, then jumps to the next group's key.
        // Partial engagement + forward motion → group should auto-complete.
        let (_dir, uid, svc) = build_services().await;

        let mark   = MarkGroupCompleteTool::new(Arc::clone(&svc));
        let record = RecordProfileTool::new(Arc::clone(&svc));

        mark.execute(json!({ "_user_id": uid, "group_id": "name"          })).await.unwrap();
        mark.execute(json!({ "_user_id": uid, "group_id": "location_time" })).await.unwrap();
        record.execute(json!({
            "_user_id": uid, "key": "work_summary",
            "value":    "staff eng, works on the gateway",
        })).await.unwrap();

        // Now the LLM moves on to `goals` without recording `hobbies` or
        // explicitly marking `work_hobbies` complete.
        record.execute(json!({
            "_user_id": uid, "key": "top_goals", "value": "- ship onboarding",
        })).await.unwrap();

        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        let progress: Value = serde_json::from_str(p.onboarding_progress.as_deref().unwrap()).unwrap();
        let done: Vec<&str> = progress["completed_groups"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(done.contains(&"work_hobbies"),
            "expected required `work_hobbies` with partial activity to auto-complete, got {:?}", done);
    }

    #[tokio::test]
    async fn progress_bar_advances_past_zero_activity_required_group() {
        // Reasoning-distilled models often narrate through a required group
        // without firing any per-key tool. Jump-ahead should still advance
        // the progress bar so the UI stays honest with the chat — the
        // silent-skip guard now lives in `complete_onboarding`, not here.
        let (_dir, uid, svc) = build_services().await;

        let mark   = MarkGroupCompleteTool::new(Arc::clone(&svc));
        let record = RecordProfileTool::new(Arc::clone(&svc));

        mark.execute(json!({ "_user_id": uid, "group_id": "name"          })).await.unwrap();
        mark.execute(json!({ "_user_id": uid, "group_id": "location_time" })).await.unwrap();

        // LLM skips `personal` + `work_hobbies` entirely, jumps to `goals`.
        record.execute(json!({
            "_user_id": uid, "key": "top_goals", "value": "- ship onboarding",
        })).await.unwrap();

        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        let progress: Value = serde_json::from_str(p.onboarding_progress.as_deref().unwrap()).unwrap();
        let done: Vec<&str> = progress["completed_groups"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(done.contains(&"work_hobbies"),
            "progress bar must advance past work_hobbies on jump-ahead, got {:?}", done);
        assert!(done.contains(&"personal"),
            "optional `personal` must also be swept up, got {:?}", done);
    }

    #[tokio::test]
    async fn complete_onboarding_refuses_silent_skip_of_required_group() {
        // Jump-ahead advances `completed_groups` liberally, but finalization
        // still requires each required group to have real activity or an
        // explicit mark. Here the LLM jumped straight to the last group —
        // `work_hobbies`, `agent_style`, `autonomy`, `privacy` were never
        // touched — so `complete_onboarding` must refuse.
        let (_dir, uid, svc) = build_services().await;

        let mark   = MarkGroupCompleteTool::new(Arc::clone(&svc));
        let record = RecordProfileTool::new(Arc::clone(&svc));

        mark.execute(json!({ "_user_id": uid, "group_id": "name"          })).await.unwrap();
        mark.execute(json!({ "_user_id": uid, "group_id": "location_time" })).await.unwrap();
        // Single record in the LAST group — everything between is zero-activity.
        record.execute(json!({
            "_user_id": uid, "key": "avatar", "value": "preset:1",
        })).await.unwrap();

        let complete = CompleteOnboardingTool::new(Arc::clone(&svc));
        let res = complete.execute(json!({ "_user_id": uid })).await.unwrap();
        assert!(!res.success, "expected refusal, got {:?}", res);
        let err = res.error.as_deref().unwrap_or("");
        for g in ["work_hobbies", "agent_style", "autonomy", "privacy"] {
            assert!(err.contains(g), "missing group `{}` in error: {}", g, err);
        }
    }

    #[tokio::test]
    async fn complete_onboarding_accepts_when_every_required_group_has_activity() {
        // Each required group has at least one answered OR skipped key.
        // Jump-ahead fills in the `completed_groups` list; the activity
        // guard is satisfied; finalization passes.
        let (_dir, uid, svc) = build_services().await;
        let record = RecordProfileTool::new(Arc::clone(&svc));
        let skip   = SkipTopicTool::new(Arc::clone(&svc));

        // One key per required group — activity recorded for each.
        record.execute(json!({ "_user_id": uid, "key": "preferred_name",     "value": "Alex" })).await.unwrap();
        record.execute(json!({ "_user_id": uid, "key": "timezone",           "value": "Australia/Sydney" })).await.unwrap();
        record.execute(json!({ "_user_id": uid, "key": "work_summary",       "value": "staff eng" })).await.unwrap();
        record.execute(json!({ "_user_id": uid, "key": "top_goals",          "value": "- ship onboarding" })).await.unwrap();
        record.execute(json!({ "_user_id": uid, "key": "verbosity",          "value": "brief" })).await.unwrap();
        record.execute(json!({ "_user_id": uid, "key": "autonomy_preference","value": "ask_first" })).await.unwrap();
        skip.execute(json!({   "_user_id": uid, "key": "off_limits_topics" })).await.unwrap();

        let complete = CompleteOnboardingTool::new(Arc::clone(&svc));
        let res = complete.execute(json!({ "_user_id": uid })).await.unwrap();
        assert!(res.success, "expected acceptance, got {:?}", res);
    }

    #[tokio::test]
    async fn complete_onboarding_accepts_when_missing_required_groups_are_explicitly_marked() {
        // Alternate acceptance path: LLM called `mark_group_complete` for a
        // group without ever firing record/skip. That's a valid "I handled
        // this out-of-band" signal and satisfies the guard.
        let (_dir, uid, svc) = build_services().await;
        let mark = MarkGroupCompleteTool::new(Arc::clone(&svc));
        for g in &svc.schema.groups {
            if !g.optional {
                mark.execute(json!({ "_user_id": uid, "group_id": g.id })).await.unwrap();
            }
        }
        let complete = CompleteOnboardingTool::new(Arc::clone(&svc));
        let res = complete.execute(json!({ "_user_id": uid })).await.unwrap();
        assert!(res.success, "expected acceptance, got {:?}", res);
    }

    #[tokio::test]
    async fn avatar_group_is_optional_so_complete_onboarding_doesnt_require_it() {
        // Avatar is UI-driven (no LLM tool backs choose_avatar_ui). Marking
        // the whole group optional is what lets onboarding finish without
        // forcing the LLM to fabricate a record_profile call.
        let (_dir, uid, svc) = build_services().await;

        let mark = MarkGroupCompleteTool::new(Arc::clone(&svc));
        for g in &svc.schema.groups {
            if g.id == "avatar" { continue; }       // deliberately skip it
            if !g.optional {
                mark.execute(json!({ "_user_id": uid, "group_id": g.id })).await.unwrap();
            }
        }

        let complete = CompleteOnboardingTool::new(Arc::clone(&svc));
        let res = complete.execute(json!({ "_user_id": uid })).await.unwrap();
        assert!(res.success,
            "complete_onboarding should succeed without avatar group, got {:?}", res);
    }

    #[tokio::test]
    async fn auto_finalize_stamps_onboarded_when_last_required_group_clears() {
        // Models that narrate completion conversationally still fire the
        // per-key tools — when the last required group clears, the backend
        // should self-heal and stamp onboarded without waiting for an
        // explicit `complete_onboarding` call.
        let (_dir, uid, svc) = build_services().await;
        let mark = MarkGroupCompleteTool::new(Arc::clone(&svc));

        // Mark every required group explicitly, except the last one. Up to
        // this point `onboarded_at` must stay null because the final
        // required group hasn't been touched.
        let required: Vec<String> = svc.schema.groups.iter()
            .filter(|g| !g.optional).map(|g| g.id.clone()).collect();
        let (last_required, earlier) = required.split_last().unwrap();
        for g in earlier {
            mark.execute(json!({ "_user_id": uid, "group_id": g })).await.unwrap();
        }
        assert!(svc.auth.get_profile(&uid).unwrap().unwrap().onboarded_at.is_none());

        // Clear the last required group — auto-finalize should fire.
        mark.execute(json!({ "_user_id": uid, "group_id": last_required })).await.unwrap();
        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        assert!(p.onboarded_at.is_some(),
            "expected auto-finalize to stamp onboarded after last required group cleared");
    }

    #[tokio::test]
    async fn auto_finalize_is_noop_while_required_groups_outstanding() {
        // Regression guard for the original bug where `try_auto_finalize`
        // mutated `completed_groups` before running the silent-skip guard.
        // A single record inside `location_time` must not mark the group
        // as completed — the group still has a pending question.
        let (_dir, uid, svc) = build_services().await;
        let record = RecordProfileTool::new(Arc::clone(&svc));
        record.execute(json!({
            "_user_id": uid, "key": "timezone", "value": "Australia/Sydney",
        })).await.unwrap();

        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        assert!(p.onboarded_at.is_none(),
            "must not finalize while required groups are untouched");

        let progress: Value = serde_json::from_str(p.onboarding_progress.as_deref().unwrap()).unwrap();
        let done: Vec<&str> = progress["completed_groups"].as_array().unwrap()
            .iter().filter_map(|v| v.as_str()).collect();
        assert!(!done.contains(&"location_time"),
            "partial group must not be marked complete by a failed finalize attempt, got {:?}", done);
    }

    #[tokio::test]
    async fn resolve_timezone_accepts_city_alias() {
        let t = ResolveTimezoneTool::new();
        let out: Value = serde_json::from_str(
            &t.execute(json!({ "city": "Melbourne" })).await.unwrap().output
        ).unwrap();
        assert_eq!(out["iana"].as_str(), Some("Australia/Melbourne"));
    }

    #[tokio::test]
    async fn skip_topic_appends_unique() {
        let (_dir, uid, svc) = build_services().await;
        let tool = SkipTopicTool::new(Arc::clone(&svc));
        tool.execute(json!({ "_user_id": uid, "key": "weight_kg" })).await.unwrap();
        tool.execute(json!({ "_user_id": uid, "key": "weight_kg" })).await.unwrap();
        tool.execute(json!({ "_user_id": uid, "key": "height_cm" })).await.unwrap();

        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        let progress: Value = serde_json::from_str(p.onboarding_progress.as_deref().unwrap()).unwrap();
        let arr = progress["skipped_keys"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[tokio::test]
    async fn mark_group_complete_validates_id() {
        let (_dir, uid, svc) = build_services().await;
        let tool = MarkGroupCompleteTool::new(Arc::clone(&svc));
        let err = tool.execute(json!({
            "_user_id": uid, "group_id": "nonexistent"
        })).await.unwrap_err();
        assert!(err.to_string().contains("unknown group id"));
    }

    #[tokio::test]
    async fn complete_onboarding_requires_all_non_optional_groups() {
        let (_dir, uid, svc) = build_services().await;
        let tool = CompleteOnboardingTool::new(Arc::clone(&svc));
        let res = tool.execute(json!({ "_user_id": uid })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.as_deref().unwrap().contains("required groups"));
    }

    #[tokio::test]
    async fn complete_onboarding_succeeds_when_all_groups_done() {
        let (_dir, uid, svc) = build_services().await;
        let mark = MarkGroupCompleteTool::new(Arc::clone(&svc));
        for g in &svc.schema.groups {
            if !g.optional {
                mark.execute(json!({ "_user_id": uid, "group_id": g.id })).await.unwrap();
            }
        }
        let complete = CompleteOnboardingTool::new(Arc::clone(&svc));
        let res = complete.execute(json!({ "_user_id": uid })).await.unwrap();
        assert!(res.success, "{:?}", res);

        let p = svc.auth.get_profile(&uid).unwrap().unwrap();
        assert!(p.onboarded_at.is_some());
    }

    #[tokio::test]
    async fn resolve_timezone_exact_and_substring() {
        let t = ResolveTimezoneTool::new();

        let out: Value = serde_json::from_str(
            &t.execute(json!({ "location_text": "Sydney" })).await.unwrap().output
        ).unwrap();
        assert_eq!(out["iana"].as_str(), Some("Australia/Sydney"));

        let out: Value = serde_json::from_str(
            &t.execute(json!({ "location_text": "I live near tokyo, japan" })).await.unwrap().output
        ).unwrap();
        assert_eq!(out["iana"].as_str(), Some("Asia/Tokyo"));
        assert!(out["confidence"].as_f64().unwrap() < 0.95);

        let out: Value = serde_json::from_str(
            &t.execute(json!({ "location_text": "Mars Base One" })).await.unwrap().output
        ).unwrap();
        assert!(out["iana"].is_null());
    }

    #[tokio::test]
    async fn all_onboarding_tools_are_system_tier() {
        let (_dir, _uid, svc) = build_services().await;
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(RecordProfileTool::new(Arc::clone(&svc))),
            Box::new(SkipTopicTool::new(Arc::clone(&svc))),
            Box::new(MarkGroupCompleteTool::new(Arc::clone(&svc))),
            Box::new(CompleteOnboardingTool::new(Arc::clone(&svc))),
            Box::new(ResolveTimezoneTool::new()),
        ];
        for t in tools {
            match t.visibility() {
                ToolVisibility::System { flow } => assert_eq!(flow, "onboarding"),
                other => panic!("{} has non-system visibility: {:?}", t.name(), other),
            }
        }
    }

    // ── Onboarding -> wiki bridge ───────────────────────────────────────────

    fn read_wiki_profile(svc: &OnboardingServices, uid: &str) -> String {
        let path = svc.data_dir.join("wikis").join("users").join(uid).join("profile.md");
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    #[tokio::test]
    async fn wiki_bridge_mirrors_user_profile_columns_to_personal_details() {
        let (_dir, uid, svc) = build_services_with_wiki().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid, "key": "preferred_name", "value": "Alex",
        })).await.unwrap();
        tool.execute(json!({
            "_user_id": uid, "key": "timezone", "value": "Australia/Sydney",
        })).await.unwrap();
        let body = read_wiki_profile(&svc, &uid);
        assert!(body.contains("## Personal details"),
            "expected '## Personal details' section, got:\n{body}");
        assert!(body.contains("**Preferred name:** Alex"),
            "expected preferred name on wiki, got:\n{body}");
        assert!(body.contains("**Timezone:** Australia/Sydney"),
            "expected timezone on wiki, got:\n{body}");
    }

    #[tokio::test]
    async fn wiki_bridge_inverts_contact_hours_into_window() {
        let (_dir, uid, svc) = build_services_with_wiki().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid, "key": "contact_hours", "value": "09:00-22:00",
        })).await.unwrap();
        let body = read_wiki_profile(&svc, &uid);
        assert!(body.contains("**Contact hours:** 09:00–22:00"),
            "expected contact-hours window rendered, got:\n{body}");
    }

    #[tokio::test]
    async fn wiki_bridge_mirrors_profile_md_section_to_wiki_heading() {
        let (_dir, uid, svc) = build_services_with_wiki().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid, "key": "top_goals",
            "value": "- ship onboarding\n- write tests",
        })).await.unwrap();
        let body = read_wiki_profile(&svc, &uid);
        assert!(body.contains("## Goals"),
            "expected '## Goals' heading, got:\n{body}");
        assert!(body.contains("ship onboarding"),
            "expected goal body, got:\n{body}");
    }

    #[tokio::test]
    async fn wiki_bridge_appends_memory_seed_under_about_me() {
        let (_dir, uid, svc) = build_services_with_wiki().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid, "key": "hobbies", "value": "bouldering and bad puns",
        })).await.unwrap();
        tool.execute(json!({
            "_user_id": uid, "key": "work_summary",
            "value": "staff eng on the gateway team",
        })).await.unwrap();
        let body = read_wiki_profile(&svc, &uid);
        assert!(body.contains("## About me"),
            "expected '## About me' section, got:\n{body}");
        assert!(body.contains("bouldering and bad puns"));
        assert!(body.contains("staff eng on the gateway team"),
            "expected both seeds appended, got:\n{body}");
    }

    #[tokio::test]
    async fn rebuild_wiki_profile_replays_full_state() {
        use crate::wiki::WikiRegistry;
        // Build a user whose state already exists, with NO wiki bridge
        // (simulating the pre-bridge era). Then point rebuild_wiki_profile
        // at it and prove every signal lands.
        let (_dir, uid, svc) = build_services().await;

        // Onboarding writes go through the tools without a wiki bridge.
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        for (key, value) in [
            ("preferred_name",     json!("Alex")),
            ("timezone",           json!("Australia/Sydney")),
            ("contact_hours",      json!("09:00-22:00")),
            ("top_goals",          json!("- ship onboarding\n- write tests")),
            ("check_in_cadence",   json!("when_needed")),
            ("hobbies",            json!("bouldering")),
            ("work_summary",       json!("staff eng on the gateway team")),
        ] {
            tool.execute(json!({"_user_id": uid, "key": key, "value": value}))
                .await.unwrap();
        }

        // Wiki dir was NOT created by any of those writes (bridge=None).
        let wiki_dir = svc.data_dir.join("wikis").join("users").join(&uid);
        assert!(!wiki_dir.exists(), "precondition: wiki dir absent before rebuild");

        // Now run the backfill.
        let wiki_reg = WikiRegistry::new(svc.data_dir.clone());
        wiki_reg.for_user(&uid).expect("wiki for_user");
        let summary = rebuild_wiki_profile(
            &svc.auth, &svc.memory, &wiki_reg, &svc.data_dir, &uid,
        ).unwrap();

        assert!(summary.personal_details);
        assert!(summary.sections.iter().any(|s| s == "Goals"),
            "expected Goals in summary.sections, got {:?}", summary.sections);
        assert!(summary.about_me_seed_count >= 2,
            "expected at least two seeds, got {}", summary.about_me_seed_count);

        let body = read_wiki_profile(&svc, &uid);
        assert!(body.contains("**Preferred name:** Alex"),
            "personal details missing:\n{body}");
        assert!(body.contains("**Contact hours:** 09:00–22:00"));
        assert!(body.contains("## Goals"));
        assert!(body.contains("ship onboarding"));
        assert!(body.contains("## About me"));
        assert!(body.contains("bouldering"));
        assert!(body.contains("staff eng on the gateway team"));
    }

    #[tokio::test]
    async fn rebuild_wiki_profile_is_idempotent() {
        use crate::wiki::WikiRegistry;
        let (_dir, uid, svc) = build_services().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid, "key": "preferred_name", "value": "Alex",
        })).await.unwrap();

        let wiki_reg = WikiRegistry::new(svc.data_dir.clone());
        wiki_reg.for_user(&uid).expect("wiki for_user");

        // Run twice.
        rebuild_wiki_profile(&svc.auth, &svc.memory, &wiki_reg, &svc.data_dir, &uid).unwrap();
        let first = read_wiki_profile(&svc, &uid);
        rebuild_wiki_profile(&svc.auth, &svc.memory, &wiki_reg, &svc.data_dir, &uid).unwrap();
        let second = read_wiki_profile(&svc, &uid);

        // The body should be unchanged after the second run — UpdateSection
        // replaces rather than appending, and PageFrontmatter.provenance
        // accumulates but we don't dig into that here. Compare just the
        // markdown body sections.
        let pd_a = crate::wiki::page::read_section(&first,  "Personal details");
        let pd_b = crate::wiki::page::read_section(&second, "Personal details");
        assert_eq!(pd_a, pd_b,
            "Personal details should be byte-identical across runs");
    }

    #[tokio::test]
    async fn wiki_bridge_no_wiki_is_a_noop() {
        // The default build_services() wires wiki=None; existing tests
        // confirm onboarding writes still succeed. This test makes the
        // expectation explicit: no wiki dir is created.
        let (_dir, uid, svc) = build_services().await;
        let tool = RecordProfileTool::new(Arc::clone(&svc));
        tool.execute(json!({
            "_user_id": uid, "key": "preferred_name", "value": "Alex",
        })).await.unwrap();
        let wiki_dir = svc.data_dir.join("wikis").join("users").join(&uid);
        assert!(!wiki_dir.exists(),
            "wiki dir should not be created when bridge isn't wired");
    }
}
