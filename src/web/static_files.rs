// SPDX-License-Identifier: AGPL-3.0-or-later

// src/web/static_files.rs
//! SPA static file serving.
//!
//! At runtime we look for a built `web/dist/` directory (the React bundle
//! produced by `npm run build`). When found, we serve it via tower-http's
//! `ServeDir` with `index.html` as the SPA fallback so client-side routes
//! like `/login` work. When absent, we serve a minimal placeholder so
//! API-only deployments aren't broken.
//!
//! Resolution order:
//!   1. `MIRA_WEB_DIR` env var (absolute path; set by `mira install`)
//!   2. `<cwd>/web/dist/` — works for `cargo run` from the project root
//!   3. `<binary_dir>/../../../web/dist/` — `target/release/mira` → repo
//!
//! When none match, the placeholder explains how to build the SPA.

use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::Response,
    Router,
};
use tower_http::services::{ServeDir, ServeFile};

/// Try to locate a built `web/dist/` on disk. Returns the directory path
/// (containing at least `index.html`) when found.
pub fn resolve_web_dist() -> Option<PathBuf> {
    if let Some(env_dir) = std::env::var_os("MIRA_WEB_DIR") {
        let p = PathBuf::from(env_dir);
        if has_index(&p) { return Some(p); }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join("web/dist");
        if has_index(&p) { return Some(p); }
    }
    if let Ok(exe) = std::env::current_exe() {
        // target/release/mira → ../../../web/dist
        if let Some(repo_guess) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            let p = repo_guess.join("web/dist");
            if has_index(&p) { return Some(p); }
        }
    }
    None
}

fn has_index(dir: &Path) -> bool {
    dir.join("index.html").is_file()
}

/// Build the router that handles every non-API route. When the built SPA
/// is present, serve it via ServeDir + index.html fallback so deep-linking
/// (e.g. `/login`, `/chat/<id>`) works on full-page reload. Otherwise
/// fall back to a placeholder pointing the user at how to build it.
pub fn spa_router() -> Router {
    // SPA fallback service — handles every non-API path that isn't
    // claimed by a more specific route below.
    let spa = match resolve_web_dist() {
        Some(dir) => {
            let index = dir.join("index.html");
            let serve = ServeDir::new(&dir).fallback(ServeFile::new(&index));
            Router::new().fallback_service(serve)
        }
        None => Router::new().fallback(placeholder_handler),
    };

    // Q1.7 — landing page mounted at /landing when web/landing/ exists.
    // `nest_service` mounts a tower service at the prefix and takes
    // precedence over the SPA's catch-all fallback. ServeDir's own
    // fallback to index.html means `/landing/` lands on the index
    // and `/landing/styles.css` etc resolve directly.
    let spa = if let Some(dir) = resolve_landing() {
        let index = dir.join("index.html");
        let serve = ServeDir::new(&dir).fallback(ServeFile::new(&index));
        spa.nest_service("/landing", serve)
    } else {
        spa
    };

    // Cache policy so deploys aren't pinned by the browser's heuristic cache.
    spa.layer(middleware::from_fn(set_cache_headers))
}

/// Set `Cache-Control` on every static response. Content-hashed `/assets/*`
/// (their filename changes every build) are `immutable` and cached for a year;
/// everything else — `index.html`, SPA deep-links, the push service worker —
/// is `no-cache` so the browser revalidates and a new build is picked up on the
/// next load instead of being served stale until a hard refresh. Without this,
/// `ServeDir` sends no `Cache-Control` and browsers heuristically cache
/// `index.html`, pinning users to the old bundle hash after every deploy.
async fn set_cache_headers(req: Request, next: Next) -> Response {
    let immutable = req.uri().path().starts_with("/assets/");
    let mut resp = next.run(req).await;
    let val = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(val));
    resp
}

/// Resolve the landing-page dir. Same precedence ladder as the SPA:
/// MIRA_LANDING_DIR env var → `<cwd>/web/landing/` → `<binary_dir>
/// /../../../web/landing/`. Returns None when none of those exist.
fn resolve_landing() -> Option<PathBuf> {
    if let Some(env_dir) = std::env::var_os("MIRA_LANDING_DIR") {
        let p = PathBuf::from(env_dir);
        if p.join("index.html").is_file() { return Some(p); }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join("web/landing");
        if p.join("index.html").is_file() { return Some(p); }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(repo_guess) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            let p = repo_guess.join("web/landing");
            if p.join("index.html").is_file() { return Some(p); }
        }
    }
    None
}

async fn placeholder_handler(_uri: axum::http::Uri) -> Response {
    let body = r#"<!DOCTYPE html>
<html><head><title>MIRA Web</title></head>
<body style="background:#0d1117;color:#e6edf3;font-family:system-ui;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
<div style="text-align:center;max-width:560px;padding:0 1.5rem">
  <h1 style="font-size:2rem;margin-bottom:0.5rem">MIRA</h1>
  <p style="color:#8b949e">No built web bundle found. Run <code>npm run build</code> in <code>web/</code> to produce <code>web/dist/</code>, or set <code>MIRA_WEB_DIR</code> to its absolute path.</p>
  <p style="color:#58a6ff;font-size:0.875rem">Backend API is running at <code>/api/status</code>.</p>
</div>
</body></html>"#;

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/html")
        .body(Body::from(body))
        .unwrap()
}
