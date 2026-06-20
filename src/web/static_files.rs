// SPDX-License-Identifier: AGPL-3.0-or-later

// src/web/static_files.rs
//! SPA static file serving.
//!
//! The React bundle (`web/dist/`, produced by `npm run build`) is **embedded
//! into the binary** at compile time via `include_dir!` (staged into `OUT_DIR`
//! by `build.rs`), so a single self-contained binary serves the UI with no
//! files on disk. A disk `web/dist/` still takes precedence when present, so
//! `cargo run` from the repo picks up live rebuilds without recompiling.
//!
//! Resolution order:
//!   1. `MIRA_WEB_DIR` env var (absolute path) — disk override
//!   2. `<cwd>/web/dist/` — works for `cargo run` from the project root
//!   3. `<binary_dir>/../../../web/dist/` — `target/release/mira` → repo
//!   4. the **embedded** bundle baked into the binary (the normal case for a
//!      released build)
//!
//! If the binary was compiled without a built SPA, the embedded bundle is a
//! placeholder page explaining how to build it.

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

use include_dir::{include_dir, Dir};

/// The React SPA, embedded at compile time. `build.rs` stages `web/dist/`
/// (or a placeholder, for builds without a built SPA) into `$OUT_DIR/web-embed`;
/// this bakes it into the binary so no on-disk `web/dist` is needed.
static EMBEDDED_WEB: Dir<'_> = include_dir!("$OUT_DIR/web-embed");

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
        None => Router::new().fallback(embedded_handler),
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

/// Serve the embedded SPA. Looks the request path up in the baked-in bundle,
/// falling back to `index.html` for client-side routes (so `/login`,
/// `/chat/<id>`, etc. work on a full-page load). Cache headers are applied by
/// the `set_cache_headers` layer on the router.
async fn embedded_handler(req: Request) -> Response {
    let raw = req.uri().path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };
    let file = EMBEDDED_WEB
        .get_file(path)
        .or_else(|| EMBEDDED_WEB.get_file("index.html"));
    match file {
        Some(f) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type_for(f.path()))
            .body(Body::from(f.contents()))
            .unwrap(),
        // `index.html` is always embedded (real or placeholder), so this arm is
        // unreachable in practice; 404 rather than panic if it somehow isn't.
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap(),
    }
}

/// Minimal extension → MIME map for the embedded bundle. Vite emits html, js,
/// css, and svg; the rest cover common static asset types.
fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html")         => "text/html; charset=utf-8",
        Some("js" | "mjs")   => "text/javascript; charset=utf-8",
        Some("css")          => "text/css; charset=utf-8",
        Some("svg")          => "image/svg+xml",
        Some("json")         => "application/json",
        Some("webmanifest")  => "application/manifest+json",
        Some("map")          => "application/json",
        Some("ico")          => "image/x-icon",
        Some("png")          => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp")         => "image/webp",
        Some("woff2")        => "font/woff2",
        Some("woff")         => "font/woff",
        Some("ttf")          => "font/ttf",
        Some("txt")          => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
