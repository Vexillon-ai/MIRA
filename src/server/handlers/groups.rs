// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/groups.rs
//! Group CRUD + membership endpoints.
//!
//! All mutations are admin-only (the memory-architecture plan scopes group
//! creation to admins so that members who don't understand the model can't
//! accidentally fracture shared context). Regular users can read:
//!   - the list of groups they belong to (`GET /api/me/groups`)
//! Admins additionally get:
//!   - `GET /api/groups`, `GET /api/groups/:id`
//!   - `POST /api/groups`, `PUT /api/groups/:id`, `DELETE /api/groups/:id`
//!   - `GET /api/groups/:id/members`
//!   - `POST /api/groups/:id/members`, `DELETE /api/groups/:id/members/:user_id`

use std::sync::Arc;

use axum::{
    extract::{Json, Path},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::{Deserialize, Serialize};

use crate::auth::{AdminUser, AuthUser, CapabilityProfile, Group, LocalAuthService, NewGroup, UpdateGroup, User};
use crate::MiraError;

// ── DTOs ──────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct GroupResponse {
    pub id:          String,
    pub name:        String,
    pub description: Option<String>,
    pub created_by:  String,
    pub created_at:  i64,
    pub updated_at:  i64,
}

impl From<Group> for GroupResponse {
    fn from(g: Group) -> Self {
        Self {
            id:          g.id,
            name:        g.name,
            description: g.description,
            created_by:  g.created_by,
            created_at:  g.created_at,
            updated_at:  g.updated_at,
        }
    }
}

#[derive(Serialize)]
pub struct MemberResponse {
    pub id:           String,
    pub username:     String,
    pub display_name: Option<String>,
    pub role:         String,
}

impl From<User> for MemberResponse {
    fn from(u: User) -> Self {
        Self {
            id:           u.id,
            username:     u.username,
            display_name: u.display_name,
            role:         u.role.as_str().to_owned(),
        }
    }
}

#[derive(Deserialize)]
pub struct AddMemberRequest {
    pub user_id: String,
}

// ── Error helper ──────────────────────────────────────────────────────────────

fn err_resp(e: MiraError) -> axum::response::Response {
    match e {
        MiraError::NotFound(m)  => (StatusCode::NOT_FOUND, m).into_response(),
        MiraError::Forbidden    => StatusCode::FORBIDDEN.into_response(),
        MiraError::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        // Duplicate-name / validation failures surface as AuthError → 400.
        MiraError::AuthError(m) => (StatusCode::BAD_REQUEST, m).into_response(),
        _                       => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── GET /api/groups ───────────────────────────────────────────────────────────

pub async fn list_groups(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
) -> impl IntoResponse {
    match auth.list_groups() {
        Ok(groups) => axum::Json(
            groups.into_iter().map(GroupResponse::from).collect::<Vec<_>>()
        ).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── POST /api/groups ──────────────────────────────────────────────────────────

pub async fn create_group(
    AdminUser(admin): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Json(req): Json<NewGroup>,
) -> impl IntoResponse {
    match auth.create_group(req, &admin.id) {
        Ok(g)  => (StatusCode::CREATED, axum::Json(GroupResponse::from(g))).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── GET /api/groups/:id ───────────────────────────────────────────────────────

pub async fn get_group(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match auth.get_group(&id) {
        Ok(Some(g)) => axum::Json(GroupResponse::from(g)).into_response(),
        Ok(None)    => StatusCode::NOT_FOUND.into_response(),
        Err(e)      => err_resp(e),
    }
}

// ── PUT /api/groups/:id ───────────────────────────────────────────────────────

pub async fn update_group(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateGroup>,
) -> impl IntoResponse {
    match auth.update_group(&id, req) {
        Ok(g)  => axum::Json(GroupResponse::from(g)).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── DELETE /api/groups/:id ────────────────────────────────────────────────────

pub async fn delete_group(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match auth.delete_group(&id) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_resp(e),
    }
}

// ── GET /api/groups/:id/members ───────────────────────────────────────────────

pub async fn list_members(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match auth.list_group_members(&id) {
        Ok(users) => axum::Json(
            users.into_iter().map(MemberResponse::from).collect::<Vec<_>>()
        ).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── POST /api/groups/:id/members ──────────────────────────────────────────────

pub async fn add_member(
    AdminUser(admin): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(group_id): Path<String>,
    Json(req): Json<AddMemberRequest>,
) -> impl IntoResponse {
    // Verify the group and user exist so we don't accept stale ids.
    if let Ok(None) = auth.get_group(&group_id) {
        return (StatusCode::NOT_FOUND, "Group not found").into_response();
    }
    if let Ok(None) = auth.get_user(&req.user_id) {
        return (StatusCode::NOT_FOUND, "User not found").into_response();
    }
    match auth.add_group_member(&group_id, &req.user_id, &admin.id) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_resp(e),
    }
}

// ── DELETE /api/groups/:id/members/:user_id ───────────────────────────────────

pub async fn remove_member(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path((group_id, user_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match auth.remove_group_member(&group_id, &user_id) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_resp(e),
    }
}

// ── GET /api/me/groups ────────────────────────────────────────────────────────

pub async fn list_my_groups(
    AuthUser(me): AuthUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
) -> impl IntoResponse {
    match auth.list_user_groups(&me.id) {
        Ok(groups) => axum::Json(
            groups.into_iter().map(GroupResponse::from).collect::<Vec<_>>()
        ).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── Capability RBAC endpoints ───────────────────────────────────────────────
//
// A PUT body is a CapabilityProfile; an all-unrestricted body (every axis
// null, no budget cap) clears the stored profile so "save empty = no
// restriction." Admins manage group + user profiles; any authenticated user
// can read their own *effective* (merged) profile so the UI can hide
// disallowed options.

// GET /api/groups/:id/capabilities
pub async fn get_group_capabilities(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match auth.get_group_capabilities(&id) {
        Ok(p)  => axum::Json(p.unwrap_or_default()).into_response(),
        Err(e) => err_resp(e),
    }
}

// PUT /api/groups/:id/capabilities
pub async fn set_group_capabilities(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
    Json(profile): Json<CapabilityProfile>,
) -> impl IntoResponse {
    let to_store = (!profile.is_unrestricted()).then_some(&profile);
    match auth.set_group_capabilities(&id, to_store) {
        Ok(())  => axum::Json(profile).into_response(),
        Err(e)  => err_resp(e),
    }
}

// GET /api/users/:id/capabilities
pub async fn get_user_capabilities(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match auth.get_user_capabilities(&id) {
        Ok(p)  => axum::Json(p.unwrap_or_default()).into_response(),
        Err(e) => err_resp(e),
    }
}

// PUT /api/users/:id/capabilities
pub async fn set_user_capabilities(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
    Json(profile): Json<CapabilityProfile>,
) -> impl IntoResponse {
    let to_store = (!profile.is_unrestricted()).then_some(&profile);
    match auth.set_user_capabilities(&id, to_store) {
        Ok(())  => axum::Json(profile).into_response(),
        Err(e)  => err_resp(e),
    }
}

// GET /api/me/capabilities — the caller's effective (merged) profile.
pub async fn get_my_capabilities(
    AuthUser(me): AuthUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
) -> impl IntoResponse {
    match auth.effective_capabilities(&me.id, &me.role) {
        Ok(p)  => axum::Json(p).into_response(),
        Err(e) => err_resp(e),
    }
}
