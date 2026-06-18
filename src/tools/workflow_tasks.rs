// SPDX-License-Identifier: AGPL-3.0-or-later

//! Workflow orchestration tools (Phase C) — `run_workflow`, `list_workflows`.
//!
//! `run_workflow` kicks off a saved [`WorkflowDefinition`](crate::agent::WorkflowDefinition)
//! DAG against the [`Orchestrator`], returns the `run_id` immediately, and
//! auto-registers a completion delivery so the user is pinged on the
//! originating channel when the whole run finishes — exactly like
//! `spawn_background_task` does for a single worker, but keyed on the
//! `agent.workflow.completed` event by `run_id`.
//!
//! `list_workflows` lets the model discover what's available (and each
//! workflow's shape) before running one.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{info, warn};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::agent::{Orchestrator, WorkflowStore};
use crate::automations::{
    agent_gate::gate_create_event_subscription, Action, AutomationStatus, AutomationsStore,
    NewEventSubscription, OwnerKind,
};
use crate::config::MiraConfig;
use crate::MiraError;

// ── run_workflow ─────────────────────────────────────────────────────────────

pub struct RunWorkflowTool {
    orchestrator: Arc<Orchestrator>,
    store:        Arc<WorkflowStore>,
    automations:  Option<Arc<AutomationsStore>>,
    config:       Arc<MiraConfig>,
}

impl RunWorkflowTool {
    pub fn new(
        orchestrator: Arc<Orchestrator>,
        store:        Arc<WorkflowStore>,
        automations:  Option<Arc<AutomationsStore>>,
        config:       Arc<MiraConfig>,
    ) -> Self {
        Self { orchestrator, store, automations, config }
    }
}

#[async_trait]
impl Tool for RunWorkflowTool {
    fn name(&self) -> &str { "run_workflow" }

    fn description(&self) -> &str {
        "Run a saved multi-agent **workflow** — a DAG of steps over named \
         agents / skills, with each step's output fed into the next. Returns \
         a `run_id` immediately; the user is pinged on this channel when the \
         whole run finishes. Call `list_workflows` first to see what's \
         available and what input each expects. Use this when a request maps \
         to an existing workflow (e.g. \"run the weekly brief\") rather than \
         orchestrating the steps by hand."
    }

    fn tier(&self) -> Tier { Tier::System }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["workflow"],
            "properties": {
                "workflow": {
                    "type": "string",
                    "description": "Name (handle) of a saved workflow, from `list_workflows`."
                },
                "input": {
                    "type": "string",
                    "description": "The run input — interpolated into steps that reference `{{input}}`. Optional if the workflow doesn't need one."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = args.get("_user_id").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty()).map(String::from)
            .ok_or_else(|| MiraError::ToolError("run_workflow called without caller identity".into()))?;
        let conv_id = args.get("_conversation_id").and_then(|v| v.as_str()).map(String::from);
        let channel = args.get("_channel").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty()).map(String::from)
            .unwrap_or_else(|| "web".to_string());

        let name = match args.get("workflow").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return Ok(ToolResult::failure("'workflow' is required")),
        };
        let input = args.get("input").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let def = match self.store.get_by_name(&name) {
            Ok(Some(d)) if d.enabled => d,
            Ok(Some(_)) => return Ok(ToolResult::failure(format!(
                "workflow '{name}' is disabled — enable it first"
            ))),
            Ok(None) => return Ok(ToolResult::failure(format!(
                "no workflow named '{name}'. Call `list_workflows` to see what's available."
            ))),
            Err(e) => return Ok(ToolResult::failure(format!("failed to load workflow '{name}': {e}"))),
        };
        let step_count = def.steps.len();

        let run_id = self.orchestrator.start(def, input, Some(user_id.clone()));

        // Auto-register completion delivery on the workflow-completed event.
        let mut subscription_id: Option<String> = None;
        if let Some(store) = self.automations.as_ref() {
            match register_workflow_delivery(
                store, &self.config, &user_id, &channel, conv_id.as_deref(), &run_id, &name,
            ).await {
                Ok(id) => subscription_id = Some(id),
                Err(e) => warn!("run_workflow: completion auto-subscribe failed (run continues): {e}"),
            }
        }

        info!("run_workflow: workflow={name} run_id={run_id} steps={step_count} channel={channel} subscription={subscription_id:?}");

        Ok(ToolResult::success(json!({
            "run_id":   run_id,
            "workflow": name,
            "status":   "running",
            "steps":    step_count,
            "delivery_channel": channel,
            "note": "The user will be pinged on this channel when the workflow finishes. \
                     Use `get_workflow_run` semantics via the API, or just wait for the ping.",
        }).to_string()))
    }
}

/// Register an `agent.workflow.completed` subscription that posts the run
/// summary back to the user on the originating channel. Mirrors
/// `agent_tasks::register_completion_delivery` but keys on `run_id`.
async fn register_workflow_delivery(
    store:    &AutomationsStore,
    config:   &MiraConfig,
    user_id:  &str,
    channel:  &str,
    conv_id:  Option<&str>,
    run_id:   &str,
    workflow: &str,
) -> Result<String, String> {
    let text_template = format!(
        "{{{{payload.status_emoji}}}} Workflow `{workflow}` {{{{payload.status_label}}}}\n\n\
         {{{{payload.summary_or_error}}}}\n\n\
         _(run_id: {run_id})_"
    );
    let action = Action::ChannelMessage {
        channel:         channel.to_string(),
        to:              None,
        conversation_id: conv_id.map(str::to_string),
        text_template,
    };
    let predicate = json!({ "eq": ["payload.run_id", run_id] });

    gate_create_event_subscription(
        store, &config.automations, user_id,
        OwnerKind::Agent, Some("Auto-delivery for run_workflow"),
    ).map_err(|e| e.to_string())?;

    let new = NewEventSubscription {
        user_id:           user_id.to_string(),
        owner_kind:        OwnerKind::Agent,
        name:              format!("Workflow {run_id} delivery"),
        description:       Some(format!("Deliver result of workflow run {run_id} on {channel}")),
        rationale:         Some("Auto-registered by run_workflow".to_string()),
        event_name:        crate::events::names::AGENT_WORKFLOW_COMPLETED.to_string(),
        predicate:         Some(predicate),
        action,
        expires_at:        None,
        status:            Some(AutomationStatus::Active),
        delete_after_fire: true,
    };
    store.create_event_subscription(new)
        .map(|sub| sub.id)
        .map_err(|e| format!("create_event_subscription: {e}"))
}

// ── list_workflows ───────────────────────────────────────────────────────────

pub struct ListWorkflowsTool {
    store: Option<Arc<WorkflowStore>>,
}

impl ListWorkflowsTool {
    pub fn new(store: Option<Arc<WorkflowStore>>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ListWorkflowsTool {
    fn name(&self) -> &str { "list_workflows" }

    fn description(&self) -> &str {
        "List the saved multi-agent workflows available on this MIRA host. \
         Each is a DAG of steps over named agents / skills. Returns each \
         workflow's handle, description, and step shape so you can pick one \
         and run it with `run_workflow`. Only enabled workflows are returned."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: ToolArgs) -> Result<ToolResult, MiraError> {
        let Some(store) = self.store.as_ref() else {
            return Ok(ToolResult::success(json!({ "workflows": [] }).to_string()));
        };
        let defs = match store.list() {
            Ok(d) => d,
            Err(e) => return Ok(ToolResult::failure(format!("failed to list workflows: {e}"))),
        };
        let workflows: Vec<Value> = defs.into_iter().filter(|d| d.enabled).map(|d| json!({
            "name":        d.name,
            "description": d.description,
            "steps": d.steps.iter().map(|s| json!({
                "id":         s.id,
                "target":     s.target_skill_id(),
                "depends_on": s.depends_on,
            })).collect::<Vec<_>>(),
        })).collect();
        Ok(ToolResult::success(json!({ "workflows": workflows }).to_string()))
    }
}
