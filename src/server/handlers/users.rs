// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/users.rs
//! Admin user management endpoints.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Json, Multipart, Path},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::{Deserialize, Serialize};

use crate::auth::{AdminUser, AuthUser, LocalAuthService, NewUser, Role, User};
use crate::voice::{normalise as normalise_voice_prefs, parse_user_prefs, to_storage_json, VoicePrefsMap};
use crate::MiraError;

// ── Avatar storage ────────────────────────────────────────────────────────────

/// Filesystem root for uploaded avatars, injected as an Extension. Held as
/// `Arc<PathBuf>` so handlers can pull the path without cloning the layer.
#[derive(Debug, Clone)]
pub struct AvatarDir(pub Arc<PathBuf>);

/// Filename extensions we accept and serve. Keep in sync with the MIME check
/// in `upload_avatar` below.
pub const AVATAR_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];

pub const AVATAR_MAX_BYTES: usize = 2 * 1024 * 1024; // 2 MiB

/// Remove any existing avatar file(s) for the user. Called before a new
/// upload and on delete; also on user deletion so orphaned files don't
/// pile up under data_dir.
pub fn clear_user_avatar_files(dir: &std::path::Path, user_id: &str) {
    for ext in AVATAR_EXTS {
        let p = dir.join(format!("{}.{}", user_id, ext));
        let _ = std::fs::remove_file(p);
    }
}

// ── DTOs ──────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct UserResponse {
    pub id:                String,
    pub username:          String,
    pub display_name:      Option<String>,
    pub email:             Option<String>,
    pub role:              String,
    pub is_active:         bool,
    pub created_at:        i64,
    pub updated_at:        i64,
    pub last_login:        Option<i64>,
    pub phone:             Option<String>,
    pub preferred_contact: Option<String>,
    pub avatar:            Option<String>,
    /// Per-channel voice preferences keyed by channel id (`web`, `tui`,
    /// `telegram`, `signal`, plus any plugin-registered channels). Each
    /// entry can override the response policy, voice id, or both.
    pub voice_prefs:       VoicePrefsMap,
}

impl From<User> for UserResponse {
    fn from(u: User) -> Self {
        Self {
            id:                u.id,
            username:          u.username,
            display_name:      u.display_name,
            email:             u.email,
            role:              u.role.as_str().to_owned(),
            is_active:         u.is_active,
            created_at:        u.created_at,
            updated_at:        u.updated_at,
            last_login:        u.last_login,
            phone:             u.phone,
            preferred_contact: u.preferred_contact,
            avatar:            u.avatar,
            voice_prefs:       parse_user_prefs(u.voice_prefs.as_deref()),
        }
    }
}

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub username:     String,
    pub display_name: Option<String>,
    pub email:        Option<String>,
    pub password:     String,
    pub role:         Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub display_name:      Option<String>,
    pub email:             Option<String>,
    pub role:              Option<String>,
    pub is_active:         Option<bool>,
    pub phone:             Option<String>,
    pub preferred_contact: Option<String>,
    /// Either `"preset:<key>"` or `null` to clear. Upload is a separate
    /// multipart endpoint — PUT only covers preset-select and clear.
    pub avatar:            Option<String>,
    /// Replace the user's per-channel voice preferences. `None` preserves the
    /// existing map; `Some` replaces it wholesale (so a client that wants to
    /// edit one channel must send the full map back). Empty entries (no
    /// `response_policy` or `voice_id` set) are dropped during normalisation.
    pub voice_prefs:       Option<VoicePrefsMap>,
}

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub new_password: String,
}

// ── Error helper ──────────────────────────────────────────────────────────────

fn err_resp(e: MiraError) -> axum::response::Response {
    match e {
        MiraError::NotFound(m)  => (StatusCode::NOT_FOUND, m).into_response(),
        MiraError::Forbidden    => StatusCode::FORBIDDEN.into_response(),
        MiraError::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        // Duplicate-username and similar validation failures surface as
        // `AuthError` — map them to 400 so the UI can show the message.
        MiraError::AuthError(m) => (StatusCode::BAD_REQUEST, m).into_response(),
        _                       => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── GET /api/users ────────────────────────────────────────────────────────────

pub async fn list_users(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
) -> impl IntoResponse {
    match auth.list_users() {
        Ok(users) => axum::Json(
            users.into_iter().map(UserResponse::from).collect::<Vec<_>>()
        ).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── POST /api/users ───────────────────────────────────────────────────────────

pub async fn create_user(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Json(req): Json<CreateUserRequest>,
) -> impl IntoResponse {
    let role = match req.role.as_deref().unwrap_or("user") {
        "admin" => Role::Admin,
        _       => Role::User,
    };

    let new = NewUser {
        username:     req.username,
        display_name: req.display_name,
        email:        req.email,
        password:     req.password,
        role,
    };

    match auth.create_user(new) {
        Ok(user) => (StatusCode::CREATED, axum::Json(UserResponse::from(user))).into_response(),
        Err(e)   => err_resp(e),
    }
}

// ── GET /api/users/:id ────────────────────────────────────────────────────────

pub async fn get_user(
    AuthUser(caller): AuthUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Users can only view their own profile unless admin.
    if caller.role != Role::Admin && caller.id != id {
        return StatusCode::FORBIDDEN.into_response();
    }

    match auth.get_user(&id) {
        Ok(Some(u)) => axum::Json(UserResponse::from(u)).into_response(),
        Ok(None)    => StatusCode::NOT_FOUND.into_response(),
        Err(e)      => err_resp(e),
    }
}

// ── PUT /api/users/:id ────────────────────────────────────────────────────────

pub async fn update_user(
    AuthUser(caller): AuthUser,
    Extension(_auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Simplified: admin can update any user; regular users cannot change roles.
    // Full implementation would accept a Json body.
    if caller.role != Role::Admin && caller.id != id {
        return StatusCode::FORBIDDEN.into_response();
    }
    // TODO: accept UpdateUserRequest body and apply changes.
    StatusCode::NOT_IMPLEMENTED.into_response()
}

// A separate version that takes the body:
pub async fn update_user_full(
    AuthUser(caller): AuthUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateUserRequest>,
) -> impl IntoResponse {
    if caller.role != Role::Admin && caller.id != id {
        return StatusCode::FORBIDDEN.into_response();
    }

    let existing = match auth.get_user(&id) {
        Ok(Some(u)) => u,
        Ok(None)    => return StatusCode::NOT_FOUND.into_response(),
        Err(e)      => return err_resp(e),
    };

    // Non-admins cannot change their own role.
    let new_role = if caller.role == Role::Admin {
        match req.role.as_deref().unwrap_or(existing.role.as_str()) {
            "admin" => Role::Admin,
            _       => Role::User,
        }
    } else {
        existing.role.clone()
    };

    let is_active = req.is_active.unwrap_or(existing.is_active);
    // Non-admins cannot deactivate themselves.
    let is_active = if caller.role != Role::Admin { existing.is_active } else { is_active };

    // Voice prefs: `None` preserves what's stored. `Some(map)` replaces the
    // entire map — clients edit by sending the merged result back. We
    // round-trip via `to_storage_json` so empty entries are dropped and the
    // disk shape stays canonical.
    let new_voice_prefs = match req.voice_prefs {
        Some(map) => to_storage_json(&normalise_voice_prefs(map)),
        None      => existing.voice_prefs,
    };

    match auth.update_user(
        &id,
        req.display_name.or(existing.display_name),
        req.email.or(existing.email),
        new_role,
        is_active,
        req.phone.or(existing.phone),
        req.preferred_contact.or(existing.preferred_contact),
        req.avatar.or(existing.avatar),
        new_voice_prefs,
    ) {
        Ok(u)  => axum::Json(UserResponse::from(u)).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── DELETE /api/users/:id ─────────────────────────────────────────────────────

pub async fn delete_user(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Extension(avatar_dir): Extension<AvatarDir>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match auth.delete_user(&id) {
        Ok(())  => {
            clear_user_avatar_files(avatar_dir.0.as_path(), &id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e)  => err_resp(e),
    }
}

// ── POST /api/users/:id/avatar (multipart) ────────────────────────────────────

pub async fn upload_avatar(
    AuthUser(caller): AuthUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Extension(avatar_dir): Extension<AvatarDir>,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if caller.role != Role::Admin && caller.id != id {
        return StatusCode::FORBIDDEN.into_response();
    }

    // Pull the first file field. Client sends `file`; we accept any single
    // field with bytes so curl-style uploads also work.
    let (bytes, content_type) = loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None)    => return (StatusCode::BAD_REQUEST, "no file field").into_response(),
            Err(e)      => return (StatusCode::BAD_REQUEST, format!("multipart error: {}", e)).into_response(),
        };

        let ct = field.content_type().map(|s| s.to_owned()).unwrap_or_default();
        let bytes = match field.bytes().await {
            Ok(b)  => b,
            Err(e) => return (StatusCode::BAD_REQUEST, format!("read error: {}", e)).into_response(),
        };
        break (bytes, ct);
    };

    if bytes.len() > AVATAR_MAX_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "max 2 MiB").into_response();
    }

    let ext = match content_type.as_str() {
        "image/png"  => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif"  => "gif",
        _ => return (StatusCode::UNSUPPORTED_MEDIA_TYPE,
                     "use png, jpeg, webp, or gif").into_response(),
    };

    // Remove any prior avatar files (user may be replacing a .png with a .jpg)
    // before writing so serves always resolve to the new extension.
    if let Err(e) = std::fs::create_dir_all(avatar_dir.0.as_path()) {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                format!("avatar dir: {}", e)).into_response();
    }
    clear_user_avatar_files(avatar_dir.0.as_path(), &id);

    let path = avatar_dir.0.join(format!("{}.{}", id, ext));
    if let Err(e) = std::fs::write(&path, &bytes) {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                format!("write: {}", e)).into_response();
    }

    let avatar_value = format!("upload:{}", ext);
    match auth.set_avatar(&id, Some(&avatar_value)) {
        Ok(u)  => axum::Json(UserResponse::from(u)).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── DELETE /api/users/:id/avatar ──────────────────────────────────────────────

pub async fn delete_avatar(
    AuthUser(caller): AuthUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Extension(avatar_dir): Extension<AvatarDir>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if caller.role != Role::Admin && caller.id != id {
        return StatusCode::FORBIDDEN.into_response();
    }

    clear_user_avatar_files(avatar_dir.0.as_path(), &id);
    match auth.set_avatar(&id, None) {
        Ok(u)  => axum::Json(UserResponse::from(u)).into_response(),
        Err(e) => err_resp(e),
    }
}

// ── POST /api/users/:id/password ──────────────────────────────────────────────

pub async fn change_password(
    AuthUser(caller): AuthUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
    Json(req): Json<ChangePasswordRequest>,
) -> impl IntoResponse {
    // Users can change their own password; admins can change any.
    if caller.role != Role::Admin && caller.id != id {
        return StatusCode::FORBIDDEN.into_response();
    }

    match auth.change_password(&id, &req.new_password) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_resp(e),
    }
}

// ── POST /api/users/:id/reset-password ───────────────────────────────────────

#[derive(Serialize)]
pub struct ResetPasswordResponse {
    pub new_password: String,
}

pub async fn reset_password(
    AdminUser(_): AdminUser,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use rand::distributions::Alphanumeric;
    use rand::Rng;
    let new_password: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();

    match auth.change_password(&id, &new_password) {
        Ok(()) => axum::Json(ResetPasswordResponse { new_password }).into_response(),
        Err(e) => err_resp(e),
    }
}
