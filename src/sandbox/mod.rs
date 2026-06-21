// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/mod.rs
//! Sandboxed code execution — 4 (Level A).
//!
//! This module defines the `CodeSandbox` trait and its Linux implementation
//! (`NamespaceSandbox`). Everything Tier 4 runs through the trait so backends
//! can be swapped (Level B container, Level C WASM) without changing callers.
//!
//! ## What's in scope for 7a-4
//!
//! * The trait surface and its data types (`Language`, `ResourceLimits`,
//! `SandboxOutput`, `SandboxError`) — stable API that 7a-5 tools build on.
//! * A working Linux backend that isolates a child with namespaces, resource
//! limits, `no_new_privs`, and a seccomp-bpf filter that kills escape
//! primitives (`ptrace`, `mount`, `unshare`, `bpf`, `kexec_*`, etc.).
//! * A stub backend for non-Linux targets (or when the `sandbox-linux`
//! feature is off) that always returns `SandboxError::Unsupported`.
//!
//! ## What is deferred to 7a-5
//!
//! * The pre-baked, read-only rootfs the child pivots into. The Linux backend
//! already accepts `ResourceLimits::rootfs`; it just errors today when set,
//! because there's no rootfs provisioning story yet. Until then the child
//! runs inside an unshared mount namespace that still sees the host's FS —
//! fine for tests, not fine for untrusted code. No user-facing Tier 4 tool
//! is exposed in this phase, so that gap doesn't leak out.
//! * A tightened seccomp *allowlist* with per-language profiles. For now we
//! ship an escape-primitive denylist; 7a-5 will convert it once we know the
//! exact syscall set the Python rootfs needs.

pub mod error;
pub mod language;
pub mod limits;
pub mod output;
pub mod rootfs;

#[cfg(all(target_os = "linux", feature = "sandbox-linux"))]
pub mod linux;

#[cfg(any(not(target_os = "linux"), not(feature = "sandbox-linux")))]
pub mod stub;

pub use error::SandboxError;
pub use language::Language;
pub use limits::{ResourceLimits, SeccompMode};
pub use output::SandboxOutput;

#[cfg(all(target_os = "linux", feature = "sandbox-linux"))]
pub use linux::NamespaceSandbox;

// Apply the syscall denylist (escape + host-mutation syscalls) to the current
// process — reuses the Tier-4 sandbox denylist for the plugin launcher. Call
// AFTER any mount/namespace setup (the denylist blocks mount/unshare/pivot_root).
#[cfg(all(target_os = "linux", feature = "sandbox-linux"))]
pub use linux::apply_seccomp_denylist as apply_plugin_seccomp;

#[cfg(any(not(target_os = "linux"), not(feature = "sandbox-linux")))]
pub use stub::UnsupportedSandbox;

use async_trait::async_trait;

// Execute untrusted or semi-trusted code in an isolated child process.
// // Implementations must honour every non-negotiable from
// `design-docs/phase7-tools-and-sandbox.md §3`: network off by default, read-only
// FS except scratch, parent-enforced resource limits, stdin/stdout only,
// one audit row per call. The trait itself doesn't perform auditing — that
// lives at the Tier 4 tool layer so it can include the original `actor`
// without the sandbox needing to know about MIRA's user model.
#[async_trait]
pub trait CodeSandbox: Send + Sync {
    async fn run(
        &self,
        language: Language,
        payload:  &str,
        stdin:    Option<&str>,
        limits:   &ResourceLimits,
    ) -> Result<SandboxOutput, SandboxError>;

    // Short name used in logs and audit rows (`"namespace"`, `"container"`,
    // `"wasm"`, `"unsupported"`). Stable strings — don't rename casually.
    fn name(&self) -> &'static str;

    // Returns true if this backend can actually run code on the current
    // host. Callers should still be prepared for `run()` to fail — a host
    // may report supported() = true but lack permission for user namespaces
    // or seccomp due to AppArmor / sysctls.
    fn supported(&self) -> bool;
}

// Convenience constructor that returns the best backend for the current
// target. Today that's `NamespaceSandbox` on Linux (with the feature on)
// and `UnsupportedSandbox` everywhere else.
pub fn default_backend() -> Box<dyn CodeSandbox> {
    #[cfg(all(target_os = "linux", feature = "sandbox-linux"))]
    { Box::new(NamespaceSandbox::new()) }
    #[cfg(any(not(target_os = "linux"), not(feature = "sandbox-linux")))]
    { Box::new(UnsupportedSandbox) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_backend_round_trips_supported_flag() {
        let backend = default_backend();
        // Two directions always hold and are environment-independent:
        //   - a supported backend is never the stub, and
        //   - the stub is never supported.
        // The converse ("unsupported at runtime ⇒ it's the stub") does NOT
        // hold: on Linux with the feature on, default_backend() is always the
        // real NamespaceSandbox, which legitimately reports supported()==false
        // where the kernel/container forbids unprivileged namespaces (e.g. a
        // locked-down CI Docker runner) while keeping its "namespace" name.
        if backend.supported() {
            assert_ne!(backend.name(), "unsupported");
        }
        if backend.name() == "unsupported" {
            assert!(!backend.supported());
        }
    }
}
