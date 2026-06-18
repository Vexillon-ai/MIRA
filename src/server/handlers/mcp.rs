// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/mcp.rs
//! Per-user MCP-server management:
//!
//! * `GET    /api/mcp/servers`            — list the caller's rows
//! * `POST   /api/mcp/servers`            — create one owned by the caller
//! * `PUT    /api/mcp/servers/{id}`       — update one the caller owns
//! * `DELETE /api/mcp/servers/{id}`       — delete one the caller owns
//! * `GET    /api/mcp/status`             — runtime connect status (caller-scoped)
//!
//! Changes to server entries require a MIRA restart to take effect —
//! doesn't hot-reload the registry. The CRUD endpoints persist
//! the new state and the user picks them up on next start.

use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};

use crate::auth::{AdminUser, AuthUser};
use crate::mcp::{
    McpCatalogEntry, McpCatalogStore, McpServerRegistry, McpServerRow, McpServerStatus,
    McpServerStore, NewMcpServer, UpdateMcpServer, UpsertCatalogEntry,
};

// `GET /api/mcp/status` — runtime connect snapshot, scoped to the
// caller's rows. Admins still see only their own here; for a global
// view we'd add a separate `/api/admin/mcp/status` (defer to Slice
// 5 if anyone asks for it).
pub async fn status(
    AuthUser(user):      AuthUser,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
) -> Json<Vec<McpServerStatus>> {
    Json(registry.status_for_user(&user.id))
}

// `GET /api/mcp/servers` — list rows owned by the caller.
pub async fn list_servers(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<McpServerStore>>,
) -> Result<Json<Vec<McpServerRow>>, StatusCode> {
    let rows = store.list_for_user(&user.id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows))
}

// `POST /api/mcp/servers` — create a row owned by the caller. The
// request body is the [`NewMcpServer`] shape; user_id is taken from
// the auth context, never accepted from the request.
pub async fn create_server(
    AuthUser(user):      AuthUser,
    Extension(store):    Extension<Arc<McpServerStore>>,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
    Json(new):           Json<NewMcpServer>,
) -> Result<(StatusCode, Json<McpServerRow>), (StatusCode, String)> {
    if new.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name required".into()));
    }
    let row = store.create(&user.id, new)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    // Hot-reload: connect the new server + publish its tools now, no restart.
    registry.reload().await;
    Ok((StatusCode::CREATED, Json(row)))
}

// `PUT /api/mcp/servers/{id}` — update a row the caller owns.
// Returns 404 when the row doesn't exist OR belongs to someone else
// (we don't leak existence to non-owners).
pub async fn update_server(
    AuthUser(user):      AuthUser,
    Extension(store):    Extension<Arc<McpServerStore>>,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
    Path(id):            Path<String>,
    Json(upd):           Json<UpdateMcpServer>,
) -> Result<Json<McpServerRow>, (StatusCode, String)> {
    let existing = store.get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "not found".into()))?;
    if existing.user_id != user.id {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    let row = store.update(&id, upd)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    registry.reload().await;
    Ok(Json(row))
}

// `DELETE /api/mcp/servers/{id}` — owner-gated delete.
pub async fn delete_server(
    AuthUser(user):      AuthUser,
    Extension(store):    Extension<Arc<McpServerStore>>,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
    Path(id):            Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let existing = store.get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "not found".into()))?;
    if existing.user_id != user.id {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    store.delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Hot-reload: drop the removed server's tools immediately.
    registry.reload().await;
    Ok(StatusCode::NO_CONTENT)
}

// ─────────────────────────────────────────────────────────────────────────────
// Recommended-server catalog (admin-managed)
// ─────────────────────────────────────────────────────────────────────────────

// `GET /api/mcp/catalog` — the enabled catalog entries any user can pick
// from to pre-fill the add-server form. Read-only for non-admins.
pub async fn catalog_list(
    AuthUser(_):        AuthUser,
    Extension(catalog): Extension<Arc<McpCatalogStore>>,
) -> Result<Json<Vec<McpCatalogEntry>>, StatusCode> {
    catalog.list_enabled()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

// `GET /api/admin/mcp/catalog` — every entry (enabled or not) for admin
// management.
pub async fn catalog_admin_list(
    AdminUser(_):       AdminUser,
    Extension(catalog): Extension<Arc<McpCatalogStore>>,
) -> Result<Json<Vec<McpCatalogEntry>>, StatusCode> {
    catalog.list_all()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

// `POST /api/admin/mcp/catalog` — add a catalog entry (admin only).
pub async fn catalog_create(
    AdminUser(_):       AdminUser,
    Extension(catalog): Extension<Arc<McpCatalogStore>>,
    Json(body):         Json<UpsertCatalogEntry>,
) -> Result<(StatusCode, Json<McpCatalogEntry>), (StatusCode, String)> {
    if body.name.trim().is_empty() || body.title.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name and title are required".into()));
    }
    let e = catalog.create(body).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok((StatusCode::CREATED, Json(e)))
}

// `PUT /api/admin/mcp/catalog/{id}` — edit a catalog entry (admin only).
pub async fn catalog_update(
    AdminUser(_):       AdminUser,
    Extension(catalog): Extension<Arc<McpCatalogStore>>,
    Path(id):           Path<String>,
    Json(body):         Json<UpsertCatalogEntry>,
) -> Result<Json<McpCatalogEntry>, (StatusCode, String)> {
    let e = catalog.update(&id, body).map_err(|e| match e {
        crate::MiraError::NotFound(_) => (StatusCode::NOT_FOUND, "not found".into()),
        other                         => (StatusCode::BAD_REQUEST, other.to_string()),
    })?;
    Ok(Json(e))
}

// `DELETE /api/admin/mcp/catalog/{id}` — remove a catalog entry (admin only).
pub async fn catalog_delete(
    AdminUser(_):       AdminUser,
    Extension(catalog): Extension<Arc<McpCatalogStore>>,
    Path(id):           Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    catalog.delete(&id).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// Suppress unused-import warnings when one of the handler aliases
// disappears from a future build — keeps `cargo check` quiet.
#[allow(dead_code)]
fn _impl_response_marker() -> impl IntoResponse { StatusCode::OK }
