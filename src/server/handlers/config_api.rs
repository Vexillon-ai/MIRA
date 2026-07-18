// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/config_api.rs
//! Config read/write API endpoints.

use std::sync::Arc;

use axum::{
    extract::{Json, Multipart},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::Serialize;
use serde_json::Value;

use crate::auth::{AdminUser, AuthUser};
use crate::config::MiraConfig;
use crate::server::handlers::users::{
    clear_user_avatar_files, AvatarDir, AVATAR_MAX_BYTES,
};
use crate::web::LiveConfig;

// ── GET /api/config ───────────────────────────────────────────────────────────

pub async fn get_config(
    AdminUser(_admin): AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
) -> impl IntoResponse {
    let cfg = live_cfg.get().await;
    // Redact sensitive fields before returning.
    let mut value = match serde_json::to_value(cfg.as_ref()) {
        Ok(v)  => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    redact_secrets(&mut value);
    axum::Json(value).into_response()
}

// ── PUT /api/config ───────────────────────────────────────────────────────────

pub async fn put_config(
    AdminUser(_admin): AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let live = live_cfg.get().await;

    // Merge: any "***" sentinel in the incoming JSON means "keep the current
    // live value" (the field was redacted by GET /api/config and not changed
    // by the user). We restore those values from the current live config so
    // secrets are never overwritten with the placeholder string.
    let mut current = match serde_json::to_value(live.as_ref()) {
        Ok(v)  => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let mut incoming = body;
    restore_redacted(&mut incoming, &mut current);

    // Reject the save if any secret still holds the literal sentinel "***" —
    // this means the live config had None for that field and the frontend sent
    // a stale placeholder. Better to refuse than to save garbage.
    if contains_unreplaced_sentinel(&incoming) {
        return (
            StatusCode::BAD_REQUEST,
            "Cannot save: one or more secret fields contain a redacted placeholder. \
             Please enter the actual value or leave the field blank to clear it."
                .to_owned(),
        )
            .into_response();
    }

    let mut new_cfg: MiraConfig = match serde_json::from_value(incoming) {
        Ok(c)  => c,
        Err(e) => return (StatusCode::BAD_REQUEST,
                          format!("Invalid config: {}", e)).into_response(),
    };

    // Pre-flight: refuse to persist a config that would crash the
    // process on next boot. The internal embedding provider dlopens
    // libonnxruntime, and our release profile is panic=abort — a
    // missing dep means the supervisor restart-loops past
    // StartLimitBurst and the server is unrecoverable from the UI.
    // Surface a structured error so the Settings page can offer to
    // install the dep instead of just showing a generic 400.
    if new_cfg.memory.embedding.provider == "internal"
       && !crate::install::deps::is_onnxruntime_available()
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "missing_dep",
                "dep":   "onnxruntime",
                "message": "The internal embedding provider requires ONNX Runtime, \
                            which is not installed on this host. Install it via \
                            POST /api/admin/deps/onnxruntime/install (or run \
                            `mira deps install`) and retry the save.",
                "install_endpoint": "/api/admin/deps/onnxruntime/install",
            })),
        ).into_response();
    }

    // config_path is #[serde(skip)] — restore from live config.
    new_cfg.config_path = live.config_path.clone();

    // Detect a live flip of the out-of-process sentinel toggle so we can
    // auto-register/start (or unregister/stop) its supervised service to match —
    // the operator shouldn't have to run `mira guardian-install` by hand after
    // ticking the box. Captured before `new_cfg` is moved into `update`.
    let was_sentinel_enabled = live.guardian.process.enabled;
    let now_sentinel_enabled = new_cfg.guardian.process.enabled;
    let cfg_path = live.config_path.clone();

    match live_cfg.update(new_cfg).await {
        Ok(())  => {
            if now_sentinel_enabled != was_sentinel_enabled {
                // Best-effort + backgrounded; logs its outcome and the Guardian
                // panel reflects the resulting state.
                crate::install::apply_guardian_enable_change(now_sentinel_enabled, cfg_path);
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e)  => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── POST /api/config/validate ─────────────────────────────────────────────────

pub async fn validate_config(
    AdminUser(_admin): AdminUser,
    Json(cfg): Json<Value>,
) -> impl IntoResponse {
    use crate::config::validate::validate_config_json;
    match validate_config_json(&cfg) {
        Ok(())   => (StatusCode::OK, "valid").into_response(),
        Err(errs) => (StatusCode::UNPROCESSABLE_ENTITY, errs.join("\n")).into_response(),
    }
}

// ── GET /api/agent/appearance ─────────────────────────────────────────────────
//
// Lightweight public-ish (AuthUser-gated) endpoint so non-admin users can
// resolve the assistant's avatar for display in chat — /api/config is
// admin-only and returns the whole config, which regular users should not
// see.

#[derive(Serialize)]
pub struct AgentAppearance {
    pub avatar:            Option<String>,
    pub avatar_updated_at: Option<i64>,
}

pub async fn get_agent_appearance(
    AuthUser(_): AuthUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
) -> impl IntoResponse {
    let cfg = live_cfg.get().await;
    axum::Json(AgentAppearance {
        avatar:            cfg.agent.avatar.clone(),
        avatar_updated_at: cfg.agent.avatar_updated_at,
    })
    .into_response()
}

// ── POST /api/config/agent-avatar (multipart) ─────────────────────────────────

pub async fn upload_agent_avatar(
    AdminUser(_): AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(avatar_dir): Extension<AvatarDir>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // Grab the first field with bytes — same shape as user upload.
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

    if let Err(e) = std::fs::create_dir_all(avatar_dir.0.as_path()) {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                format!("avatar dir: {}", e)).into_response();
    }
    // Reuse the user helper with the literal id "agent" — files are stored
    // as `agent.{ext}` so clear/write share the same naming shape.
    clear_user_avatar_files(avatar_dir.0.as_path(), "agent");

    let path = avatar_dir.0.join(format!("agent.{}", ext));
    if let Err(e) = std::fs::write(&path, &bytes) {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                format!("write: {}", e)).into_response();
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let live = live_cfg.get().await;
    let mut new_cfg: MiraConfig = (*live).clone();
    new_cfg.agent.avatar            = Some(format!("upload:{}", ext));
    new_cfg.agent.avatar_updated_at = Some(now_ms);

    match live_cfg.update(new_cfg).await {
        Ok(()) => axum::Json(AgentAppearance {
            avatar:            Some(format!("upload:{}", ext)),
            avatar_updated_at: Some(now_ms),
        })
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── PUT /api/config/agent-avatar (preset or clear) ────────────────────────────

#[derive(serde::Deserialize)]
pub struct SetAgentAvatarRequest {
    /// `"preset:<key>"` or `null` to clear. Upload uses the multipart endpoint.
    pub avatar: Option<String>,
}

pub async fn set_agent_avatar(
    AdminUser(_): AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(avatar_dir): Extension<AvatarDir>,
    Json(req): Json<SetAgentAvatarRequest>,
) -> impl IntoResponse {
    // Switching to a preset or clearing means any uploaded file is no longer
    // referenced — remove it so it doesn't linger under data_dir.
    let drops_upload = !matches!(req.avatar.as_deref(), Some(s) if s.starts_with("upload:"));
    if drops_upload {
        clear_user_avatar_files(avatar_dir.0.as_path(), "agent");
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let live = live_cfg.get().await;
    let mut new_cfg: MiraConfig = (*live).clone();
    new_cfg.agent.avatar            = req.avatar.clone();
    new_cfg.agent.avatar_updated_at = Some(now_ms);

    match live_cfg.update(new_cfg).await {
        Ok(()) => axum::Json(AgentAppearance {
            avatar:            req.avatar,
            avatar_updated_at: Some(now_ms),
        })
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── DELETE /api/config/agent-avatar ───────────────────────────────────────────

pub async fn delete_agent_avatar(
    AdminUser(_): AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(avatar_dir): Extension<AvatarDir>,
) -> impl IntoResponse {
    clear_user_avatar_files(avatar_dir.0.as_path(), "agent");

    let now_ms = chrono::Utc::now().timestamp_millis();
    let live = live_cfg.get().await;
    let mut new_cfg: MiraConfig = (*live).clone();
    new_cfg.agent.avatar            = None;
    new_cfg.agent.avatar_updated_at = Some(now_ms);

    match live_cfg.update(new_cfg).await {
        Ok(()) => axum::Json(AgentAppearance {
            avatar:            None,
            avatar_updated_at: Some(now_ms),
        })
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub(crate) fn redact_secrets(value: &mut Value) {
    if let Value::Object(map) = value {
        for (k, v) in map.iter_mut() {
            if is_secret_field(k.as_str()) {
                if !v.is_null() {
                    *v = Value::String("***".to_owned());
                }
            } else {
                redact_secrets(v);
            }
        }
    } else if let Value::Array(arr) = value {
        for item in arr.iter_mut() {
            redact_secrets(item);
        }
    }
}

// Single source of truth for which config keys are secrets — used by both the
// GET redactor and the PUT restore/sentinel-check, so they can't drift. A key
// missing here leaks in plaintext on GET *and* can be overwritten on PUT.
fn is_secret_field(k: &str) -> bool {
    matches!(
        k,
        "api_key" | "auth_token" | "webhook_secret" | "hmac_key"
            | "secret_token" | "bot_token" | "jwt_secret" | "password_hash"
            // Added after a release audit found these reaching GET in plaintext:
            | "client_secret"   // OIDC providers + Google/Outlook calendar OAuth
            | "bind_password"   // LDAP service-account bind password
            | "smtp_password"   // system outbound-email relay password
            // FCM service-account credential path → treat as secret so the
            // path (and the credential location it points at) isn't leaked
            // on config GET and can't be silently overwritten on PUT.
            | "service_account_json_path"
    )
}

/// Walk `incoming` and:
/// 1. Replace any `"***"` sentinel with the live value from `current`.
/// 2. Inject back any secret fields that are present in `current` but
///    entirely absent from `incoming` (frontend omitted them).
///
/// This ensures secrets are never lost or overwritten with a placeholder
/// when the user saves settings without touching a secret field.
fn restore_redacted(incoming: &mut Value, current: &Value) {
    match (incoming, current) {
        (Value::Object(inc_map), Value::Object(cur_map)) => {
            // Pass 1 – fix sentinels and recurse into non-secret fields.
            for (k, inc_v) in inc_map.iter_mut() {
                let is_sentinel = matches!(inc_v, Value::String(s) if s == "***")
                    && is_secret_field(k.as_str());
                if is_sentinel {
                    // Sentinel — restore the real value from the live config.
                    // If the live config has None (field absent), leave the
                    // sentinel so it round-trips to None after deserialization.
                    if let Some(cur_v) = cur_map.get(k) {
                        *inc_v = cur_v.clone();
                    }
                } else if let Some(cur_v) = cur_map.get(k) {
                    restore_redacted(inc_v, cur_v);
                }
            }
            // Pass 2 – inject back secrets that are present in `current`
            // but were not sent at all by the frontend.
            for (k, cur_v) in cur_map {
                if is_secret_field(k.as_str())
                    && !inc_map.contains_key(k)
                    && !cur_v.is_null()
                {
                    inc_map.insert(k.clone(), cur_v.clone());
                }
            }
        }
        (Value::Array(inc_arr), Value::Array(cur_arr)) => {
            for (inc_v, cur_v) in inc_arr.iter_mut().zip(cur_arr.iter()) {
                restore_redacted(inc_v, cur_v);
            }
        }
        _ => {}
    }
}

/// Return true if any secret field anywhere in `value` still holds the
/// literal sentinel `"***"` — which means `restore_redacted` could not
/// find the real value in the live config.
fn contains_unreplaced_sentinel(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(k, v)| {
            if is_secret_field(k.as_str()) {
                matches!(v, Value::String(s) if s == "***")
            } else {
                contains_unreplaced_sentinel(v)
            }
        }),
        Value::Array(arr) => arr.iter().any(contains_unreplaced_sentinel),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // GET /api/config must never return a real secret. This pins the keys that
    // were leaking in plaintext before the 0.273.x audit (client_secret,
    // bind_password, smtp_password) alongside the core ones, nested the way they
    // sit in the real config tree, so removing any from is_secret_field fails
    // loudly. The matching restore on PUT is exercised too.
    #[test]
    fn redacts_all_known_secret_fields_including_audit_finds() {
        let mut v = json!({
            "providers": { "openai": { "api_key": "sk-REAL" } },
            "security":  { "jwt_secret": "JWT-REAL" },
            "auth": {
                "oidc": { "providers": [ { "client_secret": "OIDC-REAL" } ] },
                "ldap": { "bind_password": "LDAP-REAL" }
            },
            "calendar": { "google": { "client_secret": "CAL-REAL" } },
            "system_email": { "smtp_password": "SMTP-REAL" },
            "channels": { "telegram": { "bot_token": "BOT-REAL" } }
        });
        redact_secrets(&mut v);
        let dump = v.to_string();
        for leaked in ["sk-REAL","JWT-REAL","OIDC-REAL","LDAP-REAL","CAL-REAL","SMTP-REAL","BOT-REAL"] {
            assert!(!dump.contains(leaked), "secret leaked through redaction: {leaked}");
        }
        // And a non-secret value is preserved.
        assert_eq!(v["channels"]["telegram"]["bot_token"], "***");
    }

    #[test]
    fn restore_puts_back_redacted_secret_from_live() {
        // Client sends "***" for an unchanged secret; restore_redacted must
        // recover the live value so it isn't wiped.
        let mut incoming = json!({ "security": { "jwt_secret": "***" } });
        let current      = json!({ "security": { "jwt_secret": "LIVE-SECRET" } });
        restore_redacted(&mut incoming, &current);
        assert_eq!(incoming["security"]["jwt_secret"], "LIVE-SECRET");
        assert!(!contains_unreplaced_sentinel(&incoming));
    }
}
