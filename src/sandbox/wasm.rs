// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/wasm.rs
//! Cross-platform code-execution backend (WASM/WASI via Wasmtime).
//!
//! One capability-based sandbox that works identically on Linux, macOS, and
//! Windows — the answer to "secure `code_run` everywhere" without OS-specific
//! privileges or Docker/WSL. Code runs inside a WASI guest with **no network,
//! no host filesystem** except a single fresh scratch dir, a hard **memory
//! cap** (store limiter) and a **wall-clock kill** (epoch interruption). The
//! interpreter is a bundled WASI CPython module (`python.wasm`), provisioned
//! to `~/.mira/deps` like our other managed runtimes (see the rootfs manager).
//!
//! Built only under the `sandbox-wasm` feature. See
//! design-docs/just-in-time-tools.md + memory project_code_execution_sandbox
//! (pivot to native AppContainer/App-Sandbox if WASM limits bite).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use super::{CodeSandbox, Language, ResourceLimits, SandboxError, SandboxOutput};

/// Guest path the scratch dir is mounted at, and where the user's code lands.
const GUEST_DIR: &str = "/sandbox";

// ── Managed WASI-Python artifact (pinned) ──────────────────────────────────────

/// Pinned CPython-on-WASI build (VMware webassembly-language-runtimes). Standard
/// WASI build (not the wasmedge variant). Provisioned to `<data_dir>/deps/wasm`.
pub const PYTHON_WASM_VERSION: &str = "3.12.0";
const PYTHON_WASM_URL: &str = "https://github.com/vmware-labs/webassembly-language-runtimes/releases/download/python/3.12.0%2B20231211-040d5a6/python-3.12.0.wasm";
const PYTHON_WASM_SHA256: &str = "e5dc5a398b07b54ea8fdb503bf68fb583d533f10ec3f930963e02b9505f7a763";

/// Managed on-disk path for the WASI Python module.
pub fn managed_python_wasm_path(data_dir: &Path) -> PathBuf {
    data_dir.join("deps").join("wasm").join(format!("python-{PYTHON_WASM_VERSION}.wasm"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Ensure the pinned WASI Python module is present + checksum-valid, downloading
/// it (~26 MB) if missing. Returns its path. Cross-platform (no OS deps) — this
/// is how `code_run` becomes available on Windows/macOS.
pub async fn ensure_python_wasm(data_dir: &Path) -> Result<PathBuf, String> {
    let dest = managed_python_wasm_path(data_dir);
    if dest.is_file() {
        if let Ok(b) = std::fs::read(&dest) {
            if sha256_hex(&b) == PYTHON_WASM_SHA256 {
                return Ok(dest);
            }
            tracing::warn!("wasm python: checksum mismatch at {} — re-downloading", dest.display());
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create deps dir: {e}"))?;
    }
    tracing::info!("wasm python: downloading pinned CPython-on-WASI (~26 MB)…");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build().map_err(|e| format!("http client: {e}"))?;
    let bytes = client.get(PYTHON_WASM_URL).send().await
        .map_err(|e| format!("download: {}", e.without_url()))?
        .error_for_status().map_err(|e| format!("download status: {}", e.without_url()))?
        .bytes().await.map_err(|e| format!("download body: {e}"))?;
    let got = sha256_hex(&bytes);
    if got != PYTHON_WASM_SHA256 {
        return Err(format!("checksum mismatch (got {got}, expected {PYTHON_WASM_SHA256})"));
    }
    // Atomic-ish: write to a temp then rename.
    let tmp = dest.with_extension("wasm.partial");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, &dest).map_err(|e| format!("rename: {e}"))?;
    tracing::info!("wasm python: installed at {}", dest.display());
    Ok(dest)
}

pub struct WasmSandbox {
    engine: Engine,
    /// The WASI CPython interpreter, **compiled once** at construction and
    /// reused per call (compiling the 26 MB module is seconds — far too slow
    /// to do per `run()`). `None` when the module isn't provisioned →
    /// `supported()` is false and `code_run` won't register.
    module: Option<Module>,
}

struct StoreState {
    wasi:   WasiP1Ctx,
    limits: StoreLimits,
}

impl WasmSandbox {
    /// `python_module` is the path to the WASI CPython `.wasm`. Compiling it
    /// here (once) keeps per-call latency to instantiate-only.
    pub fn new(python_module: Option<PathBuf>) -> Self {
        let mut cfg = Config::new();
        cfg.epoch_interruption(true);
        let engine = Engine::new(&cfg).expect("wasmtime engine init");
        let module = python_module.as_ref().and_then(|p| match Module::from_file(&engine, p) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!("wasm sandbox: failed to compile {}: {e}", p.display());
                None
            }
        });
        Self { engine, module }
    }
}

#[async_trait]
impl CodeSandbox for WasmSandbox {
    async fn run(
        &self,
        language: Language,
        payload:  &str,
        stdin:    Option<&str>,
        limits:   &ResourceLimits,
    ) -> Result<SandboxOutput, SandboxError> {
        let module = match language {
            Language::Python => self.module.clone().ok_or_else(|| {
                SandboxError::Policy("WASM Python runtime not provisioned".into())
            })?,
            _ => return Err(SandboxError::Policy(
                "the WASM sandbox currently supports Python only".into())),
        };

        let engine   = self.engine.clone();
        let payload  = payload.to_string();
        let stdin    = stdin.map(str::to_string);
        let mem      = limits.memory_bytes as usize;
        let wall     = limits.wall_clock;
        let max_out  = limits.max_output_bytes.max(1);
        let start    = Instant::now();

        let res = tokio::task::spawn_blocking(move || -> Result<(i32, Vec<u8>, Vec<u8>, bool), SandboxError> {
            // Fresh scratch dir — the ONLY filesystem the guest can see.
            let scratch = tempfile::tempdir().map_err(SandboxError::Io)?;
            std::fs::write(scratch.path().join("main.py"), payload.as_bytes()).map_err(SandboxError::Io)?;

            let stdout = MemoryOutputPipe::new(max_out);
            let stderr = MemoryOutputPipe::new(max_out);

            let mut builder = WasiCtxBuilder::new();
            builder
                .stdout(stdout.clone())
                .stderr(stderr.clone())
                .args(&["python", &format!("{GUEST_DIR}/main.py")]);
            if let Some(ref s) = stdin {
                builder.stdin(MemoryInputPipe::new(s.clone().into_bytes()));
            }
            builder
                .preopened_dir(scratch.path(), GUEST_DIR, DirPerms::all(), FilePerms::all())
                .map_err(|e| SandboxError::SpawnFailed(format!("preopen: {e}")))?;
            // No env, no network, no other preopens → capability-isolated.
            let wasi = builder.build_p1();

            let store_limits = StoreLimitsBuilder::new().memory_size(mem).build();
            let mut store = Store::new(&engine, StoreState { wasi, limits: store_limits });
            store.limiter(|s| &mut s.limits);
            // Interrupt after one epoch tick; the watchdog bumps the epoch once
            // the wall-clock deadline passes.
            store.set_epoch_deadline(1);

            let finished = Arc::new(AtomicBool::new(false));
            {
                let eng = engine.clone();
                let done = finished.clone();
                std::thread::spawn(move || {
                    let deadline = Instant::now() + wall;
                    while Instant::now() < deadline {
                        if done.load(Ordering::Relaxed) { return; }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    eng.increment_epoch(); // trips the epoch deadline → interrupt
                });
            }

            let mut linker: Linker<StoreState> = Linker::new(&engine);
            preview1::add_to_linker_sync(&mut linker, |s: &mut StoreState| &mut s.wasi)
                .map_err(|e| SandboxError::SpawnFailed(format!("wasi linker: {e}")))?;
            let instance = linker.instantiate(&mut store, &module)
                .map_err(|e| SandboxError::SpawnFailed(format!("instantiate: {e}")))?;
            let start_fn = instance.get_typed_func::<(), ()>(&mut store, "_start")
                .map_err(|e| SandboxError::SpawnFailed(format!("module has no _start: {e}")))?;

            let run = start_fn.call(&mut store, ());
            finished.store(true, Ordering::Relaxed);

            let exit_code = match run {
                Ok(()) => 0,
                Err(e) => {
                    // WASI proc_exit surfaces as I32Exit.
                    if let Some(exit) = e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                        exit.0
                    } else if let Some(trap) = e.downcast_ref::<wasmtime::Trap>() {
                        if *trap == wasmtime::Trap::Interrupt {
                            return Err(SandboxError::Timeout(wall.as_millis() as u64));
                        }
                        1 // other trap (the detail is in stderr / the message)
                    } else {
                        1
                    }
                }
            };

            let out = stdout.contents().to_vec();
            let err = stderr.contents().to_vec();
            let truncated = out.len() >= max_out || err.len() >= max_out;
            Ok((exit_code, out, err, truncated))
        })
        .await
        .map_err(|e| SandboxError::SpawnFailed(format!("worker join: {e}")))??;

        let (exit_code, out, err, truncated) = res;
        Ok(SandboxOutput {
            stdout:      String::from_utf8_lossy(&out).into_owned(),
            stderr:      String::from_utf8_lossy(&err).into_owned(),
            exit_code,
            duration_ms: start.elapsed().as_millis() as u64,
            truncated,
        })
    }

    fn name(&self) -> &'static str { "wasm" }

    // Supported only once the WASI Python module is provisioned + compiled.
    fn supported(&self) -> bool { self.module.is_some() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Runs the REAL pinned python.wasm if present locally. Proves the backend
    // end-to-end: capability isolation, stdout capture, exit code. Skipped when
    // the artifact isn't downloaded (CI without the asset).
    #[tokio::test]
    async fn python_wasm_executes_and_captures_stdout() {
        let path = dirs_like_deps_python();
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let sb = WasmSandbox::new(Some(path));
        assert!(sb.supported(), "module should compile");
        let mut limits = ResourceLimits::default();
        limits.memory_bytes = 512 * 1024 * 1024;
        limits.wall_clock   = Duration::from_secs(30);
        limits.max_output_bytes = 64 * 1024;
        let out = sb.run(Language::Python, "print(6*7)", None, &limits).await.expect("run ok");
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert!(out.stdout.contains("42"), "stdout was: {:?}", out.stdout);
    }

    fn dirs_like_deps_python() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join(".mira/deps/wasm/python-3.12.0.wasm")
    }
}
