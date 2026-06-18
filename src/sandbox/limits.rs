// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/limits.rs

use std::path::PathBuf;
use std::time::Duration;

/// Which seccomp filter strategy the backend installs in the child.
///
/// * `Allowlist` — only a curated set of syscalls is permitted; everything
///   else gets `SIGSYS` killed. Default since 7a-5 iteration C, after the
///   "Python startup + basic stdlib" allowlist passed a representative smoke
///   corpus (`tests/code_run_allowlist.rs`).
/// * `Denylist` — block a hand-picked set of escape primitives (`ptrace`,
///   `mount`, `unshare`, `bpf`, `kexec_*`, etc.); every other syscall is
///   allowed. The 7a-4 default — kept as an opt-out via
///   `[sandbox] seccomp_mode = "denylist"` for operators running scripts that
///   need a syscall not yet in the allowlist (file an issue and we'll add it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeccompMode {
    Denylist,
    Allowlist,
}

impl Default for SeccompMode {
    fn default() -> Self { Self::Allowlist }
}

/// Parent-enforced caps on a sandbox run. The child can never raise these;
/// they're stamped in either as setrlimit values or as parent-side watchdogs.
///
/// Default values are tuned for short Tier 4 tool calls (a quick Python
/// snippet, not a training loop). Tools with different profiles should pass
/// a customised `ResourceLimits` rather than tweaking the defaults globally.
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// Hard wall-clock deadline. The parent kills the child on expiry and
    /// returns `SandboxError::Timeout`.
    pub wall_clock:       Duration,

    /// Soft *and* hard CPU-seconds cap (`RLIMIT_CPU`). Kernel sends SIGXCPU
    /// when soft is hit, SIGKILL when hard is hit — we set them equal.
    pub cpu_seconds:      u64,

    /// Virtual address space cap in bytes (`RLIMIT_AS`). This is the
    /// practical "max memory" knob on Linux.
    pub memory_bytes:     u64,

    /// Largest file the child may create (`RLIMIT_FSIZE`).
    pub file_size_bytes:  u64,

    /// Max subprocesses the child may spawn (`RLIMIT_NPROC`).
    pub nproc:            u32,

    /// Byte cap on combined stdout+stderr captured by the parent. Beyond
    /// this the reader keeps draining (so the child doesn't block on a
    /// full pipe) but discards the extra bytes and returns
    /// `SandboxOutput.truncated = true`.
    pub max_output_bytes: usize,

    /// Read-only root filesystem the child `pivot_root`s into. When `Some`,
    /// `/tmp` inside the rootfs is bind-mounted to a fresh, auto-deleted
    /// scratch dir. When `None`, no pivot is performed — the child stays in
    /// the host's mount namespace (still unshared, so changes don't leak
    /// back out). 7a-5 is when Tier 4 tools start passing `Some`.
    pub rootfs:           Option<PathBuf>,

    /// When true (default), the child runs in a fresh network namespace with
    /// no interfaces. Tools that explicitly need egress set this to false
    /// *and* are expected to route traffic through MIRA's own HTTP policy.
    pub disable_network:  bool,

    /// Working directory the child should `chdir` into after pivot. Interpreted
    /// in the *post-pivot* mount namespace (so `/tmp` means the in-rootfs
    /// scratch tmpfs, not the host's). Ignored when `rootfs` is `None`. When
    /// `None` the child stays at `/` after pivot.
    pub working_dir:      Option<PathBuf>,

    /// Which seccomp filter to install. Defaults to `Allowlist` since 7a-5
    /// iter C — the curated set covers Python startup + common stdlib. Flip
    /// to `Denylist` for scripts that need a syscall outside the allowlist.
    pub seccomp_mode:     SeccompMode,

    /// Extra writable bind mounts to graft into the sandbox after the `/tmp`
    /// tmpfs is up. Each `(host_src, sandbox_target)` pair becomes a
    /// `MS_BIND|MS_NOSUID|MS_NODEV` mount of the host directory onto the
    /// in-rootfs target. The target MUST be under `/tmp` (the only writable
    /// surface) and must be an absolute path; this is enforced at mount
    /// time, not by Rust types.
    ///
    /// The host source is a directory the parent created before spawn — it
    /// stays on the host's mount tree, so files written by the child appear
    /// on the host after the child exits (the mount itself dies with the
    /// child's mount namespace). This is how `code_run` extracts image
    /// artifacts: parent creates a host scratch dir, requests it as
    /// `/tmp/output/`, scans it after the run.
    pub extra_writable_mounts: Vec<(PathBuf, PathBuf)>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            wall_clock:       Duration::from_secs(10),
            cpu_seconds:      5,
            memory_bytes:     256 * 1024 * 1024,
            file_size_bytes:   64 * 1024 * 1024,
            nproc:             32,
            max_output_bytes: 1024 * 1024,
            rootfs:           None,
            disable_network:  true,
            working_dir:      None,
            seccomp_mode:     SeccompMode::default(),
            extra_writable_mounts: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_restrictive_enough_for_tier4() {
        let l = ResourceLimits::default();
        assert!(l.wall_clock.as_secs() <= 30);
        assert!(l.cpu_seconds <= l.wall_clock.as_secs());
        assert!(l.memory_bytes <= 1_024 * 1024 * 1024);
        assert!(l.disable_network, "network must be off by default");
        assert!(l.rootfs.is_none(), "no rootfs by default — tools opt in");
        assert!(l.working_dir.is_none(), "no chdir by default");
        assert_eq!(l.seccomp_mode, SeccompMode::Allowlist, "allowlist is the 7a-5 iter C default");
        assert!(l.extra_writable_mounts.is_empty(), "no extra mounts by default");
    }
}
