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

        let category_override = args.get("category").and_then(|v| v.as_str())
            .map(parse_category);

        let tags: Vec<String> = args.get("tags")
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

    // Integration-style execute() tests would need a wired MemorySystem +
    // LocalAuthService, which pulls in SQLite fixtures. The HTTP handler at
    // `src/server/handlers/memory.rs::supersede_memory` already exercises
    // the same MemorySystem::supersede path end-to-end; this tool is a thin
    // wrapper that delegates the work.
}
