// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/guardian_propose.rs
//! `guardian_propose_action` — the MIRA-Guardian's *propose* tool (P4).
//!
//! The Guardian (in `active` mode only) calls this to PROPOSE a bounded,
//! reversible remediation. It does **not** execute — it records a pending
//! proposal; a human approves out-of-band and only then does deterministic
//! server code execute (P4a-2). This separation is the core guardrail: the LLM
//! can only ever propose, never directly restart/requeue/etc.
//!
//! System-visibility; added to the Guardian's allowlist only when
//! `guardian.mode = active` (see `agent::guardian::active_tools`).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::agent::audit::{AuditEvent, AuditStore, guardian_agent_id};
use crate::agent::guardian_actions::{GuardianActionKind, GuardianActionStore};
use crate::gateway::channel_manager::ChannelManager;
use crate::tools::{Tool, ToolArgs, ToolResult, ToolVisibility, Tier};
use crate::MiraError;

pub struct GuardianProposeTool {
    store: Arc<GuardianActionStore>,
    /// HMAC-chained audit log (optional). Records the "proposed" event so the
    /// full proposal→decision→execution chain is tamper-evident.
    audit: Option<Arc<AuditStore>>,
    /// Deferred ChannelManager (built after the tool registry; the gateway
    /// fills it later). Used to validate a `restart_bridge` target against the
    /// real channel account ids at propose time — so a hallucinated target
    /// like "signal"/"guardian" is rejected up front with the valid list,
    /// instead of being recorded as pending and failing "account not found" on
    /// approval. Empty (not yet filled, or no accounts) → validation is skipped
    /// and execution's own graceful failure is the backstop.
    channel_manager: Arc<OnceLock<Arc<RwLock<ChannelManager>>>>,
}

impl GuardianProposeTool {
    pub fn new(
        store: Arc<GuardianActionStore>,
        audit: Option<Arc<AuditStore>>,
        channel_manager: Arc<OnceLock<Arc<RwLock<ChannelManager>>>>,
    ) -> Self {
        Self { store, audit, channel_manager }
    }
}

#[async_trait]
impl Tool for GuardianProposeTool {
    fn name(&self) -> &str { "guardian_propose_action" }

    fn description(&self) -> &str {
        "Propose a single bounded, reversible remediation for operator approval. You do NOT \
         execute it — it is recorded as pending and the operator approves it out-of-band. Only \
         propose when a detector-confirmed problem has a clear, safe fix. `action` must be one of: \
         rerun_audit (re-run the health audit), restart_bridge (restart a wedged channel; `target` \
         = the exact channel account id from guardian_inspect's Channels section — NOT a guessed \
         label), requeue_automation (requeue a stuck schedule; `target` = its name), trim_logs \
         (relieve disk pressure). Always give a one-line `reason`."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": GuardianActionKind::all(),
                            "description": "Which bounded action to propose." },
                "target": { "type": "string",
                            "description": "Required for restart_bridge (account id) and requeue_automation (schedule name); omit otherwise." },
                "reason": { "type": "string", "description": "One-line justification tied to the triggered detector(s)." }
            },
            "required": ["action", "reason"]
        })
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::system("guardian") }
    fn tier(&self) -> Tier { Tier::System }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("").trim();
        let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let target = args.get("target").and_then(|v| v.as_str())
            .map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| s.to_string());

        let Some(kind) = GuardianActionKind::parse(action) else {
            return Ok(ToolResult::failure(format!(
                "Unknown action {action:?}. Allowed: {}.", GuardianActionKind::all().join(", ")
            )));
        };
        if kind.needs_target() && target.is_none() {
            return Ok(ToolResult::failure(format!("action '{}' requires a `target`.", kind.as_str())));
        }
        if reason.is_empty() {
            return Ok(ToolResult::failure("a one-line `reason` is required.".to_string()));
        }

        // For restart_bridge, validate the target is a REAL channel account id
        // before recording the proposal. The model is otherwise prone to
        // guessing a plausible-sounding label ("signal", "guardian",
        // "voice-synthesis-tts") that no account matches, which then fails
        // "account not found" on approval. Reject early with the valid ids so
        // the model can retry with a real one this turn. Fail open when we
        // can't enumerate accounts (manager not wired yet / none configured).
        if kind == GuardianActionKind::RestartBridge {
            if let (Some(t), Some(mgr)) = (target.as_deref(), self.channel_manager.get()) {
                let known = mgr.read().await.known_account_ids();
                if !known.is_empty() && !known.iter().any(|id| id == t) {
                    return Ok(ToolResult::failure(format!(
                        "No channel account with id {t:?} — restart_bridge `target` must be an \
                         exact channel account id. Valid ids: {}. Re-propose with one of these, \
                         or just alert the operator if none fits.",
                        known.iter().map(|id| format!("{id:?}")).collect::<Vec<_>>().join(", "),
                    )));
                }
            }
        }

        let id = self.store.create_pending(kind, target.as_deref(), &reason)?;
        if let Some(audit) = &self.audit {
            let _ = audit.record(guardian_agent_id(), None, AuditEvent::GuardianAction {
                action_id:   id.clone(),
                action_kind: kind.as_str().to_string(),
                decision:    "proposed".to_string(),
                detail:      Some(reason.clone()),
            });
        }
        let tgt = target.as_deref().map(|t| format!(" {t}")).unwrap_or_default();
        tracing::info!(
            "guardian_propose_action: proposed {}{} — {} [id={id}, pending approval]",
            kind.as_str(), tgt, reason,
        );
        Ok(ToolResult::success(format!(
            "Proposed (id={id}): {}{} — {}. Recorded as PENDING; it will run only after the \
             operator approves. Do not assume it has run.", kind.as_str(), tgt, reason,
        )))
    }
}
