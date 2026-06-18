// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/linux.rs
//! Level A sandbox — Linux namespaces + rlimits + seccomp-bpf.
//!
//! Child process isolation is applied inside `pre_exec`, which runs in the
//! forked child between `fork()` and `execve()`. Only async-signal-safe
//! syscalls are permitted there; the ordering is:
//!
//! 1. `unshare(CLONE_NEWNS|NEWPID|NEWIPC|NEWUTS|NEWUSER[|NEWNET])` — gives
//!    us a private mount/pid/ipc/hostname namespace + unprivileged user ns
//!    so we can apply the rest without being root. Network ns is added
//!    unless `ResourceLimits::disable_network` is false.
//! 2. `setrlimit(RLIMIT_CPU|AS|FSIZE|NPROC)` — parent-stamped caps the
//!    child cannot raise.
//! 3. `prctl(PR_SET_NO_NEW_PRIVS)` — setuid binaries can no longer grant
//!    privileges inside this process; this is also a prerequisite for
//!    loading a seccomp filter as an unprivileged user.
//! 4. `pivot_root` into a read-only bind of the prebaked rootfs, with
//!    tmpfs scratch at `/tmp`, `procfs` at `/proc`, and a minimal `/dev`
//!    (just `null` and `urandom`). Skipped if `limits.rootfs` is `None`,
//!    which is what tests exercising the bare child use.
//! 5. `seccomp_load(...)` — kill the child on any attempt to invoke an
//!    escape primitive (`ptrace`, `mount`, `unshare`, `bpf`, `kexec_*`,
//!    etc.). This runs after pivot so the pivot's own `mount`/`pivot_root`
//!    calls aren't blocked. Currently a denylist; 7a-5 iter C tightens it
//!    to a per-language allowlist once we know what Python's rootfs needs.
//!
//! The parent enforces wall-clock timeout + output byte cap and kills the
//! child if either is hit.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use seccompiler::{
    apply_filter, BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch,
};

use super::{CodeSandbox, Language, ResourceLimits, SandboxError, SandboxOutput, SeccompMode};

use std::os::unix::process::ExitStatusExt;

/// Level A sandbox backend. Construct with `NamespaceSandbox::new()` or via
/// `sandbox::default_backend()`.
pub struct NamespaceSandbox;

impl NamespaceSandbox {
    pub fn new() -> Self { Self }
}

impl Default for NamespaceSandbox {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl CodeSandbox for NamespaceSandbox {
    async fn run(
        &self,
        language: Language,
        payload:  &str,
        stdin:    Option<&str>,
        limits:   &ResourceLimits,
    ) -> Result<SandboxOutput, SandboxError> {
        // Surface obvious rootfs misconfiguration before the spawn, where
        // the error message survives. pre_exec failures bubble up as opaque
        // io errors, so doing the path check here is friendlier.
        if let Some(root) = limits.rootfs.as_deref() {
            if !root.is_dir() {
                return Err(SandboxError::Policy(format!(
                    "rootfs path {} does not exist or is not a directory \
                     — run `mira sandbox install python` first",
                    root.display()
                )));
            }
        }

        let (program, args, stdin_to_send) = build_command(language, payload, stdin, limits)?;

        let limits_clone = limits.clone();

        let mut cmd = Command::new(&program);
        cmd.args(&args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.env_clear();
        cmd.env("PATH",   "/usr/bin:/bin");
        cmd.env("HOME",   "/tmp");
        cmd.env("LC_ALL", "C.UTF-8");
        // python-build-standalone bakes in `/install` as PREFIX at compile
        // time and relocates at runtime by walking up from `argv[0]` looking
        // for `lib/python3.X/os.py`. Inside our pivot that landmark exists at
        // `/lib/python3.12/os.py`, but the walk-up detection has been seen to
        // fail under WSL2; setting PYTHONHOME pins the prefix unambiguously.
        // Harmless for non-Python interpreters (they ignore it).
        cmd.env("PYTHONHOME", "/");
        // Diagnostic: pass through MIRA_LD_DEBUG to glibc's loader if set.
        // Useful for debugging "undefined symbol" / "lib not found" errors —
        // try `MIRA_LD_DEBUG=libs` or `=files` or `=symbols` on the server.
        if let Ok(v) = std::env::var("MIRA_LD_DEBUG") {
            cmd.env("LD_DEBUG", v);
        }

        // SAFETY: the closure runs in the forked child between `fork()` and
        // `execve()`. It must only call async-signal-safe syscalls.
        // `unshare`, `setrlimit`, `prctl`, and the seccomp `prctl` path are
        // all documented safe in that context.
        unsafe {
            cmd.as_std_mut().pre_exec(move || apply_isolation(&limits_clone));
        }

        let started = Instant::now();
        // Stamp parent PID so the child's `os_err()` writes to the same
        // path the parent reads on spawn failure. See `STASH_PID` docs.
        STASH_PID.store(std::process::id(), std::sync::atomic::Ordering::Relaxed);
        let err_path = sandbox_err_path();
        // Stale label from a prior failed spawn would mislead this one.
        let _ = std::fs::remove_file(&err_path);
        let mut child = cmd.spawn()
            .map_err(|e| {
                let label = std::fs::read_to_string(&err_path)
                    .ok()
                    .map(|s| format!(" [pre_exec: {}]", s.trim()))
                    .unwrap_or_default();
                let _ = std::fs::remove_file(&err_path);
                SandboxError::SpawnFailed(format!("{e}{label}"))
            })?;
        // Successful spawn — drop any stale diag file.
        let _ = std::fs::remove_file(&err_path);

        if let (Some(data), Some(mut s)) = (stdin_to_send, child.stdin.take()) {
            s.write_all(data.as_bytes()).await?;
            // Closing stdin signals EOF to the child. ok() — if the pipe is
            // already closed because the child exited early, that's fine.
            s.shutdown().await.ok();
        } else {
            // Even with no payload we drop stdin so the child doesn't hang
            // on a blocking read from an inherited pipe.
            drop(child.stdin.take());
        }

        let cap = limits.max_output_bytes;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let stdout_task = tokio::spawn(capped_read(stdout, cap));
        let stderr_task = tokio::spawn(capped_read(stderr, cap));

        let status = match tokio::time::timeout(limits.wall_clock, child.wait()).await {
            Ok(Ok(s))  => s,
            Ok(Err(e)) => return Err(SandboxError::Io(e)),
            Err(_)     => {
                let _ = child.start_kill();
                stdout_task.abort();
                stderr_task.abort();
                return Err(SandboxError::Timeout(limits.wall_clock.as_millis() as u64));
            }
        };

        let (out_bytes, out_trunc) = stdout_task.await
            .map_err(|e| SandboxError::SpawnFailed(format!("stdout reader: {e}")))??;
        let (err_bytes, err_trunc) = stderr_task.await
            .map_err(|e| SandboxError::SpawnFailed(format!("stderr reader: {e}")))??;

        // A child killed by the seccomp KillProcess action exits via SIGSYS
        // (signal 31). Surface that as a policy error so callers don't have
        // to interpret a generic exit_code = -1. The kernel emits a
        // `type=1326` audit record visible in `dmesg` (or `journalctl -k`)
        // with the offending syscall number; point operators at it instead
        // of trying to install a SIGSYS handler in the child to capture
        // si_syscall — that path is async-signal-fragile and would have to
        // survive seccomp's own restrictions on what a handler can call.
        if status.signal() == Some(libc::SIGSYS) {
            let mode = match limits.seccomp_mode {
                SeccompMode::Allowlist => "allowlist",
                SeccompMode::Denylist  => "denylist",
            };
            return Err(SandboxError::Policy(format!(
                "child terminated by seccomp filter (SIGSYS) — tried a syscall \
                 outside the {mode} profile. To identify the syscall, check \
                 the kernel audit log: `dmesg | grep -E 'audit.*syscall=' | tail` \
                 (look for `comm=\"<interpreter>\"` and `syscall=N`)."
            )));
        }

        // A child the kernel/our limits had to KILL (rather than one that exited
        // on its own) means the run blew its CPU / wall-clock / memory budget —
        // RLIMIT_CPU (SIGXCPU→SIGKILL), an OOM kill (SIGKILL), or a SIGALRM
        // watchdog. The `tokio::time::timeout` guard above only catches the case
        // where the child is *still alive* at the deadline; a CPU-bound runaway
        // hits RLIMIT_CPU and is reaped a hair *before* it, so `child.wait()`
        // returns here with a kill status. Surface that as a Timeout too —
        // otherwise a force-terminated run is reported as a silent success
        // carrying exit=137. Match both the signal form and the shell's
        // `128 + signal` exit-code propagation (the `-c` wrapper exits 137/152/142
        // when its job is killed). A script that deliberately `exit 137`s is
        // pathological; treating it as "didn't finish in budget" is acceptable.
        let killed_by_limit = matches!(
            status.signal(),
            Some(libc::SIGKILL | libc::SIGXCPU | libc::SIGALRM),
        ) || matches!(status.code(), Some(137 | 152 | 142));
        if killed_by_limit {
            return Err(SandboxError::Timeout(started.elapsed().as_millis() as u64));
        }

        Ok(SandboxOutput {
            stdout:      String::from_utf8_lossy(&out_bytes).into_owned(),
            stderr:      String::from_utf8_lossy(&err_bytes).into_owned(),
            exit_code:   status.code().unwrap_or(-1),
            duration_ms: started.elapsed().as_millis() as u64,
            truncated:   out_trunc || err_trunc,
        })
    }

    fn name(&self) -> &'static str { "namespace" }

    fn supported(&self) -> bool {
        // Best-effort probe — the real spawn still has to succeed under
        // the host's AppArmor / sysctl policy, and callers should handle
        // `SandboxError::SpawnFailed` gracefully regardless.
        std::fs::metadata("/proc/self/ns/user").is_ok()
    }
}

// ── Command construction ─────────────────────────────────────────────────────

/// Translate a `Language` + payload into `(program, args, stdin)`. Typed
/// variants point at in-rootfs interpreter paths and require `limits.rootfs`
/// to be set (so the pivot puts that interpreter at `/bin/...`).
fn build_command(
    language: Language,
    payload:  &str,
    stdin:    Option<&str>,
    limits:   &ResourceLimits,
) -> Result<(std::path::PathBuf, Vec<String>, Option<String>), SandboxError> {
    match language {
        Language::Python => {
            if limits.rootfs.is_none() {
                return Err(SandboxError::Policy(
                    "Language::Python requires limits.rootfs \
                     — run `mira sandbox install python` and pass the pivot path".into(),
                ));
            }
            // Post-pivot path: the python rootfs lays the interpreter out
            // at /bin/python3 once pivoted into.
            let prog = std::path::PathBuf::from("/bin/python3");
            // -c keeps iter A simple. iter B's `code_run` tool will switch
            // to a stdin-fed invocation so user `stdin` and `payload` can
            // coexist cleanly.
            let args = vec!["-c".to_string(), payload.to_string()];
            Ok((prog, args, stdin.map(String::from)))
        }
        Language::Node | Language::Bash => Err(SandboxError::Policy(
            "Node and Bash language runtimes ship in a future iteration".into(),
        )),
        Language::Raw { program, args } => {
            // For Raw, the caller's `stdin` wins; otherwise fall back to
            // `payload` as stdin so the convention matches typed runtimes
            // where the payload *is* the script piped to the interpreter.
            let to_send = stdin.map(|s| s.to_string())
                .or_else(|| (!payload.is_empty()).then(|| payload.to_string()));
            Ok((program, args, to_send))
        }
    }
}

// ── Child-side isolation (runs inside pre_exec) ──────────────────────────────

/// Path the child uses to drop its labelled pre_exec failure for the parent.
/// Keyed by the *parent* PID so the parent and the forked child agree on the
/// path. `std::process::id()` returns the caller's PID, which differs after
/// fork — the child would write to one path and the parent would read from
/// another. The parent stamps the static below with its own PID before spawn,
/// the child inherits it across fork, and both compute the same path.
///
/// /tmp is host-visible (this runs before pivot, so /tmp is the host /tmp),
/// and the file exists for microseconds — diagnostic only.
static STASH_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn sandbox_err_path() -> std::path::PathBuf {
    let pid = STASH_PID.load(std::sync::atomic::Ordering::Relaxed);
    let pid = if pid != 0 { pid } else { std::process::id() };
    std::env::temp_dir().join(format!("mira-sandbox-preexec-{pid}.err"))
}

/// Capture `errno` from the most recent failed libc call, log the labelled
/// step to a tempfile the parent will read on spawn failure, then return the
/// raw `io::Error`. We can't put the label in the returned Error because
/// std::process::Command's child→parent failure pipe only forwards
/// `raw_os_error()` — a wrapped Error loses the errno and surfaces as EINVAL.
fn os_err(step: &'static str) -> io::Error {
    let err = io::Error::last_os_error();
    let errno = err.raw_os_error().unwrap_or(0);
    // Best-effort write; we can't bubble a logging error from pre_exec context.
    if let Ok(path) = CString::new(sandbox_err_path().as_os_str().as_bytes()) {
        let msg = format!("{step}: errno={errno} ({err})\n");
        unsafe {
            let fd = libc::open(
                path.as_ptr(),
                libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
                0o600,
            );
            if fd >= 0 {
                let _ = libc::write(fd, msg.as_ptr() as *const _, msg.len());
                libc::close(fd);
            }
        }
    }
    io::Error::from_raw_os_error(errno)
}

fn apply_isolation(limits: &ResourceLimits) -> io::Result<()> {
    // Capture host UID/GID BEFORE unshare so we can map them into the new
    // user_ns. After unshare(CLONE_NEWUSER) without a uid_map written, the
    // kernel can't represent current_fsuid() inside the new ns; any file
    // creation in a tmpfs mounted by us then returns EOVERFLOW from
    // `may_create()`'s `kuid_has_mapping()` check.
    let orig_uid = unsafe { libc::getuid() };
    let orig_gid = unsafe { libc::getgid() };

    // 1. Unshare namespaces. CLONE_NEWUSER lets us do the rest without caps.
    //    CLONE_NEWPID does NOT move us into the new pid_ns — only the *next*
    //    fork's child becomes PID 1 there. We use that below to enter the
    //    pid_ns via a double-fork so we can mount a fresh procfs reflecting
    //    only our own pid_ns instead of bind-mounting host /proc.
    let mut flags = libc::CLONE_NEWNS
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWUSER;
    if limits.disable_network {
        flags |= libc::CLONE_NEWNET;
    }
    if unsafe { libc::unshare(flags) } != 0 {
        return Err(os_err("unshare(CLONE_NEWUSER|NEWNS|NEWPID|NEWIPC|NEWUTS|NEWNET)"));
    }

    // 1a. Identity-map orig_uid → 0 in the new user_ns. /proc here is still
    //     the host's procfs (pivot hasn't run yet), so /proc/self/* refers
    //     to this child's mappings. Order: setgroups=deny is required by
    //     the kernel before gid_map can be written without CAP_SETGID.
    write_proc_self("/proc/self/setgroups", b"deny\n")
        .map_err(|e| io::Error::other(format!("write(/proc/self/setgroups): {e}")))?;
    let uid_line = format!("0 {orig_uid} 1\n");
    write_proc_self("/proc/self/uid_map", uid_line.as_bytes())
        .map_err(|e| io::Error::other(format!("write(/proc/self/uid_map): {e}")))?;
    let gid_line = format!("0 {orig_gid} 1\n");
    write_proc_self("/proc/self/gid_map", gid_line.as_bytes())
        .map_err(|e| io::Error::other(format!("write(/proc/self/gid_map): {e}")))?;

    // 1b. Double-fork to enter the new pid_ns. The inner child becomes PID 1
    //     there and gains CAP_SYS_ADMIN over the user_ns owning the pid_ns
    //     (we own that user_ns from the unshare above), which is what
    //     `mount("proc", ...)` requires. Outer reaps inner and propagates
    //     the exit status back to mira's `child.wait()`.
    let inner_pid = unsafe { libc::fork() };
    if inner_pid < 0 {
        return Err(os_err("fork(double-fork shim)"));
    }

    if inner_pid > 0 {
        // ── OUTER ───────────────────────────────────────────────────────
        // Close any CLOEXEC fds inherited from mira. The std::process::
        // Command spawn pipe write end is one of them — without this,
        // mira's `spawn()` blocks reading the pipe forever waiting for
        // EOF, because outer holds the write end open until execve (which
        // never happens here — outer just waits + exits).
        close_cloexec_fds_above_2();

        let mut status: libc::c_int = 0;
        loop {
            let r = unsafe { libc::waitpid(inner_pid, &mut status, 0) };
            if r >= 0 { break; }
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; }
            // waitpid failure is exotic (kernel bug or ECHILD). Best-effort
            // log via stash file and exit non-zero — mira will surface this.
            let _ = os_err("waitpid(inner)");
            unsafe { libc::_exit(127) };
        }

        let exit_code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            127
        };
        unsafe { libc::_exit(exit_code) };
        // unreachable
    }

    // ── INNER (PID 1 in new pid_ns) ──────────────────────────────────────
    //
    // 1c. Die promptly if outer dies (e.g. mira sends SIGKILL on wall-clock
    //     timeout). Without PDEATHSIG, inner survives as an orphan in the
    //     new pid_ns and the user program keeps running past the cap.
    //
    //     There's a tiny race: outer could have died between fork and this
    //     prctl call; we wouldn't get the signal. Don't try to detect it via
    //     getppid() — inner is PID 1 in a fresh pid_ns and its parent (outer)
    //     lives in an ancestor pid_ns, so getppid() always returns 0 here.
    //     The window is microseconds and outer's only job pre-fork is uid_map
    //     writes; if those succeed it doesn't crash. Acceptable.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        return Err(os_err("prctl(PR_SET_PDEATHSIG)"));
    }

    // 2. Parent-stamped resource caps. Set in inner so they cap exactly the
    //    user-program-running process. (Inheritance via fork would also work
    //    but keeping all per-execve setup co-located here is clearer.)
    set_rlimit(libc::RLIMIT_CPU,    limits.cpu_seconds)
        .map_err(|e| io::Error::other(format!("setrlimit(RLIMIT_CPU): {e}")))?;
    set_rlimit(libc::RLIMIT_AS,     limits.memory_bytes)
        .map_err(|e| io::Error::other(format!("setrlimit(RLIMIT_AS): {e}")))?;
    set_rlimit(libc::RLIMIT_FSIZE,  limits.file_size_bytes)
        .map_err(|e| io::Error::other(format!("setrlimit(RLIMIT_FSIZE): {e}")))?;
    set_rlimit(libc::RLIMIT_NPROC,  u64::from(limits.nproc))
        .map_err(|e| io::Error::other(format!("setrlimit(RLIMIT_NPROC): {e}")))?;

    // 3. PR_SET_NO_NEW_PRIVS — required for unprivileged seccomp load, and
    //    kills any setuid-escalation path under us.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(os_err("prctl(PR_SET_NO_NEW_PRIVS)"));
    }

    // 4. Rootfs pivot. Skipped when limits.rootfs is None (test path with
    //    bare host fs; no Tier 4 tool ever takes that branch).
    if let Some(rootfs) = limits.rootfs.as_deref() {
        pivot_into_rootfs(rootfs, &limits.extra_writable_mounts)?;
    }

    // Optional: chdir into the post-pivot working directory. Done before
    //    seccomp so the chdir syscall isn't blocked by an allowlist that
    //    happens to omit it (chdir IS in the allowlist, but order-of-operations
    //    matches what an interactive shell does and keeps cwd predictable for
    //    the script that's about to run).
    if let Some(cwd) = limits.working_dir.as_deref() {
        chdir_into(cwd)?;
    }

    // 5. seccomp-bpf — kill the child on escape primitives (denylist) or on
    //    anything outside the curated set (allowlist). Loaded AFTER pivot so
    //    the pivot's mount/pivot_root calls aren't blocked, and AFTER chdir so
    //    a tight allowlist doesn't have to whitelist the post-pivot chdir.
    match limits.seccomp_mode {
        SeccompMode::Denylist  => apply_seccomp_denylist()
            .map_err(|e| io::Error::other(format!("seccomp(denylist): {e}")))?,
        SeccompMode::Allowlist => apply_seccomp_allowlist()
            .map_err(|e| io::Error::other(format!("seccomp(allowlist): {e}")))?,
    }

    Ok(())
}

/// Close all FD_CLOEXEC fds above 2. Called by the outer half of the
/// double-fork shim so mira's `spawn()` can read EOF on its status pipe
/// instead of blocking forever — outer never reaches execve and so the
/// pipe's write end (CLOEXEC) doesn't get closed automatically there.
///
/// Inner inherited its own fd-table copy via fork; closing here does not
/// affect inner's pipe end (which closes naturally on inner's execve or
/// _exit).
fn close_cloexec_fds_above_2() {
    let max_fd = unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 {
            rl.rlim_cur as i32
        } else {
            1024
        }
    };
    for fd in 3..max_fd {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && (flags & libc::FD_CLOEXEC) != 0 {
            unsafe { libc::close(fd) };
        }
    }
}

/// Write `data` to a /proc/self/* file using raw libc (open + write + close).
/// std::fs would allocate / take locks; this is async-signal-safe enough for
/// pre_exec context. The write must succeed in a single syscall — uid_map
/// writes are checked atomically by the kernel and partial writes are an
/// error, so we don't loop.
fn write_proc_self(path: &'static str, data: &[u8]) -> io::Result<()> {
    let path_c = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior nul in proc path"))?;
    let fd = unsafe { libc::open(path_c.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(os_err("open(/proc/self/<map>)"));
    }
    let n = unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) };
    let werr = if n < 0 { Some(io::Error::last_os_error()) } else { None };
    unsafe { libc::close(fd); }
    if let Some(e) = werr {
        return Err(e);
    }
    if (n as usize) != data.len() {
        return Err(io::Error::other("short write to /proc/self/<map>"));
    }
    Ok(())
}

/// `chdir(path)` after pivot. `path` is interpreted in the post-pivot mount
/// namespace, so `/tmp` here means the in-rootfs scratch tmpfs.
fn chdir_into(path: &Path) -> io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior nul in working_dir"))?;
    if unsafe { libc::chdir(c.as_ptr()) } != 0 {
        return Err(os_err("chdir(working_dir)"));
    }
    Ok(())
}

/// Pivot the calling process into `rootfs` with a minimal mount tree:
/// read-only bind of the rootfs as the new `/`, tmpfs scratch at `/tmp`,
/// procfs at `/proc`, and a tmpfs at `/dev` containing bind-mounts of the
/// host's `/dev/null` and `/dev/urandom`. Old root is detached.
///
/// `<rootfs>/{tmp,proc,dev,old_root}` must already exist on the underlying
/// filesystem; the rootfs installer (or test setup) is responsible for
/// pre-creating them since we re-mount the bind read-only at the end.
///
/// Runs inside `pre_exec`, so allocation is best-avoided. We use static
/// C-string literals for fixed paths and CString for the rootfs-relative
/// ones (small, bounded, and pre_exec already heaps for env clear etc.).
fn pivot_into_rootfs(
    rootfs:        &Path,
    extra_mounts:  &[(std::path::PathBuf, std::path::PathBuf)],
) -> io::Result<()> {
    fn to_cstring(p: &Path) -> io::Result<CString> {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior nul in rootfs path"))
    }

    // Static literals for fixed source/target/fs-type strings.
    let c_none        = c"none";
    let c_tmpfs       = c"tmpfs";
    let c_slash       = c"/";
    let c_old_in_new  = c"/old_root";
    let c_dev_null    = c"/dev/null";
    let c_dev_urandom = c"/dev/urandom";

    // 1. MS_REC|MS_PRIVATE on / so our subsequent mounts don't propagate
    //    to the host's mount namespace (defence-in-depth: NEWNS already
    //    isolated us, this prevents accidental shared-subtree leaks).
    if unsafe {
        libc::mount(c_none.as_ptr(), c_slash.as_ptr(), std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE, std::ptr::null())
    } != 0 {
        return Err(os_err("mount(/, MS_REC|MS_PRIVATE)"));
    }

    // 2. Bind rootfs onto itself: pivot_root requires the new root to be a
    //    mountpoint distinct from the current root. Mounted RW for now so
    //    we can populate /dev under it; remounted RO before pivot.
    let rootfs_c = to_cstring(rootfs)?;
    if unsafe {
        libc::mount(rootfs_c.as_ptr(), rootfs_c.as_ptr(), std::ptr::null(),
            libc::MS_BIND | libc::MS_REC, std::ptr::null())
    } != 0 {
        return Err(os_err("mount(rootfs, MS_BIND|MS_REC)"));
    }

    // 3. tmpfs at <rootfs>/tmp — the only writable surface visible to the
    //    sandboxed child. NOSUID|NODEV harden against any setuid trickery
    //    if a future change ever exposes setuid bits.
    let tmp_c = to_cstring(&rootfs.join("tmp"))?;
    if unsafe {
        libc::mount(c_tmpfs.as_ptr(), tmp_c.as_ptr(), c_tmpfs.as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV, std::ptr::null())
    } != 0 {
        return Err(os_err("mount(<rootfs>/tmp, tmpfs)"));
    }

    // 3a. Extra writable bind mounts under /tmp. Each pair is
    //     (host_src, sandbox_target). The target must be exactly
    //     `/tmp/<segment>` (one segment, no nesting, no `..`) — that keeps the
    //     mkdir below to a single async-signal-safe libc call and prevents an
    //     attacker-controlled path from landing somewhere unexpected. /tmp is
    //     the only writable surface anyway since the rootfs itself is about to
    //     be remounted RO. The bind is MS_NOSUID|MS_NODEV (defence-in-depth)
    //     and lives in the child's mount namespace, so it dies with the child
    //     but the underlying host directory persists for the parent to read.
    for (host_src, sandbox_target) in extra_mounts {
        let target_str = sandbox_target.to_str().ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidInput, "non-utf8 extra-mount target",
        ))?;
        let segment = target_str.strip_prefix("/tmp/").ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidInput,
            "extra-mount target must start with /tmp/",
        ))?;
        if segment.is_empty() || segment.contains('/') || segment == "." || segment == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "extra-mount target must be /tmp/<segment> with a single non-empty segment",
            ));
        }

        // <rootfs>/tmp/<segment>. Single mkdir is async-signal-safe; we
        // tolerate EEXIST because the parent may have pre-created it.
        let in_rootfs = rootfs.join("tmp").join(segment);
        let target_c  = to_cstring(&in_rootfs)?;
        if unsafe { libc::mkdir(target_c.as_ptr(), 0o755) } != 0 {
            let errno = io::Error::last_os_error().raw_os_error();
            if errno != Some(libc::EEXIST) {
                return Err(os_err("mkdir(<rootfs>/tmp/<extra-mount>)"));
            }
        }

        let host_c = to_cstring(host_src)?;
        if unsafe {
            libc::mount(host_c.as_ptr(), target_c.as_ptr(), std::ptr::null(),
                libc::MS_BIND | libc::MS_NOSUID | libc::MS_NODEV, std::ptr::null())
        } != 0 {
            return Err(os_err("mount(extra writable bind, MS_BIND)"));
        }
    }

    // 4. /proc — fresh procfs reflecting the *new* pid_ns. Possible only
    //    because apply_isolation's double-fork put us inside the new pid_ns
    //    as PID 1, and we own the user_ns that owns it (so the kernel grants
    //    CAP_SYS_ADMIN over the pid_ns, which mount("proc", ...) requires).
    //
    //    The sandbox sees only its own processes — no host cmdlines, env,
    //    or fd lists. /proc/self/exe still resolves correctly inside the
    //    new pid_ns, so glibc's $ORIGIN substitution (which python-build-
    //    standalone relies on for libpython) keeps working.
    let proc_c = to_cstring(&rootfs.join("proc"))?;
    let c_proc_fs = c"proc";
    if unsafe {
        libc::mount(c_proc_fs.as_ptr(), proc_c.as_ptr(), c_proc_fs.as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, std::ptr::null())
    } != 0 {
        return Err(os_err("mount(<rootfs>/proc, fresh procfs)"));
    }

    // 5. /dev — tmpfs with bind-mounted /dev/null and /dev/urandom on top.
    //    User namespace strips CAP_MKNOD so we can't mknod char devices
    //    directly; bind-mounting from the host's pre-existing nodes is the
    //    standard workaround (bubblewrap, runc do the same).
    let dev_dir = rootfs.join("dev");
    let dev_c = to_cstring(&dev_dir)?;
    if unsafe {
        libc::mount(c_tmpfs.as_ptr(), dev_c.as_ptr(), c_tmpfs.as_ptr(),
            libc::MS_NOSUID | libc::MS_NOEXEC, std::ptr::null())
    } != 0 {
        return Err(os_err("mount(<rootfs>/dev, tmpfs)"));
    }
    for (host_src, name) in [(c_dev_null, "null"), (c_dev_urandom, "urandom")] {
        let target = dev_dir.join(name);
        let target_c = to_cstring(&target)?;
        // Touch an empty file inside the tmpfs as the bind target.
        let fd = unsafe {
            libc::open(target_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC, 0o600)
        };
        if fd < 0 {
            return Err(os_err("open(<rootfs>/dev/<node>, O_CREAT)"));
        }
        unsafe { libc::close(fd); }
        if unsafe {
            libc::mount(host_src.as_ptr(), target_c.as_ptr(), std::ptr::null(),
                libc::MS_BIND, std::ptr::null())
        } != 0 {
            let _ = name;
            return Err(os_err("mount(<rootfs>/dev/<node>, MS_BIND from host)"));
        }
    }

    // 6. Re-mount the rootfs bind read-only. Submounts (/tmp tmpfs,
    //    /proc, /dev tmpfs) are independent and stay writable.
    if unsafe {
        libc::mount(std::ptr::null(), rootfs_c.as_ptr(), std::ptr::null(),
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY, std::ptr::null())
    } != 0 {
        return Err(os_err("mount(rootfs, MS_REMOUNT|MS_BIND|MS_RDONLY)"));
    }

    // 7. pivot_root(new_root, put_old). Routed through libc::syscall because
    //    libc doesn't expose a direct wrapper.
    let old_root_c = to_cstring(&rootfs.join("old_root"))?;
    let r = unsafe {
        libc::syscall(libc::SYS_pivot_root, rootfs_c.as_ptr(), old_root_c.as_ptr())
    };
    if r != 0 {
        return Err(os_err("pivot_root(rootfs, old_root)"));
    }

    // 8. chdir(/) so cwd refers to the new root.
    if unsafe { libc::chdir(c_slash.as_ptr()) } != 0 {
        return Err(os_err("chdir(/)"));
    }

    // 9. Detach the old root mount so the child can't traverse back into
    //    the host's filesystem via /old_root.
    if unsafe { libc::umount2(c_old_in_new.as_ptr(), libc::MNT_DETACH) } != 0 {
        return Err(os_err("umount2(/old_root, MNT_DETACH)"));
    }

    Ok(())
}

fn set_rlimit(resource: u32, cap: u64) -> io::Result<()> {
    let rl = libc::rlimit {
        rlim_cur: cap as libc::rlim_t,
        rlim_max: cap as libc::rlim_t,
    };
    // libc::setrlimit expects `__rlimit_resource_t` (`u32` on glibc targets
    // we build for). The `as _` coerces to whatever the C signature wants.
    if unsafe { libc::setrlimit(resource as _, &rl) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn target_arch() -> Option<TargetArch> {
    #[cfg(target_arch = "x86_64")]  { Some(TargetArch::x86_64) }
    #[cfg(target_arch = "aarch64")] { Some(TargetArch::aarch64) }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))] { None }
}

pub fn apply_seccomp_denylist() -> io::Result<()> {
    let arch = target_arch().ok_or_else(|| io::Error::new(
        io::ErrorKind::Unsupported,
        "seccomp not supported on this architecture",
    ))?;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for &nr in DENIED_SYSCALLS {
        // Empty rule vec ⇒ match every invocation of this syscall.
        rules.insert(nr, Vec::new());
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,        // default: allow everything we didn't list
        SeccompAction::KillProcess,  // listed ⇒ kill the child immediately
        arch,
    ).map_err(|e| io::Error::other(format!("seccomp build: {e}")))?;

    let bpf: BpfProgram = filter.try_into()
        .map_err(|e| io::Error::other(format!("seccomp compile: {e}")))?;

    apply_filter(&bpf)
        .map_err(|e| io::Error::other(format!("seccomp load: {e}")))?;

    Ok(())
}

/// Allowlist mode: only `ALLOWED_SYSCALLS` are permitted; everything else
/// terminates the child with SIGSYS. The list targets a Python interpreter
/// running short scripts in the prebaked rootfs — startup (`execve`, dynamic
/// linker, `mmap`/`brk`), basic I/O on stdin/stdout/stderr, file reads from
/// the read-only rootfs, writes to the `/tmp` tmpfs, plus the futex/clone
/// primitives the GIL needs. Network syscalls are listed too; they're
/// permitted to *call* but the network namespace has no interfaces, so any
/// `connect` they make will `ENETUNREACH`. That's the desired UX (clear
/// errno, not SIGSYS).
fn apply_seccomp_allowlist() -> io::Result<()> {
    let arch = target_arch().ok_or_else(|| io::Error::new(
        io::ErrorKind::Unsupported,
        "seccomp not supported on this architecture",
    ))?;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for &nr in ALLOWED_SYSCALLS {
        rules.insert(nr, Vec::new());
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,  // default: anything not in the rules dies
        SeccompAction::Allow,         // listed ⇒ allow
        arch,
    ).map_err(|e| io::Error::other(format!("seccomp build: {e}")))?;

    let bpf: BpfProgram = filter.try_into()
        .map_err(|e| io::Error::other(format!("seccomp compile: {e}")))?;

    apply_filter(&bpf)
        .map_err(|e| io::Error::other(format!("seccomp load: {e}")))?;

    Ok(())
}

/// Escape primitives + host-wide state mutators. All of these are either
/// (a) privilege-escalation vectors under a compromised interpreter or (b)
/// things a Tier 4 tool has no legitimate reason to call. Denying them is
/// cheap insurance; 7a-5 will add a per-language allowlist on top.
const DENIED_SYSCALLS: &[i64] = &[
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_bpf,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_reboot,
    libc::SYS_sethostname,
    libc::SYS_setdomainname,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_settimeofday,
    libc::SYS_perf_event_open,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_acct,
];

/// Curated allowlist for `SeccompMode::Allowlist`. Targets the
/// python-build-standalone CPython 3.12 interpreter running short scripts
/// against the prebaked rootfs: ELF startup (execve, dynamic linker
/// `mmap`/`mprotect`), GIL primitives (`futex`, `clone`/`clone3`), basic
/// stdio and FS I/O, time + signals, and random.
///
/// Networking syscalls are intentionally allowed: the network namespace
/// already kills egress, but a `connect()` SIGSYS is much harder for a Python
/// script to debug than the `ENETUNREACH` errno it gets when allowed through.
/// Defence-in-depth lives in the namespace, not the seccomp filter.
///
/// This list is a *starting point* derived from common knowledge of CPython
/// startup and stdlib behaviour, not from `strace -c` over a corpus. Expect to
/// iterate as failures surface in audit logs. Operators opt in explicitly via
/// `[sandbox] seccomp_mode = "allowlist"`; the default remains the denylist.
const ALLOWED_SYSCALLS: &[i64] = &[
    // ── Process lifecycle ────────────────────────────────────────────
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_exit,
    libc::SYS_exit_group,
    libc::SYS_getpid,
    libc::SYS_gettid,
    libc::SYS_getppid,
    libc::SYS_getuid,
    libc::SYS_geteuid,
    libc::SYS_getgid,
    libc::SYS_getegid,
    libc::SYS_getgroups,
    libc::SYS_getpgid,
    libc::SYS_getsid,
    libc::SYS_setpgid,
    libc::SYS_setsid,
    libc::SYS_prctl,
    libc::SYS_capget,
    libc::SYS_set_tid_address,
    libc::SYS_set_robust_list,
    libc::SYS_get_robust_list,
    libc::SYS_rseq,

    // ── Memory ───────────────────────────────────────────────────────
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_mremap,
    libc::SYS_munmap,
    libc::SYS_mprotect,
    libc::SYS_madvise,
    libc::SYS_msync,
    libc::SYS_mlock,
    libc::SYS_munlock,
    libc::SYS_mincore,
    libc::SYS_membarrier,

    // ── I/O on file descriptors ──────────────────────────────────────
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_readv,
    libc::SYS_writev,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_preadv,
    libc::SYS_pwritev,
    libc::SYS_preadv2,
    libc::SYS_pwritev2,
    libc::SYS_lseek,
    libc::SYS_fcntl,
    libc::SYS_ioctl,
    libc::SYS_dup,
    libc::SYS_dup3,
    libc::SYS_pipe2,
    libc::SYS_close,
    libc::SYS_close_range,
    // libc doesn't export SYS_sendfile for aarch64 (asm-generic syscall ABI),
    // though the syscall exists there — supply the number so arm64 compiles.
    #[cfg(not(target_arch = "aarch64"))] libc::SYS_sendfile,
    #[cfg(target_arch = "aarch64")] 71,
    libc::SYS_copy_file_range,
    libc::SYS_splice,
    libc::SYS_tee,
    libc::SYS_vmsplice,
    // Same as SYS_sendfile above — libc lacks the aarch64 const.
    #[cfg(not(target_arch = "aarch64"))] libc::SYS_fadvise64,
    #[cfg(target_arch = "aarch64")] 223,
    libc::SYS_fallocate,

    // ── Filesystem ───────────────────────────────────────────────────
    libc::SYS_openat,
    libc::SYS_openat2,
    libc::SYS_statx,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_fstatfs,
    libc::SYS_statfs,
    libc::SYS_getcwd,
    libc::SYS_chdir,
    libc::SYS_fchdir,
    libc::SYS_readlinkat,
    libc::SYS_getdents64,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
    libc::SYS_ftruncate,
    libc::SYS_truncate,
    libc::SYS_unlinkat,
    libc::SYS_renameat2,
    libc::SYS_renameat,
    libc::SYS_mkdirat,
    libc::SYS_linkat,
    libc::SYS_symlinkat,
    libc::SYS_fchmod,
    libc::SYS_fchmodat,
    libc::SYS_fchown,
    libc::SYS_fchownat,
    libc::SYS_umask,
    libc::SYS_utimensat,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_sync,
    libc::SYS_sync_file_range,
    libc::SYS_inotify_init1,
    libc::SYS_inotify_add_watch,
    libc::SYS_inotify_rm_watch,

    // ── Time ─────────────────────────────────────────────────────────
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_gettimeofday,
    libc::SYS_times,
    libc::SYS_timer_create,
    libc::SYS_timer_settime,
    libc::SYS_timer_gettime,
    libc::SYS_timer_delete,
    libc::SYS_timer_getoverrun,
    libc::SYS_timerfd_create,
    libc::SYS_timerfd_settime,
    libc::SYS_timerfd_gettime,

    // ── Signals ──────────────────────────────────────────────────────
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_rt_sigtimedwait,
    libc::SYS_rt_sigsuspend,
    libc::SYS_rt_sigpending,
    libc::SYS_rt_sigqueueinfo,
    libc::SYS_rt_tgsigqueueinfo,
    libc::SYS_sigaltstack,
    libc::SYS_kill,
    libc::SYS_tgkill,
    libc::SYS_tkill,
    libc::SYS_pidfd_send_signal,

    // ── Futex / scheduling ───────────────────────────────────────────
    libc::SYS_futex,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_sched_setaffinity,
    libc::SYS_sched_getparam,
    libc::SYS_sched_getscheduler,
    libc::SYS_sched_setscheduler,
    libc::SYS_sched_get_priority_max,
    libc::SYS_sched_get_priority_min,
    libc::SYS_sched_rr_get_interval,

    // ── Random ───────────────────────────────────────────────────────
    libc::SYS_getrandom,

    // ── Process management ───────────────────────────────────────────
    libc::SYS_wait4,
    libc::SYS_waitid,
    libc::SYS_clone,
    libc::SYS_clone3,

    // ── Polling / multiplexing ───────────────────────────────────────
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_pwait,
    libc::SYS_epoll_pwait2,
    libc::SYS_ppoll,
    libc::SYS_pselect6,
    libc::SYS_eventfd2,
    libc::SYS_signalfd4,
    libc::SYS_pidfd_open,
    libc::SYS_pidfd_getfd,

    // ── Networking (allowed; net-ns gives ENETUNREACH on connect) ────
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept4,
    libc::SYS_connect,
    libc::SYS_getsockname,
    libc::SYS_getpeername,
    libc::SYS_sendto,
    libc::SYS_recvfrom,
    libc::SYS_sendmsg,
    libc::SYS_recvmsg,
    libc::SYS_sendmmsg,
    libc::SYS_recvmmsg,
    libc::SYS_setsockopt,
    libc::SYS_getsockopt,
    libc::SYS_shutdown,

    // ── Resource queries ─────────────────────────────────────────────
    libc::SYS_prlimit64,
    libc::SYS_getrlimit,
    libc::SYS_setrlimit,
    libc::SYS_getrusage,
    libc::SYS_sysinfo,
    libc::SYS_uname,
    libc::SYS_getpriority,
    libc::SYS_setpriority,

    // ── x86_64-only legacy syscalls used by glibc + CPython startup ──
    #[cfg(target_arch = "x86_64")] libc::SYS_arch_prctl,
    #[cfg(target_arch = "x86_64")] libc::SYS_open,
    #[cfg(target_arch = "x86_64")] libc::SYS_stat,
    #[cfg(target_arch = "x86_64")] libc::SYS_lstat,
    #[cfg(target_arch = "x86_64")] libc::SYS_access,
    #[cfg(target_arch = "x86_64")] libc::SYS_pipe,
    #[cfg(target_arch = "x86_64")] libc::SYS_dup2,
    #[cfg(target_arch = "x86_64")] libc::SYS_select,
    #[cfg(target_arch = "x86_64")] libc::SYS_poll,
    #[cfg(target_arch = "x86_64")] libc::SYS_pause,
    #[cfg(target_arch = "x86_64")] libc::SYS_getdents,
    #[cfg(target_arch = "x86_64")] libc::SYS_readlink,
    #[cfg(target_arch = "x86_64")] libc::SYS_unlink,
    #[cfg(target_arch = "x86_64")] libc::SYS_rmdir,
    #[cfg(target_arch = "x86_64")] libc::SYS_mkdir,
    #[cfg(target_arch = "x86_64")] libc::SYS_link,
    #[cfg(target_arch = "x86_64")] libc::SYS_symlink,
    #[cfg(target_arch = "x86_64")] libc::SYS_rename,
    #[cfg(target_arch = "x86_64")] libc::SYS_chmod,
    #[cfg(target_arch = "x86_64")] libc::SYS_chown,
    #[cfg(target_arch = "x86_64")] libc::SYS_lchown,
    #[cfg(target_arch = "x86_64")] libc::SYS_utime,
    #[cfg(target_arch = "x86_64")] libc::SYS_utimes,
    #[cfg(target_arch = "x86_64")] libc::SYS_creat,
    #[cfg(target_arch = "x86_64")] libc::SYS_alarm,
    #[cfg(target_arch = "x86_64")] libc::SYS_fork,
    #[cfg(target_arch = "x86_64")] libc::SYS_vfork,
    #[cfg(target_arch = "x86_64")] libc::SYS_time,
    #[cfg(target_arch = "x86_64")] libc::SYS_eventfd,
    #[cfg(target_arch = "x86_64")] libc::SYS_signalfd,
    #[cfg(target_arch = "x86_64")] libc::SYS_epoll_create,
    #[cfg(target_arch = "x86_64")] libc::SYS_epoll_wait,
    #[cfg(target_arch = "x86_64")] libc::SYS_inotify_init,
    #[cfg(target_arch = "x86_64")] libc::SYS_getpgrp,
    #[cfg(target_arch = "x86_64")] libc::SYS_accept,
];

// ── Parent-side I/O helpers ──────────────────────────────────────────────────

/// Read from a pipe into a byte cap. Beyond the cap, keep draining (so the
/// child doesn't block on a full pipe) but discard the overflow and flag
/// truncation.
async fn capped_read<R: AsyncReadExt + Unpin>(
    mut src: R,
    cap:     usize,
) -> io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::with_capacity(cap.min(4096));
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = src.read(&mut chunk).await?;
        if n == 0 { break; }
        let remaining = cap.saturating_sub(buf.len());
        if n <= remaining {
            buf.extend_from_slice(&chunk[..n]);
        } else {
            buf.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            // keep looping to drain the pipe
        }
    }
    Ok((buf, truncated))
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Best-effort check for unprivileged user-ns support. Returning true
    /// doesn't guarantee spawn will succeed (AppArmor on Ubuntu 24.04 can
    /// still deny it), so tests also treat `SpawnFailed` as a skip signal.
    fn ns_probably_ok() -> bool {
        if std::fs::metadata("/proc/self/ns/user").is_err() {
            return false;
        }
        match std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
            Ok(s)  => s.trim() == "1",
            Err(_) => true, // absent on newer kernels where it's always allowed
        }
    }

    fn skip_if_ns_unavailable() -> bool {
        if ns_probably_ok() { return false; }
        eprintln!("sandbox test skipped: user namespaces unavailable on this host");
        true
    }

    fn handle_spawn_skip<T>(r: Result<T, SandboxError>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(SandboxError::SpawnFailed(e))
                if e.contains("Operation not permitted") || e.contains("EPERM") =>
            {
                eprintln!("sandbox test skipped: spawn denied ({e})");
                None
            }
            Err(e) => panic!("unexpected sandbox error: {e}"),
        }
    }

    fn which(bin: &str) -> Option<PathBuf> {
        for dir in ["/bin", "/usr/bin"] {
            let p = PathBuf::from(dir).join(bin);
            if p.exists() { return Some(p); }
        }
        None
    }

    #[tokio::test]
    async fn echo_roundtrips_stdout() {
        if skip_if_ns_unavailable() { return; }
        let Some(echo) = which("echo") else {
            eprintln!("skip: /bin/echo missing"); return;
        };

        let sandbox = NamespaceSandbox::new();
        let out = handle_spawn_skip(sandbox.run(
            Language::Raw { program: echo, args: vec!["hello".into()] },
            "", None, &ResourceLimits::default(),
        ).await);
        let Some(out) = out else { return; };

        assert_eq!(out.exit_code, 0, "echo should exit 0");
        assert!(out.stdout.contains("hello"), "stdout = {:?}", out.stdout);
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn timeout_kills_runaway() {
        if skip_if_ns_unavailable() { return; }
        let Some(sleep) = which("sleep") else {
            eprintln!("skip: /bin/sleep missing"); return;
        };

        let sandbox = NamespaceSandbox::new();
        let limits = ResourceLimits {
            wall_clock: Duration::from_millis(300),
            ..ResourceLimits::default()
        };
        let r = sandbox.run(
            Language::Raw { program: sleep, args: vec!["5".into()] },
            "", None, &limits,
        ).await;
        match r {
            Err(SandboxError::Timeout(ms))   => assert!(ms >= 300 && ms <= 2000),
            Err(SandboxError::SpawnFailed(e)) => eprintln!("skip: spawn denied ({e})"),
            other => panic!("expected Timeout, got {:?}", other.map(|_| "Ok")),
        }
    }

    #[tokio::test]
    async fn output_cap_truncates() {
        if skip_if_ns_unavailable() { return; }
        let Some(sh) = which("sh") else {
            eprintln!("skip: /bin/sh missing"); return;
        };

        // Pure-POSIX loop emitting ~80 KB total.
        let script = r#"i=1; while [ $i -le 5000 ]; do echo hello-world; i=$((i+1)); done"#;

        let sandbox = NamespaceSandbox::new();
        let limits = ResourceLimits {
            max_output_bytes: 1024,
            wall_clock: Duration::from_secs(5),
            ..ResourceLimits::default()
        };
        let out = handle_spawn_skip(sandbox.run(
            Language::Raw { program: sh, args: vec!["-c".into(), script.into()] },
            "", None, &limits,
        ).await);
        let Some(out) = out else { return; };

        assert!(out.truncated, "expected truncation, got {} bytes", out.stdout.len());
        assert!(out.stdout.len() <= 1024 + 16, "captured more than cap+fudge: {}", out.stdout.len());
    }

    #[tokio::test]
    async fn missing_rootfs_path_returns_policy() {
        // The parent should reject a non-existent rootfs path before spawn,
        // with a message pointing the operator at the install command.
        let echo = which("echo").unwrap_or_else(|| PathBuf::from("/bin/echo"));
        let sandbox = NamespaceSandbox::new();
        let limits = ResourceLimits {
            rootfs: Some(PathBuf::from("/tmp/does-not-exist-12345-mira")),
            ..ResourceLimits::default()
        };
        let r = sandbox.run(
            Language::Raw { program: echo, args: vec!["hi".into()] },
            "", None, &limits,
        ).await;
        match r {
            Err(SandboxError::Policy(msg)) => {
                assert!(msg.contains("rootfs") && msg.contains("install"),
                    "error should mention rootfs + install, got: {msg}");
            }
            other => panic!("expected Policy about rootfs, got {:?}", other.map(|_| "Ok")),
        }
    }

    #[tokio::test]
    async fn python_without_rootfs_returns_policy() {
        // build_command refuses Language::Python unless the caller has set
        // limits.rootfs — there's no host /usr/bin/python3 fallback.
        let sandbox = NamespaceSandbox::new();
        let r = sandbox.run(Language::Python, "print(1)", None, &ResourceLimits::default()).await;
        match r {
            Err(SandboxError::Policy(msg)) => {
                assert!(msg.contains("Python") && msg.contains("rootfs"),
                    "error should mention Python + rootfs, got: {msg}");
            }
            other => panic!("expected Policy, got {:?}", other.map(|_| "Ok")),
        }
    }

    #[tokio::test]
    async fn pivot_isolates_into_minimal_rootfs() {
        // Build a tiny rootfs in a temp dir containing /bin/sh + its dynamic
        // dependencies, pivot into it, and verify (a) /tmp is writable and
        // (b) the pivoted child can't see the host's /etc/passwd.
        if skip_if_ns_unavailable() { return; }
        let Some(sh) = which("sh") else {
            eprintln!("skip: /bin/sh missing"); return;
        };

        let tmp = match tempfile::tempdir() {
            Ok(t)  => t,
            Err(e) => { eprintln!("skip: tempdir failed: {e}"); return; }
        };
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        // Mountpoints for the in-pivot mounts.
        for sub in ["tmp", "proc", "dev", "old_root"] {
            std::fs::create_dir_all(rootfs.join(sub)).unwrap();
        }

        if let Err(e) = copy_with_deps(&sh, &rootfs) {
            eprintln!("skip: couldn't materialise fake rootfs: {e}");
            return;
        }

        let sandbox = NamespaceSandbox::new();
        let limits = ResourceLimits {
            rootfs: Some(rootfs.clone()),
            wall_clock: Duration::from_secs(5),
            ..ResourceLimits::default()
        };

        // Test 1: /tmp writable inside pivot, no leakage of host /etc.
        // Uses POSIX `read` instead of `cat` because the minimal rootfs
        // built by copy_with_deps only contains /bin/sh — no coreutils.
        let script =
            "echo hi > /tmp/x && read x < /tmp/x && echo \"$x\" && ls /etc 2>&1 || true";
        let out = handle_spawn_skip(sandbox.run(
            Language::Raw {
                program: PathBuf::from("/bin/sh"),
                args:    vec!["-c".into(), script.into()],
            },
            "", None, &limits,
        ).await);
        let Some(out) = out else { return; };

        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert!(out.stdout.starts_with("hi"),
            "expected /tmp roundtrip; got {:?} / {:?}", out.stdout, out.stderr);
        assert!(!out.stdout.contains("passwd"),
            "host /etc should not be visible after pivot, got stdout={:?}", out.stdout);
        assert!(!out.stdout.contains("shadow"),
            "host /etc should not be visible after pivot, got stdout={:?}", out.stdout);
    }

    /// Materialise a tiny rootfs at `dest_root` containing `binary` and the
    /// shared libraries `ldd` reports it needs. Best-effort: returns Err if
    /// ldd is missing or fails (caller treats as skip).
    fn copy_with_deps(binary: &PathBuf, dest_root: &PathBuf) -> io::Result<()> {
        use std::process::Command as StdCommand;

        let output = StdCommand::new("ldd").arg(binary).output()?;
        if !output.status.success() {
            return Err(io::Error::new(io::ErrorKind::Other,
                format!("ldd failed: {}", String::from_utf8_lossy(&output.stderr))));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);

        let mut to_copy: Vec<PathBuf> = vec![binary.clone()];
        for line in stdout.lines() {
            let line = line.trim();
            // Either:  libc.so.6 => /lib/.../libc.so.6 (0x...)
            //   or:    /lib64/ld-linux-x86-64.so.2 (0x...)
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
            let rel = src.strip_prefix("/")
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
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
}
