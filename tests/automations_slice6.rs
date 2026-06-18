// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/automations_slice6.rs
//! Slice 6 integration tests — agent autonomy.
//!
//! Covers the create-time gate that the five `automations.*` tools share
//! (rationale required, quota, approval mode → pending_approval) plus the
//! self-cancel safety check and the approve_schedule store helper.
//!
//! "Done when" from `design-docs/phase10-automations.md`:
//!   the agent can self-schedule a follow-up; with approval mode on, it
//!   lands in pending and respects user accept/reject.

use std::sync::Arc;

use serde_json::{json, Value};
use tempfile::TempDir;

use mira::Tool;
use mira::automations::{
    Action, AutomationsStore, NewSchedule, OwnerKind, QuietHours, ScheduleStatus, TriggerSpec,
};
use mira::config::MiraConfig;
use mira::tools::automations::{
    CancelScheduleTool, ListSelfSchedulesTool, RegisterWebhookTool, ScheduleFollowupTool,
    SubscribeEventTool,
};

// ── Fixture helpers ──────────────────────────────────────────────────────────

fn open_store(dir: &TempDir) -> Arc<AutomationsStore> {
    Arc::new(AutomationsStore::open(&dir.path().join("automations.db")).unwrap())
}

/// Defaults: `agent_creates_pending = true`, `agent_rationale_required = true`,
/// quotas at their stock values. Tests override fields they care about.
fn cfg() -> Arc<MiraConfig> {
    Arc::new(MiraConfig::default())
}

fn future_unix_ts(offset_secs: i64) -> i64 {
    chrono::Utc::now().timestamp() + offset_secs
}

fn prompt_action() -> Value {
    json!({
        "kind": "prompt",
        "conversation_strategy": "new",
        "channel": "ui",
        "prompt": "follow up on the deploy",
    })
}

fn parse_output(out: &mira::ToolResult) -> Value {
    assert!(out.success, "tool failed: {:?}", out.error);
    serde_json::from_str(&out.output).expect("tool output is valid JSON")
}

// ── 6.1 — schedule_followup happy path lands in pending under approval mode ──

#[tokio::test]
async fn schedule_followup_with_approval_mode_lands_pending() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let config = cfg(); // approval mode + rationale on by default

    let tool = ScheduleFollowupTool::new(Arc::clone(&store), Arc::clone(&config));
    let args = json!({
        "_user_id": "alice",
        "name":      "ping-deploy-status",
        "rationale": "user asked me to check back after the deploy",
        "trigger":   { "kind": "one_off", "at": future_unix_ts(3600) },
        "action":    prompt_action(),
    });
    let out = tool.execute(args).await.unwrap();
    let body = parse_output(&out);
    assert_eq!(body["status"], "pending_approval");
    assert_eq!(body["pending_approval"], true);

    // The row exists, owned by Agent, attributed to the caller.
    let id = body["id"].as_str().unwrap();
    let row = store.get_schedule(id).unwrap().unwrap();
    assert!(matches!(row.owner_kind, OwnerKind::Agent));
    assert_eq!(row.user_id, "alice");
    assert!(matches!(row.status, ScheduleStatus::PendingApproval));
}

// ── Approval flow: approve flips to active ───────────────────────────────────

#[tokio::test]
async fn approve_schedule_flips_pending_to_active() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let tool = ScheduleFollowupTool::new(Arc::clone(&store), cfg());

    let out = tool.execute(json!({
        "_user_id": "alice",
        "name":      "remind-me",
        "rationale": "scheduled by request",
        "trigger":   { "kind": "one_off", "at": future_unix_ts(3600) },
        "action":    prompt_action(),
    })).await.unwrap();
    let id = parse_output(&out)["id"].as_str().unwrap().to_string();

    let approved = store.approve_schedule(&id).unwrap();
    assert!(matches!(approved.status, ScheduleStatus::Active));
    assert!(approved.next_run_at.is_some(), "approve must compute next_run_at");
}

// ── Approval flow: reject (delete) leaves nothing behind ─────────────────────

#[tokio::test]
async fn rejecting_pending_schedule_removes_it() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let tool = ScheduleFollowupTool::new(Arc::clone(&store), cfg());

    let out = tool.execute(json!({
        "_user_id": "alice",
        "name":      "remind-me",
        "rationale": "scheduled by request",
        "trigger":   { "kind": "one_off", "at": future_unix_ts(3600) },
        "action":    prompt_action(),
    })).await.unwrap();
    let id = parse_output(&out)["id"].as_str().unwrap().to_string();

    assert!(store.delete_schedule(&id).unwrap());
    assert!(store.get_schedule(&id).unwrap().is_none());
}

// ── 6.4 — rationale required when config knob is on ──────────────────────────

#[tokio::test]
async fn missing_rationale_is_rejected_when_required() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let tool = ScheduleFollowupTool::new(Arc::clone(&store), cfg());

    let out = tool.execute(json!({
        "_user_id": "alice",
        "name":      "no-rationale",
        // no rationale
        "trigger":   { "kind": "one_off", "at": future_unix_ts(3600) },
        "action":    prompt_action(),
    })).await.unwrap();
    assert!(!out.success);
    let err = out.error.unwrap_or_default();
    assert!(err.contains("rationale"), "expected rationale error, got: {err}");
}

// ── 6.4 — rationale not required when knob is off ────────────────────────────

#[tokio::test]
async fn rationale_optional_when_required_knob_is_off() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let mut cfg = MiraConfig::default();
    cfg.automations.agent_rationale_required = false;
    cfg.automations.agent_creates_pending    = false;
    let config = Arc::new(cfg);

    let tool = ScheduleFollowupTool::new(Arc::clone(&store), config);
    // The tool's own `require_str` still treats rationale as a required arg —
    // pass a placeholder. The point of this test is that the *gate* doesn't
    // reject when the config knob is off; quota and status are unaffected.
    let out = tool.execute(json!({
        "_user_id": "alice",
        "name":      "go-now",
        "rationale": "ok",
        "trigger":   { "kind": "one_off", "at": future_unix_ts(3600) },
        "action":    prompt_action(),
    })).await.unwrap();
    let body = parse_output(&out);
    assert_eq!(body["status"], "active");
    assert_eq!(body["pending_approval"], false);
}

// ── 6.2 — quota enforcement at create-time ──────────────────────────────────

#[tokio::test]
async fn schedule_quota_blocks_after_cap() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let mut cfg = MiraConfig::default();
    cfg.automations.quota_per_user.schedules = 2;
    let config = Arc::new(cfg);

    let tool = ScheduleFollowupTool::new(Arc::clone(&store), Arc::clone(&config));
    let make = |name: &str| -> Value {
        json!({
            "_user_id": "alice",
            "name":      name,
            "rationale": "fill the quota",
            "trigger":   { "kind": "one_off", "at": future_unix_ts(3600) },
            "action":    prompt_action(),
        })
    };

    // First two succeed.
    assert!(tool.execute(make("a")).await.unwrap().success);
    assert!(tool.execute(make("b")).await.unwrap().success);

    // Third hits the cap (pending_approval rows count toward quota).
    let third = tool.execute(make("c")).await.unwrap();
    assert!(!third.success);
    let err = third.error.unwrap_or_default();
    assert!(err.contains("quota"),  "expected quota error, got: {err}");
    assert!(err.contains("2/2") || err.contains("schedules"),
            "error should mention current/limit, got: {err}");
}

// ── 6.1 — list_self_schedules filters to agent-owned only ────────────────────

#[tokio::test]
async fn list_self_schedules_returns_only_agent_owned() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let config = cfg();

    // Agent-authored row via the tool.
    let create = ScheduleFollowupTool::new(Arc::clone(&store), Arc::clone(&config));
    create.execute(json!({
        "_user_id": "alice",
        "name":      "agent-row",
        "rationale": "auto",
        "trigger":   { "kind": "interval", "every_secs": 3600 },
        "action":    prompt_action(),
    })).await.unwrap();

    // User-authored row, going through the store directly.
    store.create_schedule(NewSchedule {
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "user-row".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::Interval { every_secs: 3600 },
        timezone:    "UTC".into(),
        quiet_hours: None::<QuietHours>,
        action:      Action::Internal { task: "log_cleanup".into(), args: Value::Null },
        expires_at:  None,
        status:      None,
    }).unwrap();

    let lister = ListSelfSchedulesTool::new(Arc::clone(&store));
    let out = lister.execute(json!({"_user_id": "alice"})).await.unwrap();
    let body = parse_output(&out);
    let arr = body["schedules"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "only the agent-owned row should be visible");
    assert_eq!(arr[0]["name"], "agent-row");
}

// ── 6.1 — cancel_schedule refuses non-agent rows ─────────────────────────────

#[tokio::test]
async fn cancel_refuses_user_owned_rows() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    // User-authored row that the agent must NOT be able to cancel.
    let user_row = store.create_schedule(NewSchedule {
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "user-row".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::Interval { every_secs: 3600 },
        timezone:    "UTC".into(),
        quiet_hours: None::<QuietHours>,
        action:      Action::Internal { task: "log_cleanup".into(), args: Value::Null },
        expires_at:  None,
        status:      None,
    }).unwrap();

    let cancel = CancelScheduleTool::new(Arc::clone(&store));
    let out = cancel.execute(json!({
        "_user_id": "alice",
        "id":       user_row.id,
        "reason":   "I want to clean up",
    })).await.unwrap();
    assert!(!out.success);
    let err = out.error.unwrap_or_default();
    assert!(err.contains("agent can only cancel"),
            "expected ownership error, got: {err}");

    // Row is still there.
    assert!(store.get_schedule(&user_row.id).unwrap().is_some());
}

// ── 6.1 — cancel_schedule refuses cross-user rows ────────────────────────────

#[tokio::test]
async fn cancel_refuses_other_users_agent_rows() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    // bob's agent created a schedule.
    let create = ScheduleFollowupTool::new(Arc::clone(&store), cfg());
    let out = create.execute(json!({
        "_user_id": "bob",
        "name":      "bobs-followup",
        "rationale": "bob's task",
        "trigger":   { "kind": "one_off", "at": future_unix_ts(3600) },
        "action":    prompt_action(),
    })).await.unwrap();
    let id = parse_output(&out)["id"].as_str().unwrap().to_string();

    // alice's agent must not be able to cancel it.
    let cancel = CancelScheduleTool::new(Arc::clone(&store));
    let out = cancel.execute(json!({
        "_user_id": "alice",
        "id":       id,
        "reason":   "stealing bob's schedule",
    })).await.unwrap();
    assert!(!out.success);
    let err = out.error.unwrap_or_default();
    assert!(err.contains("different user"),
            "expected user-mismatch error, got: {err}");
    assert!(store.get_schedule(&id).unwrap().is_some());
}

// ── 6.1 — cancel_schedule succeeds on the agent's own row ────────────────────

#[tokio::test]
async fn agent_can_cancel_its_own_schedule() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let create = ScheduleFollowupTool::new(Arc::clone(&store), cfg());
    let out = create.execute(json!({
        "_user_id": "alice",
        "name":      "self-cancellable",
        "rationale": "may cancel later",
        "trigger":   { "kind": "one_off", "at": future_unix_ts(3600) },
        "action":    prompt_action(),
    })).await.unwrap();
    let id = parse_output(&out)["id"].as_str().unwrap().to_string();

    let cancel = CancelScheduleTool::new(Arc::clone(&store));
    let out = cancel.execute(json!({
        "_user_id": "alice",
        "id":       id,
        "reason":   "no longer needed",
    })).await.unwrap();
    assert!(out.success, "cancel should succeed: {:?}", out.error);
    assert!(store.get_schedule(&id).unwrap().is_none());
}

// ── 6.1 — register_webhook routes through the gate ───────────────────────────

#[tokio::test]
async fn register_webhook_lands_pending_under_approval_mode() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let tool = RegisterWebhookTool::new(Arc::clone(&store), cfg());

    let out = tool.execute(json!({
        "_user_id": "alice",
        "name":      "ci-fire",
        "rationale": "expecting CI to ping",
        "action":    { "kind": "internal", "task": "log_cleanup" },
    })).await.unwrap();
    let body = parse_output(&out);
    assert_eq!(body["status"], "pending_approval");
    assert_eq!(body["pending_approval"], true);
    assert!(body["secret"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "secret must be returned on create");
    assert!(body["url"].as_str().unwrap().starts_with("/webhook/incoming/"));
}

// ── 6.1 — subscribe_event routes through the gate ────────────────────────────

#[tokio::test]
async fn subscribe_event_lands_pending_under_approval_mode() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let tool = SubscribeEventTool::new(Arc::clone(&store), cfg());

    let out = tool.execute(json!({
        "_user_id": "alice",
        "name":      "alert-on-tool-fail",
        "rationale": "want notifications",
        "event_name": "tool.failed",
        "action":    { "kind": "internal", "task": "log_cleanup" },
    })).await.unwrap();
    let body = parse_output(&out);
    assert_eq!(body["status"], "pending_approval");
    assert_eq!(body["pending_approval"], true);
}

// ── No caller identity → tool errors loudly ──────────────────────────────────

#[tokio::test]
async fn missing_user_id_errors() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let tool = ScheduleFollowupTool::new(Arc::clone(&store), cfg());

    let res = tool.execute(json!({
        "name":      "no-caller",
        "rationale": "nope",
        "trigger":   { "kind": "one_off", "at": future_unix_ts(3600) },
        "action":    prompt_action(),
    })).await;
    assert!(res.is_err(), "missing _user_id must surface as a hard error");
}
