// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin-package admin endpoints.
//!
//! **preview/verify** an uploaded `.mirapkg` — parse the manifest and
//! classify its trust level. No install yet (that's). Publisher trust
//! reuses the **skills trust store** (Settings → Skills → Trust store), so
//! admins manage publisher keys in one place.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Multipart, Path};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use crate::events::{Event, EventBus};

use crate::auth::middleware::AdminUser;
use crate::channel_accounts::{ChannelAccountStore, UpdateChannelAccount};
use crate::mcp::{McpServerRegistry, McpServerStore};
use crate::packages::engine::{self, ProvisionSession, SessionStatus};
use crate::packages::host::LiveHost;
use crate::packages::install::{install_package as do_install, uninstall_package as do_uninstall};
use crate::packages::session_store::ProvisionSessionStore;
use crate::packages::store::{LedgerEntry, NewInstall};
use crate::packages::{self, Capabilities, ComponentKind, PackageManifest, PackageStore, Runtime, TrustLevel};
use crate::server::handlers::channel_accounts::ChannelManagerExt;
use crate::server::handlers::onboarding::DataDir;
use crate::skills::secrets::{default_paths as secret_paths, Scope, SecretsStore};
use crate::skills::{self, trust::TrustStore};
use crate::web::LiveConfig;

const MAX_BUNDLE_BYTES: usize = 10 * 1024 * 1024; // 10 MB compressed

// Rebuild the whole app-tool surface from the currently-active installed apps and
// hot-swap it into the shared tool registry (reached via the MCP registry, which
// already holds it). Called after any install/uninstall/enable/disable so an app's
// `app__<id>__<tool>` tools go live/dead with no restart — the app analog of
// `McpServerRegistry::reload`.
fn reload_app_tools(registry: &McpServerRegistry, pkg_store: &PackageStore) {
    if let Some(tools) = registry.tool_registry() {
        // Rebuild with the registry's app deps (SSRF-guarded HTTP + vault +
        // payload dir) so hot-reloaded http/subprocess handlers keep working
        // without a restart.
        let pkgs_dir = tools.app_packages_dir().unwrap_or_else(|| std::path::PathBuf::from("packages"));
        tools.set_app_tools(packages::build_app_tools(
            pkg_store, &pkgs_dir, tools.app_http(), tools.app_secrets(),
        ));
        // FDI-2: reconcile app service containers to the new active set (start a
        // newly-enabled app's backend, reap a disabled/uninstalled one). Blocking
        // + potentially slow (image pull) → off the request thread.
        if let Some(auth_db) = pkgs_dir.parent().map(|p| p.join("auth.db")) {
            tokio::task::spawn_blocking(move || {
                packages::apps::reconcile_app_containers_at(&auth_db);
            });
        }
    }
}

#[derive(Serialize)]
pub struct ComponentSummary {
    #[serde(rename = "type")]
    pub kind: ComponentKind,
    pub runtime: Runtime,
    pub capabilities: Capabilities,
    // Component spec (admin-visible) — drives the install config form.
    pub spec: serde_json::Value,
    // The install form (cpp_provider) — typed fields the admin fills before
    // the guided wizard runs. Empty for components that need no guided config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_schema: Vec<crate::packages::wizard::ConfigField>,
}

#[derive(Serialize)]
pub struct PackageSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub publisher: Option<String>,
    pub components: Vec<ComponentSummary>,
}

impl PackageSummary {
    fn from_manifest(m: &PackageManifest) -> Self {
        Self {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version.to_string(),
            description: m.description.clone(),
            publisher: m.publisher.clone(),
            components: m
                .components
                .iter()
                .map(|c| ComponentSummary {
                    kind: c.kind,
                    runtime: c.runtime,
                    capabilities: c.capabilities.clone(),
                    spec: c.spec.clone(),
                    config_schema: c.config_schema.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
pub struct PreviewResponse {
    pub manifest: PackageSummary,
    pub trust: TrustLevel,
    pub total_bytes: u64,
    // If a package with this id is already installed, its current version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    // The reviewable update plan when this bundle is a valid newer version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<crate::packages::UpdatePlan>,
    // Why this can't be applied as an update (downgrade, needs newer MIRA, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_blocked: Option<String>,
}

// The running MIRA version, for `min_mira_version` checks.
fn mira_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| semver::Version::new(0, 0, 0))
}

fn err(msg: impl Into<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "error": msg.into() }))
}

fn bad(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, err(msg)).into_response()
}
fn ise(msg: impl Into<String>) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, err(msg)).into_response()
}

// Drain the multipart body for the `bundle` field, capping size so a hostile
// upload can't OOM the process.
async fn read_bundle_upload(mut mp: Multipart) -> Result<Vec<u8>, String> {
    while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
        if field.name().unwrap_or("") != "bundle" {
            continue;
        }
        let bytes = field.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > MAX_BUNDLE_BYTES {
            return Err(format!("bundle is {} bytes, max is {MAX_BUNDLE_BYTES}", bytes.len()));
        }
        return Ok(bytes.to_vec());
    }
    Err("no `bundle` field in multipart upload".into())
}

// `POST /api/admin/packages/preview` — admin only. Parse + verify an uploaded
// `.mirapkg`; return its manifest summary + trust level. Side-effect-free.
pub async fn preview_package(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    mp: Multipart,
) -> impl IntoResponse {
    let bytes = match read_bundle_upload(mp).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, err(e)).into_response(),
    };
    let parsed = match packages::parse_bundle(&bytes) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, err(e)).into_response(),
    };

    // Reuse the skills trust store so publisher trust lives in one place.
    let trust_path = TrustStore::default_path(&skills::default_skills_dir(&data_dir.0));
    let store = TrustStore::load(&trust_path).unwrap_or_else(|_| TrustStore::empty());
    let trust = packages::verify_package(&parsed.manifest, &store);

    // If this id is already installed, surface whether it's an update + the diff.
    let (mut installed_version, mut update, mut update_blocked) = (None, None, None);
    if let Ok(pkg_store) = PackageStore::open(&data_dir.0.join("auth.db")) {
        if let Ok(Some(installed)) = pkg_store.get(&parsed.manifest.id) {
            installed_version = Some(installed.version.clone());
            match packages::policy_check(&installed, &parsed.manifest, &trust, &mira_version()) {
                Ok(()) => update = Some(packages::plan_update(&installed, &parsed.manifest, &trust)),
                Err(block) => update_blocked = Some(block.to_string()),
            }
        }
    }

    (
        StatusCode::OK,
        Json(PreviewResponse {
            manifest: PackageSummary::from_manifest(&parsed.manifest),
            trust,
            total_bytes: parsed.total_bytes,
            installed_version,
            update,
            update_blocked,
        }),
    )
        .into_response()
}

// `POST /api/admin/packages/install` — admin only. Multipart: a `bundle`
// (.mirapkg) field and an optional `config` field (JSON object of install-time
// values, e.g. secrets). Parses + verifies, provisions each component, records
// the ledger, and hot-reloads the MCP registry.  installs `mcp_server`
// components.
pub async fn install_package(
    AdminUser(admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
    mut mp: Multipart,
) -> Response {
    let mut bundle: Option<Vec<u8>> = None;
    let mut config: HashMap<String, String> = HashMap::new();
    let mut allow_untrusted = false;
    loop {
        match mp.next_field().await {
            Ok(Some(field)) => match field.name().unwrap_or("") {
                "allow_untrusted" => {
                    allow_untrusted = field.text().await.map(|t| t == "true").unwrap_or(false);
                }
                "bundle" => {
                    let bytes = match field.bytes().await {
                        Ok(b) => b,
                        Err(e) => return bad(e.to_string()),
                    };
                    if bytes.len() > MAX_BUNDLE_BYTES {
                        return bad(format!("bundle is {} bytes, max is {MAX_BUNDLE_BYTES}", bytes.len()));
                    }
                    bundle = Some(bytes.to_vec());
                }
                "config" => {
                    let txt = match field.text().await {
                        Ok(t) => t,
                        Err(e) => return bad(e.to_string()),
                    };
                    if !txt.trim().is_empty() {
                        config = match serde_json::from_str(&txt) {
                            Ok(c) => c,
                            Err(e) => return bad(format!("config must be a JSON object of strings: {e}")),
                        };
                    }
                }
                _ => {}
            },
            Ok(None) => break,
            Err(e) => return bad(e.to_string()),
        }
    }

    let Some(bytes) = bundle else {
        return bad("no `bundle` field in multipart upload");
    };

    let parsed = match packages::parse_bundle(&bytes) {
        Ok(p) => p,
        Err(e) => return bad(e),
    };
    let trust_path = TrustStore::default_path(&skills::default_skills_dir(&data_dir.0));
    let trust_store = TrustStore::load(&trust_path).unwrap_or_else(|_| TrustStore::empty());
    let trust = packages::verify_package(&parsed.manifest, &trust_store);

    // Capability × trust gate — refuse invalid sigs; require explicit
    // acknowledgement for unverified packages that request sensitive caps.
    if let Err(e) = packages::gate_install(&trust, &parsed.manifest, allow_untrusted) {
        return bad(e);
    }

    let auth_db = data_dir.0.join("auth.db");
    let mcp_store = match McpServerStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let pkg_store = match PackageStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };

    let packages_dir = data_dir.0.join("packages");
    let outcome = match do_install(&parsed, &trust, &admin.id, &config, &packages_dir, &mcp_store, &pkg_store) {
        Ok(o) => o,
        Err(e) => return bad(e),
    };
    // Hot-reload so the new server's tools come live without a restart.
    registry.reload().await;
    // Same for app tools (app__<id>__<tool>) — no restart.
    reload_app_tools(&registry, &pkg_store);

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "installed":   true,
            "id":          outcome.package.id,
            "name":        outcome.package.name,
            "version":     outcome.package.version,
            "trust":       trust,
            "mcp_servers": outcome.mcp_server_ids,
            "apps":        outcome.app_ids,
            "warnings":    outcome.warnings,
        })),
    )
        .into_response()
}

// `GET /api/admin/packages` — admin only. List installed packages.
pub async fn list_installed(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
) -> Response {
    let pkg_store = match PackageStore::open(&data_dir.0.join("auth.db")) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    match pkg_store.list() {
        Ok(list) => (StatusCode::OK, Json(list)).into_response(),
        Err(e) => ise(e.to_string()),
    }
}

// `DELETE /api/admin/packages/{id}` — admin only. Reverse the ledger and drop
// the record, then hot-reload the registry.
pub async fn uninstall_package(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
    Path(id): Path<String>,
) -> Response {
    let auth_db = data_dir.0.join("auth.db");
    let mcp_store = match McpServerStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let pkg_store = match PackageStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let channel_store = match ChannelAccountStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let secrets = match open_secrets(&data_dir.0) {
        Ok(s) => s,
        Err(e) => return ise(e),
    };
    match do_uninstall(&id, &mcp_store, &channel_store, &secrets, &pkg_store) {
        Ok(removed) => {
            // Purge any config secrets set post-install (admin-set app config
            // secrets aren't ledgered, so the ledger reversal won't catch them).
            let _ = secrets.purge_skill(&id);
            registry.reload().await;
            reload_app_tools(&registry, &pkg_store);
            (
                StatusCode::OK,
                Json(serde_json::json!({ "uninstalled": true, "id": id, "removed_mcp_servers": removed })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, err(e)).into_response(),
    }
}

// `POST /api/admin/packages/{id}/disable` — turn a package off without removing
// it (its record + provisioned state survive). Disables the channel account(s)
// and stops the managed service(s); reversible via `/enable`.
pub async fn disable_package(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
    Path(id): Path<String>,
) -> Response {
    set_package_enabled(&data_dir.0, &id, false, &registry).await
}

// `POST /api/admin/packages/{id}/enable` — re-activate a disabled package.
pub async fn enable_package(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
    Path(id): Path<String>,
) -> Response {
    set_package_enabled(&data_dir.0, &id, true, &registry).await
}

async fn set_package_enabled(
    data_dir: &std::path::Path,
    id: &str,
    on: bool,
    registry: &McpServerRegistry,
) -> Response {
    let auth_db = data_dir.join("auth.db");
    let pkg_store = match PackageStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let pkg = match pkg_store.get(id) {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, err(format!("package {id} is not installed"))).into_response(),
        Err(e) => return ise(e.to_string()),
    };
    let channel_store = match ChannelAccountStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let mut warnings: Vec<String> = Vec::new();
    for entry in &pkg.ledger {
        match entry {
            LedgerEntry::ChannelAccount { id } => {
                // The inbound webhook resolves `enabled` from the store live, so
                // toggling it takes effect with no restart.
                if let Err(e) = channel_store.update(
                    id,
                    UpdateChannelAccount { enabled: Some(on), ..Default::default() },
                ) {
                    warnings.push(format!("channel account {id}: {e}"));
                }
            }
            LedgerEntry::Service { unit } => {
                if let Err(e) = packages::service::set_running(unit, on) {
                    warnings.push(format!("service {unit}: {e}"));
                }
            }
            _ => {}
        }
    }
    let state = if on { "active" } else { "disabled" };
    if let Err(e) = pkg_store.set_state(id, state) {
        return ise(e.to_string());
    }
    // An app's tools follow its enabled state: build_app_tools only includes
    // active apps, so a reload drops (disable) or restores (enable) them live.
    reload_app_tools(registry, &pkg_store);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "id": id, "state": state, "warnings": warnings })),
    )
        .into_response()
}

// ════════════════════════════════════════════════════════════════════════════
// Apps framework — event emit + UI serving (Phase 2)
// ════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
pub struct EmitAppEventBody {
    // The event name the app declares (must be in its permissions.emit_events).
    pub event: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

// `POST /api/admin/apps/{id}/emit` — an installed app (via its sandboxed UI, which
// postMessages the trusted SPA host, which calls this) emits a **declared** event
// onto the shared bus. The name must be allowlisted in the app's
// `permissions.emit_events`; the declared `domain`/`severity` are stamped so
// MIRA's interaction layer and the Guardian's monitor each route it correctly.
pub async fn emit_app_event(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(bus): Extension<Arc<EventBus>>,
    Path(id): Path<String>,
    Json(body): Json<EmitAppEventBody>,
) -> Response {
    let pkg_store = match PackageStore::open(&data_dir.0.join("auth.db")) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let ev = match packages::resolve_emit_event(&pkg_store, &id, &body.event) {
        Ok(e) => e,
        Err(e) => return bad(e),
    };
    bus.emit(Event::new_app(
        ev.name.clone(),
        None, // system-wide app event in slice 1 (no per-user scoping yet)
        ev.domain.clone(),
        ev.severity.clone(),
        id.clone(), // entity = the emitting app
        body.payload,
    ));
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "emitted": true, "event": ev.name, "domain": ev.domain, "severity": ev.severity,
        })),
    )
        .into_response()
}

// `GET /api/admin/apps/{id}/config` — the app's config schema, its current
// **non-secret** values, and which secret keys are set. Secret *values* are
// never returned (mirrors the skill-secrets endpoints). Admin-only.
pub async fn get_app_config(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Path(id): Path<String>,
) -> Response {
    let auth_db = data_dir.0.join("auth.db");
    let pkg_store = match PackageStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let pkg = match pkg_store.get(&id) {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, err("app not installed")).into_response(),
        Err(e) => return ise(e.to_string()),
    };
    let schema = packages::app_config_schema(&pkg_store, &id);
    // Which secret keys actually have a stored value (metadata only).
    let secrets_set: Vec<String> = match open_secrets(&data_dir.0) {
        Ok(v) => v.list(Scope::System, "", &id).map(|es| es.into_iter().map(|e| e.key).collect()).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": id,
            "config_schema": schema,
            "values": pkg.config,      // non-secret values only
            "secrets_set": secrets_set,
        })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub struct PutAppConfigBody {
    /// Key → value for every field the admin is setting. Keys declared `secret`
    /// in the app's `config_schema` are routed to the vault (value must be a
    /// string); the rest are stored in the package's non-secret `config`.
    pub config: serde_json::Map<String, serde_json::Value>,
}

// `PUT /api/admin/apps/{id}/config` — set app config. Every key must be a
// declared `config_schema` key (declared-keys gate, mirroring the MCP installer).
// Declared-`secret` keys go to the vault; the rest to `pkg.config`. Admin-only.
pub async fn put_app_config(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
    Path(id): Path<String>,
    Json(body): Json<PutAppConfigBody>,
) -> Response {
    let auth_db = data_dir.0.join("auth.db");
    let pkg_store = match PackageStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let pkg = match pkg_store.get(&id) {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, err("app not installed")).into_response(),
        Err(e) => return ise(e.to_string()),
    };

    let schema = packages::app_config_schema(&pkg_store, &id);
    if schema.is_empty() {
        return bad("this app declares no config_schema — nothing to configure".to_string());
    }
    let declared: std::collections::HashSet<&str> = schema.iter().map(|f| f.key.as_str()).collect();
    let secret_keys = packages::app_secret_keys(&pkg_store, &id);

    // Declared-keys gate: reject any undeclared key up front.
    for k in body.config.keys() {
        if !declared.contains(k.as_str()) {
            return bad(format!("unknown config key {k:?} — not in this app's config_schema"));
        }
    }

    // Split: secrets → vault (string values only); non-secrets → pkg.config.
    let mut merged = pkg.config.as_object().cloned().unwrap_or_default();
    let mut vault: Option<SecretsStore> = None;
    for (k, v) in &body.config {
        if secret_keys.contains(k) {
            let Some(s) = v.as_str() else {
                return bad(format!("secret config key {k:?} must be a string value"));
            };
            if vault.is_none() {
                vault = Some(match open_secrets(&data_dir.0) { Ok(v) => v, Err(e) => return ise(e) });
            }
            if let Err(e) = vault.as_ref().unwrap().set(Scope::System, "", &id, k, s) {
                return ise(format!("store secret {k:?}: {e}"));
            }
        } else {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Err(e) = pkg_store.set_config(&id, &serde_json::Value::Object(merged)) {
        return ise(e.to_string());
    }
    // Hot-reload the app's tools so http/subprocess handlers pick up the new
    // config immediately (they capture non-secret config at build time).
    reload_app_tools(&registry, &pkg_store);
    (StatusCode::OK, Json(serde_json::json!({ "ok": true, "id": id }))).into_response()
}

// `GET /api/admin/apps/{id}/ui/` — serve the app's declared UI entry file.
pub async fn app_ui_root(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Path(id): Path<String>,
) -> Response {
    serve_app_ui(&data_dir.0, &id, "").await
}

// `GET /api/admin/apps/{id}/ui/{*path}` — serve a file from the app's UI dir.
pub async fn app_ui_path(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Path((id, path)): Path<(String, String)>,
) -> Response {
    serve_app_ui(&data_dir.0, &id, &path).await
}

// Serve a static file from an installed app's UI directory. The SPA embeds this
// in a `sandbox="allow-scripts"` iframe (opaque origin → can't read MIRA's Bearer
// token), so the app's only path back to MIRA is a postMessage the SPA brokers.
// Path traversal is blocked by canonicalising and confirming the target sits
// under the app's install dir. `connect-src 'none'` is defence-in-depth: the app
// must not make its own network calls.
async fn serve_app_ui(data_dir: &std::path::Path, id: &str, req_path: &str) -> Response {
    use std::path::{Path as FsPath, PathBuf};

    let pkg_store = match PackageStore::open(&data_dir.join("auth.db")) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let Some(entry) = packages::app_ui_entry(&pkg_store, id) else {
        return (StatusCode::NOT_FOUND, "app not found, disabled, or has no UI").into_response();
    };

    let install_dir = data_dir.join("packages").join(id);
    let entry_rel = PathBuf::from(&entry);
    let ui_base = install_dir.join(entry_rel.parent().unwrap_or(FsPath::new("")));
    let rel = req_path.trim_start_matches('/');
    let target = if rel.is_empty() {
        install_dir.join(&entry_rel) // the entry file itself
    } else {
        ui_base.join(rel)
    };

    // Traversal safety: both the base and the resolved target must canonicalise,
    // and the target must sit under the app's install dir.
    let Ok(base) = install_dir.canonicalize() else {
        return (StatusCode::NOT_FOUND, "app files not found").into_response();
    };
    let Ok(full) = target.canonicalize() else {
        return (StatusCode::NOT_FOUND, "file not found").into_response();
    };
    if !full.starts_with(&base) {
        return (StatusCode::FORBIDDEN, "path outside app").into_response();
    }
    let bytes = match tokio::fs::read(&full).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "file not found").into_response(),
    };

    let mut resp = Response::new(Body::from(bytes));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(crate::server::web_apps::content_type_for(&full)));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("default-src 'self' 'unsafe-inline' data: blob:; connect-src 'none'"),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

// ════════════════════════════════════════════════════════════════════════════
// cpp_provider install wizard
//
// A `cpp_provider` install is a guided, resumable wizard rather than the
// one-shot mcp_server flow: MIRA mints the secrets + creates the channel
// account, then pauses on human steps (run an `occ` command, paste an
// app-password). These endpoints drive the engine (src/packages/engine.rs) and
// persist the session so it resumes across reloads. v1 is connection-only —
// the admin runs the provider; mira.write_service is deferred.
// ════════════════════════════════════════════════════════════════════════════

// Open the encrypted secret vault at the same path MIRA uses at startup.
fn open_secrets(data_dir: &std::path::Path) -> Result<SecretsStore, String> {
    let (db, key) = secret_paths(data_dir);
    SecretsStore::open(&db, &key).map_err(|e| e.to_string())
}

// MIRA's externally-reachable base URL for `${mira.base_url}`. No global config
// field yet (v1) — derive from the server bind, falling back to localhost.
async fn base_url_from(live_cfg: &LiveConfig) -> String {
    let cfg = live_cfg.get().await;
    let host = match cfg.server.host.as_str() {
        "" | "0.0.0.0" | "::" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{}", cfg.server.port)
}

// The wizard state returned to the UI after every begin/step call.
fn session_state(s: &ProvisionSession) -> serde_json::Value {
    serde_json::json!({
        "package_id": s.package_id,
        "name": s.name,
        "version": s.version,
        "trust": s.trust,
        "status": s.status,
        "steps": s.steps,
        "awaiting": s.awaiting().map(|st| st.id.clone()),
    })
}

// The parsed begin/update multipart.
struct WizardUpload {
    bundle: Vec<u8>,
    config: BTreeMap<String, serde_json::Value>,
    allow_untrusted: bool,
    // Update only: the admin re-approved a widened capability grant.
    capability_ack: bool,
    // Update only: the admin re-approved a changed signing key.
    trust_ack: bool,
}

// Parse the begin/update multipart: a `bundle` (.mirapkg), an optional `config`
// (JSON object of admin `input` answers), `allow_untrusted`, and — for an
// update — `capability_ack` / `trust_ack`.
async fn read_wizard_upload(mut mp: Multipart) -> Result<WizardUpload, String> {
    let mut bundle: Option<Vec<u8>> = None;
    let mut config: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut allow_untrusted = false;
    let mut capability_ack = false;
    let mut trust_ack = false;
    let truthy = |t: String| t == "true";
    loop {
        match mp.next_field().await {
            Ok(Some(field)) => match field.name().unwrap_or("") {
                "allow_untrusted" => allow_untrusted = field.text().await.map(truthy).unwrap_or(false),
                "capability_ack" => capability_ack = field.text().await.map(truthy).unwrap_or(false),
                "trust_ack" => trust_ack = field.text().await.map(truthy).unwrap_or(false),
                "bundle" => {
                    let bytes = field.bytes().await.map_err(|e| e.to_string())?;
                    if bytes.len() > MAX_BUNDLE_BYTES {
                        return Err(format!(
                            "bundle is {} bytes, max is {MAX_BUNDLE_BYTES}",
                            bytes.len()
                        ));
                    }
                    bundle = Some(bytes.to_vec());
                }
                "config" => {
                    let txt = field.text().await.map_err(|e| e.to_string())?;
                    if !txt.trim().is_empty() {
                        let map: serde_json::Map<String, serde_json::Value> =
                            serde_json::from_str(&txt)
                                .map_err(|e| format!("config must be a JSON object: {e}"))?;
                        config = map.into_iter().collect();
                    }
                }
                _ => {}
            },
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    let bundle = bundle.ok_or("no `bundle` field in multipart upload")?;
    Ok(WizardUpload { bundle, config, allow_untrusted, capability_ack, trust_ack })
}

// Find the package's single `cpp_provider` component (the install target).
fn cpp_component(m: &PackageManifest) -> Result<&packages::Component, String> {
    let mut it = m.components.iter().filter(|c| c.kind == ComponentKind::CppProvider);
    let first = it.next().ok_or("package has no cpp_provider component")?;
    if it.next().is_some() {
        return Err("multiple cpp_provider components are not supported in v1".into());
    }
    Ok(first)
}

// Write the package record and clear the session row. The created `external`
// channel account is live immediately (the inbound webhook resolves it from
// the store), so there's nothing to "start" here. `_mgr` stays threaded for the
// future same-host (`write_service`) tier, which will need lifecycle control.
// The session's resolved config minus secret-flagged keys (secrets live in the
// vault) — persisted so an update seeds from it without re-prompting.
fn non_secret_config(session: &ProvisionSession) -> serde_json::Value {
    let secret_keys: std::collections::BTreeSet<String> =
        serde_json::from_value::<PackageManifest>(session.manifest.clone())
            .ok()
            .and_then(|m| {
                m.components
                    .iter()
                    .find(|c| c.kind == ComponentKind::CppProvider)
                    .map(|c| c.config_schema.iter().filter(|f| f.secret).map(|f| f.key.clone()).collect())
            })
            .unwrap_or_default();
    let map: serde_json::Map<String, serde_json::Value> = session
        .config
        .iter()
        .filter(|(k, _)| !secret_keys.contains(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    serde_json::Value::Object(map)
}

async fn finalize(
    session: &ProvisionSession,
    pkg_store: &PackageStore,
    session_store: &ProvisionSessionStore,
    _mgr: &ChannelManagerExt,
) -> Result<Vec<String>, String> {
    // On an UPDATE there's already a record: keep its provisioned ledger (the
    // account, payload dir, secrets from the first install) and merge in
    // whatever this run added; likewise carry forward the prior non-secret
    // config so values not re-collected this run survive. A fresh install has
    // no prior record → ledger/config are just this session's.
    let prior = pkg_store.get(&session.package_id).ok().flatten();
    let mut ledger = prior.as_ref().map(|p| p.ledger.clone()).unwrap_or_default();
    for entry in &session.ledger {
        if !ledger.contains(entry) {
            ledger.push(entry.clone());
        }
    }
    let mut config = prior
        .as_ref()
        .and_then(|p| p.config.as_object().cloned())
        .unwrap_or_default();
    if let serde_json::Value::Object(this) = non_secret_config(session) {
        config.extend(this);
    }

    pkg_store
        .upsert(NewInstall {
            id: session.package_id.clone(),
            version: session.version.clone(),
            name: session.name.clone(),
            trust: session.trust.clone(),
            installed_by: session.admin_id.clone(),
            ledger,
            manifest: session.manifest.clone(),
            config: serde_json::Value::Object(config),
        })
        .map_err(|e| format!("record install: {e}"))?;
    let _ = session_store.delete(&session.package_id);

    // A cpp_provider's channel account is an `external` (webhook-driven) one:
    // it goes live the moment its row exists, because the inbound webhook
    // resolves accounts that aren't in the startup snapshot straight from the
    // store — no daemon to start, no restart needed.
    Ok(Vec::new())
}

#[derive(Deserialize)]
pub struct StepSubmit {
    pub step_id: String,
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
}

// `POST /api/admin/packages/cpp/install` — begin a guided cpp_provider install.
pub async fn cpp_install_begin(
    AdminUser(admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(mgr): Extension<ChannelManagerExt>,
    mp: Multipart,
) -> Response {
    let up = match read_wizard_upload(mp).await {
        Ok(t) => t,
        Err(e) => return bad(e),
    };
    let (bytes, admin_input, allow_untrusted) = (up.bundle, up.config, up.allow_untrusted);
    let parsed = match packages::parse_bundle(&bytes) {
        Ok(p) => p,
        Err(e) => return bad(e),
    };
    if let Err(e) = parsed.manifest.validate() {
        return bad(e.to_string());
    }
    let trust_path = TrustStore::default_path(&skills::default_skills_dir(&data_dir.0));
    let trust_store = TrustStore::load(&trust_path).unwrap_or_else(|_| TrustStore::empty());
    let trust = packages::verify_package(&parsed.manifest, &trust_store);
    if let Err(e) = packages::gate_install(&trust, &parsed.manifest, allow_untrusted) {
        return bad(e);
    }

    let m = parsed.manifest.clone();
    let component = match cpp_component(&m) {
        Ok(c) => c.clone(),
        Err(e) => return bad(e),
    };

    // Open stores + extract payload (so it's on disk + ledgered for teardown).
    let auth_db = data_dir.0.join("auth.db");
    let channel_store = match ChannelAccountStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let secrets = match open_secrets(&data_dir.0) {
        Ok(s) => s,
        Err(e) => return ise(e),
    };
    let pkg_store = match PackageStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let session_store = match ProvisionSessionStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let install_dir = data_dir.0.join("packages").join(&m.id);
    // Always create the install dir, even for a payload-free package — it's the
    // working dir for `command { run_by: mira }` and `mira.write_service` steps.
    if let Err(e) = std::fs::create_dir_all(&install_dir) {
        return ise(format!("create install dir: {e}"));
    }
    if let Err(e) = packages::install::extract_payload(&parsed, &install_dir) {
        let _ = std::fs::remove_dir_all(&install_dir);
        return bad(format!("extract payload: {e}"));
    }

    let base_url = base_url_from(&live_cfg).await;
    let host = LiveHost::new(channel_store, secrets, base_url);
    let pkg_id = m.id.clone();
    let admin_id = admin.id.clone();
    let cs = component.config_schema.clone();
    let sg = component.setup_guide.clone();
    let install_dir_s = install_dir.to_string_lossy().to_string();

    // The engine is synchronous (and runs blocking probes) — keep it off the
    // async runtime.
    let begun = tokio::task::spawn_blocking(move || {
        engine::begin(&pkg_id, &admin_id, &cs, &sg, &admin_input, &install_dir_s, &host)
    })
    .await;
    let mut session = match begun {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = std::fs::remove_dir_all(&install_dir);
            return bad(e);
        }
        Err(e) => return ise(format!("install task failed: {e}")),
    };

    // Carry the metadata finalize/teardown needs, and ledger the payload dir.
    session.manifest = serde_json::to_value(&m).unwrap_or(serde_json::Value::Null);
    session.trust = trust.label().to_string();
    session.version = m.version.to_string();
    session.name = m.name.clone();
    session.ledger.insert(
        0,
        LedgerEntry::Files { dir: install_dir.to_string_lossy().to_string() },
    );

    finish_turn(session, &data_dir.0, &pkg_store, &session_store, &mgr).await
}

// `POST /api/admin/packages/cpp/update` — begin a guided update of an installed
// cpp_provider package (same id, higher version). Gated by the three diffs;
// reuses the same `.../cpp/{id}/{session,step,cancel}` endpoints to drive it.
pub async fn cpp_update_begin(
    AdminUser(admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(mgr): Extension<ChannelManagerExt>,
    mp: Multipart,
) -> Response {
    let up = match read_wizard_upload(mp).await {
        Ok(t) => t,
        Err(e) => return bad(e),
    };
    let parsed = match packages::parse_bundle(&up.bundle) {
        Ok(p) => p,
        Err(e) => return bad(e),
    };
    if let Err(e) = parsed.manifest.validate() {
        return bad(e.to_string());
    }
    let trust_path = TrustStore::default_path(&skills::default_skills_dir(&data_dir.0));
    let trust_store = TrustStore::load(&trust_path).unwrap_or_else(|_| TrustStore::empty());
    let trust = packages::verify_package(&parsed.manifest, &trust_store);
    if let Err(e) = packages::gate_install(&trust, &parsed.manifest, up.allow_untrusted) {
        return bad(e);
    }
    let m = parsed.manifest.clone();
    let component = match cpp_component(&m) {
        Ok(c) => c.clone(),
        Err(e) => return bad(e),
    };

    let auth_db = data_dir.0.join("auth.db");
    let pkg_store = match PackageStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let installed = match pkg_store.get(&m.id) {
        Ok(Some(p)) => p,
        Ok(None) => return bad(format!("{} is not installed — use install, not update", m.id)),
        Err(e) => return ise(e.to_string()),
    };

    // Policy gate (id / strictly-newer / min_mira_version / valid signature).
    if let Err(block) = packages::policy_check(&installed, &m, &trust, &mira_version()) {
        return bad(block.to_string());
    }
    let plan = packages::plan_update(&installed, &m, &trust);
    // The two re-approval gates: a 409 carries the plan so the UI can show the diff.
    if plan.needs_trust_reapproval && !up.trust_ack {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "this update is signed by a different key — re-approve the publisher",
                "needs": "trust_ack",
                "plan": plan,
            })),
        )
            .into_response();
    }
    if plan.needs_capability_reapproval && !up.capability_ack {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "this update widens the capability grant — re-approve it",
                "needs": "capability_ack",
                "plan": plan,
            })),
        )
            .into_response();
    }
    let missing: Vec<String> = plan
        .config
        .new_required_inputs
        .iter()
        .filter(|k| !up.config.contains_key(*k))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return bad(format!("this update needs new required config: {}", missing.join(", ")));
    }

    // Seed config from the prior install (renames/drops applied).
    let from_version =
        semver::Version::parse(&installed.version).unwrap_or_else(|_| semver::Version::new(0, 0, 0));
    let seed = packages::apply_migrations(&installed.config, &m, &from_version);

    let channel_store = match ChannelAccountStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let secrets = match open_secrets(&data_dir.0) {
        Ok(s) => s,
        Err(e) => return ise(e),
    };
    let session_store = match ProvisionSessionStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let install_dir = data_dir.0.join("packages").join(&m.id);
    if let Err(e) = std::fs::create_dir_all(&install_dir) {
        return ise(format!("create install dir: {e}"));
    }
    // Re-extract the new payload over the install dir (the prior Files ledger
    // entry already covers this dir; finalize merges the ledgers).
    if let Err(e) = packages::install::extract_payload(&parsed, &install_dir) {
        return bad(format!("extract payload: {e}"));
    }

    let base_url = base_url_from(&live_cfg).await;
    let host = LiveHost::new(channel_store, secrets, base_url);
    let pkg_id = m.id.clone();
    let admin_id = admin.id.clone();
    let cs = component.config_schema.clone();
    let sg = component.setup_guide.clone();
    let install_dir_s = install_dir.to_string_lossy().to_string();
    let admin_input = up.config.clone();

    let begun = tokio::task::spawn_blocking(move || {
        engine::begin_update(&pkg_id, &admin_id, &cs, &sg, &seed, &admin_input, &install_dir_s, &host)
    })
    .await;
    let mut session = match begun {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return bad(e),
        Err(e) => return ise(format!("update task failed: {e}")),
    };
    session.manifest = serde_json::to_value(&m).unwrap_or(serde_json::Value::Null);
    session.trust = trust.label().to_string();
    session.version = m.version.to_string();
    session.name = m.name.clone();

    finish_turn(session, &data_dir.0, &pkg_store, &session_store, &mgr).await
}

// `GET /api/admin/packages/cpp/{id}/session` — current wizard state.
pub async fn cpp_session(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Path(id): Path<String>,
) -> Response {
    let session_store = match ProvisionSessionStore::open(&data_dir.0.join("auth.db")) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    match session_store.get(&id) {
        Ok(Some(s)) => (StatusCode::OK, Json(session_state(&s))).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, err(format!("no in-flight install for {id}"))).into_response(),
        Err(e) => ise(e.to_string()),
    }
}

// `POST /api/admin/packages/cpp/{id}/step` — submit a human step's result.
pub async fn cpp_step(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Extension(mgr): Extension<ChannelManagerExt>,
    Path(id): Path<String>,
    Json(body): Json<StepSubmit>,
) -> Response {
    let auth_db = data_dir.0.join("auth.db");
    let session_store = match ProvisionSessionStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let session = match session_store.get(&id) {
        Ok(Some(s)) => s,
        Ok(None) => return (StatusCode::NOT_FOUND, err(format!("no in-flight install for {id}"))).into_response(),
        Err(e) => return ise(e.to_string()),
    };
    // Reconstruct the wizard grammar from the session's manifest.
    let m: PackageManifest = match serde_json::from_value(session.manifest.clone()) {
        Ok(m) => m,
        Err(e) => return ise(format!("session manifest corrupt: {e}")),
    };
    let component = match cpp_component(&m) {
        Ok(c) => c.clone(),
        Err(e) => return ise(e),
    };

    let channel_store = match ChannelAccountStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let secrets = match open_secrets(&data_dir.0) {
        Ok(s) => s,
        Err(e) => return ise(e),
    };
    let pkg_store = match PackageStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let base_url = base_url_from(&live_cfg).await;
    let host = LiveHost::new(channel_store, secrets, base_url);
    let cs = component.config_schema.clone();
    let sg = component.setup_guide.clone();
    let step_id = body.step_id.clone();
    let outputs = body.outputs.clone();

    let done = tokio::task::spawn_blocking(move || {
        let mut s = session;
        let r = engine::submit_step(&mut s, &step_id, outputs, &cs, &sg, &host);
        (s, r)
    })
    .await;
    let session = match done {
        Ok((s, Ok(()))) => s,
        Ok((_, Err(e))) => return bad(e),
        Err(e) => return ise(format!("step task failed: {e}")),
    };

    finish_turn(session, &data_dir.0, &pkg_store, &session_store, &mgr).await
}

// `POST /api/admin/packages/cpp/{id}/cancel` — abandon an in-flight install,
// reversing whatever it provisioned.
pub async fn cpp_cancel(
    AdminUser(_admin): AdminUser,
    Extension(data_dir): Extension<DataDir>,
    Extension(registry): Extension<Arc<McpServerRegistry>>,
    Path(id): Path<String>,
) -> Response {
    let auth_db = data_dir.0.join("auth.db");
    let session_store = match ProvisionSessionStore::open(&auth_db) {
        Ok(s) => s,
        Err(e) => return ise(e.to_string()),
    };
    let session = match session_store.get(&id) {
        Ok(Some(s)) => s,
        Ok(None) => return (StatusCode::NOT_FOUND, err(format!("no in-flight install for {id}"))).into_response(),
        Err(e) => return ise(e.to_string()),
    };
    let (mcp_store, channel_store, pkg_store) = match (
        McpServerStore::open(&auth_db),
        ChannelAccountStore::open(&auth_db),
        PackageStore::open(&auth_db),
    ) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        _ => return ise("open stores"),
    };
    let secrets = match open_secrets(&data_dir.0) {
        Ok(s) => s,
        Err(e) => return ise(e),
    };
    let removed = packages::reverse_ledger(&session.ledger, &id, &mcp_store, &channel_store, &secrets);
    let _ = session_store.delete(&id);
    let _ = pkg_store.delete(&id); // in case a prior finalize wrote a record
    registry.reload().await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "cancelled": true, "id": id, "removed_mcp_servers": removed })),
    )
        .into_response()
}

// Shared tail of begin/step: persist the session; on completion finalize (write
// the package record + register accounts); on a blocking failure, reverse
// whatever was provisioned so nothing dangles. Returns the wizard state.
async fn finish_turn(
    session: ProvisionSession,
    data_dir: &std::path::Path,
    pkg_store: &PackageStore,
    session_store: &ProvisionSessionStore,
    mgr: &ChannelManagerExt,
) -> Response {
    if let Err(e) = session_store.put(&session) {
        return ise(format!("persist session: {e}"));
    }
    let mut warnings: Vec<String> = Vec::new();
    match session.status {
        SessionStatus::Complete => match finalize(&session, pkg_store, session_store, mgr).await {
            Ok(w) => warnings = w,
            Err(e) => return ise(e),
        },
        // A blocking step failed mid-install: auto-reverse the ledger (created
        // account, vaulted secrets, started service, payload dir) so a failed
        // attempt leaves nothing behind — then drop the session.
        SessionStatus::Failed => {
            let auth_db = data_dir.join("auth.db");
            if let (Ok(mcp), Ok(ch), Ok(secrets)) = (
                McpServerStore::open(&auth_db),
                ChannelAccountStore::open(&auth_db),
                open_secrets(data_dir),
            ) {
                packages::reverse_ledger(&session.ledger, &session.package_id, &mcp, &ch, &secrets);
            }
            let _ = session_store.delete(&session.package_id);
        }
        SessionStatus::InProgress | SessionStatus::AwaitingInput => {}
    }
    let mut state = session_state(&session);
    if let serde_json::Value::Object(ref mut map) = state {
        map.insert("warnings".into(), serde_json::json!(warnings));
    }
    (StatusCode::OK, Json(state)).into_response()
}
