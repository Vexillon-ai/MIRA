// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/triggers.rs
//! Event subscription HTTP surface.
//!
//! "Triggers" in the UI = `event_subscriptions` rows: a user-configured rule
//! that fires an [`Action`] whenever a named internal event matches its
//! predicate. CRUD here is owner-scoped just like webhooks; the actual
//! firing happens in the [`crate::events::subscriber`] task.

use std::sync::Arc;

use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{AuthUser, models::Role};
use crate::automations::{
    Action, AutomationsStore, AutomationStatus, EventSubscription, NewEventSubscription,
    OwnerKind, UpdateEventSubscription,
    agent_gate::{gate_create_event_subscription, GateError},
    predicate,
};
use crate::config::MiraConfig;
use crate::events::{self, EventBus};
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

fn err_response(e: MiraError) -> axum::response::Response {
    let msg = e.to_string();
    if msg.contains("not found") {
        return (StatusCode::NOT_FOUND, msg).into_response();
    }
    (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
}

fn is_admin(role: &Role) -> bool { matches!(role, Role::Admin) }

fn can_read(user_id: &str, role: &Role, s: &EventSubscription) -> bool {
    is_admin(role) || s.user_id == user_id || matches!(s.owner_kind, OwnerKind::System)
}

fn can_write(user_id: &str, role: &Role, s: &EventSubscription) -> bool {
    is_admin(role) || s.user_id == user_id
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub user_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub subscriptions: Vec<EventSubscription>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub name:        String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub rationale:   Option<String>,
    pub event_name:  String,
    #[serde(default)]
    pub predicate:   Option<Value>,
    pub action:      Action,
    #[serde(default)]
    pub expires_at:  Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRequest {
    pub name:        String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub rationale:   Option<String>,
    pub event_name:  String,
    #[serde(default)]
    pub predicate:   Option<Value>,
    pub action:      Action,
    #[serde(default)]
    pub expires_at:  Option<i64>,
}

fn validate(name: &str, event_name: &str, pred: &Option<Value>) -> Result<(), String> {
    if name.trim().is_empty()       { return Err("name is required".into()); }
    if event_name.trim().is_empty() { return Err("event_name is required".into()); }
    if let Some(p) = pred {
        let probe = json!({"event": {"name": event_name}, "payload": {}, "user": {}, "now": 0});
        predicate::eval(p, &probe).map(|_| ()).map_err(|e| format!("invalid predicate: {e}"))?;
    }
    Ok(())
}

pub async fn list_subs(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Query(q):         Query<ListQuery>,
) -> impl IntoResponse {
    let result = if is_admin(&user.role) {
        store.list_event_subscriptions(q.user_id.as_deref())
    } else {
        store.list_event_subscriptions(Some(user.id.as_str()))
    };
    match result {
        Ok(rows) => Json(ListResponse { subscriptions: rows }).into_response(),
        Err(e)   => err_response(e),
    }
}

pub async fn create_sub(
    AuthUser(user):    AuthUser,
    Extension(store):  Extension<Arc<AutomationsStore>>,
    Extension(config): Extension<Arc<MiraConfig>>,
    Json(req):         Json<CreateRequest>,
) -> impl IntoResponse {
    if let Err(e) = validate(&req.name, &req.event_name, &req.predicate) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    let status_override = match gate_create_event_subscription(
        &store, &config.automations, &user.id,
        OwnerKind::User, req.rationale.as_deref(),
    ) {
        Ok(v)  => v,
        Err(e) => return gate_err_response(e),
    };
    let new = NewEventSubscription {
        user_id:     user.id.clone(),
        owner_kind:  OwnerKind::User,
        name:        req.name,
        description: req.description,
        rationale:   req.rationale,
        event_name:  req.event_name,
        predicate:   req.predicate,
        action:      req.action,
        expires_at:  req.expires_at,
        status:      status_override,
        // User-created triggers persist by default — they're
        // long-lived rules, not transient one-shots. Future API
        // surface can expose a query param to override this.
        delete_after_fire: false,
    };
    match store.create_event_subscription(new) {
        Ok(s)  => (StatusCode::CREATED, Json(s)).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn get_sub(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    match store.get_event_subscription(&id) {
        Ok(Some(s)) if can_read(&user.id, &user.role, &s) => Json(s).into_response(),
        Ok(Some(_)) => (StatusCode::FORBIDDEN, "not your subscription").into_response(),
        Ok(None)    => (StatusCode::NOT_FOUND, "subscription not found").into_response(),
        Err(e)      => err_response(e),
    }
}

pub async fn update_sub(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
    Json(req):        Json<UpdateRequest>,
) -> impl IntoResponse {
    if let Err(e) = validate(&req.name, &req.event_name, &req.predicate) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    let existing = match store.get_event_subscription(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "subscription not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your subscription").into_response();
    }
    let upd = UpdateEventSubscription {
        name:        req.name,
        description: req.description,
        rationale:   req.rationale,
        event_name:  req.event_name,
        predicate:   req.predicate,
        action:      req.action,
        expires_at:  req.expires_at,
    };
    match store.update_event_subscription(&id, upd) {
        Ok(s)  => Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn delete_sub(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_event_subscription(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "subscription not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your subscription").into_response();
    }
    match store.delete_event_subscription(&id) {
        Ok(true)  => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "subscription not found").into_response(),
        Err(e)    => err_response(e),
    }
}

pub async fn pause_sub(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_event_subscription(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "subscription not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your subscription").into_response();
    }
    match store.pause_event_subscription(&id) {
        Ok(s)  => Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn resume_sub(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_event_subscription(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "subscription not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your subscription").into_response();
    }
    match store.resume_event_subscription(&id) {
        Ok(s)  => Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn approve_sub(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_event_subscription(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "subscription not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your subscription").into_response();
    }
    if !matches!(existing.status, AutomationStatus::PendingApproval) {
        return (StatusCode::CONFLICT, "subscription is not pending approval").into_response();
    }
    match store.approve_event_subscription(&id) {
        Ok(s)  => Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn reject_sub(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_event_subscription(&id) {
        Ok(Some(s)) => s,
        Ok(None)    => return (StatusCode::NOT_FOUND, "subscription not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your subscription").into_response();
    }
    if !matches!(existing.status, AutomationStatus::PendingApproval) {
        return (StatusCode::CONFLICT, "subscription is not pending approval").into_response();
    }
    match store.delete_event_subscription(&id) {
        Ok(true)  => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "subscription not found").into_response(),
        Err(e)    => err_response(e),
    }
}

#[derive(Debug, Serialize)]
pub struct EventNamesResponse {
    pub names: Vec<&'static str>,
}

// `GET /api/events/names` — the catalog of event names the running build
// emits, for the trigger-creation form's dropdown. Static for now since the
// emitter set is fixed in code; future plugins can extend by appending here.
pub async fn list_event_names() -> impl IntoResponse {
    Json(EventNamesResponse {
        names: vec![
            events::names::MESSAGE_RECEIVED,
            events::names::TOOL_FAILED,
            events::names::CONVERSATION_IDLE,
            events::names::MEMORY_THRESHOLD,
            events::names::ONBOARDING_STALE,
        ],
    })
}

#[derive(Debug, Deserialize)]
pub struct TestEmitRequest {
    pub event_name: String,
    #[serde(default)]
    pub payload:    Value,
}

#[derive(Debug, Serialize)]
pub struct TestEmitResponse {
    pub emitted: bool,
}

// `POST /api/events/test` — admin-only escape hatch to fire a synthetic
// event onto the bus. Useful for verifying a subscription end-to-end
// without standing up the producing subsystem.
pub async fn test_emit(
    AuthUser(user): AuthUser,
    Extension(bus): Extension<Arc<EventBus>>,
    Json(req):      Json<TestEmitRequest>,
) -> impl IntoResponse {
    if !is_admin(&user.role) {
        return (StatusCode::FORBIDDEN, "admin only").into_response();
    }
    if req.event_name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "event_name required").into_response();
    }
    bus.emit_named(&req.event_name, Some(user.id.clone()), req.payload);
    Json(TestEmitResponse { emitted: true }).into_response()
}
