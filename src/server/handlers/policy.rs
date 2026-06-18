// SPDX-License-Identifier: AGPL-3.0-or-later

//! Admin-defined policy rules HTTP surface (slice D3).
//!
//! All endpoints are admin-only — non-admin users get 403 via the
//! `AdminUser` extractor.
//!
//! - **GET /api/policy/rules** — list every rule, sorted by id.
//! - **POST /api/policy/rules** — create or replace a rule (idempotent
//!   upsert by id; matches how config-as-data tools work elsewhere
//!   in the codebase).
//! - **GET /api/policy/rules/{id}** — fetch one.
//! - **PUT /api/policy/rules/{id}** — update an existing rule. The
//!   path id wins over any id in the body; mismatches are 400.
//! - **DELETE /api/policy/rules/{id}** — remove. Idempotent: deleting
//!   an already-deleted rule returns `{"deleted": false}` rather than 404.
//!
//! No 404 on missing rule for delete because the UI fires deletes
//! optimistically — easier to make idempotent than to teach the UI
//! about 404 vs 200.

use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::middleware::AdminUser;
use crate::policy::{AdminRule, AdminRulesStore, Predicate};

/// What clients send when creating / updating a rule. The server fills
/// in `created_at_ms` (preserved across upserts) and `updated_at_ms`
/// (always set to now); the body's timestamps are ignored if present.
#[derive(Debug, Deserialize)]
pub struct AdminRuleInput {
    pub id:         String,
    pub name:       String,
    #[serde(default = "default_enabled")]
    pub enabled:    bool,
    pub event_kind: String,
    #[serde(default)]
    pub predicates: Vec<Predicate>,
    pub reason:     String,
}

fn default_enabled() -> bool { true }

#[derive(Debug, Serialize)]
pub struct AdminRuleResponse {
    pub rule: AdminRule,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub rules: Vec<AdminRule>,
}

pub async fn list_rules(
    AdminUser(_admin):  AdminUser,
    Extension(store):   Extension<Arc<AdminRulesStore>>,
) -> impl IntoResponse {
    match store.list() {
        Ok(rules) => (StatusCode::OK, Json(ListResponse { rules })).into_response(),
        Err(e)    => server_error(&e.to_string()),
    }
}

pub async fn get_rule(
    AdminUser(_admin):  AdminUser,
    Extension(store):   Extension<Arc<AdminRulesStore>>,
    Path(id):           Path<String>,
) -> impl IntoResponse {
    match store.get(&id) {
        Ok(Some(rule)) => (StatusCode::OK, Json(AdminRuleResponse { rule })).into_response(),
        Ok(None)       => (StatusCode::NOT_FOUND, Json(error("rule not found"))).into_response(),
        Err(e)         => server_error(&e.to_string()),
    }
}

pub async fn create_rule(
    AdminUser(_admin):  AdminUser,
    Extension(store):   Extension<Arc<AdminRulesStore>>,
    Json(body):         Json<AdminRuleInput>,
) -> impl IntoResponse {
    if let Err(msg) = validate_input(&body) {
        return (StatusCode::BAD_REQUEST, Json(error(&msg))).into_response();
    }
    let rule = AdminRule::new(body.id, body.name, body.event_kind, body.predicates, body.reason);
    let mut rule = rule;
    rule.enabled = body.enabled;
    if let Err(e) = store.upsert(&rule) {
        return server_error(&e.to_string());
    }
    // Re-read so the response has the canonical timestamps.
    match store.get(&rule.id) {
        Ok(Some(r)) => (StatusCode::CREATED, Json(AdminRuleResponse { rule: r })).into_response(),
        Ok(None)    => server_error("rule disappeared after upsert"),
        Err(e)      => server_error(&e.to_string()),
    }
}

pub async fn update_rule(
    AdminUser(_admin):  AdminUser,
    Extension(store):   Extension<Arc<AdminRulesStore>>,
    Path(path_id):      Path<String>,
    Json(body):         Json<AdminRuleInput>,
) -> impl IntoResponse {
    if !body.id.is_empty() && body.id != path_id {
        return (StatusCode::BAD_REQUEST,
            Json(error(&format!("path id {path_id:?} disagrees with body id {:?}", body.id))),
        ).into_response();
    }
    if let Err(msg) = validate_input(&body) {
        return (StatusCode::BAD_REQUEST, Json(error(&msg))).into_response();
    }
    // Existing-not-found is a 404 on PUT (vs the upsert behaviour
    // of POST). Easier to debug "I sent a PUT and it silently created
    // the wrong row" than the alternative.
    let existing = match store.get(&path_id) {
        Ok(Some(r)) => r,
        Ok(None)    => return (StatusCode::NOT_FOUND, Json(error("rule not found"))).into_response(),
        Err(e)      => return server_error(&e.to_string()),
    };
    let mut updated = AdminRule {
        id:            path_id.clone(),
        name:          body.name,
        enabled:       body.enabled,
        event_kind:    body.event_kind,
        predicates:    body.predicates,
        reason:        body.reason,
        created_at_ms: existing.created_at_ms, // preserve
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    if let Err(e) = store.upsert(&updated) {
        return server_error(&e.to_string());
    }
    // The upsert helper bumps updated_at to now anyway; re-read so
    // the response is the source of truth.
    match store.get(&path_id) {
        Ok(Some(r)) => { updated = r;
            (StatusCode::OK, Json(AdminRuleResponse { rule: updated })).into_response() }
        Ok(None)    => server_error("rule disappeared after upsert"),
        Err(e)      => server_error(&e.to_string()),
    }
}

#[derive(Debug, Serialize)]
pub struct DeleteResponse { pub deleted: bool }

pub async fn delete_rule(
    AdminUser(_admin):  AdminUser,
    Extension(store):   Extension<Arc<AdminRulesStore>>,
    Path(id):           Path<String>,
) -> impl IntoResponse {
    match store.delete(&id) {
        Ok(deleted) => (StatusCode::OK, Json(DeleteResponse { deleted })).into_response(),
        Err(e)      => server_error(&e.to_string()),
    }
}

fn validate_input(input: &AdminRuleInput) -> Result<(), String> {
    if input.id.trim().is_empty() {
        return Err("rule id must not be empty".into());
    }
    if input.name.trim().is_empty() {
        return Err("rule name must not be empty".into());
    }
    if input.reason.trim().is_empty() {
        return Err("rule reason must not be empty".into());
    }
    // Validate event_kind matches a known PolicyEvent::kind() string
    // so misspellings get caught at insert time, not silently at
    // evaluate time.
    let allowed = [
        "spawn_worker", "tool_invocation", "llm_call",
        "network_egress", "filesystem_access", "secret_read",
    ];
    if !allowed.contains(&input.event_kind.as_str()) {
        return Err(format!(
            "unknown event_kind {:?} (allowed: {})",
            input.event_kind, allowed.join(", "),
        ));
    }
    Ok(())
}

fn server_error(msg: &str) -> axum::response::Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(error(msg))).into_response()
}

fn error(msg: &str) -> serde_json::Value {
    serde_json::json!({"error": msg})
}
