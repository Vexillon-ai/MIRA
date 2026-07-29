// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/tools.rs
//! Tool registry endpoints:
//! * `GET  /api/tools`     — list registered tools with name + description.
//! * `POST /api/tools/run` — execute a tool by name with JSON args.
//!
//! Primarily consumed by the TUI's `ServerBackend` so the TUI's
//! `/tool-list` and `/tool-run` commands work against a remote server the
//! same way they do in-process, without holding an `Arc<AgentCore>` itself.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use crate::agent::AgentCore;
use crate::auth::{AuthUser, LocalAuthService};
use crate::tools::ToolResult;

#[derive(Debug, Serialize)]
pub struct ToolInfo {
    pub name:        String,
    pub description: String,
}

// GET /api/tools — returns every tool registered on the live AgentCore,
// narrowed to what the caller's RBAC capability profile permits (so a
// restricted user doesn't see — or learn the existence of — tools they can't
// run). Admins resolve to unrestricted.
pub async fn list_tools(
    AuthUser(user):    AuthUser,
    Extension(agent):  Extension<Arc<AgentCore>>,
    Extension(auth):   Extension<Arc<LocalAuthService>>,
) -> Json<Vec<ToolInfo>> {
    let caps = auth
        .effective_capabilities(&user.id, &user.role)
        .unwrap_or_default();
    let names = agent.tools.list_visible_tools();
    let mut infos: Vec<ToolInfo> = names
        .into_iter()
        .filter(|n| caps.allows_tool(n))
        .filter_map(|n| {
            agent.tools.get(&n).map(|t| ToolInfo {
                name:        t.name().to_owned(),
                description: t.description().to_owned(),
            })
        })
        .collect();
    infos.sort_by(|a, b| a.name.cmp(&b.name));
    Json(infos)
}

#[derive(Debug, Deserialize)]
pub struct RunToolRequest {
    pub name: String,
    // Argument object. `null`/missing is normalised to `{}` so tools that
    // accept zero arguments can be invoked as `{"name": "foo"}`.
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

// POST /api/tools/run — invoke a registered tool. Returns a `ToolResult`
// (`{ success, output, error }`) on HTTP 200 regardless of whether the tool
// itself succeeded — callers inspect `success` to distinguish. A 404 is
// returned when the tool name is unknown, matching the UX for other
// "not-found" resources in the API.
pub async fn run_tool(
    AuthUser(user):   AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Json(req):        Json<RunToolRequest>,
) -> impl IntoResponse {
    if agent.tools.get(&req.name).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Unknown tool: {}", req.name) })),
        ).into_response();
    }

    // enforce the caller's RBAC capability allowlist. This endpoint runs
    // tools directly on the global registry, so without this a non-admin could
    // invoke ANY tool their profile forbids — including `code_run` (arbitrary
    // code) and service-write tools — bypassing their per-user/group caps.
    // Admins resolve to unrestricted; a lookup failure falls back to
    // unrestricted (matches the chat path — a transient DB error must not lock
    // a user out), which is safe because the profile can only ever *restrict*.
    let caps = auth
        .effective_capabilities(&user.id, &user.role)
        .unwrap_or_default();
    if !caps.allows_tool(&req.name) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": format!("Your account is not permitted to use the tool '{}'.", req.name)
            })),
        ).into_response();
    }

    // Normalise args to an object and stamp the caller's id so the tool audit
    // records the real actor (not "unknown") and user-scoped tools resolve to
    // this caller. The registry parses + strips `_user_id` before the tool runs.
    let mut args = req.args.unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = args.as_object_mut() {
        obj.insert("_user_id".to_string(), serde_json::Value::String(user.id.clone()));
    }

    match agent.tools.execute(&req.name, args).await {
        Ok(r)  => (StatusCode::OK, Json(r)).into_response(),
        Err(e) => {
            // Registry-level failure (not a tool-reported failure). Return
            // 500 with a ToolResult shape so clients can parse consistently.
            let body = ToolResult::failure(format!("{}", e));
            (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
        }
    }
}
