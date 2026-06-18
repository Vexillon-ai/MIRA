// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/admin_audit.rs
//! Admin-only view of the `tool_audit` table.
//!
//! Every call that flows through `ToolRegistry::execute` writes one row; this
//! endpoint surfaces them in reverse-chronological order with filter + pagination.
//! See `design-docs/phase7-tier2-web-tools.md` §6 for the schema.

use std::sync::Arc;

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Extension};
use serde::{Deserialize, Serialize};

use crate::auth::AdminUser;
use crate::tools::audit::{AuditRow, ToolAuditStore};

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub limit:   Option<i64>,
    #[serde(default)]
    pub offset:  Option<i64>,
    #[serde(default)]
    pub actor:   Option<String>,
    #[serde(default)]
    pub tool:    Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuditListResponse {
    pub rows:  Vec<AuditRow>,
    pub total: i64,
}

/// GET /api/admin/tool_audit
pub async fn list_tool_audit(
    AdminUser(_):     AdminUser,
    Extension(store): Extension<Arc<ToolAuditStore>>,
    Query(q):         Query<AuditQuery>,
) -> impl IntoResponse {
    let limit  = q.limit.unwrap_or(100).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0).max(0);

    // Empty-string filters arrive from "all" radio buttons in the UI; treat
    // them as absent so the SQL doesn't match on an empty literal.
    let actor   = q.actor.as_deref().filter(|s| !s.is_empty());
    let tool    = q.tool.as_deref().filter(|s| !s.is_empty());
    let outcome = q.outcome.as_deref().filter(|s| !s.is_empty());

    let total = match store.count() {
        Ok(n)  => n,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = match store.list(limit, offset, actor, tool, outcome) {
        Ok(r)  => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    axum::Json(AuditListResponse { rows, total }).into_response()
}
