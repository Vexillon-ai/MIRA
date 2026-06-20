// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Stage the built React SPA (`web/dist/`, produced by `npm run build` — the
// CI `web-build` job) into `$OUT_DIR/web-embed` so it can be embedded into the
// binary via `include_dir!` (see `src/web/static_files.rs`). Embedding makes a
// single self-contained binary that serves the web UI with no on-disk
// `web/dist`.
//
// We stage into OUT_DIR (rather than embedding `web/dist` directly) so:
//   - the source tree stays clean, and
//   - a dev build that hasn't run `npm run build` still compiles: we write a
//     placeholder `index.html` and the server shows the "build the UI" page.

use std::fs;
use std::path::Path;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");

    let src_dist = Path::new(&manifest).join("web").join("dist");
    let dst = Path::new(&out_dir).join("web-embed");

    // Always start from a clean staging dir so stale files never linger.
    let _ = fs::remove_dir_all(&dst);
    fs::create_dir_all(&dst).expect("create web-embed staging dir");

    if src_dist.join("index.html").is_file() {
        copy_dir_recursive(&src_dist, &dst);
    } else {
        // SPA not built (e.g. a cargo-only dev build). Embed a placeholder so
        // include_dir! has a valid directory; the page explains how to build it.
        fs::write(dst.join("index.html"), PLACEHOLDER_HTML).expect("write placeholder index.html");
    }

    // Re-run when the built SPA changes (or this script does).
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=build.rs");
}

fn copy_dir_recursive(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).expect("read web/dist") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            fs::create_dir_all(&dest).expect("create subdir");
            copy_dir_recursive(&path, &dest);
        } else {
            fs::copy(&path, &dest).expect("copy file into web-embed");
        }
    }
}

const PLACEHOLDER_HTML: &str = r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>MIRA Web</title></head>
<body style="background:#0d1117;color:#e6edf3;font-family:system-ui;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
<div style="text-align:center;max-width:560px;padding:0 1.5rem">
  <h1 style="font-size:2rem;margin-bottom:0.5rem">MIRA</h1>
  <p style="color:#8b949e">This build was compiled without the web UI. Run <code>npm run build</code> in <code>web/</code> and rebuild, or point <code>MIRA_WEB_DIR</code> at a built <code>web/dist</code>.</p>
  <p style="color:#58a6ff;font-size:0.875rem">Backend API is running at <code>/api/status</code>.</p>
</div>
</body></html>"#;
