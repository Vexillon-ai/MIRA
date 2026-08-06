// SPDX-License-Identifier: AGPL-3.0-or-later

//! **App component** support (apps framework, Phase 2).
//!
//! An `app` package component (`ComponentKind::App`) is a first-class installable
//! app: it ships its own UI (a static bundle served from MIRA's host + embedded in
//! the SPA), declares **tools** MIRA can call, and declares **events** it may emit
//! onto the shared bus (`domain`/`severity`, so MIRA's interaction layer and the
//! Guardian's monitoring layer each pick out what's theirs).
//!
//! This module defines the typed `spec` shape, the in-process [`AppTool`] (the
//! app→MIRA tool contract), and [`build_app_tools`] — the app analog of the MCP
//! registry's `build_adapters`, which the packages handler hot-swaps into the
//! shared `ToolRegistry` after every install/uninstall (no restart).
//!
//! Slice 1 ships exactly one tool handler kind (`echo`) and no app-owned
//! subprocess/HTTP; richer handlers, sandboxing, and per-user scoping are later
//! slices.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::skills::secrets::{Scope, SecretsStore};
use crate::tools::http_policy::{HttpPolicy, RequestContext};
use crate::tools::{Tier, Tool, ToolArgs, ToolResult, ToolVisibility};
use crate::MiraError;

use super::manifest::{Capabilities, Component, ComponentKind, PackageManifest};
use super::store::{InstalledPackage, PackageStore};
use super::wizard::ConfigField;

/// Max bytes captured from a subprocess handler's stdout/stderr.
const SUBPROC_OUTPUT_CAP: usize = 64 * 1024;
/// Default + max wall-clock for a subprocess handler.
const SUBPROC_DEFAULT_TIMEOUT_SECS: u64 = 30;
const SUBPROC_MAX_TIMEOUT_SECS: u64 = 300;

/// Severities an app event may carry. `warn`+ are *issues* the Guardian monitors;
/// `info` is benign interaction that belongs to MIRA's automations layer.
const VALID_SEVERITIES: &[&str] = &["info", "warn", "warning", "error", "critical"];

// ── The `App` component spec (`Component.spec` when `kind = App`) ─────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSpec {
    pub ui: AppUi,
    #[serde(default)]
    pub tools: Vec<AppToolSpec>,
    #[serde(default)]
    pub events: Vec<AppEventSpec>,
    #[serde(default)]
    pub permissions: AppPermissions,
    /// Optional periodic reachability check for an app-backed service. Lets an
    /// app be a Guardian detection source without its own backend: the framework
    /// polls it and emits `emit_on_failure` on a transition to unhealthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<AppHealthCheck>,
}

/// A periodic reachability check for an app-backed service (apps framework). The
/// framework polls `url` (templated `${config.*}`, e.g. an API base + a token
/// header); a 2xx/3xx response = healthy. On a **transition to UNHEALTHY** the
/// framework emits `emit_on_failure` — which must be a declared, emit-allowed
/// *issue* event (severity warn+) — onto the shared bus, so the Guardian triages
/// it. Recovery is logged, not emitted. See `packages::apps_poll`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppHealthCheck {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,
    pub emit_on_failure: String,
}

fn default_health_interval() -> u64 { 300 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUi {
    /// Payload-relative path to the SPA-embedded HTML entry (e.g. `ui/index.html`).
    pub entry: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_args_schema")]
    pub args_schema: serde_json::Value,
    pub handler: AppHandler,
}

/// The tool implementation. A closed enum. Slice 1 shipped `echo`; Slice 2 adds
/// `http` (call a declared endpoint through the SSRF-guarded client).
/// `subprocess`/container handlers are deferred to Slice 2b (their confinement
/// story needs its own slice).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppHandler {
    /// Echo the `text` argument back to the caller.
    Echo,
    /// Make an HTTP request to a declared endpoint. `url`, `headers` values, and
    /// `body` are templates: `${config.KEY}` resolves against the app's stored
    /// config (non-secret values + vault secrets) and `${args.KEY}` against the
    /// tool-call arguments. The request runs through the shared [`HttpPolicy`],
    /// so it inherits the SSRF guard, denylist/allowlist, rate limits, size cap,
    /// and DNS-rebind pinning — an app can never reach an internal address.
    Http {
        #[serde(default = "default_http_method")]
        method: String,
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        /// Optional declarative projection applied to a JSON response before it is
        /// handed to the model: keep only selected fields and cap the row count.
        /// Turns a large list endpoint (e.g. Home Assistant `/api/states`, a ~40K
        /// token dump of every entity + all attributes) into a compact, useful
        /// result the model can actually reason over — without any code execution.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<HttpResponse>,
    },
    /// Run a command bundled in the app's package payload, one-shot. `command`
    /// is a payload-relative path (traversal-safe, must resolve inside the
    /// install dir); `args` and `stdin` are templated like `Http`. Runs under
    /// the SAME fail-closed confinement as a native plugin (read-only host,
    /// masked secrets, no/allowlisted network) via `mira pkg-exec`. Linux-only.
    Subprocess {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdin: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
}

fn default_http_method() -> String { "GET".to_string() }

/// Declarative projection of a JSON response body, applied before the result is
/// handed to the model. Purely a *reduction* — it selects existing fields and caps
/// rows; it never fetches, computes, or runs code. When the body is a JSON array
/// each element is projected; when it's an object the object itself is projected;
/// a non-JSON body is passed through unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HttpResponse {
    /// Dotted field paths to keep from each element, e.g. `entity_id`,
    /// `attributes.friendly_name`. The kept key is the path's last segment
    /// (`friendly_name`). Empty = keep the whole element unprojected.
    #[serde(default)]
    pub select: Vec<String>,
    /// Max number of array elements to keep (applied after projection). None = all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Look up a dotted path (`a.b.c`) in a JSON value, walking object keys.
fn json_path_get<'a>(v: &'a serde_json::Value, dotted: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for seg in dotted.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Project one JSON value to just the selected dotted paths (keyed by last segment).
/// An empty selector returns the value unchanged.
fn project_value(v: &serde_json::Value, select: &[String]) -> serde_json::Value {
    if select.is_empty() {
        return v.clone();
    }
    let mut obj = serde_json::Map::new();
    for path in select {
        if let Some(found) = json_path_get(v, path) {
            let key = path.rsplit('.').next().unwrap_or(path.as_str());
            obj.insert(key.to_string(), found.clone());
        }
    }
    serde_json::Value::Object(obj)
}

/// Apply an [`HttpResponse`] projection to a raw response body. Returns the compact
/// re-serialized JSON, or `None` when the body isn't JSON (caller keeps the raw
/// body). Arrays are projected element-wise then truncated to `limit`; a single
/// object is projected directly.
fn apply_response_projection(body: &str, tr: &HttpResponse) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let projected = match v {
        serde_json::Value::Array(items) => {
            let mut rows: Vec<serde_json::Value> =
                items.iter().map(|it| project_value(it, &tr.select)).collect();
            let full = rows.len();
            if let Some(n) = tr.limit {
                rows.truncate(n);
            }
            // A leading count line helps the model when we've capped the rows.
            let arr = serde_json::Value::Array(rows.clone());
            let body = serde_json::to_string(&arr).ok()?;
            if tr.limit.is_some_and(|n| full > n) {
                return Some(format!(
                    "{body}\n\n[showing {} of {} items — call with a specific id or filter for the rest]",
                    rows.len(), full));
            }
            return Some(body);
        }
        other => project_value(&other, &tr.select),
    };
    serde_json::to_string(&projected).ok()
}

/// Per-app execution context for `subprocess` handlers — the app's on-disk
/// install dir + its declared capabilities (network/filesystem), used to build
/// the confinement policy at call time. Shared across an app's tools.
pub struct AppExecCtx {
    pub install_dir:  PathBuf,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppEventSpec {
    pub name: String,
    pub domain: String,
    pub severity: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppPermissions {
    /// The allowlist of event names the app's UI may emit (enforced by the emit
    /// endpoint). Every entry must also be declared in `events`.
    #[serde(default)]
    pub emit_events: Vec<String>,
}

fn default_args_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

impl AppSpec {
    /// Parse + validate an `App` component's `spec`.
    pub fn parse(spec: &serde_json::Value) -> Result<Self, String> {
        let s: AppSpec = serde_json::from_value(spec.clone())
            .map_err(|e| format!("invalid app spec: {e}"))?;
        s.validate()?;
        Ok(s)
    }

    fn validate(&self) -> Result<(), String> {
        // UI entry must be a non-empty, payload-relative path (no absolute paths
        // or `..` escapes — the serving handler also enforces traversal safety).
        let entry = self.ui.entry.trim();
        if entry.is_empty() {
            return Err("app ui.entry is empty".into());
        }
        if entry.starts_with('/') || entry.split('/').any(|seg| seg == "..") {
            return Err(format!("app ui.entry must be a relative path without `..`: {entry:?}"));
        }
        for ev in &self.events {
            if !VALID_SEVERITIES.contains(&ev.severity.as_str()) {
                return Err(format!(
                    "app event {:?} has invalid severity {:?} (allowed: {})",
                    ev.name, ev.severity, VALID_SEVERITIES.join(", "),
                ));
            }
        }
        // Every emit-allowed name must be a declared event.
        for name in &self.permissions.emit_events {
            if !self.events.iter().any(|e| &e.name == name) {
                return Err(format!(
                    "permissions.emit_events lists {name:?}, which is not in the app's declared events"
                ));
            }
        }
        // Per-tool handler validation.
        for t in &self.tools {
            match &t.handler {
                AppHandler::Http { method, url, .. } => {
                    if url.trim().is_empty() {
                        return Err(format!("app tool {:?} has an http handler with an empty url", t.name));
                    }
                    if reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes()).is_err() {
                        return Err(format!("app tool {:?} has an invalid http method {method:?}", t.name));
                    }
                }
                AppHandler::Subprocess { command, .. } => {
                    let c = command.trim();
                    if c.is_empty() {
                        return Err(format!("app tool {:?} has a subprocess handler with an empty command", t.name));
                    }
                    if c.starts_with('/') || c.split('/').any(|s| s == "..") {
                        return Err(format!(
                            "app tool {:?} subprocess command must be a payload-relative path without `..`: {command:?}",
                            t.name));
                    }
                }
                AppHandler::Echo => {}
            }
        }
        // Health-check validation: emit_on_failure must be a declared,
        // emit-allowed *issue* event, and the url must be non-empty.
        if let Some(hc) = &self.health_check {
            if hc.url.trim().is_empty() {
                return Err("app health_check has an empty url".into());
            }
            if !self.permissions.emit_events.iter().any(|e| e == &hc.emit_on_failure) {
                return Err(format!(
                    "health_check.emit_on_failure {:?} is not in permissions.emit_events", hc.emit_on_failure));
            }
            match self.events.iter().find(|e| e.name == hc.emit_on_failure) {
                None => return Err(format!(
                    "health_check.emit_on_failure {:?} is not a declared event", hc.emit_on_failure)),
                Some(ev) if !matches!(ev.severity.as_str(), "warn" | "warning" | "error" | "critical") =>
                    return Err(format!(
                        "health_check.emit_on_failure {:?} must be an issue-severity event (warn+)", hc.emit_on_failure)),
                _ => {}
            }
        }
        Ok(())
    }
}

// ── App tool (the app→MIRA tool contract) ────────────────────────────────────

/// The tool-name segment for an app: its id with dots→hyphens so the full name
/// `app__<seg>__<tool>` is provider-safe (`[a-zA-Z0-9_-]`) and can't collide with
/// built-ins or `mcp__*`.
pub fn app_name_segment(app_id: &str) -> String {
    app_id.replace('.', "-")
}

/// A MIRA-callable tool exposed by an installed app. In-process. Carries its
/// app id (for `_app_id` policy attribution), the app's resolved non-secret
/// config + a secret-vault handle (for `${config.X}` in `http` handlers), and
/// the shared SSRF-guarded HTTP handle.
pub struct AppTool {
    full_name:   String,
    app_id:      String,
    description: String,
    schema:      serde_json::Value,
    handler:     AppHandler,
    /// Resolved **non-secret** config values (from `pkg.config`).
    config:      serde_json::Map<String, serde_json::Value>,
    /// Secret vault, keyed by app id — resolves secret `${config.X}` values.
    secrets:     Option<Arc<SecretsStore>>,
    /// SSRF-guarded HTTP for the `http` handler. `None` → the handler fails
    /// gracefully (minimal/test builds).
    http:        Option<Arc<HttpPolicy>>,
    /// On-disk install dir + capabilities for the `subprocess` handler. `None`
    /// (minimal/test builds) → the handler fails gracefully.
    exec:        Option<Arc<AppExecCtx>>,
}

impl AppTool {
    pub fn new(
        app_id:  &str,
        spec:    &AppToolSpec,
        config:  serde_json::Map<String, serde_json::Value>,
        http:    Option<Arc<HttpPolicy>>,
        secrets: Option<Arc<SecretsStore>>,
        exec:    Option<Arc<AppExecCtx>>,
    ) -> Self {
        Self {
            // `app__<segment>__<tool>` — the segment (id, dots→hyphens) scopes the
            // tool to its app; `app_id()` reads the real id back for policy.
            full_name:   format!("app__{}__{}", app_name_segment(app_id), spec.name),
            app_id:      app_id.to_string(),
            description: spec.description.clone(),
            schema:      spec.args_schema.clone(),
            handler:     spec.handler.clone(),
            config,
            secrets,
            http,
            exec,
        }
    }

    /// Resolve a `${config.KEY}`: non-secret config first, then the vault.
    fn config_value(&self, key: &str) -> Option<String> {
        app_config_value(&self.config, self.secrets.as_ref(), &self.app_id, key)
    }

    /// Substitute `${config.KEY}` / `${args.KEY}` tokens. A missing **config**
    /// value is a misconfiguration (`Err`); a missing **arg** renders empty.
    /// Unknown namespaces are rejected so a typo can't silently leak `${...}`.
    fn render(&self, tmpl: &str, args: &ToolArgs) -> Result<String, String> {
        substitute(tmpl, |token| {
            if let Some(key) = token.strip_prefix("config.") {
                self.config_value(key)
                    .ok_or_else(|| format!("app not configured: missing config value '{key}'"))
            } else if let Some(key) = token.strip_prefix("args.") {
                Ok(args.get(key).map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                }).unwrap_or_default())
            } else {
                Err(format!("unknown template token '${{{token}}}' (use config.* or args.*)"))
            }
        })
    }

    /// Run a `subprocess` handler: capability-gate, resolve the command inside
    /// the install dir, template args/stdin, then spawn one-shot under the same
    /// fail-closed confinement as a native plugin. Returns a tool-level result
    /// (never a hard error) so the model sees failures.
    async fn run_subprocess(
        &self,
        command: &str,
        cmd_args: &[String],
        stdin: Option<&str>,
        timeout_secs: Option<u64>,
        args: &ToolArgs,
    ) -> Result<ToolResult, MiraError> {
        // Confinement is Linux-only (the launcher's namespace sandbox fail-closes
        // elsewhere); refuse up front rather than run less-confined.
        if !cfg!(target_os = "linux") {
            return Ok(ToolResult::failure(
                "subprocess app tools require Linux confinement (unsupported on this platform)"));
        }
        let Some(ctx) = self.exec.as_ref() else {
            return Ok(ToolResult::failure("subprocess handler unavailable in this build"));
        };
        // Capability gate: the app must declare `subprocess`, and (if an
        // allowlist is declared) the command must be on it.
        if !ctx.capabilities.subprocess {
            return Ok(ToolResult::failure("this app did not declare the `subprocess` capability"));
        }
        if !ctx.capabilities.subprocess_allowlist.is_empty()
            && !ctx.capabilities.subprocess_allowlist.iter().any(|c| c == command)
        {
            return Ok(ToolResult::failure(format!(
                "command {command:?} is not in the app's subprocess_allowlist")));
        }
        // Resolve the command inside the install dir (traversal-safe, must exist).
        let resolved = match resolve_within(&ctx.install_dir, command) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e)),
        };
        // Template args + stdin against config/args.
        let mut targs = Vec::with_capacity(cmd_args.len());
        for a in cmd_args {
            match self.render(a, args) { Ok(r) => targs.push(r), Err(e) => return Ok(ToolResult::failure(e)) }
        }
        let tstdin = match stdin {
            Some(s) => match self.render(s, args) { Ok(r) => Some(r), Err(e) => return Ok(ToolResult::failure(e)) },
            None => None,
        };

        // Build the confinement policy (same logic as native `confine_command`).
        let config_map = self.config_string_map();
        let exe = match std::env::current_exe() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(e) => return Ok(ToolResult::failure(format!("cannot locate mira binary: {e}"))),
        };
        #[cfg(unix)]
        {
            let (spec, home) = super::install::app_subprocess_confine_spec(
                &ctx.install_dir, &ctx.capabilities, &config_map);
            let timeout = Duration::from_secs(
                timeout_secs.unwrap_or(SUBPROC_DEFAULT_TIMEOUT_SECS).clamp(1, SUBPROC_MAX_TIMEOUT_SECS));
            match super::apps_exec::run_confined(
                &exe, &resolved, &targs, tstdin.as_deref(),
                &ctx.install_dir, &home, &spec, timeout, SUBPROC_OUTPUT_CAP,
            ).await {
                Ok(r) if r.timed_out =>
                    Ok(ToolResult::failure(format!("subprocess timed out after {}s", timeout.as_secs()))),
                Ok(r) if r.exit_code == 0 => {
                    let mut out = r.stdout;
                    if r.truncated { out.push_str("\n…(output truncated)"); }
                    Ok(ToolResult::success(out))
                }
                Ok(r) => Ok(ToolResult::failure(format!(
                    "command exited {}: {}", r.exit_code,
                    if r.stderr.is_empty() { r.stdout } else { r.stderr }))),
                Err(e) => Ok(ToolResult::failure(e)),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (resolved, targs, tstdin, config_map, exe);
            Ok(ToolResult::failure("subprocess app tools are unsupported on this platform"))
        }
    }

    /// The app's non-secret config as `HashMap<String,String>` (the shape the
    /// confinement path/egress templating expects). Non-string values stringify.
    fn config_string_map(&self) -> std::collections::HashMap<String, String> {
        self.config.iter().map(|(k, v)| {
            let s = match v { serde_json::Value::String(s) => s.clone(), other => other.to_string() };
            (k.clone(), s)
        }).collect()
    }
}

/// Resolve a payload-relative `rel` path inside `base` (an app's install dir),
/// rejecting absolute paths, `..` escapes, and anything that canonicalises
/// outside `base` or isn't a regular file. Returns the absolute path string.
fn resolve_within(base: &std::path::Path, rel: &str) -> Result<String, String> {
    if rel.trim().is_empty() {
        return Err("subprocess command is empty".into());
    }
    if rel.starts_with('/') || rel.split('/').any(|s| s == "..") {
        return Err(format!("subprocess command must be a payload-relative path without `..`: {rel:?}"));
    }
    let base = base.canonicalize().map_err(|_| "app files not found".to_string())?;
    let full = base.join(rel).canonicalize()
        .map_err(|_| format!("bundled command {rel:?} not found in the app payload"))?;
    if !full.starts_with(&base) {
        return Err(format!("command {rel:?} resolves outside the app payload"));
    }
    if !full.is_file() {
        return Err(format!("bundled command {rel:?} is not a file"));
    }
    Ok(full.to_string_lossy().to_string())
}

/// Resolve `${config.KEY}` for an app: non-secret `config` map first, then the
/// secret vault (keyed by app id). Shared by the tool handlers and the health
/// poller so config/secret resolution can't drift between them.
pub fn app_config_value(
    config:  &serde_json::Map<String, serde_json::Value>,
    secrets: Option<&Arc<SecretsStore>>,
    app_id:  &str,
    key:     &str,
) -> Option<String> {
    if let Some(v) = config.get(key) {
        return Some(match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    }
    secrets.and_then(|s| s.get(Scope::System, "", app_id, key).ok().flatten())
}

/// Model-facing cap on an app `http` tool's response body. The transport layer
/// (`HttpPolicy`) already caps the raw body at ~5 MB to avoid OOM, but that is far
/// too large to hand a model: a single 158 KB Home Assistant `/api/states` dump is
/// ~40K tokens and overflows a local model's context window, which surfaces as the
/// follow-up generate failing. We clip what reaches the model to a sane size with a
/// clear notice so the model can recover by making a narrower call. Tuned to be
/// useful (many rows survive) yet safe for small local context windows (~6K tokens).
const APP_HTTP_MODEL_CAP_BYTES: usize = 24_000;

/// Clip `text` to `APP_HTTP_MODEL_CAP_BYTES` on a UTF-8 char boundary. Returns the
/// (possibly borrowed) body and `Some(original_byte_len)` when clipping occurred.
fn clip_for_model(text: &str) -> (&str, Option<usize>) {
    if text.len() <= APP_HTTP_MODEL_CAP_BYTES {
        return (text, None);
    }
    // Back off to the nearest char boundary at or below the cap so we never split a
    // multi-byte sequence (which would corrupt the tail of the JSON we do keep).
    let mut end = APP_HTTP_MODEL_CAP_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], Some(text.len()))
}

/// Substitute `${...}` tokens in `tmpl` using `resolve`, which fully handles each
/// token (returns its substitution or an `Err`). A dangling `${` is emitted
/// literally. The shared core of both the tool-arg renderer and `render_config`.
fn substitute(tmpl: &str, resolve: impl Fn(&str) -> Result<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(tmpl.len());
    let mut rest = tmpl;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str("${"); // dangling `${` — emit literally and stop
            rest = after;
            continue;
        };
        let token = after[..end].trim();
        rest = &after[end + 1..];
        let mut value = resolve(token)?;
        // Collapse a redundant slash at a template join: if the template continues
        // with `/` and the resolved value ends with `/` (e.g. a `base_url` of
        // "http://host:8123/" followed by "/api/states"), trim the value's trailing
        // slashes so we emit ".../api/states" and not "...//api/states" — many
        // servers (Home Assistant among them) 404 double-slash paths. This makes a
        // `base_url` config value work with or without a trailing slash. The
        // scheme's own "//" is never touched (guarded by the `:`-suffix check), and
        // the rule only fires at a literal `${...}/` seam, so query strings, JSON
        // bodies, and headers are unaffected.
        if rest.starts_with('/') && value.ends_with('/') {
            let trimmed = value.trim_end_matches('/');
            if !trimmed.is_empty() && !trimmed.ends_with(':') {
                value.truncate(trimmed.len());
            }
        }
        out.push_str(&value);
    }
    out.push_str(rest);
    Ok(out)
}

/// The bare hostnames an app may reach on the **private network** — its declared
/// `capabilities.network_egress`, each rendered with the app's config (so
/// `${config.base_url}` resolves to the admin-configured host) and reduced to a
/// hostname. Feeds the `RequestContext` LAN-egress allowlist so an app's http /
/// health-check requests can reach a declared LAN service (e.g. a home HA box)
/// while every other host stays behind the full SSRF guard.
pub fn app_egress_hosts(
    egress:  &[String],
    config:  &serde_json::Map<String, serde_json::Value>,
    secrets: Option<&Arc<SecretsStore>>,
    app_id:  &str,
) -> Vec<String> {
    egress.iter()
        .filter_map(|raw| render_config(raw, config, secrets, app_id).ok())
        .filter_map(|rendered| host_of(&rendered))
        .collect()
}

/// Reduce a URL-or-host string to a bare lowercase hostname.
fn host_of(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains("://") {
        url::Url::parse(s).ok().and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
    } else {
        Some(s.split(['/', ':']).next().unwrap_or(s).to_ascii_lowercase())
    }
}

/// Render a template referencing only `${config.KEY}` (the health poller has no
/// per-call args). Missing config → `Err`.
pub fn render_config(
    tmpl:    &str,
    config:  &serde_json::Map<String, serde_json::Value>,
    secrets: Option<&Arc<SecretsStore>>,
    app_id:  &str,
) -> Result<String, String> {
    substitute(tmpl, |token| {
        if let Some(key) = token.strip_prefix("config.") {
            app_config_value(config, secrets, app_id, key)
                .ok_or_else(|| format!("missing config value '{key}'"))
        } else {
            Err(format!("health_check templates support only ${{config.*}}, got '${{{token}}}'"))
        }
    })
}

/// The health-check target for an installed, **active** app: the check plus the
/// declared event's routing (name/domain/severity). `None` when the app declares
/// no `health_check`. Used by `apps_poll` to poll + emit on failure.
pub struct AppHealthTarget {
    pub check:          AppHealthCheck,
    pub event_name:     String,
    pub event_domain:   String,
    pub event_severity: String,
    /// The app's declared `capabilities.network_egress` (templated at poll time
    /// → the LAN-egress allowlist, so the check can reach a declared LAN host).
    pub egress:         Vec<String>,
}

pub fn app_health_target(pkg: &InstalledPackage) -> Option<AppHealthTarget> {
    if pkg.state != "active" {
        return None;
    }
    let comp = app_component_of(pkg)?;
    let spec = AppSpec::parse(&comp.spec).ok()?;
    let hc = spec.health_check?;
    let ev = spec.events.iter().find(|e| e.name == hc.emit_on_failure)?;
    Some(AppHealthTarget {
        event_name:     ev.name.clone(),
        event_domain:   ev.domain.clone(),
        event_severity: ev.severity.clone(),
        egress:         comp.capabilities.network_egress.clone(),
        check:          hc,
    })
}

#[async_trait]
impl Tool for AppTool {
    fn name(&self) -> &str { &self.full_name }

    fn description(&self) -> &str {
        if self.description.is_empty() { "App tool." } else { &self.description }
    }

    fn args_schema(&self) -> serde_json::Value { self.schema.clone() }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        match &self.handler {
            AppHandler::Echo => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                Ok(ToolResult::success(text))
            }
            AppHandler::Http { method, url, headers, body, response } => {
                let Some(http) = self.http.as_ref() else {
                    return Ok(ToolResult::failure("this app's HTTP handler is unavailable in this build"));
                };
                // Render templates; a template error is a tool-level failure the
                // model sees (not a hard MiraError).
                let url_r = match self.render(url, &args) { Ok(u) => u, Err(e) => return Ok(ToolResult::failure(e)) };
                let m = match reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes()) {
                    Ok(m) => m,
                    Err(_) => return Ok(ToolResult::failure(format!("invalid HTTP method {method:?}"))),
                };
                let mut hdrs: Vec<(String, String)> = Vec::with_capacity(headers.len());
                for (k, v) in headers {
                    match self.render(v, &args) {
                        Ok(rv) => hdrs.push((k.clone(), rv)),
                        Err(e) => return Ok(ToolResult::failure(e)),
                    }
                }
                let hdr_refs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let body_bytes = match body {
                    Some(b) => match self.render(b, &args) {
                        Ok(rb) => Some(rb.into_bytes()),
                        Err(e) => return Ok(ToolResult::failure(e)),
                    },
                    None => None,
                };
                let user_id = args.get("_user_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                // LAN egress: relax the SSRF private-network block only for the
                // app's declared, config-resolved egress hosts.
                let allow = self.exec.as_ref()
                    .map(|e| app_egress_hosts(&e.capabilities.network_egress, &self.config, self.secrets.as_ref(), &self.app_id))
                    .unwrap_or_default();
                let ctx = RequestContext::user_only(user_id).with_private_hosts(allow);
                match http.request_with_context(m, &url_r, &hdr_refs, body_bytes, &ctx).await {
                    Ok(resp) => {
                        let raw = String::from_utf8_lossy(&resp.body);
                        // Declarative projection (if declared) reduces a large JSON
                        // body to the selected fields/rows before it reaches the
                        // model; a non-JSON body falls through unchanged. The
                        // model-facing clip below is still applied as a backstop.
                        let text = match response {
                            Some(tr) => apply_response_projection(&raw, tr)
                                .map(std::borrow::Cow::Owned)
                                .unwrap_or(raw),
                            None => raw,
                        };
                        let (body, clipped) = clip_for_model(&text);
                        let mut out = format!("HTTP {} {}\n{}", resp.status, resp.final_url, body);
                        // `resp.truncated` = clipped at the 5 MB transport cap; `clipped`
                        // = clipped at the far smaller model-facing cap below.
                        if let Some(full_len) = clipped {
                            out.push_str(&format!(
                                "\n\n[app tool output clipped: showing {} of {} bytes — this endpoint \
                                 returned more than fits a model's context. Use a narrower call \
                                 (a specific entity/id or a filter) instead of listing everything.]",
                                APP_HTTP_MODEL_CAP_BYTES.min(full_len), full_len));
                        } else if resp.truncated {
                            out.push_str("\n…(response truncated)");
                        }
                        Ok(ToolResult::success(out))
                    }
                    Err(e) => Ok(ToolResult::failure(format!("request failed: {e}"))),
                }
            }
            AppHandler::Subprocess { command, args: cmd_args, stdin, timeout_secs } => {
                self.run_subprocess(command, cmd_args, stdin.as_deref(), *timeout_secs, &args).await
            }
        }
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::User }
    fn tier(&self) -> Tier {
        match self.handler {
            AppHandler::Http { .. }        => Tier::Network,
            AppHandler::Subprocess { .. }  => Tier::Code,
            AppHandler::Echo               => Tier::Pure,
        }
    }
    fn app_id(&self) -> Option<&str> { Some(&self.app_id) }
}

// ── Building the live app-tool surface ───────────────────────────────────────

/// Extract every `App` component's parsed spec from an installed package's stored
/// manifest. Silently skips malformed specs (they were validated at install; a
/// later-corrupted record shouldn't crash the reload).
fn app_specs_of(pkg: &InstalledPackage) -> Vec<AppSpec> {
    let Ok(m) = serde_json::from_value::<PackageManifest>(pkg.manifest.clone()) else {
        return Vec::new();
    };
    m.components.iter()
        .filter(|c| c.kind == ComponentKind::App)
        .filter_map(|c| AppSpec::parse(&c.spec).ok())
        .collect()
}

/// Build the full live app-tool surface from all **active** installed apps — the
/// app analog of `mcp::registry::build_adapters`. The packages handler pushes the
/// result through `ToolRegistry::set_app_tools` after every install/uninstall.
pub fn build_app_tools(
    pkg_store:    &PackageStore,
    packages_dir: &std::path::Path,
    http:         Option<Arc<HttpPolicy>>,
    secrets:      Option<Arc<SecretsStore>>,
) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    for pkg in pkg_store.list().unwrap_or_default() {
        if pkg.state != "active" {
            continue;
        }
        // Non-secret config values travel with the tool; secret ones are fetched
        // lazily from the vault at call time (never cached in the tool).
        let config = pkg.config.as_object().cloned().unwrap_or_default();
        // Per-app exec context (install dir + capabilities) for `subprocess`
        // handlers — shared across the app's tools.
        let exec = app_component_of(&pkg).map(|c| Arc::new(AppExecCtx {
            install_dir:  packages_dir.join(&pkg.id),
            capabilities: c.capabilities,
        }));
        for spec in app_specs_of(&pkg) {
            for t in &spec.tools {
                out.push(Arc::new(AppTool::new(
                    &pkg.id, t, config.clone(), http.clone(), secrets.clone(), exec.clone(),
                )));
            }
        }
    }
    out
}

/// Parse an installed package's manifest and return its first `App` component
/// (schema + config). `None` for a non-app / malformed package.
fn app_component_of(pkg: &InstalledPackage) -> Option<Component> {
    let m = serde_json::from_value::<PackageManifest>(pkg.manifest.clone()).ok()?;
    m.components.into_iter().find(|c| c.kind == ComponentKind::App)
}

/// The declared config fields of an installed app (from its manifest
/// `Component.config_schema`). Empty for an unknown/non-app package. This is the
/// **schema** (labels, `secret` flags, requiredness) — the *values* live in
/// `pkg.config` (non-secret) and the secret vault.
pub fn app_config_schema(pkg_store: &PackageStore, app_id: &str) -> Vec<ConfigField> {
    pkg_store.get(app_id).ok().flatten().as_ref()
        .and_then(app_component_of)
        .map(|c| c.config_schema)
        .unwrap_or_default()
}

/// The set of declared config keys that are marked `secret` (routed to the
/// vault rather than `pkg.config`).
pub fn app_secret_keys(pkg_store: &PackageStore, app_id: &str) -> std::collections::HashSet<String> {
    app_config_schema(pkg_store, app_id).into_iter()
        .filter(|f| f.secret)
        .map(|f| f.key)
        .collect()
}

/// The UI entry (payload-relative path, e.g. `ui/index.html`) of an installed,
/// **active** app's first `App` component — what the UI-serving handler resolves
/// against `<packages_dir>/<id>/`. `None` for an unknown/disabled/non-app package.
pub fn app_ui_entry(pkg_store: &PackageStore, app_id: &str) -> Option<String> {
    let pkg = pkg_store.get(app_id).ok()??;
    if pkg.state != "active" {
        return None;
    }
    app_specs_of(&pkg).into_iter().next().map(|s| s.ui.entry)
}

/// Resolve an app's declared event for an emit request: the event must be in the
/// app's `permissions.emit_events` allowlist **and** declared in `events`. Returns
/// the declared spec (carrying `domain`/`severity`) so the emit endpoint stamps
/// the bus event correctly. `Err` on unknown/disabled app or a disallowed event.
pub fn resolve_emit_event(
    pkg_store: &PackageStore,
    app_id: &str,
    event_name: &str,
) -> Result<AppEventSpec, String> {
    let pkg = pkg_store.get(app_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("app '{app_id}' is not installed"))?;
    if pkg.state != "active" {
        return Err(format!("app '{app_id}' is disabled"));
    }
    for spec in app_specs_of(&pkg) {
        if !spec.permissions.emit_events.iter().any(|e| e == event_name) {
            continue;
        }
        if let Some(ev) = spec.events.iter().find(|e| e.name == event_name) {
            return Ok(ev.clone());
        }
    }
    Err(format!("event '{event_name}' is not allowed for app '{app_id}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_spec() -> serde_json::Value {
        serde_json::json!({
            "ui": { "entry": "ui/index.html" },
            "tools": [{
                "name": "echo",
                "description": "Echo a message back.",
                "args_schema": {"type":"object","properties":{"text":{"type":"string"}},"required":["text"]},
                "handler": { "kind": "echo" }
            }],
            "events": [
                { "name": "app.demo.hello", "domain": "demo", "severity": "info" },
                { "name": "app.demo.issue", "domain": "demo", "severity": "warn" }
            ],
            "permissions": { "emit_events": ["app.demo.hello", "app.demo.issue"] }
        })
    }

    #[test]
    fn parses_and_validates_demo_spec() {
        let s = AppSpec::parse(&demo_spec()).unwrap();
        assert_eq!(s.tools.len(), 1);
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.permissions.emit_events.len(), 2);
    }

    #[test]
    fn rejects_emit_of_undeclared_event() {
        let mut v = demo_spec();
        v["permissions"]["emit_events"] = serde_json::json!(["app.demo.nope"]);
        assert!(AppSpec::parse(&v).is_err());
    }

    #[test]
    fn rejects_bad_severity_and_absolute_ui_entry() {
        let mut v = demo_spec();
        v["events"][0]["severity"] = serde_json::json!("loud");
        assert!(AppSpec::parse(&v).is_err());

        let mut v2 = demo_spec();
        v2["ui"]["entry"] = serde_json::json!("/etc/passwd");
        assert!(AppSpec::parse(&v2).is_err());

        let mut v3 = demo_spec();
        v3["ui"]["entry"] = serde_json::json!("../secret");
        assert!(AppSpec::parse(&v3).is_err());
    }

    #[test]
    fn app_tool_name_is_provider_safe_and_echo_works() {
        let spec = AppSpec::parse(&demo_spec()).unwrap();
        let tool = AppTool::new("com.mira.demo-hello", &spec.tools[0], Default::default(), None, None, None);
        assert_eq!(tool.name(), "app__com-mira-demo-hello__echo");
        // provider-safe charset
        assert!(tool.name().chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    }

    #[tokio::test]
    async fn echo_returns_text_arg() {
        let spec = AppSpec::parse(&demo_spec()).unwrap();
        let tool = AppTool::new("com.mira.demo-hello", &spec.tools[0], Default::default(), None, None, None);
        let r = tool.execute(serde_json::json!({"text": "hi there"})).await.unwrap();
        assert!(r.success);
        assert_eq!(r.output, "hi there");
    }

    fn http_tool(config: serde_json::Map<String, serde_json::Value>) -> AppTool {
        let spec = AppToolSpec {
            name: "call".into(),
            description: String::new(),
            args_schema: default_args_schema(),
            handler: AppHandler::Http {
                method: "GET".into(), url: "http://x".into(),
                headers: BTreeMap::new(), body: None, response: None,
            },
        };
        AppTool::new("com.mira.demo", &spec, config, None, None, None)
    }

    #[test]
    fn render_substitutes_config_and_args() {
        let mut config = serde_json::Map::new();
        config.insert("base_url".into(), serde_json::json!("http://svc.example"));
        let tool = http_tool(config);
        let args = serde_json::json!({ "q": "rust async" });
        assert_eq!(
            tool.render("${config.base_url}/s?q=${args.q}", &args).unwrap(),
            "http://svc.example/s?q=rust async",
        );
        // Missing arg → renders empty (arg was optional).
        assert_eq!(tool.render("[${args.absent}]", &args).unwrap(), "[]");
        // App identity + tier are exposed for the policy gate.
        assert_eq!(tool.app_id(), Some("com.mira.demo"));
        assert!(matches!(tool.tier(), Tier::Network));
    }

    #[test]
    fn render_collapses_redundant_slash_at_a_join() {
        // A `base_url` with a trailing slash must not produce a double slash at the
        // `${config.base_url}/api/...` seam (Home Assistant 404s "//api/...").
        let mut config = serde_json::Map::new();
        config.insert("base_url".into(), serde_json::json!("http://ha.local:8123/"));
        let tool = http_tool(config);
        let args = serde_json::json!({ "id": "light.kitchen" });
        assert_eq!(
            tool.render("${config.base_url}/api/states/${args.id}", &args).unwrap(),
            "http://ha.local:8123/api/states/light.kitchen",
        );
        // Without a trailing slash, the same template renders identically.
        let mut config2 = serde_json::Map::new();
        config2.insert("base_url".into(), serde_json::json!("http://ha.local:8123"));
        assert_eq!(
            http_tool(config2).render("${config.base_url}/api/", &args).unwrap(),
            "http://ha.local:8123/api/",
        );
        // The scheme's own "//" is never eaten, and the collapse only fires at a
        // literal `${...}/` seam — a token followed by a non-slash is left intact.
        let mut config3 = serde_json::Map::new();
        config3.insert("base_url".into(), serde_json::json!("http://ha.local:8123/"));
        assert_eq!(
            http_tool(config3).render("${config.base_url}?x=1", &args).unwrap(),
            "http://ha.local:8123/?x=1",
        );
    }

    #[test]
    fn clip_for_model_bounds_output_on_a_char_boundary() {
        // Small output passes through untouched.
        let (body, clipped) = clip_for_model("short");
        assert_eq!(body, "short");
        assert!(clipped.is_none());
        // Oversized output is clipped to the cap and reports the original length.
        let big = "x".repeat(APP_HTTP_MODEL_CAP_BYTES + 5_000);
        let (body, clipped) = clip_for_model(&big);
        assert!(body.len() <= APP_HTTP_MODEL_CAP_BYTES);
        assert_eq!(clipped, Some(APP_HTTP_MODEL_CAP_BYTES + 5_000));
        // A multi-byte char straddling the cap is never split (result stays valid UTF-8).
        let mut s = "a".repeat(APP_HTTP_MODEL_CAP_BYTES - 1);
        s.push('é'); // 2 bytes — its start sits at the cap boundary
        s.push_str(&"b".repeat(100));
        let (body, clipped) = clip_for_model(&s);
        assert!(clipped.is_some());
        assert!(std::str::from_utf8(body.as_bytes()).is_ok());
    }

    #[test]
    fn response_projection_reduces_and_caps_a_json_array() {
        // A Home-Assistant-shaped array with big attribute bags.
        let body = serde_json::json!([
            { "entity_id": "light.kitchen", "state": "on",
              "attributes": { "friendly_name": "Kitchen", "brightness": 254, "supported": [1,2,3] } },
            { "entity_id": "sensor.temp", "state": "21.5",
              "attributes": { "friendly_name": "Living Room Temp", "unit": "°C" } },
            { "entity_id": "switch.fan", "state": "off",
              "attributes": { "friendly_name": "Office Fan" } }
        ]).to_string();
        let tr = HttpResponse {
            select: vec!["entity_id".into(), "state".into(), "attributes.friendly_name".into()],
            limit: Some(2),
        };
        let out = apply_response_projection(&body, &tr).unwrap();
        // Only selected fields survive (keyed by last path segment), attributes dropped.
        assert!(out.contains("\"friendly_name\":\"Kitchen\""));
        assert!(out.contains("\"entity_id\":\"light.kitchen\""));
        assert!(!out.contains("brightness"));
        assert!(!out.contains("\"supported\""));
        // limit capped to 2 rows, with a note about the rest.
        assert!(!out.contains("switch.fan"));
        assert!(out.contains("showing 2 of 3 items"));
        // Massively smaller than the raw body.
        assert!(out.len() < body.len());
    }

    #[test]
    fn response_projection_passes_through_non_json_and_projects_objects() {
        let tr = HttpResponse { select: vec!["state".into()], limit: None };
        // Non-JSON body → None (caller keeps raw).
        assert!(apply_response_projection("not json at all", &tr).is_none());
        // A single object is projected directly (get_state shape).
        let obj = serde_json::json!({ "entity_id": "light.x", "state": "on", "attributes": {"z":1} }).to_string();
        let out = apply_response_projection(&obj, &tr).unwrap();
        assert_eq!(out, "{\"state\":\"on\"}");
        // Empty selector keeps the whole element.
        let keep_all = HttpResponse { select: vec![], limit: None };
        let arr = serde_json::json!([{ "a": 1 }]).to_string();
        assert_eq!(apply_response_projection(&arr, &keep_all).unwrap(), "[{\"a\":1}]");
    }

    #[test]
    fn render_errors_on_missing_config_and_unknown_namespace() {
        let tool = http_tool(serde_json::Map::new());
        // Missing config value is a misconfiguration, not a silent empty.
        assert!(tool.render("${config.api_key}", &serde_json::json!({})).is_err());
        // Unknown namespace is rejected (typo can't leak a literal `${...}`).
        assert!(tool.render("${env.HOME}", &serde_json::json!({})).is_err());
    }

    #[tokio::test]
    async fn http_handler_without_policy_fails_gracefully() {
        // No HttpPolicy wired (minimal build) → a clean tool failure, not a panic.
        let r = http_tool(serde_json::Map::new())
            .execute(serde_json::json!({})).await.unwrap();
        assert!(!r.success);
    }

    // ── subprocess handler (Slice 2b) ─────────────────────────────────────────

    fn subproc_tool(install_dir: PathBuf, caps: Capabilities, command: &str) -> AppTool {
        let spec = AppToolSpec {
            name: "run".into(),
            description: String::new(),
            args_schema: default_args_schema(),
            handler: AppHandler::Subprocess {
                command: command.into(), args: vec![], stdin: None, timeout_secs: None,
            },
        };
        AppTool::new(
            "com.mira.sub", &spec, serde_json::Map::new(), None, None,
            Some(Arc::new(AppExecCtx { install_dir, capabilities: caps })),
        )
    }

    #[test]
    fn resolve_within_is_traversal_safe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("run.sh"), b"#!/bin/sh\n").unwrap();
        // A real payload-relative file resolves to an absolute path under base.
        let got = resolve_within(dir.path(), "run.sh").unwrap();
        assert!(got.ends_with("run.sh"));
        // Absolute + `..` + missing + non-file are all rejected.
        assert!(resolve_within(dir.path(), "/etc/passwd").is_err());
        assert!(resolve_within(dir.path(), "../secret").is_err());
        assert!(resolve_within(dir.path(), "nope.sh").is_err());
        assert!(resolve_within(dir.path(), "").is_err());
    }

    #[test]
    fn subprocess_validation_rejects_bad_command_paths() {
        let mk = |cmd: &str| serde_json::json!({
            "ui": { "entry": "ui/index.html" },
            "tools": [{ "name": "run", "args_schema": {"type":"object"},
                        "handler": { "kind": "subprocess", "command": cmd } }],
        });
        assert!(AppSpec::parse(&mk("bin/tool")).is_ok());
        assert!(AppSpec::parse(&mk("")).is_err());          // empty
        assert!(AppSpec::parse(&mk("/usr/bin/x")).is_err()); // absolute
        assert!(AppSpec::parse(&mk("../x")).is_err());       // traversal
    }

    #[tokio::test]
    async fn subprocess_capability_gate_blocks_undeclared_and_off_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();

        // Capability not declared → refused before any spawn.
        let no_cap = subproc_tool(dir.path().to_path_buf(), Capabilities::default(), "run.sh");
        let r = no_cap.execute(serde_json::json!({})).await.unwrap();
        // On non-Linux the platform gate fires first; either way it's a failure.
        assert!(!r.success);
        if cfg!(target_os = "linux") {
            assert!(r.error.as_deref().unwrap_or("").contains("subprocess"));
        }

        // Declared but the command isn't on a non-empty allowlist → refused.
        if cfg!(target_os = "linux") {
            let caps = Capabilities {
                subprocess: true,
                subprocess_allowlist: vec!["other.sh".into()],
                ..Default::default()
            };
            let off = subproc_tool(dir.path().to_path_buf(), caps, "run.sh");
            let r = off.execute(serde_json::json!({})).await.unwrap();
            assert!(!r.success);
            assert!(r.error.as_deref().unwrap_or("").contains("allowlist"));
        }
    }

    // ── health_check + poller helpers (poller feature) ────────────────────────

    fn spec_with_health(hc: serde_json::Value, ev_sev: &str) -> serde_json::Value {
        serde_json::json!({
            "ui": { "entry": "ui/index.html" },
            "events": [{ "name": "app.x.down", "domain": "x", "severity": ev_sev }],
            "permissions": { "emit_events": ["app.x.down"] },
            "health_check": hc,
        })
    }

    #[test]
    fn health_check_validation() {
        // Valid: emit_on_failure is a declared, emit-allowed warn event.
        assert!(AppSpec::parse(&spec_with_health(
            serde_json::json!({ "url": "${config.base_url}/api/", "emit_on_failure": "app.x.down" }), "warn")).is_ok());
        // Undeclared / not-emit-allowed event → rejected.
        assert!(AppSpec::parse(&spec_with_health(
            serde_json::json!({ "url": "u", "emit_on_failure": "app.x.nope" }), "warn")).is_err());
        // Empty url → rejected.
        assert!(AppSpec::parse(&spec_with_health(
            serde_json::json!({ "url": "", "emit_on_failure": "app.x.down" }), "warn")).is_err());
        // Pointing at an info (non-issue) event → rejected: only issues drive the Guardian.
        assert!(AppSpec::parse(&spec_with_health(
            serde_json::json!({ "url": "u", "emit_on_failure": "app.x.down" }), "info")).is_err());
    }

    #[test]
    fn app_egress_hosts_render_and_reduce_to_hostnames() {
        let mut config = serde_json::Map::new();
        config.insert("base_url".into(), serde_json::json!("http://homeassistant.local:8123"));
        // `${config.base_url}` → bare host; a literal host passes through lowercased.
        let hosts = app_egress_hosts(
            &["${config.base_url}".into(), "HA.example.com".into()],
            &config, None, "com.mira.ha",
        );
        assert_eq!(hosts, vec!["homeassistant.local".to_string(), "ha.example.com".to_string()]);
        // An egress entry whose config is missing is dropped (not a wildcard).
        assert!(app_egress_hosts(&["${config.nope}".into()], &config, None, "com.mira.ha").is_empty());
    }

    #[test]
    fn render_config_resolves_only_config_tokens() {
        let mut config = serde_json::Map::new();
        config.insert("base_url".into(), serde_json::json!("http://ha.local:8123"));
        assert_eq!(
            render_config("${config.base_url}/api/", &config, None, "com.mira.x").unwrap(),
            "http://ha.local:8123/api/",
        );
        // Missing config → Err (so the poller skips an unconfigured app, not "down").
        assert!(render_config("${config.token}", &config, None, "com.mira.x").is_err());
        // args.* is not allowed in a health-check template.
        assert!(render_config("${args.x}", &config, None, "com.mira.x").is_err());
    }
}
