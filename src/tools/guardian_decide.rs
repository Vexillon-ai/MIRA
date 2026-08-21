// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/guardian_decide.rs
//! `guardian_decide` — conversational approval of MIRA-Guardian proposals (P4b).
//!
//! Lets the operator approve/decline a pending Guardian proposal by replying on
//! any channel ("approve" / "decline") instead of clicking the web button. The
//! main agent calls this when the operator responds; deterministic server code
//! then executes (same shared `execute_action` as the web approve handler).
//!
//! Authorization via the trusted injected `_user_id`: the Guardian's operator
//! (`notify_user_id`) may decide any action; a **member-scoped** action may also
//! be decided by that member's **guardian** (co-parent). System/household actions
//! stay operator-only. A member-scoped device *approval* is routed to the web UI
//! (which runs the ownership-verified actuation) rather than actuated from
//! chat; *declines* are handled in chat. Acts on the most-recent pending proposal
//! when no id is given.

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
    /// The Guardian's operator (`notify_user_id`) — may decide any action.
    authorized_user: Option<String>,
    /// Companion system — resolves a ward's guardian(s) so a co-parent guardian
    /// (not just the operator) can decide a member-scoped action by chat. `None`
    /// when the companion feature is off → operator-only (prior behaviour).
    companion:       Option<Arc<crate::companion::CompanionSystem>>,
}

impl GuardianDecideTool {
    pub fn new(
        store:           Arc<GuardianActionStore>,
        automations:     Option<Arc<AutomationsStore>>,
        channel_manager: Arc<OnceLock<Arc<RwLock<ChannelManager>>>>,
        audit:           Option<Arc<AuditStore>>,
        authorized_user: Option<String>,
        companion:       Option<Arc<crate::companion::CompanionSystem>>,
    ) -> Self {
        Self { store, automations, channel_manager, audit, authorized_user, companion }
    }

    fn record(&self, id: &str, kind: &str, decision: &str, detail: String) {
        if let Some(a) = &self.audit {
            let _ = a.record(guardian_agent_id(), None, AuditEvent::GuardianAction {
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
        use crate::agent::guardian_actions::{approval_scope, ApprovalScope};

        // Trusted caller id — `_user_id` is injected by the turn, not the model.
        let caller = args.get("_user_id").and_then(|v| v.as_str()).unwrap_or("");
        if caller.is_empty() {
            return Ok(ToolResult::failure("Only an authorised operator or guardian can decide Guardian actions."));
        }
        let is_operator = self.authorized_user.as_deref() == Some(caller);
        let store_opt = self.companion.as_ref().map(|c| c.store());

        // Coarse gate: the operator, or anyone who is a guardian of at least one
        // ward, may reach the decision logic. The fine-grained per-action check
        // (System = operator-only; Member = that member's guardian or operator)
        // is applied once the target action is known.
        let is_any_guardian = store_opt
            .map(|s| !s.wards_of(caller).unwrap_or_default().is_empty())
            .unwrap_or(false);
        if !is_operator && !is_any_guardian {
            return Ok(ToolResult::failure(
                "Only MIRA-Guardian's operator or a ward's guardian can approve or decline its actions."));
        }

        let action = match args.get("action_id").and_then(|v| v.as_str()) {
            Some(id) => self.store.get(id)?,
            None     => self.store.list(Some(GuardianActionStatus::Pending), 1)?.into_iter().next(),
        };
        let Some(a) = action.filter(|a| a.status == GuardianActionStatus::Pending) else {
            return Ok(ToolResult::success("There is no pending MIRA-Guardian proposal to act on."));
        };

        // Fine-grained authorization by the action's approval scope.
        let scope = approval_scope(a.kind, a.target.as_deref());
        let authorized = match &scope {
            ApprovalScope::System     => is_operator,
            ApprovalScope::Member(m)  => is_operator || store_opt
                .map(|s| crate::companion::governance::guardians_of(s, m).iter().any(|g| g == caller))
                .unwrap_or(false),
        };
        if !authorized {
            return Ok(ToolResult::failure(match &scope {
                ApprovalScope::System    => "Only the Guardian's operator can decide household/system actions.",
                ApprovalScope::Member(_) => "Only this member's guardian (or the operator) can decide this action.",
            }.to_string()));
        }

        let decision = args.get("decision").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase();
        let kind = a.kind.as_str();
        let approve = matches!(decision.as_str(), "approve" | "approved" | "yes" | "do it" | "go ahead" | "ok");
        let decline = matches!(decision.as_str(), "decline" | "declined" | "no" | "reject" | "deny" | "hold" | "stop");

        if approve {
            // Member-scoped device actions are NOT actuated from chat: the
            // ownership-verified actuation path lives in the web approve
            // endpoint, which resolves the tool registry. Route the approver
            // there instead of executing-then-failing, and leave the row Pending
            // so the web approval still works.
            if matches!(scope, ApprovalScope::Member(_)) {
                return Ok(ToolResult::success(format!(
                    "This affects a family member's device. For safety it's approved in the web UI \
                     (Settings → Guardian → pending actions), where MIRA verifies the device is \
                     registered to that member before acting. It's still pending (id={}).", a.id)));
            }
            self.record(&a.id, kind, "approved", format!("approved via chat by {caller}"));
            let mgr = self.channel_manager.get();
            // System actions only here — they don't consult the companion store,
            // so `None` is correct (and fails closed for any member kind).
            match execute_action(a.kind, a.target.as_deref(), self.automations.as_ref(), mgr, None, None).await {
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
            // Declining never actuates — safe by chat for the operator or any
            // authorised guardian.
            let _ = self.store.decide(&a.id, GuardianActionStatus::Declined, &format!("declined via chat by {caller}"));
            self.record(&a.id, kind, "declined", format!("declined via chat by {caller}"));
            Ok(ToolResult::success(format!("Declined the pending '{kind}' proposal.")))
        } else {
            Ok(ToolResult::failure("`decision` must be 'approve' or 'decline'."))
        }
    }
}
