// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/automations.rs
//! HTTP surface for the automations subsystem (/).
//!
//! Coverage:
//! - `/api/schedules` — full CRUD + lifecycle (pause / resume / snooze /
//! run-now) + cron preview.
//! - `/api/automations` — unified list view (schedules now; webhooks +
//! event subs in) and the runs audit log.
//!
//! Authorization mirrors the rest of the API: a user sees their own rows
//! plus `system`-owned ones (so heartbeats are visible from settings).
//! Admins see everything; the per-row write checks fall through for them.
//! Agent-owned rows are visible to the owning user.

use std::sync::Arc;

use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::auth::AuthUser;
use crate::auth::models::Role;
use crate::automations::{
    Action, AutomationsStore, NewSchedule, OwnerKind, QuietHours, RunFilter,
    Schedule, ScheduleStatus, TriggerSpec, UpdateSchedule, Worker,
    agent_gate::{gate_create_schedule, GateError},
    next_run_at::next_n_runs,
};
use crate::config::MiraConfig;
use crate::MiraError;

fn gate_err_response(e: GateError) -> axum::response::Response {
    use GateError::*;
    match e {
        QuotaExceeded { .. } | RationaleRequired => {
            (StatusCode::FORBIDDEN, e.to_string()).into_response()
        }
        Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Common helpers ───────────────────────────────────────────────────────────

fn err_response(e: MiraError) -> axum::response::Response {
    // Map "not found"-ish ConfigErrors to 404 so the typical create→fetch→404
    // loop is distinguishable from 500. Everything else is a 500; we don't
    // try to discriminate validation errors from store errors here — the
    // handlers do their own up-front validation for the 400 cases.
    let msg = e.to_string();
    if msg.contains("not found") {
        return (StatusCode::NOT_FOUND, msg).into_response();
    }
    (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
}

fn is_admin(user_role: &Role) -> bool {
    matches!(user_role, Role::Admin)
}

// User can read this row if they own it, the row is system-owned, or they
// are admin.
fn can_read(user_id: &str, role: &Role, sched: &Schedule) -> bool {
    is_admin(role)
        || sched.user_id == user_id
        || matches!(sched.owner_kind, OwnerKind::System)
}

// User can write (pause, edit, run-now, delete) only their own rows.
// `system` rows cannot be deleted at all (the seeder resurrects them on
// boot); the store enforces that guard, but pause/resume/snooze are still
// admin-only since heartbeats are global.
fn can_write(user_id: &str, role: &Role, sched: &Schedule) -> bool {
    is_admin(role) || sched.user_id == user_id
}

// ── DTOs ─────────────────────────────────────────────────────────────────────

// Request shape for POST /api/schedules. We don't accept `user_id` from the
// caller — it's always the authenticated user (admins use ?user_id= on
// list endpoints; create-on-behalf-of can land in a later slice if needed).
#[derive(Debug, Deserialize)]
pub struct CreateScheduleRequest {
    pub name:                String,
    #[serde(default)]
    pub description:         Option<String>,
    #[serde(default)]
    pub rationale:           Option<String>,
    pub trigger:             TriggerSpec,
    #[serde(default = "default_tz")]
    pub timezone:            String,
    #[serde(default)]
    pub quiet_hours:         Option<QuietHours>,
    pub action:              Action,
    #[serde(default)]
    pub expires_at:          Option<i64>,
}

fn default_tz() -> String { "UTC".to_string() }

// Request shape for PUT /api/schedules/{id}.
#[derive(Debug, Deserialize)]
pub struct UpdateScheduleRequest {
    pub name:        String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub rationale:   Option<String>,
    pub trigger:     TriggerSpec,
    #[serde(default = "default_tz")]
    pub timezone:    String,
    #[serde(default)]
    pub quiet_hours: Option<QuietHours>,
    pub action:      Action,
    #[serde(default)]
    pub expires_at:  Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SnoozeRequest {
    // Unix seconds. Clamped to `>= now` server-side.
    pub until: i64,
}

#[derive(Debug, Deserialize)]
pub struct ListSchedulesQuery {
    // Admin-only override: list a specific user's schedules. Ignored for
    // non-admins — they always see their own + system.
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NextFiresQuery {
    pub n: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct NextFiresResponse {
    pub next_fires: Vec<i64>,
}

#[derive(Debug, Serialize)]
pub struct SchedulesListResponse {
    pub schedules: Vec<Schedule>,
}

#[derive(Debug, Serialize)]
pub struct AutomationsListResponse {
    // One unified envelope; `kind = "schedule"` for now. Webhooks and
    // event-subs join in  with their own `kind`.
    pub items: Vec<AutomationItem>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationItem {
    Schedule(Schedule),
}

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    // Filter by source kind: `schedule`, `webhook`, `event`.
    pub source:  Option<String>,
    // Specific source row id (e.g. a schedule UUID).
    pub id:      Option<String>,
    // Outcome filter: `success`, `failure`, `skipped`.
    pub outcome: Option<String>,
    // Pagination cursor: only return runs with `started_at < before`.
    pub before:  Option<i64>,
    pub limit:   Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct RunsListResponse {
    pub runs: Vec<crate::automations::AutomationRun>,
}

// ── Validation ───────────────────────────────────────────────────────────────

fn validate_create(req: &CreateScheduleRequest) -> Result<(), String> {
    if req.name.trim().is_empty() {
        return Err("name is required".into());
    }
    // Reject obvious bad triggers up front so the user gets a 400 instead
    // of a 500-on-create followed by a row that never fires.
    crate::automations::next_run_at::next_run_at(&req.trigger, &req.timezone, 0)
        .map_err(|e| format!("invalid trigger: {e}"))?;
    Ok(())
}

fn validate_update(req: &UpdateScheduleRequest) -> Result<(), String> {
    if req.name.trim().is_empty() {
        return Err("name is required".into());
    }
    crate::automations::next_run_at::next_run_at(&req.trigger, &req.timezone, 0)
        .map_err(|e| format!("invalid trigger: {e}"))?;
    Ok(())
}

// ── Schedules CRUD ───────────────────────────────────────────────────────────

pub async fn list_schedules(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Query(q):         Query<ListSchedulesQuery>,
) -> impl IntoResponse {
    // Admin override: ?user_id= picks whose rows to show. Non-admin users
    // always see own + system.
    let result = if is_admin(&user.role) {
        match q.user_id.as_deref() {
            Some(u) => store.list_schedules(Some(u)),
            None    => store.list_schedules(None),
        }
    } else {
        store.list_schedules_visible_to(&user.id, false)
    };
    match result {
        Ok(rows) => Json(SchedulesListResponse { schedules: rows }).into_response(),
        Err(e)   => err_response(e),
    }
}

pub async fn create_schedule(
    AuthUser(user):    AuthUser,
    Extension(store):  Extension<Arc<AutomationsStore>>,
    Extension(config): Extension<Arc<MiraConfig>>,
    Json(req):         Json<CreateScheduleRequest>,
) -> impl IntoResponse {
    if let Err(e) = validate_create(&req) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    let status_override = match gate_create_schedule(
        &store, &config.automations, &user.id,
        OwnerKind::User, req.rationale.as_deref(),
    ) {
        Ok(v)  => v,
        Err(e) => return gate_err_response(e),
    };
    let new = NewSchedule {
        user_id:     user.id.clone(),
        owner_kind:  OwnerKind::User,
        name:        req.name,
        description: req.description,
        rationale:   req.rationale,
        trigger:     req.trigger,
        timezone:    req.timezone,
        quiet_hours: req.quiet_hours,
        action:      req.action,
        expires_at:  req.expires_at,
        status:      status_override,
    };
    match store.create_schedule(new) {
        Ok(s)  => (StatusCode::CREATED, Json(s)).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn get_schedule(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    match store.get_schedule(&id) {
        Ok(Some(s)) if can_read(&user.id, &user.role, &s) => Json(s).into_response(),
        Ok(Some(_)) => (StatusCode::FORBIDDEN, "not your schedule").into_response(),
        Ok(None)    => (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => err_response(e),
    }
}

pub async fn update_schedule(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
    Json(req):        Json<UpdateScheduleRequest>,
) -> impl IntoResponse {
    if let Err(e) = validate_update(&req) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    // Authorize against the existing row before mutating.
    let existing = match store.get_schedule(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your schedule").into_response();
    }
    let upd = UpdateSchedule {
        name:        req.name,
        description: req.description,
        rationale:   req.rationale,
        trigger:     req.trigger,
        timezone:    req.timezone,
        quiet_hours: req.quiet_hours,
        action:      req.action,
        expires_at:  req.expires_at,
    };
    match store.update_schedule(&id, upd) {
        Ok(s)  => Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn delete_schedule(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_schedule(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if matches!(existing.owner_kind, OwnerKind::System) {
        // The seeder restores these on every boot; deletion would be a UX
        // lie. Use pause if you want to silence one.
        return (StatusCode::FORBIDDEN, "system schedules cannot be deleted; pause instead").into_response();
    }
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your schedule").into_response();
    }
    match store.delete_schedule(&id) {
        Ok(true)  => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)    => err_response(e),
    }
}

// ── Cron preview ────────────────────────────────────────────────────────────

pub async fn next_fires(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
    Query(q):         Query<NextFiresQuery>,
) -> impl IntoResponse {
    let n = q.n.unwrap_or(5).clamp(1, 50);
    let s = match store.get_schedule(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_read(&user.id, &user.role, &s) {
        return (StatusCode::FORBIDDEN, "not your schedule").into_response();
    }
    let after = chrono::Utc::now().timestamp();
    match next_n_runs(&s.trigger, &s.timezone, after, n) {
        Ok(v)  => Json(NextFiresResponse { next_fires: v }).into_response(),
        Err(e) => err_response(e),
    }
}

// ── Lifecycle: run-now / pause / resume / snooze ─────────────────────────────

pub async fn run_now(
    AuthUser(user):    AuthUser,
    Extension(store):  Extension<Arc<AutomationsStore>>,
    Extension(worker): Extension<Arc<Worker>>,
    Path(id):          Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_schedule(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your schedule").into_response();
    }
    if let Err(e) = worker.run_now(&id).await {
        warn!("run_now({id}) failed: {e}");
        return err_response(e);
    }
    // Return the post-run state so the UI can render the updated counters
    // without a follow-up GET.
    match store.get_schedule(&id) {
        Ok(Some(s)) => Json(s).into_response(),
        Ok(None)    => (StatusCode::OK, "ran").into_response(),
        Err(e)      => err_response(e),
    }
}

pub async fn pause_schedule(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_schedule(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => return err_response(e),
    };
    // System rows can be paused (the design wants users to be able to
    // silence the weekly_reflection nudge etc.). Pause requires write —
    // for system rows, that means admin only.
    if matches!(existing.owner_kind, OwnerKind::System) && !is_admin(&user.role) {
        return (StatusCode::FORBIDDEN, "only admin can pause system schedules").into_response();
    }
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your schedule").into_response();
    }
    match store.pause_schedule(&id) {
        Ok(s)  => Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn resume_schedule(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_schedule(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if matches!(existing.owner_kind, OwnerKind::System) && !is_admin(&user.role) {
        return (StatusCode::FORBIDDEN, "only admin can resume system schedules").into_response();
    }
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your schedule").into_response();
    }
    match store.resume_schedule(&id) {
        Ok(s)  => Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn approve_schedule(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_schedule(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your schedule").into_response();
    }
    if !matches!(existing.status, ScheduleStatus::PendingApproval) {
        return (StatusCode::CONFLICT, "schedule is not pending approval").into_response();
    }
    match store.approve_schedule(&id) {
        Ok(s)  => Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn reject_schedule(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_schedule(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your schedule").into_response();
    }
    if !matches!(existing.status, ScheduleStatus::PendingApproval) {
        return (StatusCode::CONFLICT, "schedule is not pending approval").into_response();
    }
    match store.delete_schedule(&id) {
        Ok(true)  => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)    => err_response(e),
    }
}

pub async fn snooze_schedule(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
    Json(req):        Json<SnoozeRequest>,
) -> impl IntoResponse {
    let existing = match store.get_schedule(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "schedule not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if matches!(existing.owner_kind, OwnerKind::System) && !is_admin(&user.role) {
        return (StatusCode::FORBIDDEN, "only admin can snooze system schedules").into_response();
    }
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your schedule").into_response();
    }
    if matches!(existing.status, ScheduleStatus::Expired | ScheduleStatus::Failed) {
        return (
            StatusCode::CONFLICT,
            "schedule is in a terminal state — cannot snooze",
        ).into_response();
    }
    match store.snooze_schedule(&id, req.until) {
        Ok(s)  => Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

// ── /api/automations unified list ────────────────────────────────────────────

pub async fn list_automations(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
) -> impl IntoResponse {
    let result = if is_admin(&user.role) {
        store.list_schedules(None)
    } else {
        store.list_schedules_visible_to(&user.id, false)
    };
    match result {
        Ok(rows) => {
            let items = rows.into_iter().map(AutomationItem::Schedule).collect();
            Json(AutomationsListResponse { items }).into_response()
        }
        Err(e) => err_response(e),
    }
}

pub async fn list_runs(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Query(q):         Query<RunsQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    // Non-admins see only their own runs, regardless of source filter.
    // Admins see all and can pivot by source/id freely.
    let user_filter = if is_admin(&user.role) { None } else { Some(user.id.as_str()) };
    let f = RunFilter {
        user_id:        user_filter,
        source_kind:    q.source.as_deref(),
        source_id:      q.id.as_deref(),
        outcome:        q.outcome.as_deref(),
        before_started: q.before,
        limit,
    };
    match store.list_runs_filtered(f) {
        Ok(runs) => Json(RunsListResponse { runs }).into_response(),
        Err(e)   => err_response(e),
    }
}
