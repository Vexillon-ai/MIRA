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
use crate::auth::AuthUser;
use crate::tools::ToolResult;

#[derive(Debug, Serialize)]
pub struct ToolInfo {
    pub name:        String,
    pub description: String,
}

// GET /api/tools — returns every tool registered on the live AgentCore.
pub async fn list_tools(
    AuthUser(_user):   AuthUser,
    Extension(agent):  Extension<Arc<AgentCore>>,
) -> Json<Vec<ToolInfo>> {
    let names = agent.tools.list_visible_tools();
    let mut infos: Vec<ToolInfo> = names
        .into_iter()
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
    AuthUser(_user):  AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Json(req):        Json<RunToolRequest>,
) -> impl IntoResponse {
    if agent.tools.get(&req.name).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Unknown tool: {}", req.name) })),
        ).into_response();
    }
    let args = req.args.unwrap_or_else(|| serde_json::json!({}));
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
