// SPDX-License-Identifier: AGPL-3.0-or-later

// src/mcp/browser.rs
//! Managed Chrome provisioning for the Puppeteer MCP server.
//!
//! The catalog's Puppeteer server (`@modelcontextprotocol/server-puppeteer`)
//! needs a Chrome/Chromium binary. Puppeteer normally self-downloads one on
//! `npm install`, but that's unreliable under a Windows **service** account
//! (the default cache dir `%USERPROFILE%\.cache\puppeteer` the download writes
//! to and the one the launch later reads can differ) — so browser automation
//! "works in WSL but not on Windows".
//!
//! MIRA fixes this the same way it manages the Node/uv runtimes: it downloads a
//! pinned **Chrome for Testing** build into a **service-stable, writable**
//! directory under `~/.mira/deps/puppeteer/`, records the resolved executable
//! path, and injects `PUPPETEER_EXECUTABLE_PATH` (+ `PUPPETEER_CACHE_DIR`) into
//! the server's environment at spawn. Pointing Puppeteer straight at the
//! executable side-steps Puppeteer's version-pinned browser resolution, so it
//! works regardless of which Chrome build we fetched.
//!
//! The download + unzip is done **natively in Rust** (reqwest + the `zip`
//! crate) rather than via `@puppeteer/browsers`, which shells out to a system
//! `unzip` on Linux (often absent → extraction fails). So this needs no Node,
//! no `npx`, and no external archive tool — just network access.

use std::collections::HashMap;
use std::path::PathBuf;

/// Chrome-for-Testing version manifest (stable channel + per-platform URLs).
const CFT_VERSIONS_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";

/// `~/.mira/deps/puppeteer` — the managed Chrome cache (and `--path` target).
pub fn cache_dir() -> Option<PathBuf> {
    crate::install::deps::dep_install_dir("puppeteer").ok()
}

/// File holding the absolute path to the provisioned Chrome executable, written
/// by [`ensure_chrome`]. A marker keeps [`chrome_path`] cheap + cross-platform
/// (no guessing the `chrome/<platform>-<ver>/…` layout per OS).
fn marker_path() -> Option<PathBuf> {
    cache_dir().map(|d| d.join(".chrome-path"))
}

/// The provisioned Chrome executable, if present and still on disk. Scan-only —
/// never downloads. Used at MCP spawn to decide whether to inject
/// `PUPPETEER_EXECUTABLE_PATH`.
pub fn chrome_path() -> Option<PathBuf> {
    let marker = marker_path()?;
    let raw = std::fs::read_to_string(&marker).ok()?;
    let p = PathBuf::from(raw.trim());
    if p.is_file() { Some(p) } else { None }
}

/// True when an MCP server row is the Puppeteer browser server (so we should
/// manage Chrome for it). Matches the package name in the command/args or any
/// `PUPPETEER*` env key the user set.
pub fn server_uses_puppeteer(
    command: Option<&str>,
    args:    &[String],
    env:     &HashMap<String, String>,
) -> bool {
    if env.keys().any(|k| k.to_ascii_uppercase().starts_with("PUPPETEER")) {
        return true;
    }
    if command.is_some_and(|c| c.contains("puppeteer")) {
        return true;
    }
    args.iter().any(|a| a.contains("puppeteer"))
}

/// Chrome-for-Testing platform key + the path of the executable inside the
/// extracted `chrome-<platform>/` folder, for the current target.
fn platform_target() -> Result<(&'static str, &'static str), String> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ("linux64", "chrome-linux64/chrome"),
        ("windows", "x86_64") => ("win64", "chrome-win64/chrome.exe"),
        ("windows", "x86") => ("win32", "chrome-win32/chrome.exe"),
        ("macos", "aarch64") => (
            "mac-arm64",
            "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        ),
        ("macos", "x86_64") => (
            "mac-x64",
            "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        ),
        (os, arch) => return Err(format!("no Chrome-for-Testing build for {os}/{arch}")),
    })
}

/// Ensure a managed Chrome is installed under [`cache_dir`], downloading the
/// pinned Chrome-for-Testing stable build (~150–200 MB) and unzipping it
/// natively if missing. Returns the executable path. Idempotent: instant no-op
/// once the marker points at a live binary, unless `force`.
///
/// No external tools required (no Node/npx/unzip) — just network access.
pub async fn ensure_chrome(force: bool) -> Result<PathBuf, String> {
    if !force {
        if let Some(p) = chrome_path() {
            return Ok(p);
        }
    }
    let (platform, exe_subpath) = platform_target()?;
    let dir = cache_dir().ok_or("could not resolve ~/.mira/deps/puppeteer")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create cache dir: {e}"))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    // 1) Resolve the stable build's download URL for this platform.
    let manifest: serde_json::Value = client
        .get(CFT_VERSIONS_URL)
        .send()
        .await
        .map_err(|e| format!("fetch CfT manifest: {}", e.without_url()))?
        .error_for_status()
        .map_err(|e| format!("CfT manifest status: {}", e.without_url()))?
        .json()
        .await
        .map_err(|e| format!("parse CfT manifest: {e}"))?;

    let stable = &manifest["channels"]["Stable"];
    let version = stable["version"].as_str().unwrap_or("unknown").to_string();
    let url = stable["downloads"]["chrome"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|d| d["platform"].as_str() == Some(platform))
                .and_then(|d| d["url"].as_str())
        })
        .ok_or_else(|| format!("no Chrome download URL for platform {platform}"))?
        .to_string();

    tracing::info!("mcp/puppeteer: downloading managed Chrome {version} ({platform}, ~150 MB) into {}…", dir.display());

    // 2) Download the zip.
    let bytes = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("download Chrome: {}", e.without_url()))?
        .error_for_status()
        .map_err(|e| format!("Chrome download status: {}", e.without_url()))?
        .bytes()
        .await
        .map_err(|e| format!("read Chrome body: {e}"))?;

    // 3) Extract natively (zip crate restores unix perms, so chrome stays +x).
    let dir2 = dir.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let cursor = std::io::Cursor::new(bytes);
        let mut zip = zip::ZipArchive::new(cursor).map_err(|e| format!("open zip: {e}"))?;
        zip.extract(&dir2).map_err(|e| format!("extract zip: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("extract task: {e}"))??;

    let exec = dir.join(exe_subpath);
    if !exec.is_file() {
        return Err(format!("Chrome binary not found after extract at {}", exec.display()));
    }
    // Belt-and-suspenders: ensure the executable bit on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&exec) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o755);
            let _ = std::fs::set_permissions(&exec, perms);
        }
    }

    if let Some(marker) = marker_path() {
        let _ = std::fs::write(&marker, exec.to_string_lossy().as_bytes());
    }
    tracing::info!("mcp/puppeteer: managed Chrome {version} ready at {}", exec.display());
    Ok(exec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_puppeteer_servers() {
        let empty = HashMap::new();
        assert!(server_uses_puppeteer(
            Some("npx"),
            &["-y".into(), "@modelcontextprotocol/server-puppeteer".into()],
            &empty,
        ));
        let mut env = HashMap::new();
        env.insert("PUPPETEER_LAUNCH_OPTIONS".into(), "{}".into());
        assert!(server_uses_puppeteer(Some("node"), &[], &env));
        // Unrelated server.
        assert!(!server_uses_puppeteer(
            Some("npx"),
            &["-y".into(), "@modelcontextprotocol/server-filesystem".into()],
            &empty,
        ));
    }

    // Full provisioning: fetch the CfT manifest, download + natively unzip
    // Chrome, then run `chrome --version`. Hits the network + writes ~500 MB,
    // so it's `#[ignore]`. Run with:
    //   cargo test --lib mcp::browser::tests::downloads_and_runs -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn downloads_and_runs_chrome() {
        // Force the cache into a temp dir so we don't clobber a real install.
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("HOME", tmp.path()); }

        let exec = ensure_chrome(true).await.expect("provision chrome");
        assert!(exec.is_file(), "chrome should exist at {}", exec.display());
        // Marker round-trips.
        assert_eq!(chrome_path().as_deref(), Some(exec.as_path()));

        let out = std::process::Command::new(&exec)
            .arg("--version")
            .output()
            .expect("run chrome --version");
        let ver = String::from_utf8_lossy(&out.stdout);
        eprintln!("chrome --version → {ver}");
        assert!(ver.to_lowercase().contains("chrome"), "got: {ver}");
    }
}
