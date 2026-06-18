// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/language.rs

use std::path::PathBuf;

// The runtime the sandbox should exec. Typed variants map to a pinned
// interpreter path + canonical argv inside the rootfs; `Raw` is an escape
// hatch for tests and for tools that already know the exact binary to run.
// // Per-language rootfs provisioning (Python, Node) ships in 5. Until
// then the typed variants return `SandboxError::Policy` until the host-side
// wiring is done. `Raw` works today — it's how the unit tests exercise the
// isolation mechanism against host binaries like `/bin/echo`.
#[derive(Debug, Clone)]
pub enum Language {
    // CPython 3, pinned interpreter + pinned stdlib. No pip.
    Python,
    // Node.js, pinned interpreter + pinned core modules.
    Node,
    // POSIX shell, intended for tiny glue scripts only.
    Bash,
    // Execute an arbitrary program with fixed argv. Used by tests and by
    // future tools that don't need a language runtime (e.g. running a
    // compiled checker against a file). The `payload` string is fed to the
    // child's stdin when `stdin = None` at call-time; otherwise it's
    // ignored and the caller's `stdin` wins.
    Raw {
        program: PathBuf,
        args:    Vec<String>,
    },
}

impl Language {
    // Stable short name for logs and audit rows.
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Python   => "python",
            Language::Node     => "node",
            Language::Bash     => "bash",
            Language::Raw { .. } => "raw",
        }
    }
}
