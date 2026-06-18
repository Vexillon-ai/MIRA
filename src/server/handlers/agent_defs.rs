// SPDX-License-Identifier: AGPL-3.0-or-later

//! Named-agent definition CRUD (Phase B). Admin-gated management of reusable
//! agent profiles (`/api/agents/definitions`). Invocation (spawning a worker
//! bound to a definition, by name, from chat or MIRA's own activity) is B2.

use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use crate::agent::definitions::{AgentDefinitionStore, NewAgentDefinition};
use crate::auth::middleware::AdminUser;

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn unavailable() -> Response {
    err(StatusCode::SERVICE_UNAVAILABLE, "named-agent store not wired in this build")
}

/// `GET /api/agents/definitions` — list all named agents.
pub async fn list_definitions(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<AgentDefinitionStore>>>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.list() {
        Ok(defs) => (StatusCode::OK, Json(defs)).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /api/agents/definitions` — create a named agent.
pub async fn create_definition(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<AgentDefinitionStore>>>,
    Json(new): Json<NewAgentDefinition>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.create(new) {
        Ok(def) => (StatusCode::CREATED, Json(def)).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// `GET /api/agents/definitions/{id}`
pub async fn get_definition(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<AgentDefinitionStore>>>,
    Path(id): Path<String>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.get(&id) {
        Ok(Some(def)) => (StatusCode::OK, Json(def)).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no agent definition with that id"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `PUT /api/agents/definitions/{id}` — update (also used for enable/disable).
pub async fn update_definition(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<AgentDefinitionStore>>>,
    Path(id): Path<String>,
    Json(new): Json<NewAgentDefinition>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.update(&id, new) {
        Ok(def) => (StatusCode::OK, Json(def)).into_response(),
        Err(crate::MiraError::NotFound(m)) => err(StatusCode::NOT_FOUND, m),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// `DELETE /api/agents/definitions/{id}`
pub async fn delete_definition(
    AdminUser(_admin): AdminUser,
    store: Option<Extension<Arc<AgentDefinitionStore>>>,
    Path(id): Path<String>,
) -> Response {
    let Some(Extension(store)) = store else { return unavailable() };
    match store.delete(&id) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "deleted": true }))).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
