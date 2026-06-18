// SPDX-License-Identifier: AGPL-3.0-or-later

//! Lightweight process confinement for spawned plugin components — the native
//! launcher.
//!
//! When MIRA installs a packaged stdio component, it rewrites the spawn command
//! to go through `mira pkg-exec …`, which applies a hardening layer and then
//! `exec`s the real server (stdin/stdout/env inherited, so JSON-RPC over stdio
//! keeps working).
//!
//! Layers, in the order they apply:
//! - **v1** — `PR_SET_NO_NEW_PRIVS` (blocks setuid privilege escalation), no
//!   core dumps, a bounded max file size. Deliberately does NOT touch
//!   RLIMIT_AS / RLIMIT_NPROC (those break real runtimes / are per-user footguns).
//! - **v2a** — network isolation: a fresh, empty network namespace (no egress)
//!   when the component declares no network capability. Fail-closed.
//! - **v2b-1** — a seccomp-bpf denylist that kills escape/host-mutation syscalls
//!   (ptrace, mount, unshare, kexec, bpf, module-load, …). Best-effort.
//! - **v2b-2** — mount-namespace **filesystem scoping**: the host root is
//!   remounted read-only (the plugin can read system files so its interpreter +
//!   libs load, but can't write anywhere on the host), with writable "holes"
//!   bound back for only the paths the component declared (plus its private data
//!   dir and a private `/tmp`), and secret-bearing paths (provider keys, auth db,
//!   ssh/aws creds) masked behind empty overlays so the plugin can't read them.
//!   Fail-closed.
//!
//! A true *egress allowlist* (vs all-or-nothing network) remains the container
//! tier. (See design-docs/plugin-packages.md, "Capability & sandbox model".)

/// What the launcher applies before exec.
#[derive(Debug, Clone, Default)]
pub struct ConfineSpec {
    /// Max file size the process may create, in MiB. `None` leaves the default.
    pub fsize_mb: Option<u64>,
    /// Run with **no network** — a fresh, empty network namespace (only a
    /// down loopback). Used when the component declares no network egress.
    /// **Fail-closed**: if the namespace can't be created, the launch aborts
    /// rather than running unconfined. (Linux only.)
    pub no_network: bool,
    /// Enter a mount namespace and remount the host root **read-only**, then
    /// carve writable holes ([`Self::rw_paths`]) and mask secrets
    /// ([`Self::mask_paths`]). **Fail-closed.** (Linux only.)
    pub fs_scope: bool,
    /// Paths kept writable when [`Self::fs_scope`] is set (the component's
    /// declared filesystem capabilities + its private data dir). Everything
    /// else on the host is read-only.
    pub rw_paths: Vec<String>,
    /// Paths hidden behind an empty overlay when [`Self::fs_scope`] is set
    /// (directories → empty read-only tmpfs; files → empty read-only bind).
    /// Absent paths are skipped (nothing to hide).
    pub mask_paths: Vec<String>,
    /// Allowlisted egress hosts (native-tier egress allowlist). When non-empty,
    /// the launcher creates a network namespace and asks the privileged helper to
    /// filter it down to exactly these hosts. If the helper is unavailable, it
    /// degrades **offline** (the empty netns) + a notice — never silently open.
    pub egress: Vec<String>,
}

/// Build the `mira pkg-exec` wrapper invocation for a real command. Returns the
/// `(command, args)` the MCP host should spawn; spawning it confines + exec's
/// the real command. Pure — unit-testable without exec.
pub fn wrap(
    mira_exe: &str,
    command: &str,
    args: &[String],
    spec: &ConfineSpec,
) -> (String, Vec<String>) {
    let mut a = vec!["pkg-exec".to_string()];
    if let Some(mb) = spec.fsize_mb {
        a.push("--fsize-mb".to_string());
        a.push(mb.to_string());
    }
    if spec.no_network {
        a.push("--no-network".to_string());
    }
    if spec.fs_scope {
        a.push("--fs-scope".to_string());
        for p in &spec.rw_paths {
            a.push("--rw-path".to_string());
            a.push(p.clone());
        }
        for p in &spec.mask_paths {
            a.push("--mask-path".to_string());
            a.push(p.clone());
        }
    }
    for h in &spec.egress {
        a.push("--egress-host".to_string());
        a.push(h.clone());
    }
    a.push("--".to_string());
    a.push(command.to_string());
    a.extend(args.iter().cloned());
    (mira_exe.to_string(), a)
}

/// Apply confinement to the current process, then `exec` `argv`. Only returns
/// (an error) if exec fails — on success it never returns (the image is
/// replaced).
#[cfg(unix)]
pub fn exec_confined(spec: &ConfineSpec, argv: &[String]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    if argv.is_empty() {
        return std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command");
    }

    // Namespace-based isolation (network + filesystem) is FAIL-CLOSED: if
    // requested but it can't be set up, abort rather than run the server with
    // access it shouldn't have.
    #[cfg(target_os = "linux")]
    {
        // An egress allowlist needs a network namespace (to filter) and a mount
        // namespace (to point resolv.conf at the helper's resolver).
        let want_net = spec.no_network || !spec.egress.is_empty();
        let want_mount = spec.fs_scope || !spec.egress.is_empty();
        if want_net || want_mount {
            // One user namespace grants the caps for both the (optional) net
            // namespace and the (optional) mount namespace + remounts.
            if let Err(e) = enter_user_ns(want_net, want_mount) {
                return std::io::Error::new(
                    e.kind(),
                    format!("namespace setup requested but failed (fail-closed): {e}"),
                );
            }
        }
        // Native-tier egress allowlist: now that we're in a fresh (empty) netns,
        // ask the privileged helper to wire in filtered connectivity. Pathname
        // unix sockets connect across netns, so this reaches the host daemon. If
        // the helper is unavailable, we DEGRADE OFFLINE — the netns stays empty
        // (no egress) and we notify — never silently unfiltered (fail-closed).
        let mut dns_ip: Option<String> = None;
        if !spec.egress.is_empty() {
            match request_egress(&spec.egress) {
                Ok(ip) => dns_ip = Some(ip),
                Err(e) => eprintln!(
                    "mira pkg-exec: egress allowlist unavailable — running OFFLINE (no network). \
                     Install the privileged helper (`sudo mira helper-install`) for filtered \
                     egress. ({e})"
                ),
            }
        }
        if spec.fs_scope {
            if let Err(e) = apply_fs_scope(&spec.rw_paths, &spec.mask_paths) {
                return std::io::Error::new(
                    e.kind(),
                    format!("filesystem scoping requested but failed (fail-closed): {e}"),
                );
            }
        }
        // Point the confined process's DNS at the helper's per-slot dnsmasq
        // (after fs_scope's read-only remount; bind works in our private mount ns).
        if let Some(ip) = dns_ip.as_deref() {
            if let Err(e) = set_resolv_conf(ip) {
                eprintln!("mira pkg-exec: could not set resolv.conf to {ip} (continuing): {e}");
            }
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        if spec.no_network || spec.fs_scope {
            return std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "namespace confinement (no-network / fs-scope) is linux-only (fail-closed)",
            );
        }
    }

    apply_limits(spec);
    // Block escape + host-mutation syscalls (ptrace, mount, unshare, kexec, bpf,
    // module-load, reboot, keyctl, …) — reuses MIRA's Tier-4 sandbox denylist.
    // Applied LAST so it doesn't block our own namespace/mount setup above.
    // Best-effort defence-in-depth: the namespaces + no-new-privs remain if this
    // can't load.
    #[cfg(all(target_os = "linux", feature = "sandbox-linux"))]
    if let Err(e) = crate::sandbox::apply_plugin_seccomp() {
        eprintln!("mira pkg-exec: seccomp not applied (continuing): {e}");
    }
    std::process::Command::new(&argv[0]).args(&argv[1..]).exec()
}

/// Enter an unprivileged user namespace (which grants the caps to create the
/// other namespaces + do mounts without real privilege), optionally also a fresh
/// network namespace (`net`) and/or a mount namespace (`mount_ns`), then map our
/// uid/gid into it. After a net-ns the process has only a (down) loopback.
#[cfg(all(unix, target_os = "linux"))]
fn enter_user_ns(net: bool, mount_ns: bool) -> std::io::Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let mut flags = libc::CLONE_NEWUSER;
    if net {
        flags |= libc::CLONE_NEWNET;
    }
    if mount_ns {
        flags |= libc::CLONE_NEWNS;
    }
    if unsafe { libc::unshare(flags) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Order matters: setgroups=deny is required before gid_map without CAP_SETGID.
    std::fs::write("/proc/self/setgroups", b"deny\n")?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))?;
    Ok(())
}

/// Ask the privileged helper to provision native-tier egress filtering for THIS
/// process's network namespace (we've already entered it). Returns the resolver
/// IP (the helper's per-slot dnsmasq) to point `resolv.conf` at. Errors propagate
/// so the caller can degrade offline.
#[cfg(all(unix, target_os = "linux"))]
fn request_egress(allow: &[String]) -> Result<String, String> {
    let sock = crate::privhelper::default_socket();
    let pid = unsafe { libc::getpid() } as u32;
    let data = crate::privhelper::client::net_allow(&sock, pid, allow, None)?;
    data.get("dns")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "helper response missing dns ip".to_string())
}

/// Point the confined process's DNS at `dns_ip` by bind-mounting a one-line
/// `resolv.conf` over `/etc/resolv.conf` inside our (private) mount namespace.
/// Needed because the filtered netns can only reach the helper's resolver.
#[cfg(all(unix, target_os = "linux"))]
fn set_resolv_conf(dns_ip: &str) -> std::io::Result<()> {
    let path = "/tmp/.mira-resolv.conf";
    std::fs::write(path, format!("nameserver {dns_ip}\noptions ndots:0\n"))?;
    let src = std::ffi::CString::new(path).unwrap();
    let dst = c"/etc/resolv.conf";
    if unsafe {
        libc::mount(src.as_ptr(), dst.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null())
    } != 0
    {
        return Err(os_ctx("bind resolv.conf"));
    }
    Ok(())
}

/// Remount the host filesystem read-only inside our private mount namespace,
/// then bind back writable holes and mask secrets. Must run after
/// [`enter_user_ns`] with `mount_ns = true` (we need CAP_SYS_ADMIN over the new
/// mount ns, which the user ns grants).
#[cfg(all(unix, target_os = "linux"))]
fn apply_fs_scope(rw_paths: &[String], mask_paths: &[String]) -> std::io::Result<()> {
    use std::ffi::CString;
    let cstr = |s: &str| -> std::io::Result<CString> {
        CString::new(s).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("path has NUL: {s}"))
        })
    };
    let tmpfs = c"tmpfs";

    // 1. Make every mount private so our remounts don't propagate back to the
    //    host mount namespace and we're free to change them.
    let root = c"/";
    if unsafe {
        libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(os_ctx("mount(/, MS_REC|MS_PRIVATE)"));
    }

    // 2. Remount the entire host tree READ-ONLY (recursively). This is the
    //    write-confinement: the plugin can still *read* system files (so its
    //    interpreter + shared libs load) but can't *modify* anything on the host.
    flip_readonly(root, true)?;

    // 3. A private tmpfs at /tmp — writable scratch that dies with the namespace
    //    and never touches the host's /tmp. Mounted before the masks so the
    //    file-mask placeholder can live here.
    let tmp = c"/tmp";
    if unsafe {
        libc::mount(
            tmpfs.as_ptr(),
            tmp.as_ptr(),
            tmpfs.as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(os_ctx("mount(/tmp, tmpfs)"));
    }

    // 4. Carve writable holes — exactly the paths the component declared (plus
    //    its private data dir). A fresh bind of a now-RO path inherits RO; clear
    //    the flag to make just this subtree writable again.
    for p in rw_paths {
        let cp = cstr(p)?;
        if unsafe {
            libc::mount(
                cp.as_ptr(),
                cp.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(os_ctx(&format!("bind rw hole {p}")));
        }
        flip_readonly(&cp, false)?;
    }

    // 5. Mask secret-bearing paths the plugin must never read. Best-effort:
    //    the read-only root above already write-confines the plugin, so a path
    //    we can't overlay (an odd fs, a WSL drvfs mount) warns loudly rather
    //    than bricking the launch. Absent paths are nothing to hide.
    let mut placeholder_made = false;
    for m in mask_paths {
        // Follow symlinks: a secret dir reached via a link (e.g. ~/.aws ->
        // /mnt/c/Users/.../.aws on WSL) must be classified by its real target.
        let is_dir = match std::fs::metadata(m) {
            Ok(md) => md.is_dir(),
            Err(_) => continue,
        };
        if let Err(e) = mask_path(m, is_dir, &mut placeholder_made) {
            eprintln!("mira pkg-exec: warning: could not mask {m}: {e} (continuing)");
        }
    }
    Ok(())
}

/// Overlay a single secret path with an empty surface so a plugin can't read it:
/// a directory (incl. a symlink to one) gets an empty read-only tmpfs; a file
/// gets an empty read-only bind. Returns the mount error so the caller can decide
/// fail-closed vs best-effort.
#[cfg(all(unix, target_os = "linux"))]
fn mask_path(target: &str, is_dir: bool, placeholder_made: &mut bool) -> std::io::Result<()> {
    use std::ffi::CString;
    let cm = CString::new(target)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has NUL"))?;
    if is_dir {
        let tmpfs = c"tmpfs";
        if unsafe {
            libc::mount(
                tmpfs.as_ptr(),
                cm.as_ptr(),
                tmpfs.as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                c"size=16k".as_ptr().cast(),
            )
        } != 0
        {
            return Err(os_ctx("mask dir (tmpfs)"));
        }
        // Fresh single mount — a classic remount flips it read-only reliably.
        let _ = unsafe {
            libc::mount(
                std::ptr::null(),
                cm.as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                std::ptr::null(),
            )
        };
    } else {
        if !*placeholder_made {
            std::fs::write("/tmp/.mira-mask-empty", b"")?;
            *placeholder_made = true;
        }
        let empty = c"/tmp/.mira-mask-empty";
        if unsafe {
            libc::mount(empty.as_ptr(), cm.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null())
        } != 0
        {
            return Err(os_ctx("mask file (bind)"));
        }
        // Bind mounts ignore MS_RDONLY on the initial mount; a remount sets it.
        let _ = unsafe {
            libc::mount(
                std::ptr::null(),
                cm.as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
                std::ptr::null(),
            )
        };
    }
    Ok(())
}

/// Recursively set (or clear) the read-only flag on the mount subtree at `path`,
/// via `mount_setattr(AT_RECURSIVE)` (Linux 5.12+). On an older kernel
/// (`ENOSYS`) fall back to a classic non-recursive remount of the mount itself —
/// best-effort, covers the common single-mount layout.
#[cfg(all(unix, target_os = "linux"))]
fn flip_readonly(path: &std::ffi::CStr, readonly: bool) -> std::io::Result<()> {
    let attr = libc::mount_attr {
        attr_set: if readonly { libc::MOUNT_ATTR_RDONLY } else { 0 },
        attr_clr: if readonly { 0 } else { libc::MOUNT_ATTR_RDONLY },
        propagation: 0,
        userns_fd: 0,
    };
    let r = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_RECURSIVE,
            &attr as *const libc::mount_attr,
            std::mem::size_of::<libc::mount_attr>(),
        )
    };
    if r == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ENOSYS) {
        return Err(os_ctx_err("mount_setattr", err));
    }
    // Old-kernel fallback: classic bind-remount of just this mount.
    let mut flags = libc::MS_REMOUNT | libc::MS_BIND;
    if readonly {
        flags |= libc::MS_RDONLY;
    }
    if unsafe {
        libc::mount(std::ptr::null(), path.as_ptr(), std::ptr::null(), flags, std::ptr::null())
    } != 0
    {
        return Err(os_ctx("fallback remount (MS_REMOUNT|MS_BIND)"));
    }
    Ok(())
}

/// `errno`-with-context error helper.
#[cfg(all(unix, target_os = "linux"))]
fn os_ctx(ctx: &str) -> std::io::Error {
    os_ctx_err(ctx, std::io::Error::last_os_error())
}

#[cfg(all(unix, target_os = "linux"))]
fn os_ctx_err(ctx: &str, e: std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("{ctx}: {e}"))
}

#[cfg(not(unix))]
pub fn exec_confined(_spec: &ConfineSpec, _argv: &[String]) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, "pkg-exec is unix-only")
}

#[cfg(unix)]
fn apply_limits(spec: &ConfineSpec) {
    // Block privilege escalation via setuid binaries (Linux only).
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
    // No core dumps; bounded file size. Best-effort — a failed setrlimit is
    // non-fatal (we still exec; better a slightly-less-confined server than a
    // dead one).
    unsafe {
        let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
        if let Some(mb) = spec.fsize_mb {
            let bytes = mb.saturating_mul(1024 * 1024);
            let r = libc::rlimit { rlim_cur: bytes, rlim_max: bytes };
            libc::setrlimit(libc::RLIMIT_FSIZE, &r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_builds_pkg_exec_invocation() {
        let (cmd, args) = wrap(
            "/usr/bin/mira",
            "python3",
            &["server.py".to_string()],
            &ConfineSpec { fsize_mb: Some(1024), ..Default::default() },
        );
        assert_eq!(cmd, "/usr/bin/mira");
        assert_eq!(
            args,
            vec!["pkg-exec", "--fsize-mb", "1024", "--", "python3", "server.py"]
        );
    }

    #[test]
    fn wrap_without_limits_is_minimal() {
        let (_c, args) = wrap("/m", "node", &["x".into()], &ConfineSpec::default());
        assert_eq!(args, vec!["pkg-exec", "--", "node", "x"]);
    }

    #[test]
    fn wrap_includes_no_network_flag() {
        let (_c, args) = wrap(
            "/m",
            "python3",
            &["s.py".into()],
            &ConfineSpec { no_network: true, ..Default::default() },
        );
        assert_eq!(args, vec!["pkg-exec", "--no-network", "--", "python3", "s.py"]);
    }

    #[test]
    fn wrap_emits_fs_scope_rw_and_mask_paths_in_order() {
        let (_c, args) = wrap(
            "/m",
            "python3",
            &["s.py".into()],
            &ConfineSpec {
                fsize_mb: Some(512),
                no_network: true,
                fs_scope: true,
                rw_paths: vec!["/data/a".into(), "/data/b".into()],
                mask_paths: vec!["/home/u/.ssh".into()],
                egress: vec![],
            },
        );
        assert_eq!(
            args,
            vec![
                "pkg-exec",
                "--fsize-mb",
                "512",
                "--no-network",
                "--fs-scope",
                "--rw-path",
                "/data/a",
                "--rw-path",
                "/data/b",
                "--mask-path",
                "/home/u/.ssh",
                "--",
                "python3",
                "s.py",
            ]
        );
    }

    #[test]
    fn wrap_emits_egress_hosts() {
        let (_c, args) = wrap(
            "/m",
            "node",
            &["s.js".into()],
            &ConfineSpec {
                egress: vec!["api.example.com".into(), "github.com".into()],
                ..Default::default()
            },
        );
        assert_eq!(
            args,
            vec![
                "pkg-exec",
                "--egress-host",
                "api.example.com",
                "--egress-host",
                "github.com",
                "--",
                "node",
                "s.js",
            ]
        );
    }

    #[test]
    fn wrap_fs_scope_without_paths_just_flags_the_scope() {
        let (_c, args) = wrap(
            "/m",
            "node",
            &["x".into()],
            &ConfineSpec { fs_scope: true, ..Default::default() },
        );
        assert_eq!(args, vec!["pkg-exec", "--fs-scope", "--", "node", "x"]);
    }
}
