// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/web_apps.rs
//! Serve coding-agent-built web apps at an isolated per-app origin.
//!
//! When MIRA's coding agent builds something like a Snake game, the
//! deliverable lands on disk as `<task-dir>/output/index.html` (+ assets)
//! under the task-artifacts root — but nothing served it, so asking MIRA to
//! "open the game" had no honest path: the model would confabulate a
//! browser-open it cannot perform. This module closes that gap.
//!
//! ## How it works
//!
//! Each app is served at **`http://<task_id>.<host_suffix>:<port>/`**. That is
//! a *distinct browser origin* from the MIRA app itself (which lives at, e.g.,
//! `http://127.0.0.1:<port>/`), so a model-built app — arbitrary HTML/JS —
//! **cannot** read MIRA's session token from `localStorage`, nor call MIRA's
//! authenticated API as the signed-in user. `localhost` gives that isolation
//! for free: every major browser resolves `*.localhost` to loopback natively
//! (RFC 6761), so no extra port, DNS, or firewall rule is needed.
//!
//! The `task_id` (a high-entropy UUIDv7) *is* the capability — the same model
//! as `/api/artifacts/<sha>`: unguessable, so the URL itself is the access
//! grant. Serving is therefore a pure function of `(task_id, path)` with no
//! registry or shared state.
//!
//! A single [`dispatch`] middleware, mounted as the **outermost** layer, peeks
//! at the `Host` header: if it's `<label>.<host_suffix>` it serves that app's
//! file and returns; otherwise the request flows to MIRA's normal router
//! untouched. So the feature is invisible to every non-app request.

use std::path::Path;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path as UrlPath, Request, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use crate::config::ServerConfig;
use crate::task_artifacts::TaskArtifactsStore;

/// State threaded into the [`dispatch`] middleware.
#[derive(Clone)]
pub struct WebAppState {
    pub store:       Arc<TaskArtifactsStore>,
    /// Host suffix that marks an app request, e.g. `localhost`.
    pub host_suffix: String,
}

/// Extract the app `task_id` from a `Host` header value, or `None` if this is
/// not an app request. An app host is exactly `<label>.<suffix>` (optionally
/// with `:port`), where `<label>` is a single non-empty segment (no dots) —
/// so `abc.localhost` is an app, while bare `localhost`, `127.0.0.1`, or a
/// multi-label `a.b.localhost` are not.
pub fn app_task_id_from_host<'a>(host: &'a str, suffix: &str) -> Option<&'a str> {
    // Strip an optional `:port`. IPv6 literals never match a `.suffix` form,
    // so a naive rsplit on ':' is safe here.
    let hostname = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let dot_suffix = format!(".{suffix}");
    let label = hostname.strip_suffix(&dot_suffix)?;
    if label.is_empty() || label.contains('.') {
        return None;
    }
    Some(label)
}

/// The link(s) a built app is reachable at, per the configured serving mode.
pub struct AppLinks {
    /// The URL to give the user.
    pub primary: String,
    /// A second URL that also works (only in `both` mode — the port-based one).
    pub alt:     Option<String>,
}

fn scheme(server: &ServerConfig) -> &'static str {
    if server.tls_cert_path.is_some() && server.tls_key_path.is_some() { "https" } else { "http" }
}

/// Subdomain-mode URL: `http://<task_id>.<host_suffix>:<port>/`.
pub fn subdomain_url(server: &ServerConfig, task_id: &str) -> String {
    format!("{}://{task_id}.{}:{}/", scheme(server), server.web_apps.host_suffix, server.port)
}

/// The effective `port`-mode listener port — `0` means `server.port + 1`.
pub fn effective_apps_port(server: &ServerConfig) -> u16 {
    match server.web_apps.port {
        0 => server.port.saturating_add(1),
        p => p,
    }
}

/// Host used to build `port`-mode URLs: explicit `advertised_host`, else the
/// host parsed out of `public_base_url`, else `server.host` when it's a
/// concrete address (not a wildcard bind), else `localhost`.
pub fn advertised_host(server: &ServerConfig) -> String {
    if let Some(h) = server.web_apps.advertised_host.as_ref().filter(|h| !h.is_empty()) {
        return h.clone();
    }
    if let Some(h) = server.public_base_url.as_deref().and_then(host_from_base_url) {
        return h;
    }
    // The remote-access URL (Tailscale / DDNS / reverse proxy) is the host a
    // mobile or off-LAN client actually reaches MIRA at — a good fallback so
    // port-mode app links point somewhere the phone can resolve.
    if let Some(h) = server.remote_url.as_deref().and_then(host_from_base_url) {
        return h;
    }
    let h = server.host.trim();
    if !h.is_empty() && h != "0.0.0.0" && h != "::" {
        return h.to_string();
    }
    "localhost".to_string()
}

/// The host-relative path a `port`/`both`-mode app is served at: `/a/<task_id>/`.
/// A client that knows its own base host + the [`effective_apps_port`] can build a
/// URL reachable from wherever *it* connects (LAN, Tailscale, …) — which the fixed
/// [`port_url`] can't, since one `advertised_host` can't be right for every client.
/// The mobile app uses this + `web_app_port` to construct a reachable link.
pub fn port_path(task_id: &str) -> String {
    format!("/a/{task_id}/")
}

/// Port-mode URL: `http://<advertised_host>:<apps_port>/a/<task_id>/`. The
/// trailing slash matters — the app's relative asset refs resolve against it.
pub fn port_url(server: &ServerConfig, task_id: &str) -> String {
    format!(
        "{}://{}:{}/a/{task_id}/",
        scheme(server),
        advertised_host(server),
        effective_apps_port(server),
    )
}

/// Extract the host from a base URL like `https://host:8443/foo`, dropping the
/// scheme, any userinfo, the port, and the path. IPv6 literals keep brackets.
fn host_from_base_url(base: &str) -> Option<String> {
    let after     = base.split("://").nth(1).unwrap_or(base);
    let authority = after.split('/').next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(rest) = authority.strip_prefix('[') {
        format!("[{}]", rest.split(']').next()?)          // IPv6 literal
    } else {
        authority.split(':').next().unwrap_or(authority).to_string()
    };
    (!host.is_empty()).then_some(host)
}

/// The mode-aware link(s) for a built app. Independent of whether serving is
/// currently enabled, so the tool can still tell the user where the app lives.
pub fn web_app_links(server: &ServerConfig, task_id: &str) -> AppLinks {
    match server.web_apps.mode.trim().to_ascii_lowercase().as_str() {
        "port" => AppLinks { primary: port_url(server, task_id), alt: None },
        "both" => AppLinks {
            primary: subdomain_url(server, task_id),
            alt:     Some(port_url(server, task_id)),
        },
        // "subdomain" and any unrecognised value fall back to the safe default.
        _ => AppLinks { primary: subdomain_url(server, task_id), alt: None },
    }
}

/// Back-compat convenience: the primary URL a built app is reachable at.
pub fn web_app_url(server: &ServerConfig, task_id: &str) -> String {
    web_app_links(server, task_id).primary
}

/// A friendly display name for a built app: the `<title>` of its
/// `output/index.html`, or `None` when there's no readable title. Best-effort +
/// bounded (only the head of the file is scanned) so this never becomes a cost on
/// the tool path.
pub fn web_app_title(store: &TaskArtifactsStore, task_id: &str) -> Option<String> {
    let path = store.resolve_file(task_id, "output/index.html")?;
    let bytes = std::fs::read(&path).ok()?;
    let head = &bytes[..bytes.len().min(64 * 1024)]; // <title> lives in <head>, near the top
    let text = String::from_utf8_lossy(head);
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let gt = lower[open..].find('>')? + open + 1;
    let end = lower[gt..].find("</title>")? + gt;
    let title = text[gt..end]
        .trim()
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'")
        .replace("&quot;", "\"");
    let title: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(80).collect())
}

/// Turn a slug like `snake-game` into a friendly `Snake game` (fallback name when
/// the app has no `<title>`).
fn humanize_slug(slug: &str) -> String {
    let words = slug.replace(['-', '_'], " ");
    let words = words.trim();
    let mut c = words.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The friendly display name for a built app + a ready-to-use markdown link
/// `[name](url)` the model can hand the user verbatim. `name` is the app's
/// `<title>`, else the humanized `slug`, else `"the app"`. The URL is the
/// **canonical main-host form** (see [`canonical_app_url`]) — reachable from the
/// same access path as the API (incl. through a reverse proxy). The model must
/// never construct its own.
pub fn web_app_named_link(
    store:   &TaskArtifactsStore,
    server:  &ServerConfig,
    task_id: &str,
    slug:    Option<&str>,
) -> (String, String) {
    let name = web_app_title(store, task_id)
        .or_else(|| slug.map(str::trim).filter(|s| !s.is_empty()).map(humanize_slug))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "the app".to_string());
    let markdown = format!("[{name}]({})", canonical_app_url(server, task_id));
    (name, markdown)
}

/// The canonical URL to give a user for a built app: the absolute main-host URL
/// when a public base is known (`<public base>/a/<id>/`), else the host-relative
/// `/a/<id>/` — which the web UI resolves against whatever origin the user is on.
/// Either form is reachable from the same access path as the API (LAN, reverse
/// proxy, tunnel) via the main-host serving — unlike the old `*.localhost` /
/// internal-port URLs.
pub fn canonical_app_url(server: &ServerConfig, task_id: &str) -> String {
    main_host_url(server, task_id).unwrap_or_else(|| main_host_path(task_id))
}

/// Whether the configured mode runs the separate `port`-mode listener.
pub fn port_mode_enabled(server: &ServerConfig) -> bool {
    matches!(server.web_apps.mode.trim().to_ascii_lowercase().as_str(), "port" | "both")
}

/// Whether the configured mode runs the subdomain host-dispatch middleware.
pub fn subdomain_mode_enabled(server: &ServerConfig) -> bool {
    !matches!(server.web_apps.mode.trim().to_ascii_lowercase().as_str(), "port")
}

/// Does this task have a servable web app (a readable `output/index.html`)?
pub fn has_web_app(store: &TaskArtifactsStore, task_id: &str) -> bool {
    store.resolve_file(task_id, "output/index.html").is_some()
}

/// Host-dispatch middleware. If the request targets an app subdomain, serve the
/// app file and short-circuit; otherwise fall through to the normal router.
pub async fn dispatch(
    State(state): State<WebAppState>,
    req:          Request,
    next:         Next,
) -> Response {
    let task_id = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| app_task_id_from_host(h, &state.host_suffix))
        .map(|s| s.to_string());

    match task_id {
        Some(task_id) => serve_app_file(&state.store, &task_id, req.uri().path(), false).await,
        None          => next.run(req).await,
    }
}

/// Serve one file from a task's `output/` dir. `req_path` is the request path
/// (e.g. `/`, `/game.js`); `/` maps to `index.html`. Path-traversal is blocked
/// by [`TaskArtifactsStore::resolve_file`], which canonicalises and confirms
/// the target sits under the task dir.
async fn serve_app_file(
    store: &TaskArtifactsStore,
    task_id: &str,
    req_path: &str,
    // Whether this file is being served into a sandboxed (opaque-origin) iframe
    // on MIRA's MAIN host (the reverse-proxy-reachable `/a/<id>/_/…` path). In
    // that context a `connect-src 'self'` CSP would break asset loading (the
    // document's origin is opaque, so 'self' matches nothing), and isolation is
    // already provided by the iframe sandbox — so we omit the confining CSP.
    // The separate-origin serving (subdomain/port) keeps it.
    sandboxed: bool,
) -> Response {
    let raw = req_path.trim_start_matches('/');
    let rel = if raw.is_empty() { "index.html" } else { raw };
    let full = format!("output/{rel}");

    let Some(path) = store.resolve_file(task_id, &full) else {
        return (StatusCode::NOT_FOUND, "web app file not found").into_response();
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b)  => b,
        Err(_) => return (StatusCode::NOT_FOUND, "web app file not found").into_response(),
    };

    let mut resp = Response::new(Body::from(bytes));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type_for(&path)));
    // Built apps can be rebuilt in place under the same task_id → revalidate.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if !sandboxed {
        // Separate-origin serving: keep the (arbitrary, model-generated) app
        // contained — it may run inline scripts but can only talk back to its
        // own origin, never a third party or MIRA's API.
        h.insert(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(
                "default-src 'self' 'unsafe-inline' 'unsafe-eval' data: blob:; connect-src 'self'",
            ),
        );
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // Don't leak the capability URL (which contains the task_id) via Referer.
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

// ── port mode ────────────────────────────────────────────────────────────────

/// Router for `port`/`both` mode: a **separate listener** (its own port, hence
/// its own browser origin) serving every app under `/a/<task_id>/…`. Unlike
/// subdomain mode, all apps share this one origin — fine for a single-user box,
/// weaker isolation between apps for multi-user. Reachable over any host that
/// can reach the listener (a LAN / WSL-gateway IP), which is the whole point.
pub fn app_port_router(store: Arc<TaskArtifactsStore>) -> Router {
    Router::new()
        .route("/a/{task_id}",         get(port_serve_root))
        .route("/a/{task_id}/",        get(port_serve_root))
        .route("/a/{task_id}/{*path}", get(port_serve_path))
        .with_state(store)
}

async fn port_serve_root(
    State(store):     State<Arc<TaskArtifactsStore>>,
    UrlPath(task_id): UrlPath<String>,
) -> Response {
    serve_app_file(&store, &task_id, "", false).await
}

async fn port_serve_path(
    State(store):             State<Arc<TaskArtifactsStore>>,
    UrlPath((task_id, path)): UrlPath<(String, String)>,
) -> Response {
    serve_app_file(&store, &task_id, &path, false).await
}

// ── main-host serving (reverse-proxy reachable, sandboxed) ───────────────────
//
// Subdomain/port modes serve apps at a SEPARATE origin — great isolation, but
// unreachable behind a reverse proxy that only exposes one host on 443 (Traefik,
// `tailscale serve`), where the app port isn't addressable. So we ALSO serve apps
// from MIRA's MAIN HTTP server at `/a/<task_id>/…` (the host every deployment
// already exposes). To keep the model-built, untrusted app from running with
// MIRA's own origin privileges (reading the session token from localStorage,
// calling the API), `/a/<id>/` returns a trusted SHELL that runs the app inside a
// `sandbox`ed iframe (no `allow-same-origin` → opaque origin) whose files are
// served from `/a/<id>/_/…`. Reachable everywhere, isolated everywhere.

/// The host-relative path the main server serves an app's shell at: `/a/<id>/`.
/// The mobile app builds `<its own base host>` + this — reachable through any
/// reverse proxy / tunnel / LAN that fronts the main host, with no extra config.
pub fn main_host_path(task_id: &str) -> String {
    format!("/a/{task_id}/")
}

/// The canonical absolute main-host app URL, when a public base is known
/// (`public_base_url`). `None` when we can't determine an externally-valid base —
/// callers then rely on the host-relative [`main_host_path`] built against the
/// client's own base.
pub fn main_host_url(server: &ServerConfig, task_id: &str) -> Option<String> {
    let base = server.public_base_url.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .trim_end_matches('/');
    Some(format!("{base}/a/{task_id}/"))
}

/// Trusted shell served at `/a/<id>/` on the main origin: it embeds the untrusted
/// app in a sandboxed (opaque-origin) iframe loaded from `/a/<id>/_/`. The shell
/// itself is our HTML (no app code), so it's safe on MIRA's origin.
fn app_shell_html(task_id: &str) -> String {
    // `allow-scripts` WITHOUT `allow-same-origin` = opaque origin: the app runs
    // its JS but cannot read MIRA's localStorage/token or call its API.
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>App</title>\
         <style>html,body{{margin:0;height:100%;background:#fff;overflow:hidden}}\
         iframe{{border:0;width:100vw;height:100vh;display:block}}</style></head>\
         <body><iframe src=\"/a/{task_id}/_/\" \
         sandbox=\"allow-scripts allow-forms allow-popups allow-modals allow-pointer-lock allow-downloads\" \
         allow=\"fullscreen; autoplay\"></iframe></body></html>"
    )
}

/// Router for the main-host app path. Public — the unguessable `task_id` is the
/// capability. Merged into MIRA's main router.
pub fn app_main_router(store: Arc<TaskArtifactsStore>) -> Router {
    Router::new()
        .route("/a/{task_id}",           get(main_serve_shell))
        .route("/a/{task_id}/",          get(main_serve_shell))
        .route("/a/{task_id}/_",         get(main_serve_inner_root))
        .route("/a/{task_id}/_/",        get(main_serve_inner_root))
        .route("/a/{task_id}/_/{*path}", get(main_serve_inner_path))
        .with_state(store)
}

async fn main_serve_shell(
    State(store):     State<Arc<TaskArtifactsStore>>,
    UrlPath(task_id): UrlPath<String>,
) -> Response {
    if !has_web_app(&store, &task_id) {
        return (StatusCode::NOT_FOUND, "web app not found").into_response();
    }
    let mut resp = Response::new(Body::from(app_shell_html(&task_id)));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // The shell frames only its own inner path and runs no app code itself.
    h.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("default-src 'none'; frame-src 'self'; style-src 'unsafe-inline'; base-uri 'none'"),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

async fn main_serve_inner_root(
    State(store):     State<Arc<TaskArtifactsStore>>,
    UrlPath(task_id): UrlPath<String>,
) -> Response {
    serve_app_file(&store, &task_id, "", true).await
}

async fn main_serve_inner_path(
    State(store):             State<Arc<TaskArtifactsStore>>,
    UrlPath((task_id, path)): UrlPath<(String, String)>,
) -> Response {
    serve_app_file(&store, &task_id, &path, true).await
}

/// Extension → MIME map covering what a built static web app emits.
fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("html" | "htm")  => "text/html; charset=utf-8",
        Some("js" | "mjs")    => "text/javascript; charset=utf-8",
        Some("css")           => "text/css; charset=utf-8",
        Some("json")          => "application/json",
        Some("wasm")          => "application/wasm",
        Some("svg")           => "image/svg+xml",
        Some("png")           => "image/png",
        Some("jpg" | "jpeg")  => "image/jpeg",
        Some("gif")           => "image/gif",
        Some("webp")          => "image/webp",
        Some("ico")           => "image/x-icon",
        Some("mp3")           => "audio/mpeg",
        Some("wav")           => "audio/wav",
        Some("ogg")           => "audio/ogg",
        Some("mp4")           => "video/mp4",
        Some("webm")          => "video/webm",
        Some("woff2")         => "font/woff2",
        Some("woff")          => "font/woff",
        Some("ttf")           => "font/ttf",
        Some("txt")           => "text/plain; charset=utf-8",
        _                     => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing_identifies_app_requests() {
        // App requests: single label + suffix, with/without port.
        assert_eq!(app_task_id_from_host("abc123.localhost", "localhost"), Some("abc123"));
        assert_eq!(app_task_id_from_host("abc123.localhost:8087", "localhost"), Some("abc123"));
        assert_eq!(
            app_task_id_from_host("019f3827-4387-7b11-a977-23ed0b39b79b.localhost:8087", "localhost"),
            Some("019f3827-4387-7b11-a977-23ed0b39b79b"),
        );
        // Not app requests: bare host, ip, multi-label.
        assert_eq!(app_task_id_from_host("localhost", "localhost"), None);
        assert_eq!(app_task_id_from_host("localhost:8087", "localhost"), None);
        assert_eq!(app_task_id_from_host("127.0.0.1:8087", "localhost"), None);
        assert_eq!(app_task_id_from_host("a.b.localhost", "localhost"), None);
        // Custom suffix.
        assert_eq!(app_task_id_from_host("t1.apps.example", "apps.example"), Some("t1"));
    }

    #[test]
    fn url_build_uses_scheme_port_suffix() {
        let mut s = ServerConfig::default();
        s.port = 8087;
        s.web_apps.host_suffix = "localhost".to_string();
        assert_eq!(web_app_url(&s, "task-9"), "http://task-9.localhost:8087/");
        s.tls_cert_path = Some("/c".into());
        s.tls_key_path  = Some("/k".into());
        assert_eq!(web_app_url(&s, "task-9"), "https://task-9.localhost:8087/");
    }

    #[test]
    fn mode_selects_primary_and_alt_urls() {
        let mut s = ServerConfig::default();
        s.port = 8087;
        s.host = "0.0.0.0".to_string();               // wildcard bind
        s.web_apps.advertised_host = Some("198.51.100.10".to_string());

        // subdomain: only a subdomain link.
        s.web_apps.mode = "subdomain".to_string();
        let l = web_app_links(&s, "t1");
        assert_eq!(l.primary, "http://t1.localhost:8087/");
        assert!(l.alt.is_none());

        // port: primary is the /a/<id>/ URL on the +1 port over the LAN host.
        s.web_apps.mode = "port".to_string();
        let l = web_app_links(&s, "t1");
        assert_eq!(l.primary, "http://198.51.100.10:8088/a/t1/");
        assert!(l.alt.is_none());

        // both: subdomain primary, port alternate.
        s.web_apps.mode = "both".to_string();
        let l = web_app_links(&s, "t1");
        assert_eq!(l.primary, "http://t1.localhost:8087/");
        assert_eq!(l.alt.as_deref(), Some("http://198.51.100.10:8088/a/t1/"));
    }

    #[test]
    fn advertised_host_derivation() {
        let mut s = ServerConfig::default();
        s.host = "0.0.0.0".to_string();
        // Falls back to localhost when host is a wildcard and nothing else set.
        assert_eq!(advertised_host(&s), "localhost");
        // Derives from public_base_url.
        s.public_base_url = Some("https://mira.example.com:8443/app".to_string());
        assert_eq!(advertised_host(&s), "mira.example.com");
        // Explicit advertised_host wins.
        s.web_apps.advertised_host = Some("203.0.113.5".to_string());
        assert_eq!(advertised_host(&s), "203.0.113.5");
        // Concrete server.host used when no override/base url.
        let mut s2 = ServerConfig::default();
        s2.host = "192.0.2.9".to_string();
        assert_eq!(advertised_host(&s2), "192.0.2.9");
        // remote_url (Tailscale / DDNS) is used when no explicit host / public URL —
        // so port-mode links point where a mobile client reaches MIRA.
        let mut s3 = ServerConfig::default();
        s3.host = "0.0.0.0".to_string();
        s3.remote_url = Some("https://mybox.tail1234.ts.net/".to_string());
        assert_eq!(advertised_host(&s3), "mybox.tail1234.ts.net");
    }

    #[test]
    fn default_mode_is_both_and_exposes_portable_path() {
        // Default mode now serves the port path too, so mobile/LAN clients can
        // build a reachable URL from their own base host.
        let s = ServerConfig::default();
        assert_eq!(s.web_apps.mode, "both");
        assert!(port_mode_enabled(&s));
        assert!(subdomain_mode_enabled(&s));
        assert_eq!(port_path("019f-abc"), "/a/019f-abc/");
    }

    #[test]
    fn explicit_apps_port_and_default() {
        let mut s = ServerConfig::default();
        s.port = 8087;
        assert_eq!(effective_apps_port(&s), 8088); // 0 → server.port + 1
        s.web_apps.port = 9000;
        assert_eq!(effective_apps_port(&s), 9000);
    }

    #[test]
    fn humanize_slug_titlecases_words() {
        assert_eq!(humanize_slug("snake-game"), "Snake game");
        assert_eq!(humanize_slug("my_cool_app"), "My cool app");
        assert_eq!(humanize_slug(""), "");
    }

    #[test]
    fn title_extraction_and_named_link() {
        let dir   = tempfile::tempdir().unwrap();
        let store = TaskArtifactsStore::new(dir.path().to_path_buf());
        let p = store.allocate("com.mira.claudecode", "task-snake", None, None, "x").unwrap();
        std::fs::write(
            p.join("output/index.html"),
            b"<!doctype html><html><head><title>  Snake &amp; Ladders  </title></head><body></body></html>",
        ).unwrap();

        // <title> extracted, entity-decoded, whitespace-collapsed.
        assert_eq!(web_app_title(&store, "task-snake").as_deref(), Some("Snake & Ladders"));
        assert_eq!(web_app_title(&store, "task-missing"), None);

        let mut s = ServerConfig::default();
        s.port = 8087;
        s.web_apps.host_suffix = "localhost".to_string();

        // Title wins → a friendly markdown link with the canonical (main-host,
        // reverse-proxy-reachable) URL. No public_base_url set → host-relative path.
        let (name, md) = web_app_named_link(&store, &s, "task-snake", Some("snake-game"));
        assert_eq!(name, "Snake & Ladders");
        assert_eq!(md, "[Snake & Ladders](/a/task-snake/)");
        // With a public base it's the absolute main-host URL (no port).
        s.public_base_url = Some("https://mira.example.com".to_string());
        let (_, md2) = web_app_named_link(&store, &s, "task-snake", None);
        assert_eq!(md2, "[Snake & Ladders](https://mira.example.com/a/task-snake/)");

        // No <title> → humanized slug fallback.
        let p2 = store.allocate("com.mira.claudecode", "task-plain", None, None, "x").unwrap();
        std::fs::write(p2.join("output/index.html"), b"<canvas></canvas>").unwrap();
        let (name2, _) = web_app_named_link(&store, &s, "task-plain", Some("snake-game"));
        assert_eq!(name2, "Snake game");

        // No <title>, no slug → generic "the app".
        let (name3, _) = web_app_named_link(&store, &s, "task-plain", None);
        assert_eq!(name3, "the app");
    }

    #[tokio::test]
    async fn port_router_serves_app() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt; // oneshot

        let dir   = tempfile::tempdir().unwrap();
        let store = Arc::new(TaskArtifactsStore::new(dir.path().to_path_buf()));
        let p = store.allocate("com.mira.claudecode", "task-snake", None, None, "x").unwrap();
        std::fs::write(p.join("output/index.html"), b"<canvas></canvas>").unwrap();

        let app = app_port_router(Arc::clone(&store));

        // /a/<id>/ serves index.html.
        let resp = app.clone()
            .oneshot(HttpRequest::builder().uri("/a/task-snake/").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "text/html; charset=utf-8");

        // Unknown app → 404.
        let resp = app
            .oneshot(HttpRequest::builder().uri("/a/nope/index.html").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn canonical_url_prefers_public_base_else_relative_path() {
        let mut s = ServerConfig::default();
        assert_eq!(main_host_path("t1"), "/a/t1/");
        // No public base → host-relative path (resolves against the client's origin).
        assert_eq!(main_host_url(&s, "t1"), None);
        assert_eq!(canonical_app_url(&s, "t1"), "/a/t1/");
        // Public base → absolute main-host URL (reverse-proxy reachable, no port).
        s.public_base_url = Some("https://mira.example.com/".to_string());
        assert_eq!(main_host_url(&s, "t1").as_deref(), Some("https://mira.example.com/a/t1/"));
        assert_eq!(canonical_app_url(&s, "t1"), "https://mira.example.com/a/t1/");
    }

    #[tokio::test]
    async fn main_router_serves_sandboxed_shell_and_inner() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        let dir   = tempfile::tempdir().unwrap();
        let store = Arc::new(TaskArtifactsStore::new(dir.path().to_path_buf()));
        let p = store.allocate("com.mira.claudecode", "task-snake", None, None, "x").unwrap();
        std::fs::write(p.join("output/index.html"), b"<canvas id=game></canvas>").unwrap();
        std::fs::write(p.join("output/game.js"), b"console.log('hi')").unwrap();

        let app = app_main_router(Arc::clone(&store));
        let read = |resp: Response| async {
            axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()
        };

        // /a/<id>/ → the trusted SHELL: a sandboxed iframe (no allow-same-origin),
        // NOT the app HTML directly.
        let resp = app.clone()
            .oneshot(HttpRequest::builder().uri("/a/task-snake/").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = String::from_utf8_lossy(&read(resp).await).into_owned();
        assert!(html.contains("<iframe"), "{html}");
        assert!(html.contains("sandbox=\"allow-scripts"), "{html}");
        assert!(!html.contains("allow-same-origin"), "iframe must NOT be same-origin: {html}");
        assert!(html.contains("/a/task-snake/_/"), "{html}");
        assert!(!html.contains("<canvas"), "shell must not inline the app: {html}");

        // /a/<id>/_/ → the raw app index, sandboxed (no confining CSP that would
        // break asset loading under an opaque origin).
        let resp = app.clone()
            .oneshot(HttpRequest::builder().uri("/a/task-snake/_/").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("content-security-policy").is_none(), "sandboxed inner keeps no confining CSP");
        assert!(String::from_utf8_lossy(&read(resp).await).contains("<canvas"));

        // /a/<id>/_/game.js → raw asset.
        let resp = app.clone()
            .oneshot(HttpRequest::builder().uri("/a/task-snake/_/game.js").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Unknown app shell → 404.
        let resp = app
            .oneshot(HttpRequest::builder().uri("/a/nope/").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serves_index_and_blocks_traversal() {
        let dir   = tempfile::tempdir().unwrap();
        let store = TaskArtifactsStore::new(dir.path().to_path_buf());
        let p = store.allocate("com.mira.claudecode", "task-snake", None, None, "x").unwrap();
        std::fs::write(p.join("output/index.html"), b"<canvas></canvas>").unwrap();

        assert!(has_web_app(&store, "task-snake"));
        assert!(!has_web_app(&store, "task-missing"));

        // Root → index.html, served as HTML with the isolation headers.
        let resp = serve_app_file(&store, "task-snake", "/", false).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "text/html; charset=utf-8");
        assert!(resp.headers().get("content-security-policy").is_some());
        assert_eq!(resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");

        // Traversal escape → 404.
        let resp = serve_app_file(&store, "task-snake", "/../../MANIFEST.json", false).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Unknown file → 404.
        let resp = serve_app_file(&store, "task-snake", "/nope.js", false).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
