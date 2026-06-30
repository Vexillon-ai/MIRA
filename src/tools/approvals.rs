// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/approvals.rs
//! `pending_approvals` + `approve_pending` — surface and action the items
//! waiting for a user's approval: agent-created **schedules** (which land in
//! `pending_approval` when the approval gate is on) and **wiki** edits sitting
//! in the review queue.
//!
//! Companion check-ins and the daily briefing nudge the user when these pile
//! up (so important items don't get forgotten); these tools let the user
//! *summarise* and *approve* them right in the conversation.
//!
//! ## Guardrail
//!
//! `approve_pending` flips human-gated items live — that's the whole point of
//! the approval gate (a human reviews agent-proposed automations / wiki edits).
//! Only call it when the **user explicitly asks** to approve (e.g. "approve
//! all", "approve the backup schedule"). Never call it proactively, and never
//! from a check-in/briefing turn.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult, ToolVisibility};
use crate::automations::types::ScheduleStatus;
use crate::automations::AutomationsStore;
use crate::wiki::WikiRegistry;
use crate::MiraError;

fn caller(args: &ToolArgs) -> Result<String, MiraError> {
    args.get("_user_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| MiraError::ToolError(
            "approvals tool called without caller identity".into(),
        ))
}

// Pending agent-created schedules for the user, as summary JSON.
fn pending_schedules(store: &AutomationsStore, user_id: &str) -> Vec<Value> {
    store.list_schedules(Some(user_id)).unwrap_or_default()
        .into_iter()
        .filter(|s| matches!(s.status, ScheduleStatus::PendingApproval))
        .map(|s| json!({
            "id":        s.id,
            "name":      s.name,
            "trigger":   s.trigger,
            "timezone":  s.timezone,
            "rationale": s.rationale,
            "created_at": s.created_at,
        }))
        .collect()
}

// Pending wiki review-queue ops for the user, as summary JSON.
fn pending_wiki(reg: &WikiRegistry, user_id: &str) -> Vec<Value> {
    let Ok(wiki) = reg.for_user(user_id) else { return Vec::new() };
    wiki.list_pending_ops().unwrap_or_default()
        .into_iter()
        .map(|e| json!({
            "id":         e.op_id,
            "kind":       e.op.kind(),
            "path":       e.op.target_path().to_string(),
            "confidence": e.confidence,
            "created_at": e.created_at.to_rfc3339(),
        }))
        .collect()
}

// ── pending_approvals (read-only) ─────────────────────────────────────────────

pub struct PendingApprovalsTool {
    automations: Option<Arc<AutomationsStore>>,
    wiki:        Option<Arc<WikiRegistry>>,
}

impl PendingApprovalsTool {
    pub fn new(automations: Option<Arc<AutomationsStore>>, wiki: Option<Arc<WikiRegistry>>) -> Self {
        Self { automations, wiki }
    }
}

#[async_trait]
impl Tool for PendingApprovalsTool {
    fn name(&self) -> &str { "pending_approvals" }

    fn description(&self) -> &str {
        "List the things waiting for THIS user's approval: agent-created \
         schedules that haven't been approved yet, and wiki edits in the review \
         queue. Use this to summarise what's pending when the user asks (or when \
         a check-in/briefing mentioned pending approvals and they want details). \
         Returns each item's id, a short summary, and counts. Read-only — to \
         actually approve, use `approve_pending` after the user says so."
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = caller(&args)?;
        let schedules = self.automations.as_ref()
            .map(|s| pending_schedules(s, &user_id))
            .unwrap_or_default();
        let wiki = self.wiki.as_ref()
            .map(|r| pending_wiki(r, &user_id))
            .unwrap_or_default();
        Ok(ToolResult::success(json!({
            "pending_schedule_count": schedules.len(),
            "pending_wiki_count":     wiki.len(),
            "schedules":              schedules,
            "wiki_edits":             wiki,
        }).to_string()))
    }
}

// ── approve_pending (mutating, user-gated) ────────────────────────────────────

pub struct ApprovePendingTool {
    automations: Option<Arc<AutomationsStore>>,
    wiki:        Option<Arc<WikiRegistry>>,
}

impl ApprovePendingTool {
    pub fn new(automations: Option<Arc<AutomationsStore>>, wiki: Option<Arc<WikiRegistry>>) -> Self {
        Self { automations, wiki }
    }
}

#[async_trait]
impl Tool for ApprovePendingTool {
    fn name(&self) -> &str { "approve_pending" }

    fn description(&self) -> &str {
        "Approve items waiting for the user's approval — agent-created \
         schedules and/or wiki review-queue edits. ONLY call this when the user \
         has explicitly told you to approve (e.g. \"approve all\", \"approve the \
         backup schedule\"); never approve on your own initiative, and never \
         from a check-in or briefing. `kind` selects what to approve \
         (`schedule` | `wiki` | `all`, default `all`). Omit `ids` to approve \
         every pending item of that kind, or pass specific ids (from \
         `pending_approvals`) to approve just those. Returns how many were \
         approved and any that failed."
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    // Mutates MIRA-owned state only (flips statuses in our DBs) — Pure tier,
    // same as the other automations/wiki tools. The real safety is the
    // user-instruction guardrail in the description + the human in the loop.
    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["schedule", "wiki", "all"],
                    "description": "Which kind of pending item to approve. Default `all`."
                },
                "ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Specific item ids to approve (from `pending_approvals`). Omit to approve all pending items of the chosen kind."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = caller(&args)?;
        let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("all");
        let ids: Option<Vec<String>> = args.get("ids").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
        let id_filter = |id: &str| ids.as_ref().map(|set| set.iter().any(|x| x == id)).unwrap_or(true);

        let mut approved_schedules = 0usize;
        let mut approved_wiki = 0usize;
        let mut errors: Vec<String> = Vec::new();

        // Schedules.
        if matches!(kind, "schedule" | "all") {
            if let Some(store) = self.automations.as_ref() {
                let pending: Vec<String> = store.list_schedules(Some(&user_id)).unwrap_or_default()
                    .into_iter()
                    .filter(|s| matches!(s.status, ScheduleStatus::PendingApproval))
                    .map(|s| s.id)
                    .filter(|id| id_filter(id))
                    .collect();
                for id in pending {
                    match store.approve_schedule(&id) {
                        Ok(_)  => approved_schedules += 1,
                        Err(e) => errors.push(format!("schedule {id}: {e}")),
                    }
                }
            }
        }

        // Wiki ops.
        if matches!(kind, "wiki" | "all") {
            if let Some(reg) = self.wiki.as_ref() {
                if let Ok(wiki) = reg.for_user(&user_id) {
                    let pending: Vec<String> = wiki.list_pending_ops().unwrap_or_default()
                        .into_iter()
                        .map(|e| e.op_id)
                        .filter(|id| id_filter(id))
                        .collect();
                    for id in pending {
                        // Reviewer = the user themselves (they instructed it).
                        match wiki.approve_op(&id, &user_id) {
                            Ok(())  => approved_wiki += 1,
                            Err(e)  => errors.push(format!("wiki op {id}: {e}")),
                        }
                    }
                }
            }
        }

        Ok(ToolResult::success(json!({
            "approved_schedules": approved_schedules,
            "approved_wiki_edits": approved_wiki,
            "errors": errors,
        }).to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn requires_caller_identity() {
        let tool = PendingApprovalsTool::new(None, None);
        let r = tool.execute(json!({})).await;
        assert!(r.is_err(), "missing _user_id should error");
    }

    #[tokio::test]
    async fn degrades_gracefully_with_no_stores() {
        // No automations/wiki wired → empty lists, never panics.
        let list = PendingApprovalsTool::new(None, None);
        let r = list.execute(json!({"_user_id": "u1"})).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("\"pending_schedule_count\":0"));
        assert!(r.output.contains("\"pending_wiki_count\":0"));

        let approve = ApprovePendingTool::new(None, None);
        let r = approve.execute(json!({"_user_id": "u1", "kind": "all"})).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("\"approved_schedules\":0"));
        assert!(r.output.contains("\"approved_wiki_edits\":0"));
    }
}
