// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/code_run_e2e.rs
//! End-to-end test for the `code_run` tool: real `NamespaceSandbox`, real
//! pivot into a tiny fake rootfs.
//!
//! Linux-only (the sandbox itself is). Skipped on hosts without unprivileged
//! user namespaces — that's expected on stock Ubuntu 24.04 with AppArmor's
//! default policy and tells us nothing about the tool's correctness.
//!
//! The "python" we install in the fake rootfs is a `/bin/sh` wrapper that
//! exposes a `-c <cmd>` interface, just like real Python. The tool sends
//! `Language::Python` as `python3 -c "$code"`, so the wrapper sees the same
//! argv shape. We verify:
//!   1. The tool returns success with stdout from the pivoted child.
//!   2. /etc/passwd from the host is not visible after pivot.
//!   3. Asking for a 999-second timeout against a 2-second cap clamps down
//!      and produces a Timeout failure for an infinite loop.

#![cfg(all(target_os = "linux", feature = "sandbox-linux"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use mira::config::CodeRunConfig;
use mira::sandbox::{default_backend, CodeSandbox, SeccompMode};
use mira::tools::Tool;
use mira::tools::code_run::CodeRunTool;

// ── Host capability probe ────────────────────────────────────────────────────

fn ns_probably_ok() -> bool {
    if std::fs::metadata("/proc/self/ns/user").is_err() { return false; }
    match std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
        Ok(s)  => s.trim() == "1",
        Err(_) => true,
    }
}

fn skip_if_unsupported() -> bool {
    if !ns_probably_ok() {
        eprintln!("skip: unprivileged user namespaces unavailable");
        return true;
    }
    false
}

fn which(bin: &str) -> Option<PathBuf> {
    for dir in ["/bin", "/usr/bin"] {
        let p = PathBuf::from(dir).join(bin);
        if p.exists() { return Some(p); }
    }
    None
}

/// Copy `binary` and the shared libs `ldd` reports it needs into `dest_root`,
/// preserving directory layout (so `/bin/sh` lands at `<dest_root>/bin/sh`).
fn copy_with_deps(binary: &Path, dest_root: &Path) -> std::io::Result<()> {
    let output = std::process::Command::new("ldd").arg(binary).output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "ldd failed: {}", String::from_utf8_lossy(&output.stderr)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut to_copy: Vec<PathBuf> = vec![binary.to_path_buf()];
    for line in stdout.lines() {
        let line = line.trim();
        let path_str = if let Some(idx) = line.find(" => ") {
            line[idx + 4..].split_whitespace().next().unwrap_or("")
        } else if line.starts_with('/') {
            line.split_whitespace().next().unwrap_or("")
        } else {
            continue;
        };
        if path_str.starts_with('/') {
            to_copy.push(PathBuf::from(path_str));
        }
    }

    for src in to_copy {
        if !src.exists() { continue; }
        let rel = src.strip_prefix("/").unwrap();
        let dst = dest_root.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
        let perm = std::fs::metadata(&src)?.permissions();
        std::fs::set_permissions(&dst, perm)?;
    }
    Ok(())
}

/// Set up a fake rootfs in a fresh tempdir. Returns the rootfs path *and*
/// the TempDir guard (caller must keep it alive until the test ends).
fn build_fake_python_rootfs() -> Option<(tempfile::TempDir, PathBuf)> {
    let tmp = tempfile::tempdir().ok()?;
    let rootfs = tmp.path().join("rootfs");
    std::fs::create_dir_all(&rootfs).ok()?;
    for sub in ["tmp", "proc", "dev", "old_root", "bin"] {
        std::fs::create_dir_all(rootfs.join(sub)).ok()?;
    }

    let sh = which("sh")?;
    if copy_with_deps(&sh, &rootfs).is_err() {
        return None;
    }

    // /bin/python3 wrapper. The tool sends `python3 -c "<code>"`; the wrapper
    // unwraps the -c, eval()s the snippet, and exits with its status.
    let wrapper = "#!/bin/sh\ncase \"$1\" in -c) shift; eval \"$1\" ;; esac\n";
    let py3 = rootfs.join("bin/python3");
    std::fs::write(&py3, wrapper).ok()?;
    let mut perm = std::fs::metadata(&py3).ok()?.permissions();
    use std::os::unix::fs::PermissionsExt;
    perm.set_mode(0o755);
    std::fs::set_permissions(&py3, perm).ok()?;

    Some((tmp, rootfs))
}

fn cfg(max_secs: u64) -> CodeRunConfig {
    CodeRunConfig {
        enabled:                true,
        allowed_languages:      vec!["python".into()],
        max_wall_clock_seconds: max_secs,
        max_memory_mb:          128,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn code_run_executes_in_pivoted_rootfs_and_hides_host_etc() {
    if skip_if_unsupported() { return; }
    let Some((_guard, rootfs)) = build_fake_python_rootfs() else {
        eprintln!("skip: couldn't build fake rootfs"); return;
    };

    let backend: Arc<dyn CodeSandbox> = Arc::from(default_backend());
    if !backend.supported() {
        eprintln!("skip: backend reports unsupported"); return;
    }
    let tool = CodeRunTool::new(backend, rootfs.clone(), cfg(5), SeccompMode::Denylist, Arc::new(mira::artifacts::ArtifactStore::new(std::env::temp_dir()).unwrap()));

    let r = tool.execute(json!({
        "language": "python",
        // The "python" here is sh (see wrapper). We exercise three things —
        // a marker on stdout, /tmp writability, and host /etc absence — using
        // only shell builtins (`read`, `[`), since the minimal fake rootfs has
        // just /bin/sh and no coreutils (`cat`/`ls` would be "not found").
        "code": "echo MIRA_OK; echo data > /tmp/x; read v < /tmp/x; echo \"$v\"; \
                 if [ -r /etc/passwd ]; then read p < /etc/passwd; echo \"$p\"; fi",
    })).await.expect("tool call should not error");

    // Some hosts (AppArmor lockdown, missing CAP_SYS_ADMIN inside userns)
    // refuse the spawn even when the probe said ok. Treat that as a skip,
    // not a failure — it's the same shape as the linux.rs sandbox tests.
    if !r.success {
        let err = r.error.as_deref().unwrap_or("");
        if err.contains("Operation not permitted") || err.contains("EPERM") {
            eprintln!("skip: spawn denied by host policy ({err})");
            return;
        }
        panic!("code_run failed unexpectedly: {err}");
    }

    assert!(r.output.contains("exit=0"), "expected exit=0, got: {}", r.output);
    assert!(r.output.contains("MIRA_OK"),
        "expected MIRA_OK marker on stdout, got: {}", r.output);
    assert!(r.output.contains("data"),
        "expected /tmp roundtrip 'data' on stdout, got: {}", r.output);
    assert!(!r.output.contains("root:x:0:0"),
        "host /etc/passwd must not be visible inside the pivot, got: {}", r.output);
}

#[tokio::test]
async fn code_run_clamps_caller_timeout_and_kills_runaway() {
    if skip_if_unsupported() { return; }
    let Some((_guard, rootfs)) = build_fake_python_rootfs() else {
        eprintln!("skip: couldn't build fake rootfs"); return;
    };

    let backend: Arc<dyn CodeSandbox> = Arc::from(default_backend());
    if !backend.supported() {
        eprintln!("skip: backend reports unsupported"); return;
    }
    // Cap is 2s; caller asks for 999s — must clamp down.
    let tool = CodeRunTool::new(backend, rootfs.clone(), cfg(2), SeccompMode::Denylist, Arc::new(mira::artifacts::ArtifactStore::new(std::env::temp_dir()).unwrap()));

    let r = tool.execute(json!({
        "language": "python",
        "code":     "while :; do :; done",
        "timeout_seconds": 999,
    })).await.expect("tool call should not error");

    if r.success {
        panic!("expected timeout failure, got success: {}", r.output);
    }
    let err = r.error.unwrap_or_default();
    if err.contains("Operation not permitted") || err.contains("EPERM") {
        eprintln!("skip: spawn denied by host policy ({err})"); return;
    }
    assert!(err.contains("timed out"), "expected timeout failure, got: {err}");
    // Confirms the tool's clamp logic, not just the sandbox's enforcement.
    assert!(err.contains("limit 2s") || err.contains("limit 2 s") || err.contains("(limit 2s)"),
        "error should reference the clamped 2s ceiling, got: {err}");
}

#[tokio::test]
async fn code_run_chdir_into_working_dir() {
    if skip_if_unsupported() { return; }
    let Some((_guard, rootfs)) = build_fake_python_rootfs() else {
        eprintln!("skip: couldn't build fake rootfs"); return;
    };

    let backend: Arc<dyn CodeSandbox> = Arc::from(default_backend());
    if !backend.supported() {
        eprintln!("skip: backend reports unsupported"); return;
    }
    let tool = CodeRunTool::new(backend, rootfs.clone(), cfg(5), SeccompMode::Denylist, Arc::new(mira::artifacts::ArtifactStore::new(std::env::temp_dir()).unwrap()));

    // Setting working_dir to /tmp must put pwd at /tmp inside the pivot.
    let r = tool.execute(json!({
        "language":    "python",
        "code":        "pwd",
        "working_dir": "/tmp",
    })).await.expect("tool call should not error");

    if !r.success {
        let err = r.error.as_deref().unwrap_or("");
        if err.contains("Operation not permitted") || err.contains("EPERM") {
            eprintln!("skip: spawn denied by host policy ({err})");
            return;
        }
        panic!("code_run failed unexpectedly: {err}");
    }
    assert!(r.output.contains("/tmp"), "pwd should report /tmp, got: {}", r.output);
}
