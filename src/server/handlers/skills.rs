// SPDX-License-Identifier: AGPL-3.0-or-later

//! Skills HTTP surface.
//!
//! - **GET /api/skills** — list installed Skills with status, permissions,
//!   and load diagnostics. Per-user `enabled` flag is projected from
//!   `SkillPrefsStore` for the requesting user (A5).
//! - **PUT /api/skills/{id}/preferences** — toggle the requesting user's
//!   enable/disable preference for one Skill (A5).
//! - **POST /api/skills/preview** — admin-only. Multipart upload of a
//!   `.miraskill` archive (tar.gz). Parse + validate; return the manifest
//!   for review without writing to disk (A6).
//! - **POST /api/skills/install** — admin-only. Same upload, but extract
//!   the archive into `<data_dir>/skills/<id>/`. The agent only sees the
//!   new Skill on next restart (A6).
//! - **DELETE /api/skills/{id}** — admin-only. Remove the Skill directory
//!   (A6). Per-user prefs survive so re-installs preserve user choices.
//!
//! Implementation note: we re-scan the Skills directory on each GET
//! rather than caching the registry. Scanning is cheap and re-scanning
//! tells the truth about what's currently installed. The agent only
//! refreshes its registered SkillTools on restart (hot-reload is post-v1).

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Extension, Multipart, Path};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use tar::Archive;
use tempfile::TempDir;
use tracing::{info, warn};

use crate::auth::middleware::{AdminUser, AuthUser};
use crate::server::handlers::onboarding::DataDir;
use crate::skills::{
    self,
    loader::{LoadError, SkillRegistry},
    manifest::{Permissions, SkillManifest, ToolSpec},
    prefs::SkillPrefsStore,
    trust::{TrustEntry, TrustStore},
};

/// Open the trust store at its conventional path under `<data_dir>`. If
/// the file is malformed, log and fall back to an empty store so the
/// rest of the API stays alive — admins can fix the file via the
/// trust-store endpoints.
fn load_trust_store(data_dir: &std::path::Path) -> TrustStore {
    let path = TrustStore::default_path(&skills::default_skills_dir(data_dir));
    match TrustStore::load(&path) {
        Ok(s) => s,
        Err(e) => {
            warn!("trust store at {} unreadable: {e}", path.display());
            TrustStore::empty()
        }
    }
}

// ── Archive limits ────────────────────────────────────────────────────────
//
// Conservative caps so a malicious upload can't fill the disk or push
// MIRA OOM via a zip-bomb-style archive. Tunable via constants here for
// now; if real-world Skills bump up against them we'll move them to
// config.
const MAX_ARCHIVE_BYTES: usize    = 10 * 1024 * 1024;  // 10 MB compressed
const MAX_FILE_BYTES:    u64      =  1 * 1024 * 1024;  // 1 MB per file
const MAX_ENTRIES:       usize    = 200;

/// Newtype wrapper around the optional `Arc<SkillPrefsStore>` so the
/// router can attach it as an `Extension` without colliding with other
/// `Option<Arc<...>>` extensions.
#[derive(Clone)]
pub struct SkillPrefsExt(pub Option<Arc<SkillPrefsStore>>);

#[derive(Debug, Serialize)]
pub struct SkillsResponse {
    pub skills_dir: String,
    pub loaded:     Vec<SkillSummary>,
    pub errors:     Vec<SkillLoadErrorDto>,
}

#[derive(Debug, Serialize)]
pub struct SkillSummary {
    pub id:           String,
    pub version:      String,
    pub display_name: String,
    pub description:  String,
    pub authors:      Vec<String>,
    pub license:      Option<String>,

    /// Manifest declared a `[verification]` block. Doesn't mean the
    /// signature is valid — that lives in `verified` (always false until
    /// slice A7).
    pub signed:       bool,

    /// True iff the manifest signature checked against a trusted
    /// publisher key in the trust store at scan time.
    pub verified:     bool,
    /// Trust-store label of the publisher key, when the signature
    /// validated or at least pointed at a known key. Helps users
    /// distinguish "this Skill is from MIRA Team" from "this Skill is
    /// from someone I added a key for last week".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher_label:    Option<String>,
    /// Why the Skill is not verified. Populated for signed-but-failed
    /// or unsigned-with-trust-store-configured manifests; absent when
    /// verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_error: Option<String>,

    pub permissions:  PermissionsDto,
    pub tools:        Vec<ToolSummary>,

    /// Filesystem path where the Skill is installed. Surface this so
    /// admins can find a Skill on disk without grepping logs.
    pub root_dir:     String,

    /// Per-user enable/disable lands in slice A5. Until then every
    /// loaded Skill is treated as enabled — the field is here so the
    /// UI can build out without churn later.
    pub enabled:      bool,

    /// System skill: built-in capability that can be enabled/disabled but
    /// not uninstalled (the UI hides the remove control). See
    /// [`SkillMeta::system`].
    pub system:       bool,
}

#[derive(Debug, Serialize)]
pub struct PermissionsDto {
    pub network_egress:                   Vec<String>,
    pub filesystem:                       Vec<String>,
    pub subprocess:                       bool,
    pub subprocess_allowlist:             Vec<String>,
    pub secrets:                          Vec<SecretDto>,
    pub llm_providers:                    Vec<String>,
    pub max_llm_spend_per_invocation_usd: Option<f64>,
}

/// One declared secret as the web UI sees it. Always has the typed
/// shape regardless of whether the manifest used the legacy bare-name
/// form — flat is easier on the React side.
#[derive(Debug, Serialize)]
pub struct SecretDto {
    pub key:         String,
    pub description: Option<String>,
    pub required:    bool,
    pub sensitive:   bool,
    /// `"system"` | `"user"` | `"either"` — the manifest's hint.
    pub scope_hint:  String,
    /// Inline example surfaced under the input field in the UI.
    /// Mirrors the manifest's optional `example` field; null when
    /// absent. Used both as the input placeholder and as a hint
    /// line directly under the input.
    pub example:     Option<String>,
}

impl From<&crate::skills::manifest::SecretSpec> for SecretDto {
    fn from(s: &crate::skills::manifest::SecretSpec) -> Self {
        let scope_hint = match s.scope_hint() {
            crate::skills::manifest::SecretScopeHint::System => "system",
            crate::skills::manifest::SecretScopeHint::User   => "user",
            crate::skills::manifest::SecretScopeHint::Either => "either",
        }.to_string();
        Self {
            key:         s.key().to_string(),
            description: s.description().map(String::from),
            required:    s.required(),
            sensitive:   s.sensitive(),
            scope_hint,
            example:     s.example().map(String::from),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ToolSummary {
    pub name: String,
    /// One of "builtin", "prompt", "executable" — the manifest tool kind.
    pub kind: &'static str,
    /// For builtin: the underlying impl name. For prompt: template path.
    /// For executable: the executable path. Surfaces the binding so
    /// reviewers can spot a Skill that wraps a sensitive built-in.
    pub binding: String,
}

#[derive(Debug, Serialize)]
pub struct SkillLoadErrorDto {
    pub path:  String,
    pub error: String,
}

pub async fn list_skills(
    AuthUser(user):              AuthUser,
    Extension(data_dir):         Extension<DataDir>,
    Extension(SkillPrefsExt(prefs)): Extension<SkillPrefsExt>,
) -> Json<SkillsResponse> {
    let skills_dir = skills::default_skills_dir(&data_dir.0);
    let mira_version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is always valid semver");
    let trust = load_trust_store(&data_dir.0);

    let registry = skills::load_dir_with_trust(&skills_dir, &mira_version, Some(&trust));
    let disabled = prefs.as_ref()
        .map(|p| p.disabled_for_user(&user.id))
        .unwrap_or_default();

    Json(build_response(&skills_dir, &registry, &disabled))
}

#[derive(Debug, Deserialize)]
pub struct SetEnabledRequest {
    pub enabled: bool,
}

/// PUT /api/skills/{id}/preferences — toggle the calling user's
/// enable/disable for one Skill.
///
/// Returns 404 if `id` doesn't match any currently-loaded Skill (so users
/// can't accumulate preferences for skills they uninstalled).
pub async fn set_skill_enabled(
    AuthUser(user):                  AuthUser,
    Extension(data_dir):             Extension<DataDir>,
    Extension(SkillPrefsExt(prefs)): Extension<SkillPrefsExt>,
    Path(skill_id):                  Path<String>,
    Json(req):                       Json<SetEnabledRequest>,
) -> impl IntoResponse {
    let Some(prefs) = prefs else {
        return (StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "skill prefs store unavailable"})))
            .into_response();
    };

    // Guard: refuse to record a preference for a skill that isn't installed.
    let skills_dir = skills::default_skills_dir(&data_dir.0);
    let mira_version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is always valid semver");
    let registry = skills::load_dir(&skills_dir, &mira_version);
    if registry.loaded.iter().all(|s| s.manifest.skill.id != skill_id) {
        return (StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("skill {skill_id:?} not found")})))
            .into_response();
    }

    if let Err(e) = prefs.set_enabled(&user.id, &skill_id, req.enabled) {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})))
            .into_response();
    }

    (StatusCode::OK,
     Json(serde_json::json!({"ok": true, "skill_id": skill_id, "enabled": req.enabled})))
        .into_response()
}

fn build_response(
    skills_dir: &PathBuf,
    registry: &SkillRegistry,
    disabled_for_user: &std::collections::HashSet<String>,
) -> SkillsResponse {
    SkillsResponse {
        skills_dir: skills_dir.display().to_string(),
        loaded:     registry.loaded.iter()
            .map(|s| skill_summary_from_loaded(
                s, !disabled_for_user.contains(&s.manifest.skill.id),
            ))
            .collect(),
        errors:     registry.errors.iter().map(|e| load_error_dto(e)).collect(),
    }
}

/// Project a `LoadedSkill` (carries verification fields) onto the API
/// shape. Used by the GET handler.
fn skill_summary_from_loaded(s: &skills::loader::LoadedSkill, enabled: bool) -> SkillSummary {
    let mut summary = skill_summary(&s.manifest, &s.root_dir, s.signed, enabled);
    summary.verified           = s.verified;
    summary.publisher_label    = s.publisher_label.clone();
    summary.verification_error = s.verification_error.clone();
    // Loader resolves system-ness (manifest flag OR bundled set), which may be
    // broader than the manifest's own `system` field.
    summary.system             = s.system;
    summary
}

/// Build a summary directly from a manifest. Used by `preview_skill`,
/// where we don't have a `LoadedSkill` yet — verification is done
/// separately and patched in.
fn skill_summary(
    manifest: &SkillManifest,
    root_dir: &PathBuf,
    signed: bool,
    enabled: bool,
) -> SkillSummary {
    SkillSummary {
        id:           manifest.skill.id.clone(),
        version:      manifest.skill.version.to_string(),
        display_name: manifest.skill.display_name.clone(),
        description:  manifest.skill.description.clone(),
        authors:      manifest.skill.authors.clone(),
        license:      manifest.skill.license.clone(),
        signed,
        verified:     false, // patched by skill_summary_from_loaded / preview path
        publisher_label:    None,
        verification_error: None,
        permissions:  permissions_dto(&manifest.permissions),
        tools:        manifest.tools.iter()
            .map(|(name, spec)| tool_summary(name, spec))
            .collect::<Vec<_>>()
            .tap_sort_by_name(),
        root_dir:     root_dir.display().to_string(),
        enabled,
        system:       manifest.skill.system,
    }
}

fn permissions_dto(p: &Permissions) -> PermissionsDto {
    PermissionsDto {
        network_egress:                   p.network_egress.clone(),
        filesystem:                       p.filesystem.clone(),
        subprocess:                       p.subprocess,
        subprocess_allowlist:             p.subprocess_allowlist.clone(),
        secrets:                          p.secrets.iter().map(SecretDto::from).collect(),
        llm_providers:                    p.llm_providers.clone(),
        max_llm_spend_per_invocation_usd: p.max_llm_spend_per_invocation_usd,
    }
}

fn tool_summary(name: &str, spec: &ToolSpec) -> ToolSummary {
    let (kind, binding) = match spec {
        ToolSpec::Builtin    { r#impl }                  => ("builtin",    r#impl.clone()),
        ToolSpec::Prompt     { template }                => ("prompt",     template.clone()),
        ToolSpec::Executable { path, .. }                => ("executable", path.clone()),
    };
    ToolSummary {
        name: name.to_string(),
        kind,
        binding,
    }
}

fn load_error_dto(e: &LoadError) -> SkillLoadErrorDto {
    SkillLoadErrorDto {
        path:  e.path.display().to_string(),
        error: e.error.clone(),
    }
}

/// Local extension trait so we can sort the tool list alphabetically by name
/// inline without an extra binding. Keeps `skill_summary` readable.
trait TapSortByName {
    fn tap_sort_by_name(self) -> Self;
}
impl TapSortByName for Vec<ToolSummary> {
    fn tap_sort_by_name(mut self) -> Self {
        self.sort_by(|a, b| a.name.cmp(&b.name));
        self
    }
}

// Suppress unused-Arc warning when this handler is registered without
// touching Arc directly.
#[allow(dead_code)]
fn _arc_anchor(_: Arc<()>) {}

// ─────────────────────────────────────────────────────────────────────────
// Slice A6 — install / uninstall flow
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct PreviewResponse {
    /// What we'd install: the same shape /api/skills returns per Skill,
    /// minus the `enabled`/`root_dir`/`signed`/`verified` fields that
    /// only apply once a skill is on disk.
    pub manifest:    SkillSummary,
    /// `true` if a skill with this id is already installed. Caller
    /// should warn the user; install with `force=true` to overwrite.
    pub conflicts:   bool,
    /// Bytes the archive will occupy after extraction (sum of file
    /// sizes). Surfaced so admins see disk usage before committing.
    pub total_bytes: u64,
}

#[derive(Debug, Deserialize, Default)]
pub struct InstallQuery {
    /// Overwrite an existing skill with the same id. Off by default so
    /// admins don't accidentally clobber an installed Skill they meant
    /// to coexist with.
    #[serde(default)]
    pub force: bool,
}

/// POST /api/skills/preview — admin only. Multipart upload of a
/// `.miraskill` archive. Validates the contents (size caps, no path
/// traversal, matching id) and returns the manifest for review. Does
/// not write anything to `<data_dir>/skills/`.
pub async fn preview_skill(
    AdminUser(_admin):   AdminUser,
    Extension(data_dir): Extension<DataDir>,
    multipart:           Multipart,
) -> impl IntoResponse {
    let archive = match read_archive_upload(multipart).await {
        Ok(b)  => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(error(&e))).into_response(),
    };
    match parse_archive(&archive) {
        Ok(parsed) => {
            let conflicts = skills_dir_for_id(&data_dir.0, &parsed.manifest.skill.id).exists();
            let trust = load_trust_store(&data_dir.0);
            let signed = parsed.manifest.verification.is_some();
            let outcome = skills::verify_manifest(&parsed.manifest, &trust);

            let mut summary = skill_summary(
                &parsed.manifest,
                &PathBuf::from("(uploaded — not yet installed)"),
                signed,
                true, // enabled — preview default
            );
            summary.verified           = outcome.verified;
            summary.publisher_label    = outcome.publisher_label;
            summary.verification_error = outcome.reason;

            (StatusCode::OK, Json(PreviewResponse {
                manifest:    summary,
                conflicts,
                total_bytes: parsed.total_bytes,
            })).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(error(&e))).into_response(),
    }
}

#[derive(Debug, Serialize)]
pub struct InstallResponse {
    pub installed:    bool,
    pub skill_id:     String,
    pub root_dir:     String,
    pub overwritten:  bool,
    /// True when the agent's tool registry has not yet been refreshed —
    /// the new SkillTool only appears after a server restart. UI surfaces
    /// this as a "Restart MIRA to use this Skill" notice.
    pub restart_required: bool,
}

/// POST /api/skills/install?force=<bool> — admin only. Same upload as
/// preview, but extracts the archive into `<data_dir>/skills/<id>/`
/// after validation. Atomic: writes to a temp dir alongside, then
/// renames into place.
pub async fn install_skill(
    AdminUser(admin):    AdminUser,
    Extension(data_dir): Extension<DataDir>,
    axum::extract::Query(q): axum::extract::Query<InstallQuery>,
    multipart:           Multipart,
) -> impl IntoResponse {
    let archive = match read_archive_upload(multipart).await {
        Ok(b)  => b,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(error(&e))).into_response(),
    };
    let parsed = match parse_archive(&archive) {
        Ok(p)  => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(error(&e))).into_response(),
    };

    let dest = skills_dir_for_id(&data_dir.0, &parsed.manifest.skill.id);
    let overwritten = dest.exists();
    if overwritten && !q.force {
        return (
            StatusCode::CONFLICT,
            Json(error(&format!(
                "skill {:?} already installed — pass ?force=true to overwrite",
                parsed.manifest.skill.id,
            ))),
        ).into_response();
    }

    if let Err(e) = write_skill_atomic(&dest, &parsed) {
        warn!("install_skill failed for {:?} (admin={:?}): {e}", parsed.manifest.skill.id, admin.id);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error(&format!("install failed: {e}"))),
        ).into_response();
    }

    info!(
        "Skill installed: {:?} v{} → {} (admin={}, overwritten={})",
        parsed.manifest.skill.id, parsed.manifest.skill.version,
        dest.display(), admin.id, overwritten,
    );

    (StatusCode::OK, Json(InstallResponse {
        installed:        true,
        skill_id:         parsed.manifest.skill.id,
        root_dir:         dest.display().to_string(),
        overwritten,
        restart_required: true,
    })).into_response()
}

#[derive(Debug, Serialize)]
pub struct UninstallResponse {
    pub uninstalled:      bool,
    pub skill_id:         String,
    pub restart_required: bool,
}

/// DELETE /api/skills/{id} — admin only. Removes the skill directory.
/// Per-user prefs are intentionally left in place — re-installing the
/// same id later restores user choices.
pub async fn uninstall_skill(
    AdminUser(admin):    AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Path(skill_id):      Path<String>,
) -> impl IntoResponse {
    let dest = skills_dir_for_id(&data_dir.0, &skill_id);
    if !dest.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(error(&format!("skill {skill_id:?} not installed"))),
        ).into_response();
    }
    // System skills are built-in capabilities (tools/scheduler/UI live in the
    // binary); the manifest only exposes them. Removing it wouldn't remove the
    // feature, so refuse — the user disables it instead, per-user. System-ness
    // = bundled set OR an explicit `system = true` manifest flag (mirrors the
    // loader), so every bundled skill is protected without per-file flags.
    let manifest_system = std::fs::read_to_string(dest.join("skill.toml")).ok()
        .and_then(|t| SkillManifest::parse(&t).ok())
        .map(|m| m.skill.system)
        .unwrap_or(false);
    if skills::bundled::is_bundled(&skill_id) || manifest_system {
        return (
            StatusCode::CONFLICT,
            Json(error(&format!(
                "skill {skill_id:?} is a system skill and cannot be uninstalled — \
                 disable it on the Skills page instead"
            ))),
        ).into_response();
    }
    if let Err(e) = std::fs::remove_dir_all(&dest) {
        warn!("uninstall failed for {skill_id:?} (admin={:?}): {e}", admin.id);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error(&format!("uninstall failed: {e}"))),
        ).into_response();
    }

    // If the user just uninstalled a *bundled* Skill, drop a marker so
    // the next boot doesn't silently re-extract it. Without this the
    // bundled version would reappear and the admin's intent would be
    // ignored.
    if skills::bundled::is_bundled(&skill_id) {
        let skills_dir = skills::default_skills_dir(&data_dir.0);
        if let Err(e) = skills::bundled::mark_uninstalled(&skills_dir, &skill_id) {
            warn!(
                "uninstall succeeded but failed to mark {skill_id:?} as uninstalled bundled: {e} \
                 — the next MIRA restart will re-extract it",
            );
        }
    }

    info!("Skill uninstalled: {skill_id:?} (admin={})", admin.id);
    (StatusCode::OK, Json(UninstallResponse {
        uninstalled:      true,
        skill_id,
        restart_required: true,
    })).into_response()
}

// ── Secrets management (slice 4) ─────────────────────────────────────────
//
// Three admin endpoints over the [`SecretsStore`]:
//   GET    /api/admin/skills/{id}/secrets         — list keys (no values)
//   PUT    /api/admin/skills/{id}/secrets/{key}   — set value
//   DELETE /api/admin/skills/{id}/secrets/{key}   — clear
//
// Scope is an optional query param: `?scope=system` (default) or
// `?scope=user:<id>`. Operating on per-user secrets is admin-only too —
// a regular user can't read or set their own here; the existing chat-tier
// `secrets` is set by the admin on the user's behalf. Per-user
// self-service lands when we have an in-app secrets page wired into the
// non-admin nav.
//
// **Values** never round-trip through the API. The response shape lists
// only keys + metadata; setting a value writes it directly into the
// vault. This avoids accidentally logging secrets in HTTP traces.

/// Wrapper around the gateway's `Arc<SecretsStore>` so axum's Extension
/// system can route it. Optional Arc inside: when the vault failed to
/// open at boot the handlers return 503 with a clear error.
#[derive(Clone)]
pub struct SecretsStoreExt(pub Option<Arc<crate::skills::SecretsStore>>);

#[derive(Debug, Deserialize)]
pub struct SecretsScopeQuery {
    /// `system` (default) or `user:<id>`.
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SecretListEntry {
    pub key:        String,
    pub scope:      String,
    pub scope_id:   String,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct SetSecretBody {
    /// The plaintext value. Cannot be empty — call DELETE to remove.
    pub value: String,
}

fn parse_scope_query(
    raw: Option<&str>,
) -> Result<(crate::skills::SecretScope, String), String> {
    match raw {
        None | Some("system") => Ok((crate::skills::SecretScope::System, String::new())),
        Some(s) if s.starts_with("user:") => {
            let id = s["user:".len()..].trim();
            if id.is_empty() {
                Err("scope `user:` needs an id (e.g. `user:<uuid>`)".into())
            } else {
                Ok((crate::skills::SecretScope::User, id.to_string()))
            }
        }
        Some(other) => Err(format!(
            "unknown scope {other:?}; use `system` or `user:<id>`"
        )),
    }
}

fn vault_unavailable() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(error("skill secrets vault unavailable — see server logs for the open-time failure")),
    ).into_response()
}

/// GET /api/admin/skills/{id}/secrets[?scope=...]
pub async fn list_skill_secrets(
    AdminUser(_admin):    AdminUser,
    Extension(vault):     Extension<SecretsStoreExt>,
    Path(skill_id):       Path<String>,
    axum::extract::Query(q): axum::extract::Query<SecretsScopeQuery>,
) -> impl IntoResponse {
    let Some(store) = vault.0 else { return vault_unavailable(); };
    let (scope, scope_id) = match parse_scope_query(q.scope.as_deref()) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(error(&e))).into_response(),
    };
    match store.list(scope, &scope_id, &skill_id) {
        Ok(entries) => {
            let dto: Vec<SecretListEntry> = entries.into_iter().map(|e| SecretListEntry {
                key:        e.key,
                scope:      e.scope.as_str().to_string(),
                scope_id:   e.scope_id,
                updated_at: e.updated_at,
            }).collect();
            (StatusCode::OK, Json(dto)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error(&format!("list secrets: {e}"))),
        ).into_response(),
    }
}

/// PUT /api/admin/skills/{id}/secrets/{key}[?scope=...]
pub async fn set_skill_secret(
    AdminUser(admin):     AdminUser,
    Extension(vault):     Extension<SecretsStoreExt>,
    Path((skill_id, key)): Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<SecretsScopeQuery>,
    Json(body):           Json<SetSecretBody>,
) -> impl IntoResponse {
    let Some(store) = vault.0 else { return vault_unavailable(); };
    let (scope, scope_id) = match parse_scope_query(q.scope.as_deref()) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(error(&e))).into_response(),
    };
    if body.value.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error("value cannot be empty — DELETE to clear")),
        ).into_response();
    }
    match store.set(scope, &scope_id, &skill_id, &key, &body.value) {
        Ok(()) => {
            info!(
                "skill secret set via API: skill={} key={} scope={} (admin={:?}, value redacted)",
                skill_id, key, scope.as_str(), admin.id,
            );
            (StatusCode::NO_CONTENT, ()).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(error(&format!("set secret: {e}"))),
        ).into_response(),
    }
}

/// DELETE /api/admin/skills/{id}/secrets/{key}[?scope=...]
pub async fn delete_skill_secret(
    AdminUser(admin):     AdminUser,
    Extension(vault):     Extension<SecretsStoreExt>,
    Path((skill_id, key)): Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<SecretsScopeQuery>,
) -> impl IntoResponse {
    let Some(store) = vault.0 else { return vault_unavailable(); };
    let (scope, scope_id) = match parse_scope_query(q.scope.as_deref()) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(error(&e))).into_response(),
    };
    match store.delete(scope, &scope_id, &skill_id, &key) {
        Ok(true) => {
            info!(
                "skill secret deleted via API: skill={} key={} scope={} (admin={:?})",
                skill_id, key, scope.as_str(), admin.id,
            );
            (StatusCode::NO_CONTENT, ()).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(error(&format!("no secret {skill_id}.{key} in scope {}", scope.as_str()))),
        ).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error(&format!("delete secret: {e}"))),
        ).into_response(),
    }
}

// ── LLM aliases (slice 4) ────────────────────────────────────────────────
//
// Read + write `agent.llm_aliases` from the live config. Each skill's
// `permissions.llm_providers` picks aliases from this map; setting an
// alias steers every spawn of the matching skill onto a different
// provider/model without editing the skill manifest itself.

/// One row in the alias map as the UI sees it.
#[derive(Debug, Serialize, Deserialize)]
pub struct LlmAliasDto {
    pub alias:    String,
    pub provider: String,
    pub model:    Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetLlmAliasesBody {
    pub aliases: Vec<LlmAliasDto>,
}

/// GET /api/admin/llm-aliases — current `agent.llm_aliases` map.
pub async fn list_llm_aliases(
    AdminUser(_admin):  AdminUser,
    Extension(live):    Extension<Arc<crate::web::LiveConfig>>,
) -> impl IntoResponse {
    let cfg = live.get().await;
    let mut out: Vec<LlmAliasDto> = cfg.agent.llm_aliases.iter().map(|(name, a)| LlmAliasDto {
        alias:    name.clone(),
        provider: a.provider.clone(),
        model:    a.model.clone(),
    }).collect();
    out.sort_by(|a, b| a.alias.cmp(&b.alias));
    (StatusCode::OK, Json(out)).into_response()
}

/// PUT /api/admin/llm-aliases — replace the whole map.
pub async fn set_llm_aliases(
    AdminUser(admin):  AdminUser,
    Extension(live):   Extension<Arc<crate::web::LiveConfig>>,
    Json(body):        Json<SetLlmAliasesBody>,
) -> impl IntoResponse {
    use std::collections::HashMap;
    use crate::config::LlmAlias;

    let mut new_map: HashMap<String, LlmAlias> = HashMap::new();
    for entry in &body.aliases {
        if entry.alias.trim().is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(error("alias name cannot be empty")),
            ).into_response();
        }
        if entry.provider.trim().is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(error(&format!("alias {:?} has empty provider", entry.alias))),
            ).into_response();
        }
        new_map.insert(entry.alias.clone(), LlmAlias {
            provider: entry.provider.clone(),
            model:    entry.model.clone(),
        });
    }

    // Snapshot the current config, splice in the new aliases, persist
    // via LiveConfig::update which handles save() + broadcast.
    let mut snapshot = (*live.get().await).clone();
    snapshot.agent.llm_aliases = new_map;
    if let Err(e) = live.update(snapshot).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error(&format!("persist config: {e}"))),
        ).into_response();
    }

    info!(
        "llm aliases updated via API: count={} (admin={:?})",
        body.aliases.len(), admin.id,
    );
    (StatusCode::OK, Json(serde_json::json!({"updated": body.aliases.len()}))).into_response()
}

// ── Probe (slice 4 follow-up #1) ─────────────────────────────────────────
//
// "Test connection" check that confirms a skill's configured env vars
// actually let it talk to its upstream. Currently only the coding skill
// has a real probe (runs `claude --print ping`); other skills get a
// 422 with a clear "no probe defined" message so the UI can hide the
// button. Per-user scope is honoured: an admin testing alice's key
// uses alice's secrets, not the system fallback.

#[derive(Debug, Serialize)]
pub struct ProbeResult {
    pub ok:         bool,
    pub message:    String,
    pub latency_ms: u128,
}

/// POST /api/admin/skills/{id}/probe[?scope=...]
pub async fn probe_skill(
    AdminUser(admin):     AdminUser,
    Extension(vault):     Extension<SecretsStoreExt>,
    Path(skill_id):       Path<String>,
    axum::extract::Query(q): axum::extract::Query<SecretsScopeQuery>,
) -> impl IntoResponse {
    let Some(store) = vault.0 else { return vault_unavailable(); };
    let (scope, scope_id) = match parse_scope_query(q.scope.as_deref()) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(error(&e))).into_response(),
    };

    info!(
        "skill probe via API: skill={skill_id} scope={} (admin={:?})",
        scope.as_str(), admin.id,
    );

    // Probes are wired per-skill — adding a new branch here is the
    // signal to the UI (`SkillsPage::hasProbe`) that this skill should
    // show a Test connection button. Returning 422 — not 404 — for
    // unknown ids so the UI can distinguish "skill not installed"
    // (would be 404 from another endpoint) from "skill installed but
    // probe not implemented".
    let user_for_lookup = match scope {
        crate::skills::SecretScope::User   => Some(scope_id.as_str()),
        crate::skills::SecretScope::System => None,
    };
    match skill_id.as_str() {
        "com.mira.claudecode" => {
            let env = store.env_vars_for(user_for_lookup, &skill_id);
            let result = probe_claude_code(env).await;
            (StatusCode::OK, Json(result)).into_response()
        }
        "com.mira.opencode" => {
            let env = store.env_vars_for(user_for_lookup, &skill_id);
            let result = probe_opencode(env).await;
            (StatusCode::OK, Json(result)).into_response()
        }
        _ => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(error(&format!(
                "skill {skill_id:?} has no connection probe defined"
            ))),
        ).into_response(),
    }
}

/// Run `claude --print "ping"` with whatever env the vault returns.
/// Bounded at 10s — long enough for a real network round-trip,
/// short enough that a wedged claude binary doesn't hang the
/// admin UI.
async fn probe_claude_code(env: std::collections::HashMap<String, String>) -> ProbeResult {
    let start = std::time::Instant::now();
    if which_on_path("claude").is_none() {
        return ProbeResult {
            ok: false,
            message: "`claude` CLI not on the server's PATH — install it first".into(),
            latency_ms: start.elapsed().as_millis(),
        };
    }
    // Use stream-json so the same NDJSON parser shape claude_code.rs
    // already understands surfaces auth/login errors as structured
    // `result` events; pick_error_message below knows that format.
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("--print").arg("--output-format").arg("stream-json").arg("--verbose").arg("ping");
    cmd.arg("--bare").arg("--dangerously-skip-permissions");
    for (k, v) in env { cmd.env(k, v); }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let fut = cmd.output();
    let output = match tokio::time::timeout(std::time::Duration::from_secs(10), fut).await {
        Ok(Ok(o))  => o,
        Ok(Err(e)) => return ProbeResult {
            ok: false,
            message: format!("spawn failed: {e}"),
            latency_ms: start.elapsed().as_millis(),
        },
        Err(_) => return ProbeResult {
            ok: false,
            message: "probe timed out after 10s".into(),
            latency_ms: start.elapsed().as_millis(),
        },
    };
    let latency_ms = start.elapsed().as_millis();
    if output.status.success() {
        return ProbeResult { ok: true, message: "OK".into(), latency_ms };
    }
    // Trim noisy claude logs to the last meaningful line so the UI
    // toast is readable. Claude Code dumps its login/auth errors to
    // stdout (NDJSON `result` events) and operational errors to
    // stderr — pick whichever side has content. Falls back to
    // "exit code N" when both are empty.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let msg = pick_error_message(&stdout, &stderr)
        .unwrap_or_else(|| format!("exit code {}", output.status.code().unwrap_or(-1)));
    ProbeResult { ok: false, message: msg, latency_ms }
}

/// Run `opencode run --format json "ping"` with whatever env the vault
/// returns. OPENCODE_MODEL is a synthetic vault key (not a real env var
/// OpenCode reads) — pulled out of the env map and forwarded as a
/// `--model` flag the same way the production adapter does, so the
/// probe exercises the exact model the user has configured. Bounded
/// at 15s — model providers can be slow on cold-cache requests.
async fn probe_opencode(mut env: std::collections::HashMap<String, String>) -> ProbeResult {
    let start = std::time::Instant::now();
    if which_on_path("opencode").is_none() {
        return ProbeResult {
            ok: false,
            message: "`opencode` CLI not on the server's PATH — install it first".into(),
            latency_ms: start.elapsed().as_millis(),
        };
    }
    let model_override = env.remove("OPENCODE_MODEL");
    let mut cmd = tokio::process::Command::new("opencode");
    cmd.arg("run").arg("--format").arg("json").arg("--dangerously-skip-permissions");
    if let Some(m) = model_override.as_deref() { cmd.arg("--model").arg(m); }
    cmd.arg("ping");
    for (k, v) in env { cmd.env(k, v); }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let fut = cmd.output();
    let output = match tokio::time::timeout(std::time::Duration::from_secs(15), fut).await {
        Ok(Ok(o))  => o,
        Ok(Err(e)) => return ProbeResult {
            ok: false,
            message: format!("spawn failed: {e}"),
            latency_ms: start.elapsed().as_millis(),
        },
        Err(_) => return ProbeResult {
            ok: false,
            message: "probe timed out after 15s".into(),
            latency_ms: start.elapsed().as_millis(),
        },
    };
    let latency_ms = start.elapsed().as_millis();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // OpenCode's NDJSON shape differs from Claude Code's. Auth /
    // model errors surface as `{"type":"error","message":"..."}`
    // events on stdout — exit code is generally 0 even on failure.
    // Walk stdout in reverse for the most recent error event.
    if let Some(msg) = pick_opencode_error_message(&stdout) {
        return ProbeResult { ok: false, message: msg, latency_ms };
    }
    if output.status.success() {
        return ProbeResult { ok: true, message: "OK".into(), latency_ms };
    }
    // Non-zero exit and no JSON error event — likely a usage error
    // (bad flag) or environmental failure. Surface stderr tail.
    let msg = stderr.lines().rev().find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| format!("exit code {}", output.status.code().unwrap_or(-1)));
    ProbeResult { ok: false, message: msg, latency_ms }
}

/// OpenCode-flavoured error picker. Walks stdout NDJSON in reverse
/// looking for the most recent `{"type":"error","message":"..."}` line
/// the adapter would treat as terminal. Returns None when no error
/// event is present (success path).
fn pick_opencode_error_message(stdout: &str) -> Option<String> {
    for line in stdout.lines().rev() {
        let t = line.trim();
        if t.is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(t) else { continue; };
        if v.get("type").and_then(|x| x.as_str()) != Some("error") { continue; }
        // OpenCode uses both "message" and "error" depending on version;
        // try both. Each layer of nesting (state.error.message) shows
        // up in some tool-failure shapes.
        if let Some(m) = v.get("message").and_then(|x| x.as_str()) {
            if !m.trim().is_empty() { return Some(m.trim().to_string()); }
        }
        if let Some(m) = v.pointer("/error/message").and_then(|x| x.as_str()) {
            if !m.trim().is_empty() { return Some(m.trim().to_string()); }
        }
        if let Some(m) = v.pointer("/state/error/message").and_then(|x| x.as_str()) {
            if !m.trim().is_empty() { return Some(m.trim().to_string()); }
        }
    }
    None
}

/// Find the most useful single line out of stdout/stderr for surfacing
/// a probe failure to the admin. Strategy: try to parse each stream-json
/// line as `{"type":"result", "is_error":true, "result": "..."}` (the
/// shape Claude Code emits on auth failures); fall back to the last
/// non-empty line of stderr; fall back to the last non-empty line of
/// stdout. Returns None when there's nothing usable.
fn pick_error_message(stdout: &str, stderr: &str) -> Option<String> {
    // Pass 1: structured result events on stdout.
    for line in stdout.lines().rev() {
        let t = line.trim();
        if t.is_empty() { continue; }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            if v.get("type").and_then(|x| x.as_str()) == Some("result")
                && v.get("is_error").and_then(|x| x.as_bool()) == Some(true)
            {
                if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
                    if !r.trim().is_empty() {
                        return Some(r.trim().to_string());
                    }
                }
            }
        }
    }
    // Pass 2: stderr last non-empty line.
    if let Some(line) = stderr.lines().rev().find(|l| !l.trim().is_empty()) {
        return Some(line.trim().to_string());
    }
    // Pass 3: stdout last non-empty line.
    if let Some(line) = stdout.lines().rev().find(|l| !l.trim().is_empty()) {
        return Some(line.trim().to_string());
    }
    None
}

/// Tiny `which`-style helper; mirrors the one in gateway/builder.rs but
/// kept local so the handler doesn't reach into another module's
/// private surface.
fn which_on_path(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() { return Some(candidate); }
    }
    None
}

// ── Bundled-skill refresh (slice 4 follow-up #2) ─────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct RefreshBundledBody {
    /// Force overwrite even when versions match.
    #[serde(default)]
    pub force: bool,
    /// Limit to one skill id (optional).
    #[serde(default)]
    pub id:    Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RefreshBundledRow {
    pub id:      String,
    /// `extracted` | `refreshed` | `forced` | `up_to_date` | `skipped`.
    pub kind:    &'static str,
    /// Old version (when known) for refreshed/forced rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from:    Option<String>,
    /// New version for refreshed/forced rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to:      Option<String>,
    /// Reason when `kind == "skipped"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason:  Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RefreshBundledResponse {
    pub report:           Vec<RefreshBundledRow>,
    /// True if at least one skill was extracted/refreshed/forced — UI
    /// uses this to nudge the user toward a restart.
    pub restart_required: bool,
}

/// POST /api/admin/skills/refresh-bundled
pub async fn refresh_bundled_skills(
    AdminUser(admin):    AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Json(body):          Json<Option<RefreshBundledBody>>,
) -> impl IntoResponse {
    use crate::skills::bundled::{extract_or_refresh, RefreshOutcome};
    let body = body.unwrap_or_default();
    let skills_dir = skills::default_skills_dir(&data_dir.0);
    let report = match extract_or_refresh(&skills_dir, body.force) {
        Ok(r)  => r,
        Err(e) => return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error(&format!("refresh failed: {e}"))),
        ).into_response(),
    };

    let mut rows: Vec<RefreshBundledRow> = Vec::new();
    let mut restart_required = false;
    for (id, outcome) in report {
        if let Some(filter) = body.id.as_deref() {
            if id != filter { continue; }
        }
        match outcome {
            RefreshOutcome::Extracted => {
                rows.push(RefreshBundledRow {
                    id, kind: "extracted",
                    from: None, to: None, reason: None,
                });
                restart_required = true;
            }
            RefreshOutcome::Refreshed { from, to } => {
                rows.push(RefreshBundledRow {
                    id, kind: "refreshed",
                    from: Some(from), to: Some(to), reason: None,
                });
                restart_required = true;
            }
            RefreshOutcome::Forced { from, to } => {
                rows.push(RefreshBundledRow {
                    id, kind: "forced",
                    from: Some(from), to: Some(to), reason: None,
                });
                restart_required = true;
            }
            RefreshOutcome::UpToDate => {
                rows.push(RefreshBundledRow {
                    id, kind: "up_to_date",
                    from: None, to: None, reason: None,
                });
            }
            RefreshOutcome::Skipped { reason } => {
                rows.push(RefreshBundledRow {
                    id, kind: "skipped",
                    from: None, to: None, reason: Some(reason),
                });
            }
        }
    }

    info!(
        "bundled skills refresh via API: rows={} restart_required={} (admin={:?})",
        rows.len(), restart_required, admin.id,
    );
    (StatusCode::OK, Json(RefreshBundledResponse { report: rows, restart_required })).into_response()
}

// ── helpers ─────────────────────────────────────────────────────────────

fn skills_dir_for_id(data_dir: &std::path::Path, id: &str) -> PathBuf {
    skills::default_skills_dir(data_dir).join(id)
}

fn error(msg: &str) -> serde_json::Value {
    serde_json::json!({"error": msg})
}

/// Parsed contents of an uploaded `.miraskill`. Files are kept in
/// memory until the caller decides whether to write them out — keeps
/// preview side-effect-free and lets install fail validation before
/// touching disk.
struct ParsedArchive {
    manifest:    SkillManifest,
    files:       Vec<(PathBuf, Vec<u8>)>,
    total_bytes: u64,
}

/// Drain the multipart body looking for the `archive` field. Caps the
/// total size so a hostile upload can't OOM the process.
async fn read_archive_upload(mut multipart: Multipart) -> Result<Vec<u8>, String> {
    while let Some(field) = multipart.next_field().await.map_err(|e| e.to_string())? {
        let name = field.name().unwrap_or("").to_owned();
        if name != "archive" {
            continue;
        }
        let bytes = field.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "archive is {} bytes, max allowed is {}",
                bytes.len(), MAX_ARCHIVE_BYTES,
            ));
        }
        return Ok(bytes.to_vec());
    }
    Err("no `archive` field in multipart upload".into())
}

/// Decode the gzip+tar, walk its entries, validate path safety, parse
/// the manifest, return the in-memory file list.
fn parse_archive(bytes: &[u8]) -> Result<ParsedArchive, String> {
    let gz = GzDecoder::new(bytes);
    let mut tar = Archive::new(gz);

    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut top_dir: Option<String> = None;
    let mut total_bytes: u64 = 0;

    for entry_res in tar.entries().map_err(|e| format!("not a valid tar.gz archive: {e}"))? {
        if files.len() >= MAX_ENTRIES {
            return Err(format!("archive exceeds entry limit of {MAX_ENTRIES}"));
        }
        let mut entry = entry_res.map_err(|e| format!("malformed tar entry: {e}"))?;

        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err("symlinks and hard links are not allowed in skill archives".into());
        }

        let path = entry.path().map_err(|e| format!("entry has invalid path: {e}"))?
            .into_owned();

        // Reject path traversal and absolute paths.
        if path.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_))) {
            return Err(format!("entry {path:?} contains illegal path component (.. or absolute)"));
        }

        // Establish the top-level directory and require every entry to
        // live under it.
        let mut comps = path.components();
        let first = match comps.next() {
            Some(std::path::Component::Normal(s)) => s.to_string_lossy().to_string(),
            None                                  => continue, // empty path, skip
            _                                     => return Err(format!("entry {path:?} has illegal first segment")),
        };
        match top_dir.as_deref() {
            None        => top_dir = Some(first.clone()),
            Some(known) if known == first => {}
            Some(known) => return Err(format!(
                "archive must contain a single top-level directory; saw {known:?} and {first:?}",
            )),
        }

        // Skip directory entries — they're created on demand when files write.
        if entry_type.is_dir() {
            continue;
        }
        if !entry_type.is_file() {
            // Character/block devices, fifos, etc. — never legitimate.
            return Err(format!("entry {path:?} has unsupported type {:?}", entry_type));
        }

        let size = entry.header().size().map_err(|e| format!("can't read entry size: {e}"))?;
        if size > MAX_FILE_BYTES {
            return Err(format!(
                "entry {path:?} is {size} bytes, exceeds per-file cap of {MAX_FILE_BYTES}",
            ));
        }
        total_bytes = total_bytes.saturating_add(size);

        let mut buf = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut buf).map_err(|e| format!("can't read {path:?}: {e}"))?;
        files.push((path, buf));
    }

    let top_dir = top_dir.ok_or_else(|| "archive is empty".to_string())?;

    // Find skill.toml at the top level.
    let manifest_relpath = std::path::Path::new(&top_dir).join("skill.toml");
    let manifest_bytes = files.iter()
        .find(|(p, _)| p == &manifest_relpath)
        .map(|(_, b)| b.clone())
        .ok_or_else(|| format!("archive missing {top_dir}/skill.toml"))?;

    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .map_err(|e| format!("skill.toml is not valid UTF-8: {e}"))?;
    let manifest = SkillManifest::parse(manifest_text)
        .map_err(|e| format!("skill.toml: {e}"))?;
    if let Err(errs) = manifest.validate() {
        let joined = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
        return Err(format!("skill.toml validation failed: {joined}"));
    }
    if manifest.skill.id != top_dir {
        return Err(format!(
            "archive top-level directory {top_dir:?} doesn't match manifest id {:?} — \
             rename the directory or update the manifest before packaging",
            manifest.skill.id,
        ));
    }

    // Verify that every executable referenced in the manifest is actually
    // present in the archive.
    for rel in manifest.executable_paths() {
        let abs = std::path::Path::new(&top_dir).join(rel);
        if !files.iter().any(|(p, _)| p == &abs) {
            return Err(format!("manifest references executable {rel:?} but it's not in the archive"));
        }
    }

    Ok(ParsedArchive { manifest, files, total_bytes })
}

/// Write the parsed archive to disk atomically: extract into a sibling
/// `.tmp` directory, then `rename` it into place. If the destination
/// already exists we move it aside first and only delete after the new
/// directory is in place — so a crash mid-rename leaves the user with
/// either the old install or the new one, never an empty hole.
fn write_skill_atomic(dest: &std::path::Path, parsed: &ParsedArchive) -> std::io::Result<()> {
    let parent = dest.parent().ok_or_else(|| std::io::Error::new(
        std::io::ErrorKind::InvalidInput, "skills dir has no parent",
    ))?;
    std::fs::create_dir_all(parent)?;

    let staging = TempDir::new_in(parent)?;
    let id = parsed.manifest.skill.id.as_str();
    let stage_root = staging.path().join(id);
    std::fs::create_dir_all(&stage_root)?;

    for (rel, bytes) in &parsed.files {
        // strip the top-level dir, since stage_root already plays that role
        let rel_under_top: PathBuf = rel.components().skip(1).collect();
        let abs = stage_root.join(&rel_under_top);
        if let Some(p) = abs.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&abs, bytes)?;
    }

    // Move existing install aside.
    let backup = if dest.exists() {
        let b = parent.join(format!("{id}.replaced-{}", chrono::Utc::now().timestamp_millis()));
        std::fs::rename(dest, &b)?;
        Some(b)
    } else {
        None
    };

    if let Err(e) = std::fs::rename(&stage_root, dest) {
        // Restore the backup if we have one.
        if let Some(b) = backup {
            let _ = std::fs::rename(&b, dest);
        }
        return Err(e);
    }

    if let Some(b) = backup {
        let _ = std::fs::remove_dir_all(&b);
    }
    // staging TempDir auto-cleans on drop.
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Slice A7 — trust-store management
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TrustStoreResponse {
    pub trust_store_path: String,
    pub entries:          Vec<TrustEntryDto>,
}

// We deliberately *don't* surface the raw public key bytes here —
// the fingerprint is what admins compare against published values.
// Saves API surface and keeps the response copy/pasteable.
#[derive(Debug, Serialize)]
pub struct TrustEntryDto {
    pub fingerprint: String,
    pub label:       String,
    pub added_at:    i64,
}

impl From<&TrustEntry> for TrustEntryDto {
    fn from(e: &TrustEntry) -> Self {
        Self { fingerprint: e.fingerprint.clone(), label: e.label.clone(), added_at: e.added_at }
    }
}

#[derive(Debug, Deserialize)]
pub struct AddTrustEntryRequest {
    pub label:      String,
    /// Base64 (standard, padded or unpadded) of the 32-byte ed25519
    /// public key. The fingerprint is derived; admins don't supply it.
    pub public_key: String,
}

/// GET /api/skills/trust-store — admin only. Lists all trusted publisher
/// keys MIRA accepts when verifying Skill manifests.
pub async fn list_trust_store(
    AdminUser(_admin):   AdminUser,
    Extension(data_dir): Extension<DataDir>,
) -> Json<TrustStoreResponse> {
    let path = TrustStore::default_path(&skills::default_skills_dir(&data_dir.0));
    let store = load_trust_store(&data_dir.0);
    Json(TrustStoreResponse {
        trust_store_path: path.display().to_string(),
        entries:          store.iter().map(TrustEntryDto::from).collect(),
    })
}

/// POST /api/skills/trust-store — admin only. Adds a new trusted
/// publisher key. Body: `{label, public_key}`. Returns the derived
/// fingerprint so admins can verify it against an out-of-band source
/// (publisher's website, GPG-signed README, etc).
pub async fn add_trust_entry(
    AdminUser(admin):    AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Json(req):           Json<AddTrustEntryRequest>,
) -> impl IntoResponse {
    let path = TrustStore::default_path(&skills::default_skills_dir(&data_dir.0));
    let mut store = match TrustStore::load(&path) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR,
                          Json(error(&format!("trust store: {e}")))).into_response(),
    };
    let entry = match store.add(req.label, &req.public_key) {
        Ok(e) => e,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(error(&e.to_string()))).into_response(),
    };
    if let Err(e) = store.save_self() {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(error(&format!("save trust store: {e}")))).into_response();
    }
    info!("Trust-store entry added: {} ({}) by admin={}",
        entry.label, entry.fingerprint, admin.id);
    (StatusCode::OK, Json(TrustEntryDto::from(&entry))).into_response()
}

/// DELETE /api/skills/trust-store/{fingerprint} — admin only. Removes a
/// trusted publisher key. Existing Skills signed by this publisher
/// will go unverified on the next /api/skills scan.
pub async fn remove_trust_entry(
    AdminUser(admin):    AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Path(fingerprint):   Path<String>,
) -> impl IntoResponse {
    let path = TrustStore::default_path(&skills::default_skills_dir(&data_dir.0));
    let mut store = match TrustStore::load(&path) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR,
                          Json(error(&format!("trust store: {e}")))).into_response(),
    };
    if !store.remove(&fingerprint) {
        return (StatusCode::NOT_FOUND,
                Json(error(&format!("fingerprint {fingerprint:?} not found in trust store")))).into_response();
    }
    if let Err(e) = store.save_self() {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(error(&format!("save trust store: {e}")))).into_response();
    }
    info!("Trust-store entry removed: {} by admin={}", fingerprint, admin.id);
    (StatusCode::OK, Json(serde_json::json!({"removed": true, "fingerprint": fingerprint}))).into_response()
}
