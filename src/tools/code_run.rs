// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/code_run.rs
//! `code_run` — Tier 4 sandboxed code execution (5 iteration B).
//!
//! Runs a short script in the prebaked rootfs and returns its stdout/stderr +
//! exit code to the model. Admin-visibility only this iteration; per-user opt-in
//! arrives in a later phase.
//!
//! ## Why admin-only
//!
//! The sandbox already kills escape primitives via seccomp + namespaces, so
//! the *kernel-level* blast radius is bounded. The reason this tool is gated
//! to admins is product-shaped, not security-shaped: we want telemetry on
//! actual model usage (audit row per call, see `tools/audit.rs`) before
//! surfacing it to every user. Promotion to `User` is its own task.
//!
//! ## Wiring
//!
//! Construction takes:
//! - the rootfs pivot path (host-side), required and pre-validated by the
//! builder — the tool itself only checks `is_dir()` defensively at call
//! time so a mid-run uninstall is reported clearly.
//! - the `CodeSandbox` backend, usually `sandbox::default_backend()`.
//! - the `CodeRunConfig` from `MiraConfig.sandbox.code_run` for the language
//! allowlist + per-call resource ceilings.
//!
//! ## Per-call resource limits
//!
//! Every invocation builds a fresh `ResourceLimits` from defaults, then:
//! 1. Sets `rootfs` to the configured pivot path.
//! 2. Clamps `wall_clock` to `min(caller_request, max_wall_clock_seconds)`.
//! 3. Sets `memory_bytes` from `max_memory_mb`.
//!
//! `disable_network`, `cpu_seconds`, and `nproc` use `ResourceLimits::default()`
//! values — exposing them through tool args invites footguns and the defaults
//! are already conservative.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult, ToolVisibility};
use crate::artifacts::{ArtifactStore, ALLOWED_EXTENSIONS};
use crate::config::CodeRunConfig;
use crate::sandbox::{CodeSandbox, Language, ResourceLimits, SandboxError, SeccompMode};
use crate::MiraError;

// Where in the sandbox the per-call host scratch dir is bind-mounted.
// Must match what we tell the model in the tool description, and the
// single-segment-under-/tmp constraint enforced by the Linux backend.
const SANDBOX_OUTPUT_DIR: &str = "/tmp/output";

// Hard cap on the number of artifacts captured per call. Past this, the
// extras are silently dropped (with a note in the tool output) so a runaway
// loop can't fill the artifact store.
const MAX_ARTIFACTS_PER_CALL: usize = 16;

// Per-file size cap for captured artifacts. A 100x100 PNG is ~3 KB; even a
// detailed chart rarely exceeds 2 MB. Bigger files are skipped with a note.
const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;

// Hard cap on the `code` payload in bytes. Beyond this we bounce — long
// scripts probably want a file in the rootfs anyway, and `-c` has its own
// kernel-side ARG_MAX limit we should stay well under.
const MAX_CODE_BYTES: usize = 64 * 1024;

// Hard cap on the `stdin` payload.
const MAX_STDIN_BYTES: usize = 64 * 1024;

// Hard cap on `working_dir`. Long enough for any reasonable in-rootfs path,
// short enough that we don't risk PATH_MAX issues at the kernel boundary.
const MAX_WORKING_DIR_LEN: usize = 256;

// Packages that, when imported, route a call to the scientific backend
// (Pyodide). They need native/binary wheels Pyodide ships but the pure-WASI /
// namespace-stdlib backends can't import, so the default backend would fail.
// Matched as whole words against import targets — a cheap heuristic, not a real
// parser. Kept here (always-compiled) so routing works regardless of the
// `sandbox-wasm` feature gate on the pyodide module.
//
// `import X` exposes the *import* name (e.g. `sklearn`, `cv2`, `PIL`, `skimage`,
// `bs4`), not the pip name — list both where they differ.
const SCIENTIFIC_PACKAGES: &[&str] = &[
    "numpy", "pandas", "matplotlib", "scipy",
    "sklearn", "scikit-learn", "skimage", "scikit-image",
    "sympy", "statsmodels", "networkx",
    "PIL", "pillow", "cv2", "opencv-python", "opencv",
    "shapely", "geopandas", "lxml",
    "seaborn", "plotly", "bokeh", "altair",
    "numba", "xarray", "h5py",
];

// Substrings that, anywhere in the code, signal Pyodide is needed regardless of
// the import shape: `micropip` only exists inside Pyodide, and a `savefig` call
// implies matplotlib output even if the import was dynamic.
const SCIENTIFIC_STRONG_SIGNALS: &[&str] = &["micropip", ".savefig("];

// Pull the candidate module token(s) out of a single source line that performs
// an import — static (`import x`, `import x.y as z`, `from x.y import z`) or
// dynamic (`importlib.import_module("x")`, `__import__('x')`). Returns the names
// lowercased-as-written for whole-word comparison. Cheap and forgiving: it errs
// toward yielding a token rather than missing one.
fn import_targets(line: &str) -> Vec<String> {
    let t = line.trim_start();
    let mut out = Vec::new();

    // Dynamic imports: grab whatever's inside the first quoted string.
    if t.contains("import_module(") || t.contains("__import__(") {
        if let Some(name) = quoted_arg(t) {
            // For "a.b.c" the top package is what gets resolved/installed.
            out.push(name.split('.').next().unwrap_or(&name).to_string());
        }
    }

    // Static imports.
    let rest = if let Some(r) = t.strip_prefix("from ") {
        // `from pkg.sub import x` → first token before whitespace.
        r.split_whitespace().next().map(str::to_string)
    } else if let Some(r) = t.strip_prefix("import ") {
        // `import a, b.c as d` → each comma group's first dotted root.
        for grp in r.split(',') {
            if let Some(tok) = grp.split_whitespace().next() {
                out.push(tok.split('.').next().unwrap_or(tok).to_string());
            }
        }
        None
    } else {
        None
    };
    if let Some(r) = rest {
        out.push(r.split('.').next().unwrap_or(&r).to_string());
    }
    out
}

// First single- or double-quoted substring on a line, if any.
fn quoted_arg(line: &str) -> Option<String> {
    for q in ['"', '\''] {
        if let Some(start) = line.find(q) {
            if let Some(len) = line[start + 1..].find(q) {
                return Some(line[start + 1..start + 1 + len].to_string());
            }
        }
    }
    None
}

// True when the script wants the scientific stack — used to route to the
// scientific backend when one is wired. Looks at static + dynamic import
// targets (whole-word match against the package list) plus strong signals like
// `micropip` / `savefig`.
fn wants_scientific(code: &str) -> bool {
    if SCIENTIFIC_STRONG_SIGNALS.iter().any(|s| code.contains(s)) {
        return true;
    }
    for line in code.lines() {
        for target in import_targets(line) {
            if SCIENTIFIC_PACKAGES.iter().any(|p| p.eq_ignore_ascii_case(&target)) {
                return true;
            }
        }
    }
    false
}

pub struct CodeRunTool {
    sandbox:      Arc<dyn CodeSandbox>,
    // Optional second backend for scientific Python (Pyodide). When present and
    // the script imports a scientific package, the call routes here instead of
    // `sandbox`. None on builds/hosts without it — routing then no-ops.
    scientific:   Option<Arc<dyn CodeSandbox>>,
    // Whether the scientific (Pyodide) backend is *intended* per config
    // (`sandbox.pyodide.enabled` / `backend = "pyodide"`), regardless of whether
    // it's provisioned yet. Lets a chart/scientific request that lands without a
    // working backend explain WHY — "enabled but still downloading / Node
    // missing" vs "not enabled" — instead of a bare ModuleNotFoundError the model
    // papers over with ASCII art.
    scientific_enabled: bool,
    rootfs_path:  PathBuf,
    cfg:          CodeRunConfig,
    seccomp_mode: SeccompMode,
    artifacts:    Arc<ArtifactStore>,
}

impl CodeRunTool {
    // Construct the tool. `rootfs_path` is the host-side pivot directory
    // (e.g. `RootfsManager::python_pivot_root()`). `sandbox` is the backend
    // to use — `sandbox::default_backend()` in production. `seccomp_mode`
    // controls which seccomp filter the per-call `ResourceLimits` requests.
    // `artifacts` is the store for image files the script writes to
    // `/tmp/output/` — they get content-addressed and surfaced as
    // markdown image refs in the tool output.
    pub fn new(
        sandbox:      Arc<dyn CodeSandbox>,
        rootfs_path:  PathBuf,
        cfg:          CodeRunConfig,
        seccomp_mode: SeccompMode,
        artifacts:    Arc<ArtifactStore>,
    ) -> Self {
        Self { sandbox, scientific: None, scientific_enabled: false, rootfs_path, cfg, seccomp_mode, artifacts }
    }

    // Attach a scientific (Pyodide) backend. Calls whose code imports a
    // scientific package route here; everything else stays on the primary
    // backend. `None` is a no-op (keeps the call sites tidy).
    pub fn with_scientific(mut self, backend: Option<Arc<dyn CodeSandbox>>) -> Self {
        self.scientific = backend;
        self
    }

    // Record whether the scientific backend is *intended* per config (may still
    // be provisioning). Drives the actionable message when a scientific import
    // lands without a working backend.
    pub fn with_scientific_enabled(mut self, on: bool) -> Self {
        self.scientific_enabled = on;
        self
    }

    // Can the effective backend actually run the scientific stack? True when a
    // Pyodide backend is wired, or the primary backend *is* Pyodide.
    fn scientific_capable(&self) -> bool {
        self.scientific.is_some() || self.sandbox.name() == "pyodide"
    }

    // Pick the backend for this call: the scientific one when it's wired and
    // the script wants it, otherwise the primary backend.
    fn select_backend(&self, code: &str) -> &Arc<dyn CodeSandbox> {
        match &self.scientific {
            Some(sci) if wants_scientific(code) => sci,
            _ => &self.sandbox,
        }
    }

    fn parse_language(&self, raw: &str) -> Result<Language, String> {
        let lang = raw.trim().to_lowercase();
        if !self.cfg.allowed_languages.iter().any(|a| a == &lang) {
            return Err(format!(
                "language `{lang}` not in allowed_languages ({})",
                self.cfg.allowed_languages.join(", ")
            ));
        }
        match lang.as_str() {
            "python" => Ok(Language::Python),
            // Iteration B ships python only; allowlist gates the rest before
            // we get here, but keep the match exhaustive for clarity.
            other    => Err(format!("language `{other}` is allowlisted but not yet wired")),
        }
    }

    // Only the Linux "namespace" backend pivots into a host rootfs; the WASM
    // and Pyodide backends isolate differently (capability preopen / wasm-in-V8)
    // and have no rootfs to pivot into. Used to skip the rootfs-dir pre-flight
    // and `limits.rootfs` for those backends.
    fn backend_needs_rootfs(name: &str) -> bool {
        name == "namespace"
    }

    fn build_limits(
        &self,
        backend_name:           &str,
        requested_timeout_secs: Option<u64>,
        working_dir:            Option<PathBuf>,
        host_output_dir:        Option<PathBuf>,
    ) -> ResourceLimits {
        let mut limits = ResourceLimits::default();
        // Namespace backend pivots into the rootfs; WASM/Pyodide ignore it.
        limits.rootfs = if Self::backend_needs_rootfs(backend_name) {
            Some(self.rootfs_path.clone())
        } else {
            None
        };

        let cap_secs = self.cfg.max_wall_clock_seconds.max(1);
        let chosen   = requested_timeout_secs.unwrap_or(cap_secs).min(cap_secs).max(1);
        limits.wall_clock  = Duration::from_secs(chosen);
        // Keep CPU floor below wall clock — RLIMIT_CPU is whole seconds and
        // a 0 here would SIGKILL immediately.
        limits.cpu_seconds = chosen;

        limits.memory_bytes = self.cfg.max_memory_mb.saturating_mul(1024 * 1024);
        limits.working_dir  = working_dir;
        limits.seccomp_mode = self.seccomp_mode;

        if let Some(host) = host_output_dir {
            limits.extra_writable_mounts = vec![
                (host, PathBuf::from(SANDBOX_OUTPUT_DIR)),
            ];
        }
        limits
    }
}

#[async_trait]
impl Tool for CodeRunTool {
    fn name(&self) -> &str { "code_run" }

    fn description(&self) -> &str {
        "Execute a short script in an isolated, read-only sandbox. Currently \
         supports Python only. The script runs with no network, a small \
         memory cap, and a wall-clock timeout. /tmp is writable for the \
         duration of the call but is discarded afterwards. Use this when you \
         need to compute something a calculator can't, or run a tiny one-off \
         script — not for long-running tasks. Returns the script's exit \
         code, stdout, and stderr.\n\n\
         THIS is the right tool for real, accurate data charts and \
         visualizations — pie/bar/line charts, BI/business graphs, plots — \
         via Python's scientific stack (matplotlib, numpy, pandas, and other \
         common libraries are available; missing wheels are fetched on demand). \
         Prefer this over `image_generate` for anything data-driven: \
         `image_generate` makes artistic pictures and will NOT produce an \
         accurate chart. Never hand-draw a chart as ASCII/text when a real one \
         is possible — write matplotlib and save a PNG.\n\n\
         To show the user an image (chart, plot, generated picture, etc.), \
         save it to `/tmp/output/` with one of these extensions: .png, .jpg, \
         .jpeg, .gif, .svg, .webp. Files written there are captured as \
         artifacts and rendered inline in chat — you do NOT need to print \
         the bytes or base64-encode them. Markdown image references for the \
         saved files are appended to the tool output automatically. When \
         you want to show one of those images in your reply, copy the \
         `![alt](/api/artifacts/<sha>.<ext>)` markdown EXACTLY as printed — \
         keep it relative (no host, no scheme, no protocol prefix). Adding \
         a hostname will break the image."
    }

    fn visibility(&self) -> ToolVisibility { ToolVisibility::Admin }
    fn tier(&self)       -> Tier           { Tier::Code }

    // True only when the rootfs path actually exists. Lets the tools page
    // badge a "rootfs not installed" state without unregistering the tool.
    fn enabled(&self) -> bool {
        // A non-namespace primary backend (WASM) needs no rootfs; the namespace
        // backend needs its rootfs installed. A wired scientific backend also
        // makes the tool useful on its own.
        !Self::backend_needs_rootfs(self.sandbox.name())
            || self.rootfs_path.is_dir()
            || self.scientific.is_some()
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["language", "code"],
            "properties": {
                "language": {
                    "type": "string",
                    "description": "The language runtime to use. Currently `python` only.",
                    "enum": self.cfg.allowed_languages,
                },
                "code": {
                    "type": "string",
                    "description": "The script to run. Up to 64 KB.",
                },
                "stdin": {
                    "type": "string",
                    "description": "Optional bytes to feed the script's stdin. Up to 64 KB.",
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "description": format!(
                        "Optional wall-clock deadline in seconds. Clamped to \
                         the configured ceiling ({}s).",
                        self.cfg.max_wall_clock_seconds
                    ),
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional absolute path inside the sandbox \
                         to chdir into before running. Interpreted in the \
                         post-pivot mount namespace, so `/tmp` is the writable \
                         scratch tmpfs. Defaults to `/`.",
                },
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let lang_raw = args.get("language").and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError("code_run: `language` is required".into()))?;
        let code = args.get("code").and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError("code_run: `code` is required".into()))?;
        let stdin = args.get("stdin").and_then(|v| v.as_str());
        let timeout_secs = args.get("timeout_seconds")
            .and_then(|v| v.as_u64());
        let working_dir_raw = args.get("working_dir").and_then(|v| v.as_str());

        if code.len() > MAX_CODE_BYTES {
            return Ok(ToolResult::failure(format!(
                "code_run: `code` exceeds {}-byte cap (got {})",
                MAX_CODE_BYTES, code.len()
            )));
        }
        if let Some(s) = stdin {
            if s.len() > MAX_STDIN_BYTES {
                return Ok(ToolResult::failure(format!(
                    "code_run: `stdin` exceeds {}-byte cap (got {})",
                    MAX_STDIN_BYTES, s.len()
                )));
            }
        }

        let working_dir = match working_dir_raw {
            None => None,
            Some(p) => match validate_working_dir(p) {
                Ok(pb) => Some(pb),
                Err(e) => return Ok(ToolResult::failure(format!("code_run: {e}"))),
            },
        };

        let language = match self.parse_language(lang_raw) {
            Ok(l)  => l,
            Err(e) => return Ok(ToolResult::failure(format!("code_run: {e}"))),
        };

        // Route the call: scientific backend (Pyodide) when the script imports a
        // scientific package and one is wired, else the primary backend.
        let sci_wanted = wants_scientific(code);

        // The script reaches for the scientific stack (matplotlib/numpy/…) but no
        // backend on this host can run it. Fail with an ACTIONABLE message — and
        // tell the model NOT to fall back to ASCII — instead of letting it hit a
        // bare ModuleNotFoundError on the plain-CPython backend. Only fires on a
        // genuine scientific attempt, per design.
        if sci_wanted && !self.scientific_capable() {
            let why = if self.scientific_enabled {
                "The scientific Python backend (matplotlib/numpy/pandas) is enabled \
                 but not ready on this host yet — it downloads on first enable and \
                 needs a resolvable Node runtime. Retry in a moment; if it keeps \
                 failing, check that MIRA can find Node and that the Pyodide download \
                 finished (see the server logs)."
            } else {
                "This needs the scientific Python backend (matplotlib/numpy/pandas), \
                 which isn't enabled on this host. An admin can turn it on with \
                 `sandbox.pyodide.enabled = true` (it downloads once, then works)."
            };
            return Ok(ToolResult::failure(format!(
                "code_run: {why} Do NOT substitute an ASCII/text chart — tell the user \
                 the chart backend isn't available and why, and offer to retry once it is."
            )));
        }

        let backend = self.select_backend(code);
        let backend_name = backend.name();

        // Defensive: the namespace tool may have been registered when the rootfs
        // was present, then uninstalled mid-process. Surface that as a clean
        // failure instead of letting pre_exec EINVAL out. Only applies to the
        // namespace backend — WASM/Pyodide have no rootfs.
        if Self::backend_needs_rootfs(backend_name) && !self.rootfs_path.is_dir() {
            return Ok(ToolResult::failure(format!(
                "code_run: rootfs missing at {} — run `mira sandbox install python`",
                self.rootfs_path.display()
            )));
        }

        // Per-call host scratch dir, bind-mounted into the sandbox at
        // /tmp/output/. Lives only for the duration of this call — the
        // tempdir guard removes it once we've drained any artifacts.
        let scratch = match tempfile::Builder::new()
            .prefix("mira-coderun-")
            .tempdir()
        {
            Ok(d)  => d,
            Err(e) => return Ok(ToolResult::failure(format!(
                "code_run: cannot create output scratch dir: {e}"
            ))),
        };
        let host_output = scratch.path().to_path_buf();

        let limits = self.build_limits(backend_name, timeout_secs, working_dir, Some(host_output.clone()));

        let result = backend.run(language, code, stdin, &limits).await;

        match result {
            Ok(out) => {
                let mut text = format_output(&out, lang_raw);
                let captured = capture_artifacts(&host_output, &self.artifacts);
                if !captured.is_empty() {
                    text.push_str("--- artifacts ---\n");
                    for line in &captured {
                        text.push_str(line);
                        text.push('\n');
                    }
                }
                Ok(ToolResult::success(text))
            }
            Err(SandboxError::Timeout(ms)) => Ok(ToolResult::failure(format!(
                "code_run: timed out after {} ms (limit {}s)",
                ms, limits.wall_clock.as_secs()
            ))),
            Err(SandboxError::Policy(msg)) => Ok(ToolResult::failure(
                format!("code_run: policy: {msg}")
            )),
            Err(SandboxError::Unsupported) => Ok(ToolResult::failure(
                "code_run: sandbox backend reports the host can't run Tier 4 tools"
            )),
            Err(e) => {
                let mut msg = format!("code_run: sandbox error: {e}");
                if sci_wanted {
                    // A chart/scientific run that got a backend but errored (e.g. Node
                    // couldn't spawn). Keep the model from quietly drawing ASCII.
                    msg.push_str(
                        " — the scientific (chart) backend failed to run. Do NOT substitute \
                         an ASCII/text chart; tell the user the chart backend errored.",
                    );
                }
                Ok(ToolResult::failure(msg))
            }
        }
    }
}

// Scan `dir` for image files (top-level only — no recursion, since the
// model is told to write directly to `/tmp/output/`), save each into the
// artifact store, and return the lines to splice into the tool output.
// // Each line is either a markdown image ref (`![name](/api/artifacts/...)`)
// or a `# skipped:` note explaining why a file was dropped (too big, bad
// extension, count cap hit, read failed). The model sees these in the tool
// result and the user sees the rendered image in the chat bubble.
fn capture_artifacts(dir: &std::path::Path, store: &ArtifactStore) -> Vec<String> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) => {
            out.push(format!("# skipped: cannot read /tmp/output: {e}"));
            return out;
        }
    };

    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() { continue }

        let name = path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("(unknown)")
            .to_string();
        let ext = path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase());
        let Some(ext) = ext else {
            out.push(format!("# skipped {name}: no file extension"));
            continue;
        };
        if !ALLOWED_EXTENSIONS.iter().any(|e| *e == ext) {
            out.push(format!("# skipped {name}: extension `{ext}` not in artifact allowlist"));
            continue;
        }
        if meta.len() > MAX_ARTIFACT_BYTES {
            out.push(format!(
                "# skipped {name}: {} bytes exceeds {}-byte cap",
                meta.len(), MAX_ARTIFACT_BYTES,
            ));
            continue;
        }
        if count >= MAX_ARTIFACTS_PER_CALL {
            out.push(format!(
                "# skipped {name}: per-call artifact cap of {MAX_ARTIFACTS_PER_CALL} reached",
            ));
            continue;
        }

        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                out.push(format!("# skipped {name}: read failed: {e}"));
                continue;
            }
        };
        match store.save_bytes(&bytes, &ext) {
            Ok(id) => {
                out.push(id.markdown_image(&name));
                count += 1;
            }
            Err(e) => {
                out.push(format!("# skipped {name}: store error: {e}"));
            }
        }
    }
    out
}

// Validate the caller-supplied `working_dir`: must be absolute, under the
// length cap, and free of `..` segments (so a curious script can't ask the
// sandbox to chdir up out of `/tmp` before running its payload). The path is
// interpreted in the post-pivot mount namespace, so `/etc` here means the
// rootfs's `/etc`, not the host's.
fn validate_working_dir(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("`working_dir` is empty".into());
    }
    if trimmed.len() > MAX_WORKING_DIR_LEN {
        return Err(format!(
            "`working_dir` exceeds {}-char cap (got {})",
            MAX_WORKING_DIR_LEN, trimmed.len()
        ));
    }
    if !trimmed.starts_with('/') {
        return Err(format!("`working_dir` must be absolute, got `{trimmed}`"));
    }
    if trimmed.split('/').any(|seg| seg == "..") {
        return Err(format!("`working_dir` must not contain `..` segments, got `{trimmed}`"));
    }
    Ok(PathBuf::from(trimmed))
}

// Human-readable summary of a sandbox run. The model consumes this string,
// and the registry truncates it to 512 bytes for the audit row — so put the
// most-useful headers up front.
fn format_output(out: &crate::sandbox::SandboxOutput, language: &str) -> String {
    let mut s = format!(
        "language={language} exit={} duration_ms={} stdout_bytes={} stderr_bytes={}{}\n",
        out.exit_code,
        out.duration_ms,
        out.stdout.len(),
        out.stderr.len(),
        if out.truncated { " truncated=true" } else { "" },
    );
    s.push_str("--- stdout ---\n");
    if out.stdout.is_empty() {
        s.push_str("(empty)\n");
    } else {
        s.push_str(&out.stdout);
        if !out.stdout.ends_with('\n') { s.push('\n'); }
    }
    s.push_str("--- stderr ---\n");
    if out.stderr.is_empty() {
        s.push_str("(empty)\n");
    } else {
        s.push_str(&out.stderr);
        if !out.stderr.ends_with('\n') { s.push('\n'); }
    }
    s
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxOutput;
    use serde_json::json;

    // Recording stub: captures the args of the most recent `run()` call so
    // we can assert on language, payload, and limits without a real child.
    struct StubSandbox {
        out: SandboxOutput,
        last: std::sync::Mutex<Option<(Language, String, Option<String>, ResourceLimits)>>,
    }

    impl StubSandbox {
        fn ok(stdout: &str) -> Arc<Self> {
            Arc::new(Self {
                out: SandboxOutput {
                    stdout:      stdout.to_string(),
                    stderr:      String::new(),
                    exit_code:   0,
                    duration_ms: 7,
                    truncated:   false,
                },
                last: std::sync::Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl CodeSandbox for StubSandbox {
        async fn run(
            &self,
            language: Language,
            payload:  &str,
            stdin:    Option<&str>,
            limits:   &ResourceLimits,
        ) -> Result<SandboxOutput, SandboxError> {
            *self.last.lock().unwrap() = Some((                language,
                payload.to_string(),
                stdin.map(String::from),
                limits.clone(),
            ));
            Ok(self.out.clone())
        }
        // Stands in for the namespace backend in the rootfs-related tests, so
        // report "namespace" — that's the only backend that needs a rootfs.
        fn name(&self)      -> &'static str { "namespace" }
        fn supported(&self) -> bool         { true }
    }

    fn cfg() -> CodeRunConfig {
        CodeRunConfig {
            enabled:                true,
            allowed_languages:      vec!["python".into()],
            max_wall_clock_seconds: 5,
            max_memory_mb:          128,
        }
    }

    // Build a fresh ArtifactStore in its own tempdir. Each test gets an
    // isolated store so they don't interact via the FS.
    fn store() -> (Arc<ArtifactStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s   = ArtifactStore::new(dir.path()).unwrap();
        (Arc::new(s), dir)
    }

    #[tokio::test]
    async fn rejects_disallowed_language() {
        let sb   = StubSandbox::ok("");
        let tmp  = tempfile::tempdir().unwrap();
        let (st, _sd) = store();
        let tool = CodeRunTool::new(sb.clone(), tmp.path().to_path_buf(), cfg(), SeccompMode::Denylist, st);
        let r = tool.execute(json!({"language": "node", "code": "1"})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("not in allowed_languages"));
    }

    #[tokio::test]
    async fn rejects_oversize_code() {
        let sb   = StubSandbox::ok("");
        let tmp  = tempfile::tempdir().unwrap();
        let (st, _sd) = store();
        let tool = CodeRunTool::new(sb, tmp.path().to_path_buf(), cfg(), SeccompMode::Denylist, st);
        let big  = "x".repeat(MAX_CODE_BYTES + 1);
        let r    = tool.execute(json!({"language": "python", "code": big})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("exceeds"));
    }

    #[tokio::test]
    async fn missing_rootfs_dir_returns_failure() {
        let sb   = StubSandbox::ok("");
        let (st, _sd) = store();
        // Path that does not exist on disk.
        let tool = CodeRunTool::new(sb, PathBuf::from("/tmp/nope-xyzzy-mira"), cfg(), SeccompMode::Denylist, st);
        let r    = tool.execute(json!({"language": "python", "code": "print(1)"})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("rootfs missing"));
    }

    #[tokio::test]
    async fn forwards_payload_and_clamps_timeout() {
        let sb   = StubSandbox::ok("hi\n");
        let tmp  = tempfile::tempdir().unwrap();
        let (st, _sd) = store();
        let tool = CodeRunTool::new(sb.clone(), tmp.path().to_path_buf(), cfg(), SeccompMode::Denylist, st);

        // Caller asks for 100s; cfg ceiling is 5s — must clamp.
        let r = tool.execute(json!({
            "language": "python",
            "code":     "print('hi')",
            "stdin":    "world",
            "timeout_seconds": 100,
        })).await.unwrap();
        assert!(r.success, "got: {r:?}");
        assert!(r.output.contains("exit=0"));
        assert!(r.output.contains("hi"));

        let last = sb.last.lock().unwrap();
        let (lang, payload, stdin, limits) = last.as_ref().unwrap();
        assert!(matches!(lang, Language::Python));
        assert_eq!(payload, "print('hi')");
        assert_eq!(stdin.as_deref(), Some("world"));
        assert_eq!(limits.wall_clock.as_secs(), 5);
        assert_eq!(limits.memory_bytes, 128 * 1024 * 1024);
        assert!(limits.rootfs.is_some());
    }

    #[tokio::test]
    async fn enabled_reflects_rootfs_presence() {
        let sb = StubSandbox::ok("");
        let tmp = tempfile::tempdir().unwrap();
        let (st1, _sd1) = store();
        let (st2, _sd2) = store();
        let tool_present = CodeRunTool::new(sb.clone(), tmp.path().to_path_buf(), cfg(), SeccompMode::Denylist, st1);
        assert!(tool_present.enabled());

        let tool_missing = CodeRunTool::new(sb, PathBuf::from("/tmp/nope-xyzzy-mira"), cfg(), SeccompMode::Denylist, st2);
        assert!(!tool_missing.enabled());
    }

    #[tokio::test]
    async fn translates_timeout_error() {
        struct TimeoutSandbox;
        #[async_trait]
        impl CodeSandbox for TimeoutSandbox {
            async fn run(&self, _: Language, _: &str, _: Option<&str>, _: &ResourceLimits)
                -> Result<SandboxOutput, SandboxError> { Err(SandboxError::Timeout(5000)) }
            fn name(&self)      -> &'static str { "to" }
            fn supported(&self) -> bool         { true }
        }
        let tmp  = tempfile::tempdir().unwrap();
        let (st, _sd) = store();
        let tool = CodeRunTool::new(Arc::new(TimeoutSandbox), tmp.path().to_path_buf(), cfg(), SeccompMode::Denylist, st);
        let r = tool.execute(json!({"language": "python", "code": "while True: pass"})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn forwards_working_dir_and_seccomp_mode() {
        let sb   = StubSandbox::ok("");
        let tmp  = tempfile::tempdir().unwrap();
        let (st, _sd) = store();
        let tool = CodeRunTool::new(sb.clone(), tmp.path().to_path_buf(), cfg(), SeccompMode::Allowlist, st);

        let r = tool.execute(json!({
            "language": "python",
            "code":     "print('ok')",
            "working_dir": "/tmp",
        })).await.unwrap();
        assert!(r.success, "got: {r:?}");

        let last = sb.last.lock().unwrap();
        let limits = &last.as_ref().unwrap().3;
        assert_eq!(limits.working_dir.as_deref(), Some(std::path::Path::new("/tmp")));
        assert_eq!(limits.seccomp_mode, SeccompMode::Allowlist);
    }

    #[tokio::test]
    async fn rejects_relative_working_dir() {
        let sb   = StubSandbox::ok("");
        let tmp  = tempfile::tempdir().unwrap();
        let (st, _sd) = store();
        let tool = CodeRunTool::new(sb, tmp.path().to_path_buf(), cfg(), SeccompMode::Denylist, st);
        let r = tool.execute(json!({
            "language": "python",
            "code":     "print(1)",
            "working_dir": "tmp",
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("must be absolute"));
    }

    #[tokio::test]
    async fn rejects_dotdot_in_working_dir() {
        let sb   = StubSandbox::ok("");
        let tmp  = tempfile::tempdir().unwrap();
        let (st, _sd) = store();
        let tool = CodeRunTool::new(sb, tmp.path().to_path_buf(), cfg(), SeccompMode::Denylist, st);
        let r = tool.execute(json!({
            "language": "python",
            "code":     "print(1)",
            "working_dir": "/tmp/../etc",
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains(".."));
    }

    // `execute()` must request a `/tmp/output` bind mount via
    // `extra_writable_mounts` so the script can drop image files there. The
    // host source is the per-call scratch tempdir.
    #[tokio::test]
    async fn requests_tmp_output_bind_mount() {
        let sb   = StubSandbox::ok("");
        let tmp  = tempfile::tempdir().unwrap();
        let (st, _sd) = store();
        let tool = CodeRunTool::new(sb.clone(), tmp.path().to_path_buf(), cfg(), SeccompMode::Allowlist, st);

        let _ = tool.execute(json!({"language": "python", "code": "print('x')"}))
            .await.unwrap();

        let last = sb.last.lock().unwrap();
        let limits = &last.as_ref().unwrap().3;
        assert_eq!(limits.extra_writable_mounts.len(), 1);
        assert_eq!(limits.extra_writable_mounts[0].1, std::path::Path::new("/tmp/output"));
        // Host source is the per-call scratch tempdir; it's torn down once
        // execute() returns, so we can't probe is_dir() here. The
        // captures_image_artifacts test below proves the dir is live during
        // the sandbox run by writing files into it from the stub.
        let host = &limits.extra_writable_mounts[0].0;
        assert!(host.file_name().and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with("mira-coderun-")),
            "expected mira-coderun- tempdir, got {host:?}");
    }

    // A sandbox stub that, on `run()`, drops two files into the host
    // scratch dir: one valid PNG and one disallowed `.exe`. The valid file
    // should be captured as a markdown image ref; the `.exe` should be
    // skipped with a note.
    struct ArtifactDroppingSandbox;
    #[async_trait]
    impl CodeSandbox for ArtifactDroppingSandbox {
        async fn run(
            &self,
            _: Language, _: &str, _: Option<&str>,
            limits: &ResourceLimits,
        ) -> Result<SandboxOutput, SandboxError> {
            let host = &limits.extra_writable_mounts[0].0;
            std::fs::write(host.join("smile.png"), b"\x89PNG\r\n\x1a\nfake-png").unwrap();
            std::fs::write(host.join("nope.exe"), b"MZ\x90\x00").unwrap();
            Ok(SandboxOutput {
                stdout: "saved\n".into(), stderr: String::new(),
                exit_code: 0, duration_ms: 1, truncated: false,
            })
        }
        fn name(&self) -> &'static str { "drop" }
        fn supported(&self) -> bool { true }
    }

    #[test]
    fn wants_scientific_matches_import_lines_only() {
        assert!(wants_scientific("import numpy as np\nprint(np.pi)"));
        assert!(wants_scientific("from pandas import DataFrame"));
        assert!(wants_scientific("import matplotlib.pyplot as plt"));
        assert!(wants_scientific("import os, numpy")); // multi-import line
        assert!(wants_scientific("from scipy.stats import norm")); // dotted from
        assert!(wants_scientific("    import cv2")); // indented, import-name form
        // Plain stdlib / arithmetic stays on the primary backend.
        assert!(!wants_scientific("print(6*7)"));
        assert!(!wants_scientific("import json\nimport math"));
        // A bare mention in a string/comment (not an import line) doesn't route.
        assert!(!wants_scientific("x = 'numpy is great'  # pandas too"));
        // A substring of a non-scientific module must NOT false-positive.
        assert!(!wants_scientific("import numpydoc_stub")); // not "numpy"
    }

    #[test]
    fn wants_scientific_catches_dynamic_imports_and_strong_signals() {
        assert!(wants_scientific("mod = importlib.import_module('pandas')"));
        assert!(wants_scientific("np = __import__(\"numpy\")"));
        // micropip only exists inside Pyodide.
        assert!(wants_scientific("import micropip\nawait micropip.install('foo')"));
        // savefig implies matplotlib output even with a dynamic import.
        assert!(wants_scientific("plt.savefig('/tmp/output/c.png')"));
        // Dynamic import of a stdlib module does not route.
        assert!(!wants_scientific("importlib.import_module('json')"));
    }

    // A scientific import routes to the wired scientific backend; plain code
    // stays on the primary one. Both stubs report distinct stdout so we can
    // tell which actually ran.
    #[tokio::test]
    async fn routes_scientific_imports_to_scientific_backend() {
        let tmp = tempfile::tempdir().unwrap();

        // Scientific code → scientific backend.
        let primary = StubSandbox::ok("PRIMARY\n");
        let sci     = StubSandbox::ok("SCI\n");
        let (st, _sd) = store();
        let tool = CodeRunTool::new(
            primary.clone(), tmp.path().to_path_buf(), cfg(), SeccompMode::Allowlist, st,
        ).with_scientific(Some(sci.clone()));
        let r = tool.execute(json!({
            "language": "python",
            "code": "import numpy as np\nprint(np.pi)",
        })).await.unwrap();
        assert!(r.success, "got {r:?}");
        assert!(r.output.contains("SCI"), "scientific backend should run, got: {}", r.output);
        assert!(sci.last.lock().unwrap().is_some(), "scientific backend should have run");
        assert!(primary.last.lock().unwrap().is_none(), "primary should NOT have run");

        // Plain code → primary backend.
        let primary2 = StubSandbox::ok("PRIMARY\n");
        let sci2     = StubSandbox::ok("SCI\n");
        let (st2, _sd2) = store();
        let tool2 = CodeRunTool::new(
            primary2.clone(), tmp.path().to_path_buf(), cfg(), SeccompMode::Allowlist, st2,
        ).with_scientific(Some(sci2.clone()));
        let r2 = tool2.execute(json!({
            "language": "python",
            "code": "print(6*7)",
        })).await.unwrap();
        assert!(r2.success, "got {r2:?}");
        assert!(r2.output.contains("PRIMARY"), "primary backend should run, got: {}", r2.output);
        assert!(primary2.last.lock().unwrap().is_some(), "primary should have run");
        assert!(sci2.last.lock().unwrap().is_none(), "scientific should NOT have run");
    }

    #[tokio::test]
    async fn captures_image_artifacts_and_skips_disallowed_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, _sd) = store();
        let tool = CodeRunTool::new(
            Arc::new(ArtifactDroppingSandbox),
            tmp.path().to_path_buf(), cfg(), SeccompMode::Allowlist, Arc::clone(&st),
        );

        let r = tool.execute(json!({"language": "python", "code": "1"})).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("--- artifacts ---"),
            "expected artifacts block, got: {}", r.output);
        assert!(r.output.contains("![smile.png](/api/artifacts/"),
            "expected captured png ref, got: {}", r.output);
        assert!(r.output.contains("# skipped nope.exe"),
            "expected skip note for nope.exe, got: {}", r.output);
    }
}
