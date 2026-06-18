// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/guardian_decide.rs
//! `guardian_decide` — conversational approval of MIRA-Guardian proposals (P4b).
//!
//! Lets the operator approve/decline a pending Guardian proposal by replying on
//! any channel ("approve" / "decline") instead of clicking the web button. The
//! main agent calls this when the operator responds; deterministic server code
//! then executes (same shared `execute_action` as the web approve handler).
//!
//! Authorized to the Guardian's configured operator only (the watchdog
//! `notify_user_id`) via the trusted injected `_user_id` — a non-operator turn
//! cannot decide. Acts on the most-recent pending proposal when no id is given.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::agent::audit::{guardian_agent_id, AuditEvent, AuditStore};
use crate::agent::guardian_actions::{execute_action, GuardianActionStatus, GuardianActionStore};
use crate::automations::AutomationsStore;
use crate::gateway::channel_manager::ChannelManager;
use crate::tools::{Tool, ToolArgs, ToolResult, ToolVisibility, Tier};
use crate::MiraError;

pub struct GuardianDecideTool {
    store:           Arc<GuardianActionStore>,
    automations:     Option<Arc<AutomationsStore>>,
    /// Deferred — the ChannelManager is built after the tool registry, so this
    /// is filled by the gateway later. Empty → restart_bridge isn't executable
    /// yet (the action fails gracefully).
    channel_manager: Arc<OnceLock<Arc<RwLock<ChannelManager>>>>,
    audit:           Option<Arc<AuditStore>>,
    /// The only user permitted to decide (the Guardian's `notify_user_id`).
    authorized_user: Option<String>,
}

impl GuardianDecideTool {
    pub fn new(
        store:           Arc<GuardianActionStore>,
        automations:     Option<Arc<AutomationsStore>>,
        channel_manager: Arc<OnceLock<Arc<RwLock<ChannelManager>>>>,
        audit:           Option<Arc<AuditStore>>,
        authorized_user: Option<String>,
    ) -> Self {
        Self { store, automations, channel_manager, audit, authorized_user }
    }

    fn record(&self, id: &str, kind: &str, decision: &str, detail: String) {
        if let Some(a) = &self.audit {
            let _ = a.record(guardian_agent_id(), AuditEvent::GuardianAction {
                action_id: id.to_string(), action_kind: kind.to_string(),
                decision: decision.to_string(), detail: Some(detail),
            });
        }
    }
}

#[async_trait]
impl Tool for GuardianDecideTool {
    fn name(&self) -> &str { "guardian_decide" }

    fn description(&self) -> &str {
        "Approve or decline a pending MIRA-Guardian action proposal on the operator's behalf. \
         Call this only when the operator clearly approves or declines a Guardian proposal you've \
         told them about (e.g. they reply 'approve', 'do it', 'decline', 'no'). `decision` is \
         'approve' or 'decline'. Omit `action_id` to act on the most-recent pending proposal. \
         Approving executes the bounded action server-side; declining never executes."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "decision":  { "type": "string", "enum": ["approve", "decline"] },
                "action_id": { "type": "string", "description": "Optional; defaults to the latest pending proposal." }
            },
            "required": ["decision"]
        })
    }

    // Admin-tier (kept out of the user palette); the real gate is the operator
    // check below, which the turn-time allowlist bypass can't sidestep.
    fn visibility(&self) -> ToolVisibility { ToolVisibility::Admin }
    fn tier(&self) -> Tier { Tier::System }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        // Trusted operator gate — `_user_id` is injected by the turn, not the model.
        let caller = args.get("_user_id").and_then(|v| v.as_str()).unwrap_or("");
        match self.authorized_user.as_deref() {
            Some(u) if !caller.is_empty() && u == caller => {}
            _ => return Ok(ToolResult::failure(
                "Only MIRA-Guardian's configured operator can approve or decline its actions.")),
        }

        let action = match args.get("action_id").and_then(|v| v.as_str()) {
            Some(id) => self.store.get(id)?,
            None     => self.store.list(Some(GuardianActionStatus::Pending), 1)?.into_iter().next(),
        };
        let Some(a) = action.filter(|a| a.status == GuardianActionStatus::Pending) else {
            return Ok(ToolResult::success("There is no pending MIRA-Guardian proposal to act on."));
        };

        let decision = args.get("decision").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase();
        let kind = a.kind.as_str();
        let approve = matches!(decision.as_str(), "approve" | "approved" | "yes" | "do it" | "go ahead" | "ok");
        let decline = matches!(decision.as_str(), "decline" | "declined" | "no" | "reject" | "deny" | "hold" | "stop");

        if approve {
            self.record(&a.id, kind, "approved", format!("approved via chat by {caller}"));
            let mgr = self.channel_manager.get();
            match execute_action(a.kind, a.target.as_deref(), self.automations.as_ref(), mgr).await {
                Ok(msg) => {
                    let _ = self.store.decide(&a.id, GuardianActionStatus::Executed, &msg);
                    self.record(&a.id, kind, "executed", msg.clone());
                    Ok(ToolResult::success(format!("Approved + executed '{kind}': {msg}")))
                }
                Err(e) => {
                    let _ = self.store.decide(&a.id, GuardianActionStatus::Failed, &e);
                    self.record(&a.id, kind, "failed", e.clone());
                    Ok(ToolResult::success(format!("Approved '{kind}' but execution FAILED: {e}")))
                }
            }
        } else if decline {
            let _ = self.store.decide(&a.id, GuardianActionStatus::Declined, &format!("declined via chat by {caller}"));
            self.record(&a.id, kind, "declined", format!("declined via chat by {caller}"));
            Ok(ToolResult::success(format!("Declined the pending '{kind}' proposal.")))
        } else {
            Ok(ToolResult::failure("`decision` must be 'approve' or 'decline'."))
        }
    }
}
