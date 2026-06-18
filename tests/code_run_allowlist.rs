// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/code_run_allowlist.rs
//! Smoke test for `SeccompMode::Allowlist` against the *real* installed
//! Python rootfs. The fake-sh wrapper in `code_run_e2e.rs` doesn't exercise
//! real CPython startup syscalls; this one does.
//!
//! Skipped when the Python rootfs isn't installed or the host can't run
//! unprivileged user namespaces. Run `mira sandbox install python` first.

#![cfg(all(target_os = "linux", feature = "sandbox-linux"))]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use mira::config::CodeRunConfig;
use mira::sandbox::{default_backend, CodeSandbox, SeccompMode};
use mira::sandbox::rootfs::RootfsManager;
use mira::tools::Tool;
use mira::tools::code_run::CodeRunTool;

fn ns_probably_ok() -> bool {
    if std::fs::metadata("/proc/self/ns/user").is_err() { return false; }
    match std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
        Ok(s)  => s.trim() == "1",
        Err(_) => true,
    }
}

fn installed_python_rootfs() -> Option<PathBuf> {
    let data_dir = dirs::home_dir()?.join(".mira/data");
    let mgr = RootfsManager::new(&data_dir);
    if mgr.python_installed() { Some(mgr.python_pivot_root()) } else { None }
}

fn cfg() -> CodeRunConfig {
    CodeRunConfig {
        enabled:                true,
        allowed_languages:      vec!["python".into()],
        max_wall_clock_seconds: 5,
        max_memory_mb:          256,
    }
}

async fn run_under_allowlist(code: &str) -> (bool, String, Option<String>) {
    let backend: Arc<dyn CodeSandbox> = Arc::from(default_backend());
    let rootfs = installed_python_rootfs().expect("rootfs check is the caller's job");
    let tool = CodeRunTool::new(backend, rootfs, cfg(), SeccompMode::Allowlist, Arc::new(mira::artifacts::ArtifactStore::new(std::env::temp_dir()).unwrap()));
    let r = tool.execute(json!({ "language": "python", "code": code }))
        .await
        .expect("tool call should not error at the dispatch level");
    (r.success, r.output, r.error)
}

/// One async test that walks a small corpus. A single test keeps the failure
/// message in one place: which snippet died, what the sandbox reported.
#[tokio::test]
async fn allowlist_handles_representative_python_corpus() {
    if !ns_probably_ok() {
        eprintln!("skip: unprivileged user namespaces unavailable"); return;
    }
    if installed_python_rootfs().is_none() {
        eprintln!("skip: Python rootfs not installed (run `mira sandbox install python`)");
        return;
    }
    if !default_backend().supported() {
        eprintln!("skip: backend reports unsupported"); return;
    }

    // (label, snippet, marker substring expected on stdout in the success body)
    let corpus: &[(&str, &str, &str)] = &[
        ("hello",       "print('hello')",                                                       "hello"),
        ("compute",     "print(sum(range(1000)))",                                              "499500"),
        ("json",        "import json; print(json.dumps({'a': 1, 'b': [1,2,3]}))",               "\"a\""),
        ("re",          "import re; print(re.match(r'a(b+)c', 'abbbc').group(1))",              "bbb"),
        ("os_listdir",  "import os; print(len(os.listdir('/')))",                               ""),
        ("time_sleep",  "import time; time.sleep(0.05); print('slept')",                        "slept"),
        ("random",      "import random; random.seed(0); print(random.random())",                "0."),
        ("file_io",     "open('/tmp/x','w').write('hi'); print(open('/tmp/x').read())",         "hi"),
        ("datetime",    "import datetime; print(datetime.datetime.now().year)",                 "20"),
        ("hashlib",     "import hashlib; print(hashlib.sha256(b'abc').hexdigest()[:6])",        "ba7816"),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (label, code, marker) in corpus {
        let (ok, out, err) = run_under_allowlist(code).await;
        if !ok {
            failures.push(format!("[{label}] failed: err={:?} out={out}", err));
            continue;
        }
        if !marker.is_empty() && !out.contains(marker) {
            failures.push(format!("[{label}] missing marker {marker:?} in output: {out}"));
        }
    }

    assert!(failures.is_empty(),
        "allowlist smoke failures ({} of {}):\n - {}",
        failures.len(), corpus.len(), failures.join("\n - "));
}
