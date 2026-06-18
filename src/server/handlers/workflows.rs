// SPDX-License-Identifier: AGPL-3.0-or-later

//! Workflow CRUD + run observability (Phase C). Admin-gated management of
//! saved orchestrations (`/api/workflows`) and read access to run history
//! (`/api/workflows/runs`). Execution is driven by the `run_workflow` tool /
//! the [`Orchestrator`](crate::agent::Orchestrator), not these handlers.

use std::sync::Arc;

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;

use crate::agent::{NewWorkflowDefinition, Orchestrator, WorkflowStore};
use crate::auth::middleware::AdminUser;

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn unavailable() -> Response {
    err(StatusCode::SERVICE_UNAVAILABLE, "workflow store not wired in this build")
}

/// `GET /api/workflows`
pub async fn list_workflows(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<WorkflowStore>>>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.list() {
        Ok(defs) => (StatusCode::OK, Json(defs)).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /api/workflows`
pub async fn create_workflow(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<WorkflowStore>>>,
    Json(new): Json<NewWorkflowDefinition>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.create(new) {
        Ok(def) => (StatusCode::CREATED, Json(def)).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// `GET /api/workflows/{id}`
pub async fn get_workflow(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<WorkflowStore>>>,
    Path(id): Path<String>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.get(&id) {
        Ok(Some(def)) => (StatusCode::OK, Json(def)).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no workflow with that id"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `PUT /api/workflows/{id}`
pub async fn update_workflow(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<WorkflowStore>>>,
    Path(id): Path<String>,
    Json(new): Json<NewWorkflowDefinition>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.update(&id, new) {
        Ok(def) => (StatusCode::OK, Json(def)).into_response(),
        Err(crate::MiraError::NotFound(m)) => err(StatusCode::NOT_FOUND, m),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// `DELETE /api/workflows/{id}`
pub async fn delete_workflow(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<WorkflowStore>>>,
    Path(id): Path<String>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.delete(&id) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "deleted": true }))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct RunWorkflowBody {
    #[serde(default)]
    pub input: String,
}

/// `POST /api/workflows/{id}/run` — start a run from the UI. Returns the
/// `run_id`; the caller polls `/api/workflows/runs/{run_id}` for progress.
pub async fn run_workflow(
    AdminUser(admin): AdminUser,
    store: Option<Extension<Arc<WorkflowStore>>>,
    orchestrator: Option<Extension<Arc<Orchestrator>>>,
    Path(id): Path<String>,
    Json(body): Json<RunWorkflowBody>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    let Some(Extension(orch)) = orchestrator else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "orchestrator not wired in this build");
    };
    let def = match store.get(&id) {
        Ok(Some(d)) if d.enabled => d,
        Ok(Some(_)) => return err(StatusCode::CONFLICT, "workflow is disabled"),
        Ok(None) => return err(StatusCode::NOT_FOUND, "no workflow with that id"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let run_id = orch.start(def, body.input, Some(admin.id.clone()));
    (StatusCode::ACCEPTED, Json(serde_json::json!({ "run_id": run_id }))).into_response()
}

#[derive(Deserialize)]
pub struct RunsQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/workflows/runs?limit=N` — most-recent runs first.
pub async fn list_runs(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<WorkflowStore>>>,
    Query(q): Query<RunsQuery>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    match store.list_runs(limit) {
        Ok(runs) => (StatusCode::OK, Json(runs)).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct ApproveBody {
    pub step_id: String,
    /// `"approve"` (default) runs the checkpoint step; `"reject"` skips it.
    #[serde(default)]
    pub decision: Option<String>,
}

/// `POST /api/workflows/runs/{id}/approve` — approve or reject a paused
/// checkpoint step, resuming the run.
pub async fn approve_run(
    AdminUser(_admin): AdminUser,
    orchestrator: Option<Extension<Arc<Orchestrator>>>,
    Path(id): Path<String>,
    Json(body): Json<ApproveBody>,
) -> Response {
    let Some(Extension(orch)) = orchestrator else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "orchestrator not wired in this build");
    };
    let approve = !matches!(body.decision.as_deref(), Some("reject"));
    match orch.act_on_checkpoint(&id, &body.step_id, approve) {
        Ok(run) => (StatusCode::OK, Json(run)).into_response(),
        Err(crate::MiraError::NotFound(m)) => err(StatusCode::NOT_FOUND, m),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// `GET /api/workflows/runs/{id}` — full run state incl. per-step status/output.
pub async fn get_run(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<WorkflowStore>>>,
    Path(id): Path<String>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.get_run(&id) {
        Ok(Some(run)) => (StatusCode::OK, Json(run)).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no run with that id"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
