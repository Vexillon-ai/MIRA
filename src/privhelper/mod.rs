// SPDX-License-Identifier: AGPL-3.0-or-later

//! Privileged helper — MIRA's least-privilege elevation core.
//!
//! The big, network-facing main process stays unprivileged; the few operations
//! that genuinely need elevation are factored into a tiny, auditable **root
//! daemon** (`mira-helper.service`) that the main process calls on demand over a
//! locked unix socket, via a **fixed enum of vetted operations** (never arbitrary
//! exec). Privilege separation — the same pattern as sudo / polkit / dockerd.
//!
//! - [`daemon`] — the root daemon (`mira helper-daemon`).
//! - [`client`] — the unprivileged caller + the availability/`cap_net_admin`
//!   **probe** that drives capability-tier selection (privileged vs best-effort).
//! - [`protocol`] — the request/response wire types.
//! - [`net`] — the native-tier **egress allowlist** plumbing (the first real op).
//! - [`unit`] — the systemd system unit.
//!
//! First consumer: the native-tier **egress allowlist** (`NetAllow`/`NetTeardown`)
//! — filtering a native confined subprocess's egress needs host-side
//! veth/NAT/nftables, i.e. real `CAP_NET_ADMIN` in the host netns, which only this
//! helper can provide. See [`net`].

pub mod client;
pub mod daemon;
pub mod net;
pub mod protocol;
pub mod unit;
pub mod wsl;

use std::path::{Path, PathBuf};

pub use protocol::{Request, Response, DEFAULT_SOCKET};

/// The default socket path as a `PathBuf`.
pub fn default_socket() -> PathBuf {
    PathBuf::from(DEFAULT_SOCKET)
}

/// Resolve the MIRA-user uid the socket should be owned by: explicit flag, else
/// `SUDO_UID` (the user who ran `sudo mira helper-install`).
pub fn resolve_owner_uid(explicit: Option<u32>) -> Result<u32, String> {
    if let Some(u) = explicit {
        return Ok(u);
    }
    if let Ok(s) = std::env::var("SUDO_UID") {
        if let Ok(u) = s.parse::<u32>() {
            return Ok(u);
        }
    }
    Err("could not determine the MIRA user uid (run via sudo, or pass --owner-uid)".into())
}

/// The dedicated unprivileged system user the daemon runs as.
const HELPER_USER: &str = "mira-helper";
/// System path the helper binary is installed to (so the non-root user can exec
/// it without access to the operator's home).
const HELPER_BIN_DIR: &str = "/usr/local/lib/mira";
const HELPER_BIN: &str = "/usr/local/lib/mira/mira-helper";

/// Install + start the helper service. Must run as root (`sudo mira
/// helper-install`). Creates a dedicated non-root system user, installs the
/// binary to a system path that user can exec, enables IP forwarding at the host
/// level (the non-root daemon can't write the root-owned sysctl), writes the
/// unit, and (re)starts the service.
pub fn install(mira_bin: &Path, socket: &Path, owner_uid: u32) -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("helper-install must run as root — try: sudo mira helper-install".into());
    }

    // 1. Dedicated unprivileged system user the daemon runs as.
    ensure_system_user(HELPER_USER)?;

    // 2. Install the binary to a system path. Copy-then-rename so re-installing
    //    over the running daemon's binary doesn't hit ETXTBSY.
    std::fs::create_dir_all(HELPER_BIN_DIR).map_err(|e| format!("mkdir {HELPER_BIN_DIR}: {e}"))?;
    let staged = format!("{HELPER_BIN}.new");
    std::fs::copy(mira_bin, &staged).map_err(|e| format!("copy helper binary: {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod helper binary: {e}"))?;
    }
    std::fs::rename(&staged, HELPER_BIN).map_err(|e| format!("install helper binary: {e}"))?;

    // 3. The non-root daemon can't write the root-owned net.ipv4.ip_forward sysctl,
    //    so enable forwarding at the host level here (idempotent).
    let _ = std::fs::write("/etc/sysctl.d/99-mira-helper.conf", "net.ipv4.ip_forward=1\n");
    let _ = std::process::Command::new("sysctl").args(["-wq", "net.ipv4.ip_forward=1"]).status();

    // 4. Reset egress state from any previous daemon so the new non-root daemon
    //    starts clean (it can't kill a root-era dropped-uid dnsmasq).
    reset_egress_state();

    // 5. Unit → ExecStart points at the installed system binary.
    let unit = unit::render(Path::new(HELPER_BIN), socket, owner_uid);
    let unit_path = "/etc/systemd/system/mira-helper.service";
    std::fs::write(unit_path, unit).map_err(|e| format!("write {unit_path}: {e}"))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "mira-helper.service"])?;
    // `restart` (not `enable --now`) so re-running install on an already-running
    // service actually redeploys the new binary/unit instead of no-op'ing.
    systemctl(&["restart", "mira-helper.service"])?;

    // 6. On WSL, also install the host-alias boot hook so the operator can reach
    //    Windows-host services by a stable name (`windows-host`) instead of the
    //    NAT-gateway IP that changes every reboot. Best-effort: a failure here
    //    never blocks the (already-running) helper. See `wsl`.
    if crate::install::is_wsl() {
        match wsl::install_host_alias(wsl::DEFAULT_ALIAS) {
            Ok(d)  => println!(
                "✓ WSL host-alias '{}' installed — point Windows-host service URLs at {}",
                wsl::DEFAULT_ALIAS,
                d.get("use_url_form").and_then(|v| v.as_str()).unwrap_or("http://windows-host:<PORT>"),
            ),
            Err(e) => eprintln!("note: WSL host-alias setup skipped: {e}"),
        }
    }
    Ok(())
}

/// Create the dedicated system user if it doesn't already exist.
fn ensure_system_user(name: &str) -> Result<(), String> {
    let exists = std::process::Command::new("getent")
        .args(["passwd", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if exists {
        return Ok(());
    }
    let out = std::process::Command::new("useradd")
        .args(["--system", "--no-create-home", "--shell", "/usr/sbin/nologin", name])
        .output()
        .map_err(|e| format!("useradd {name}: {e}"))?;
    // exit 9 = name already in use (lost a race) — fine.
    if out.status.success() || out.status.code() == Some(9) {
        Ok(())
    } else {
        Err(format!("useradd {name} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Kill any leftover per-slot dnsmasq (root can signal the dropped-uid ones the
/// new non-root daemon couldn't) and drop the root-era marker dir. Remaining
/// nft tables / veths are cleaned by the new daemon's startup reaper.
fn reset_egress_state() {
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                continue;
            };
            let is_dnsmasq = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|c| c.trim() == "dnsmasq")
                .unwrap_or(false);
            if !is_dnsmasq {
                continue;
            }
            if let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) {
                if String::from_utf8_lossy(&cmd).contains("mira_egr") {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all("/run/mira-helper-state");
}

pub(crate) fn systemctl(args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| format!("systemctl {args:?}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}
