// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/memory_supersede.rs
//! Model-callable memory update (Tier 1 — pure).
//!
//! Mirror of `POST /api/memory/{id}/supersede`: the model can correct a
//! memory it already holds rather than silently accumulating stale facts.
//! Uses the same append-only supersede path — the old row is kept for audit,
//! the new row becomes the visible one.
//!
//! Scoping: ownership is enforced via `get_visible`, which already handles
//! User/Group/System visibility. Group memories additionally require a live
//! membership check (memberships can change mid-session). System-scope rows
//! are admin-only and this tool refuses to touch them because the chat-layer
//! identity isn't necessarily admin.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::debug;

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::auth::LocalAuthService;
use crate::memory::MemorySystem;
use crate::memory::storage::{Category, Scope};
use crate::MiraError;

pub struct MemorySupersedeTool {
    memory: Arc<MemorySystem>,
    auth:   Arc<LocalAuthService>,
}

impl MemorySupersedeTool {
    pub fn new(memory: Arc<MemorySystem>, auth: Arc<LocalAuthService>) -> Self {
        Self { memory, auth }
    }
}

#[async_trait]
impl Tool for MemorySupersedeTool {
    fn name(&self) -> &str { "memory_supersede" }

    fn description(&self) -> &str {
        "Replace an existing memory with a corrected version. Pass the \
         `memory_id` (from a prior memory surface) and the new `content`; \
         optionally override `category` or `tags`. The old memory is kept \
         for audit but stops being visible — the new row becomes the \
         canonical one. Use this when the user corrects a fact you've \
         already stored ('actually, my cat's name is Ziggy, not Zigzag') \
         rather than writing a second contradictory memory. Refuses to \
         touch memories the caller cannot see, and cannot touch \
         system-scope memories at all."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["memory_id", "content"],
            "properties": {
                "memory_id": {
                    "type": "integer",
                    "minimum": 1,
                    "description":
                        "Numeric id of the memory to replace, as returned by \
                         a prior memory lookup."
                },
                "content": {
                    "type": "string",
                    "description": "The corrected memory content. Required."
                },
                "category": {
                    "type": "string",
                    "enum": ["fact", "preference", "skill", "relationship", "project"],
                    "description":
                        "Optional category override. Defaults to the old \
                         memory's category."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description":
                        "Optional replacement tag list. When omitted, the \
                         new memory ships with no tags (matches the HTTP API)."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = args.get("_user_id").and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError(
                "memory_supersede called without _user_id (chat handler must inject)".to_string()
            ))?
            .to_owned();

        let old_id = match args.get("memory_id").and_then(|v| v.as_u64()) {
            Some(n) if n > 0 => n,
            _ => return Ok(ToolResult::failure(
                "memory_supersede: `memory_id` must be a positive integer",
            )),
        };

        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("").trim();
        if content.is_empty() {
            return Ok(ToolResult::failure("memory_supersede: `content` is required"));
        }
        // W3: refuse credential-like content here too (this tool bypasses
        // remember_fact's choke-point guard).
        if crate::memory::auto_extract::looks_like_secret(content) {
            return Ok(ToolResult::failure(
                "memory_supersede: refusing to store what looks like a credential. Don't save secrets to memory.",
            ));
        }

        let category_override = args.get("category").and_then(|v| v.as_str())
            .map(parse_category);

        let provided_tags: Vec<String> = args.get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter()
                .filter_map(|e| e.as_str().map(|s| s.trim().to_owned()))
                .filter(|s| !s.is_empty())
                .collect())
            .unwrap_or_default();

        // Resolve the caller's group ids for visibility scoping. Degrade to
        // "no groups" on auth-db hiccups — same as the HTTP handler.
        let groups = self.auth.list_user_group_ids(&user_id).unwrap_or_default();

        let existing = match self.memory.get_visible(old_id, &user_id, &groups)? {
            Some(m) => m,
            None    => return Ok(ToolResult::failure(
                format!("memory_supersede: memory {} not visible to caller", old_id),
            )),
        };

        if existing.scope == Scope::System {
            return Ok(ToolResult::failure(
                "memory_supersede: system-scope memories are admin-only",
            ));
        }

        // Re-check group membership at call time — visibility reads are
        // cached-looking; membership can change during a session.
        if existing.scope == Scope::Group {
            if let Some(gid) = &existing.scope_id {
                let ok = self.auth.is_group_member(gid, &user_id)?;
                if !ok {
                    return Ok(ToolResult::failure(
                        format!("memory_supersede: no longer a member of group {}", gid),
                    ));
                }
            }
        }

        let category = category_override.unwrap_or_else(|| existing.category.clone());
        // Inherit the old row's tags when the model gives none. When it DOES give
        // tags, still carry over the conflict `key:…` (and `entity:…`) so the new
        // row stays correctable — dropping the key would orphan the chain.
        let tags = if provided_tags.is_empty() {
            existing.tags.clone()
        } else {
            // A `key:`/`entity:` conflict key is derived server-side — never trust
            // one the model supplied (it could hijack another fact). Drop any from
            // the model's tags and carry over the OLD row's instead.
            let mut t: Vec<String> = provided_tags.into_iter()
                .filter(|x| !x.starts_with("key:") && !x.starts_with("entity:"))
                .collect();
            for inherited in existing.tags.iter()
                .filter(|x| x.starts_with("key:") || x.starts_with("entity:"))
            {
                if !t.iter().any(|x| x == inherited) { t.push(inherited.clone()); }
            }
            t
        };

        // SUPERSEDE SAFETY GATE. Overwriting a live row is the one
        // irreversible-looking memory op, and the model reaches for this tool a
        // lot, so it must pass the SAME three checks every other write path does —
        // otherwise KEEP BOTH (add the new fact alongside, overwrite nothing):
        //   1. CARDINALITY — the old row must carry a SINGLE-valued conflict key.
        //      A multi-valued key (languages, skills, hobbies) accumulates, so
        //      "speaks French" must never be overwritten by "speaks Arabic" (the
        //      data loss, still reachable through this tool). A row with
        //      no conflict key at all is treated as not-single → keep both.
        //   2. GROUNDING — the new fact's subject must be grounded in the turn's
        //      user message (injected as `_user_message`), so a mis-attributed
        //      third-party fact can't overwrite the user's own.
        //   3. VALUE — the new content must actually CONTRADICT the old one.
        use crate::memory::auto_extract as ax;
        let old_key: Option<String> = existing.tags.iter()
            .find(|t| t.starts_with("key:"))
            .map(|t| t.trim_start_matches("key:").to_string());
        let per_thing     = old_key.as_deref().map(ax::is_per_thing_key).unwrap_or(false);
        let single_valued = old_key.as_deref().map(ax::key_is_single_valued).unwrap_or(false);
        let grounded = match (old_key.as_deref(), args.get("_user_message").and_then(|v| v.as_str())) {
            (Some(k), Some(msg)) => {
                let subj = k.split('.').next().unwrap_or("user");
                if subj == "user" { ax::user_fact_grounded(msg, content) }
                else                { ax::subject_grounded(subj, msg) }
            }
            // No key subject to ground against, or a non-chat caller with no turn
            // message: can't verify grounding, so don't block on it here — the
            // cardinality + value checks still apply.
            _ => true,
        };
        let contradicts = ax::value_verdict(&existing.content, content, per_thing)
            == ax::ValueVerdict::Contradicts;
        if !(single_valued && grounded && contradicts) {
            // Keep both: store the new fact as its own memory, don't replace.
            let add_tags: Vec<String> = tags.iter()
                .filter(|t| !t.starts_with("key:") && !t.starts_with("entity:")).cloned().collect();
            let new_id = self.memory.store_scoped(
                content.to_owned(), category.clone(), add_tags, None,
                existing.scope.clone(), existing.scope_id.as_deref(), &user_id,
                &[user_id.clone()], None, None, None,
            ).await?;
            let note = if !single_valued {
                "that memory holds a list of values (e.g. languages/skills), so the new fact was added alongside it rather than replacing it — nothing was overwritten"
            } else if !grounded {
                "the new statement isn't grounded as being about that memory's subject, so both were kept — nothing was overwritten"
            } else {
                "the new statement didn't clearly contradict the old one, so both were kept — nothing was overwritten"
            };
            return Ok(ToolResult::success(json!({
                "old_id": old_id, "new_id": new_id, "superseded": false, "note": note,
            }).to_string()));
        }

        debug!(
            "memory_supersede: user={} old_id={} scope={:?} new_category={}",
            user_id, old_id, existing.scope, category,
        );

        let new_id = self.memory.supersede(
            old_id,
            content.to_owned(),
            category.clone(),
            tags.clone(),
            None,
            &user_id,
        ).await?;

        let body = json!({
            "old_id":   old_id,
            "new_id":   new_id,
            "category": category.as_str(),
            "tags":     tags,
        });
        Ok(ToolResult::success(body.to_string()))
    }
}

fn parse_category(s: &str) -> Category {
    match s {
        "preference"   => Category::Preference,
        "skill"        => Category::Skill,
        "relationship" => Category::Relationship,
        "project"      => Category::Project,
        _              => Category::Fact,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_category_covers_known_variants() {
        assert!(matches!(parse_category("preference"),   Category::Preference));
        assert!(matches!(parse_category("skill"),        Category::Skill));
        assert!(matches!(parse_category("relationship"), Category::Relationship));
        assert!(matches!(parse_category("project"),      Category::Project));
        assert!(matches!(parse_category("fact"),         Category::Fact));
        // Unknown defaults to Fact so the model can't DoS on typos.
        assert!(matches!(parse_category("unknown"),      Category::Fact));
    }

    // ── The supersede safety gate, end-to-end through the tool ───────────
    use std::sync::Arc;
    use tempfile::tempdir;
    use crate::memory::{MemorySystem, MemorySource, WriteOutcome, ListSort};

    fn wired() -> (Arc<MemorySystem>, Arc<LocalAuthService>) {
        let dir = tempdir().unwrap();
        let mpath = dir.path().join("m.db");
        let apath = dir.path().join("auth.db");
        std::mem::forget(dir);
        let mem  = Arc::new(MemorySystem::new(mpath).unwrap());
        let auth = Arc::new(LocalAuthService::new(&apath, "jwt-test-secret".into(), 7).unwrap());
        (mem, auth)
    }

    // Store a keyed fact the way the extractor would, return its id.
    async fn keyed(mem: &Arc<MemorySystem>, user: &str, subj: &str, attr: &str, card: &str, content: &str, g: &str) -> u64 {
        match mem.remember_fact(
            user, content.to_string(), Category::Fact, Some(subj), Some(attr), card,
            Some(g), &["tool".to_owned()], true, MemorySource::UserExplicit("t".into()), None, None, None,
        ).await.unwrap() {
            WriteOutcome::Added(id) => id,
            WriteOutcome::Updated { new_id, .. } => new_id,
            o => panic!("unexpected write outcome: {o:?}"),
        }
    }

    fn live_contents(mem: &Arc<MemorySystem>, user: &str) -> Vec<String> {
        let (rows, _) = mem.list_visible_filtered(user, &[], None, None, None, None, None, 100, 0, ListSort::Recent).unwrap();
        rows.iter().map(|m| m.content.clone()).collect()
    }

    #[tokio::test]
    async fn supersede_tool_keeps_both_on_a_multi_valued_key() {
        // "speaks French" is MULTI (languages accumulate). The model calling this
        // tool to "replace" it with "speaks Arabic" must KEEP BOTH — the
        // data loss, still reachable through the tool the model reaches for most.
        let (mem, auth) = wired();
        let fr = keyed(&mem, "alice", "user", "language", "multi", "User speaks French", "I speak French").await;
        let tool = MemorySupersedeTool::new(Arc::clone(&mem), Arc::clone(&auth));
        let r = tool.execute(json!({
            "_user_id": "alice", "memory_id": fr, "content": "User speaks Arabic",
            "_user_message": "actually I speak Arabic too",
        })).await.unwrap();
        assert!(r.success, "{:?}", r.error);
        assert!(r.output.contains("\"superseded\":false"), "must keep both: {}", r.output);
        let live = live_contents(&mem, "alice");
        assert!(live.iter().any(|c| c.contains("French")), "French lost: {live:?}");
        assert!(live.iter().any(|c| c.contains("Arabic")), "Arabic missing: {live:?}");
    }

    #[tokio::test]
    async fn supersede_tool_overwrites_single_valued_grounded_contradiction() {
        let (mem, auth) = wired();
        let id = keyed(&mem, "alice", "user", "location", "single", "User lives in Perth", "I live in Perth").await;
        let tool = MemorySupersedeTool::new(Arc::clone(&mem), Arc::clone(&auth));
        let r = tool.execute(json!({
            "_user_id": "alice", "memory_id": id, "content": "User lives in Sydney",
            "_user_message": "I live in Sydney now",
        })).await.unwrap();
        assert!(r.success, "{:?}", r.error);
        assert!(!r.output.contains("\"superseded\":false"), "a grounded single-valued correction should supersede: {}", r.output);
        let live = live_contents(&mem, "alice");
        assert!(live.iter().any(|c| c.contains("Sydney")), "Sydney missing: {live:?}");
        assert!(!live.iter().any(|c| c.contains("Perth")), "Perth should be gone: {live:?}");
    }

    #[tokio::test]
    async fn supersede_tool_keeps_both_when_misgrounded() {
        // The turn is about someone else ("I heard Sara…"): even a single-valued,
        // contradicting value must not overwrite the user's own fact.
        let (mem, auth) = wired();
        let id = keyed(&mem, "alice", "user", "location", "single", "User lives in Sydney", "I live in Sydney").await;
        let tool = MemorySupersedeTool::new(Arc::clone(&mem), Arc::clone(&auth));
        let r = tool.execute(json!({
            "_user_id": "alice", "memory_id": id, "content": "User lives in Doha",
            "_user_message": "I heard Sara moved to Doha",
        })).await.unwrap();
        assert!(r.success, "{:?}", r.error);
        assert!(r.output.contains("\"superseded\":false"), "mis-grounded overwrite must keep both: {}", r.output);
        let live = live_contents(&mem, "alice");
        assert!(live.iter().any(|c| c.contains("Sydney")), "Sydney lost: {live:?}");
    }
}
