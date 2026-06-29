// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/pyodide.rs
//! Scientific-Python code-execution backend (Pyodide-on-Node).
//!
//! Pyodide is CPython compiled to WebAssembly (emscripten) plus the full
//! scientific stack — numpy, pandas, matplotlib, scipy, … — with on-demand
//! wheel loading. Unlike the pure-WASI backend (`wasm.rs`, wasmtime) it needs
//! a JavaScript host, so we run it under the **Node runtime MIRA already
//! provisions for MCP**. That's the whole trade:
//!
//! * **wasmtime/WASI** (`wasm.rs`) — strongest isolation (no host process is
//!   privileged), pure-Python + stdlib only. The default.
//! * **Pyodide-on-Node** (this file) — full scientific stack + matplotlib→PNG
//!   artifacts, but the *Node host* is an ordinary OS process (the Python
//!   inside it still can't touch syscalls/FS/net beyond what we grant). Weaker
//!   boundary; fine for semi-trusted code. Opt-in / auto-selected when a
//!   script imports a scientific package.
//!
//! Provisioning downloads the pinned `pyodide` npm tarball (~6 MB) and unpacks
//! it under `<data_dir>/deps/pyodide/node_modules/pyodide`, writes the embedded
//! `pyodide_runner.mjs` alongside it, and (optionally) pre-warms a wheel cache.
//! Built only under the `sandbox-wasm` feature (it shares the "no Docker, runs
//! everywhere" goal and the deps layout). See design-docs/code-execution-sandbox.md.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use super::{CodeSandbox, Language, ResourceLimits, SandboxError, SandboxOutput};

/// One line of the runner's stdout carries the JSON result behind this marker.
const RESULT_MARKER: &str = "__MIRA_PYODIDE_RESULT__";

/// Embedded Node runner — written to the deps dir at provision time so a
/// rebuilt binary always ships a matching runner (no drift between Rust and JS).
const RUNNER_SRC: &str = include_str!("pyodide_runner.mjs");

// ── Pinned Pyodide distribution ────────────────────────────────────────────────

/// Pinned Pyodide version (Python 3.12 wheels). Bump deliberately — newer
/// Pyodide ships newer numpy/pandas, but also new wheel ABIs.
pub const PYODIDE_VERSION: &str = "0.26.4";
const PYODIDE_TARBALL_URL: &str = "https://registry.npmjs.org/pyodide/-/pyodide-0.26.4.tgz";
const PYODIDE_TARBALL_SHA256: &str =
    "04c2d423c77ec87025d2a61e7d82fe00355a8f24471d60e012da5b3731e32ea5";

/// Default packages pre-warmed into the on-disk wheel cache so the first
/// scientific run is offline-fast. Overridable via `sandbox.pyodide.prewarm`.
pub const DEFAULT_PREWARM: &[&str] = &["numpy", "pandas", "matplotlib"];

/// `<data_dir>/deps/pyodide` — root of the provisioned distribution.
pub fn pyodide_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("deps").join("pyodide")
}

/// Where the npm package unpacks to (`node_modules/pyodide`), so the runner's
/// bare `import "pyodide"` resolves with cwd = the deps dir.
fn pyodide_pkg_dir(data_dir: &Path) -> PathBuf {
    pyodide_dir(data_dir).join("node_modules").join("pyodide")
}

fn runner_path(data_dir: &Path) -> PathBuf {
    pyodide_dir(data_dir).join("pyodide_runner.mjs")
}

fn cache_dir(data_dir: &Path) -> PathBuf {
    pyodide_dir(data_dir).join("cache")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Resolve the Node binary to run the runner with, plus extra PATH dirs so a
/// managed Node resolves regardless of the service's PATH (mirrors the MCP
/// client's spawn path). Returns `(node_command, extra_path_dirs)`.
fn resolve_node() -> (String, Vec<PathBuf>) {
    (
        crate::install::deps::resolve_mcp_command("node"),
        crate::install::deps::managed_runtime_bin_dirs(),
    )
}

/// Whether the Pyodide distribution looks provisioned (package + runner present).
pub fn is_provisioned(data_dir: &Path) -> bool {
    pyodide_pkg_dir(data_dir).join("pyodide.mjs").is_file() && runner_path(data_dir).is_file()
}

/// Ensure the pinned Pyodide distribution + runner are present, downloading and
/// unpacking the npm tarball (~6 MB) if missing, then (best-effort) pre-warming
/// the wheel cache. Idempotent. Cross-platform — no npm/npx needed (we unpack
/// the tarball ourselves and run plain `node`).
pub async fn ensure_pyodide(data_dir: &Path, prewarm: &[String]) -> Result<(), String> {
    let dir = pyodide_dir(data_dir);
    if !is_provisioned(data_dir) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("create pyodide dir: {e}"))?;

        tracing::info!("pyodide: downloading pinned distribution v{PYODIDE_VERSION} (~6 MB)…");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let bytes = client
            .get(PYODIDE_TARBALL_URL)
            .send()
            .await
            .map_err(|e| format!("download: {}", e.without_url()))?
            .error_for_status()
            .map_err(|e| format!("download status: {}", e.without_url()))?
            .bytes()
            .await
            .map_err(|e| format!("download body: {e}"))?;
        let got = sha256_hex(&bytes);
        if got != PYODIDE_TARBALL_SHA256 {
            return Err(format!(
                "pyodide tarball checksum mismatch (got {got}, expected {PYODIDE_TARBALL_SHA256})"
            ));
        }

        // The npm tarball unpacks every entry under a top-level `package/` dir;
        // strip it so files land directly in node_modules/pyodide.
        let pkg_dir = pyodide_pkg_dir(data_dir);
        if pkg_dir.exists() {
            let _ = std::fs::remove_dir_all(&pkg_dir);
        }
        std::fs::create_dir_all(&pkg_dir).map_err(|e| format!("create pkg dir: {e}"))?;
        let gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(gz);
        for entry in archive.entries().map_err(|e| format!("read tarball: {e}"))? {
            let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
            let path = entry.path().map_err(|e| format!("tar path: {e}"))?.into_owned();
            // Strip the leading `package/` component.
            let rel: PathBuf = path.components().skip(1).collect();
            if rel.as_os_str().is_empty() {
                continue;
            }
            let dest = pkg_dir.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
            }
            entry.unpack(&dest).map_err(|e| format!("unpack {}: {e}", rel.display()))?;
        }
        if !pkg_dir.join("pyodide.mjs").is_file() {
            return Err("pyodide tarball missing pyodide.mjs after unpack".into());
        }

        // Write the embedded runner next to the deps dir.
        std::fs::write(runner_path(data_dir), RUNNER_SRC)
            .map_err(|e| format!("write runner: {e}"))?;
        tracing::info!("pyodide: installed at {}", dir.display());
    }

    // Pre-warm the wheel cache (best-effort — a failure here doesn't block use;
    // the wheels just download on first real run instead).
    let warm: Vec<String> = if prewarm.is_empty() {
        DEFAULT_PREWARM.iter().map(|s| s.to_string()).collect()
    } else {
        prewarm.to_vec()
    };
    std::fs::create_dir_all(cache_dir(data_dir)).ok();
    if let Err(e) = prewarm_cache(data_dir, &warm).await {
        tracing::warn!("pyodide: pre-warm failed (wheels will download on demand): {e}");
    }
    Ok(())
}

/// Load the given packages once with a disk cache set, so their wheels are
/// fetched and cached for later offline runs.
async fn prewarm_cache(data_dir: &Path, packages: &[String]) -> Result<(), String> {
    if packages.is_empty() {
        return Ok(());
    }
    tracing::info!("pyodide: pre-warming wheel cache for {}…", packages.join(", "));
    let sandbox = PyodideSandbox::new(data_dir);
    if !sandbox.supported() {
        return Err("node not available".into());
    }
    // Importing the packages triggers loadPackagesFromImports → cached to disk.
    let imports = packages
        .iter()
        .map(|p| format!("import {}", import_name(p)))
        .collect::<Vec<_>>()
        .join("\n");
    let code = format!("{imports}\nprint('prewarm ok')");
    let mut limits = ResourceLimits::default();
    limits.wall_clock = Duration::from_secs(300);
    let out = sandbox
        .run(Language::Python, &code, None, &limits)
        .await
        .map_err(|e| e.to_string())?;
    if out.exit_code != 0 {
        return Err(format!("pre-warm run failed: {}", out.stderr));
    }
    Ok(())
}

/// Map a pip/package name to its import name for the pre-warm snippet.
fn import_name(pkg: &str) -> &str {
    match pkg {
        "scikit-learn" => "sklearn",
        "pillow" => "PIL",
        "matplotlib" => "matplotlib.pyplot",
        other => other,
    }
}

pub struct PyodideSandbox {
    node: String,
    extra_path: Vec<PathBuf>,
    deps_dir: PathBuf,
    runner: PathBuf,
    cache: PathBuf,
    ready: bool,
}

impl PyodideSandbox {
    /// Build a handle against a provisioned `<data_dir>/deps/pyodide`. Cheap —
    /// no compilation; the heavy lifting (loadPyodide) happens per `run()` in
    /// the Node child.
    pub fn new(data_dir: &Path) -> Self {
        let (node, extra_path) = resolve_node();
        Self {
            node,
            extra_path,
            deps_dir: pyodide_dir(data_dir),
            runner: runner_path(data_dir),
            cache: cache_dir(data_dir),
            ready: is_provisioned(data_dir),
        }
    }
}

#[derive(serde::Serialize)]
struct PyRequest<'a> {
    code: &'a str,
    load_from_imports: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdin: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_dir: Option<String>,
    cache_dir: String,
    mem_mb: u64,
}

#[derive(serde::Deserialize)]
struct PyResult {
    ok: bool,
    stdout: String,
    stderr: String,
    error: Option<String>,
}

#[async_trait]
impl CodeSandbox for PyodideSandbox {
    async fn run(
        &self,
        language: Language,
        payload: &str,
        stdin: Option<&str>,
        limits: &ResourceLimits,
    ) -> Result<SandboxOutput, SandboxError> {
        match language {
            Language::Python => {}
            _ => {
                return Err(SandboxError::Policy(
                    "the Pyodide sandbox supports Python only".into(),
                ))
            }
        }
        if !self.ready {
            return Err(SandboxError::Policy(
                "Pyodide runtime not provisioned".into(),
            ));
        }

        // The first extra_writable_mount's host source is our artifact dir; the
        // runner mounts it at /tmp/output (matching SANDBOX_OUTPUT_DIR).
        let output_dir = limits
            .extra_writable_mounts
            .first()
            .map(|(host, _)| host.to_string_lossy().into_owned());

        let req = PyRequest {
            code: payload,
            load_from_imports: true,
            stdin,
            output_dir,
            cache_dir: self.cache.to_string_lossy().into_owned(),
            mem_mb: limits.memory_bytes / (1024 * 1024),
        };
        let req_json = serde_json::to_string(&req)
            .map_err(|e| SandboxError::SpawnFailed(format!("encode request: {e}")))?;

        // Write the request to a temp file (avoids giant argv / shell quoting).
        let req_file = tempfile::Builder::new()
            .prefix("mira-pyodide-")
            .suffix(".json")
            .tempfile()
            .map_err(SandboxError::Io)?;
        std::fs::write(req_file.path(), req_json.as_bytes()).map_err(SandboxError::Io)?;

        // PATH augmented with managed runtime dirs so `node` resolves.
        let path_var = {
            let mut dirs = self.extra_path.clone();
            if let Some(existing) = std::env::var_os("PATH") {
                dirs.extend(std::env::split_paths(&existing));
            }
            std::env::join_paths(dirs).unwrap_or_default()
        };
        // Cap V8's heap roughly in line with the memory limit (advisory — the
        // real isolation is that user Python can't escape wasm).
        let heap_mb = (limits.memory_bytes / (1024 * 1024)).clamp(256, 4096);

        let mut cmd = tokio::process::Command::new(&self.node);
        cmd.arg(format!("--max-old-space-size={heap_mb}"))
            .arg(&self.runner)
            .arg(req_file.path())
            .current_dir(&self.deps_dir)
            .env("PATH", &path_var)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let start = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| SandboxError::SpawnFailed(format!("spawn node: {e}")))?;

        // Drain both pipes concurrently so a chatty child never blocks on a full
        // pipe, then wait for exit — all under one wall-clock deadline.
        let mut child_out = child.stdout.take().expect("piped stdout");
        let mut child_err = child.stderr.take().expect("piped stderr");
        let read_out = async move {
            let mut buf = Vec::new();
            let _ = child_out.read_to_end(&mut buf).await;
            buf
        };
        let read_err = async move {
            let mut buf = Vec::new();
            let _ = child_err.read_to_end(&mut buf).await;
            buf
        };

        let combined = async {
            let (o, e, status) = tokio::join!(read_out, read_err, child.wait());
            (o, e, status)
        };

        let (out_bytes, err_bytes, _status) = match tokio::time::timeout(limits.wall_clock, combined).await {
            Ok(v) => v,
            Err(_) => {
                // Deadline blown — kill the Node host (kill_on_drop also covers
                // the early-return path) and report a clean timeout.
                let _ = child.start_kill();
                return Err(SandboxError::Timeout(limits.wall_clock.as_millis() as u64));
            }
        };
        let duration_ms = start.elapsed().as_millis() as u64;

        let raw_stdout = String::from_utf8_lossy(&out_bytes);
        let host_stderr = String::from_utf8_lossy(&err_bytes).into_owned();

        // Find the result marker line; everything before it on stdout is host
        // noise (Pyodide's loader chatter goes to our captured stderr already).
        let parsed: Option<PyResult> = raw_stdout
            .lines()
            .rev()
            .find_map(|l| l.find(RESULT_MARKER).map(|i| &l[i + RESULT_MARKER.len()..]))
            .and_then(|json| serde_json::from_str(json).ok());

        let Some(res) = parsed else {
            return Err(SandboxError::SpawnFailed(format!(
                "no result from pyodide runner (node stderr: {})",
                host_stderr.chars().take(2000).collect::<String>()
            )));
        };

        let max_out = limits.max_output_bytes.max(1);
        let mut stdout = res.stdout;
        let mut stderr = res.stderr;
        if let Some(err) = res.error {
            if !res.ok {
                // Surface the Python exception on stderr where callers expect it.
                if !stderr.is_empty() {
                    stderr.push('\n');
                }
                stderr.push_str(&err);
            }
        }
        let truncated = stdout.len() > max_out || stderr.len() > max_out;
        if stdout.len() > max_out {
            stdout.truncate(max_out);
        }
        if stderr.len() > max_out {
            stderr.truncate(max_out);
        }

        Ok(SandboxOutput {
            stdout,
            stderr,
            exit_code: if res.ok { 0 } else { 1 },
            duration_ms,
            truncated,
        })
    }

    fn name(&self) -> &'static str {
        "pyodide"
    }

    // Supported once provisioned and a Node binary is resolvable. We don't shell
    // out to check node here (cheap-construction contract); a missing node
    // surfaces as a SpawnFailed at run time.
    fn supported(&self) -> bool {
        self.ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Full end-to-end: download + unpack the pinned Pyodide dist, then run a
    // numpy + matplotlib script that writes a PNG to the mounted output dir.
    // Hits the network (npm + jsdelivr) and needs Node on PATH, so it's
    // `#[ignore]` — run explicitly with:
    //   cargo test --features sandbox-wasm pyodide_end_to_end -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn pyodide_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        ensure_pyodide(data_dir, &[]).await.expect("provision pyodide");
        assert!(is_provisioned(data_dir), "should be provisioned");

        let sandbox = PyodideSandbox::new(data_dir);
        assert!(sandbox.supported());

        let out_dir = tmp.path().join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let mut limits = ResourceLimits::default();
        limits.wall_clock = Duration::from_secs(120);
        limits.extra_writable_mounts = vec![(out_dir.clone(), PathBuf::from("/tmp/output"))];

        let code = r#"
import numpy as np
import pandas as pd
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
df = pd.DataFrame({"x": np.arange(5), "y": np.arange(5) ** 2})
print("mean_y=", df["y"].mean())
plt.plot(df["x"], df["y"])
plt.savefig("/tmp/output/chart.png")
print("done")
"#;
        let res = sandbox
            .run(Language::Python, code, None, &limits)
            .await
            .expect("run ok");
        eprintln!("exit={} stdout={:?} stderr={:?}", res.exit_code, res.stdout, res.stderr);
        assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
        assert!(res.stdout.contains("mean_y= 6.0"), "stdout: {}", res.stdout);
        assert!(res.stdout.contains("done"));
        let png = out_dir.join("chart.png");
        assert!(png.is_file(), "chart.png should be written to the host output dir");
        assert!(std::fs::metadata(&png).unwrap().len() > 1000, "png should be non-trivial");
    }
}
