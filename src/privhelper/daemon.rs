// SPDX-License-Identifier: AGPL-3.0-or-later

//! The privileged helper daemon (`mira helper-daemon`, run by the root
//! `mira-helper.service`).
//!
//! Binds a unix socket, locks it down to the MIRA user, and serves the fixed
//! [`Request`] op set. Security posture: connections are accepted only from the
//! configured owner uid (verified via `SO_PEERCRED`, not just socket perms);
//! every request is **audit-logged** to the journal; unknown ops can't even
//! deserialize. The daemon does the minimum the op needs and nothing else.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use super::net::NetManager;
use super::protocol::{Request, Response};

pub struct DaemonOpts {
    pub socket_path: PathBuf,
    /// Only accept connections from this uid (and root). When `None`, any local
    /// peer may connect (socket perms still apply).
    pub owner_uid: Option<u32>,
}

/// Run the daemon loop. Blocks; returns only on a fatal bind/listen error.
pub fn run(opts: &DaemonOpts) -> std::io::Result<()> {
    if let Some(parent) = opts.socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A stale socket from a previous run would make bind() fail with EADDRINUSE.
    let _ = std::fs::remove_file(&opts.socket_path);
    let listener = UnixListener::bind(&opts.socket_path)?;
    lock_socket(&opts.socket_path, opts.owner_uid)?;

    audit(&format!(
        "listening on {} (owner_uid={:?}, cap_net_admin={})",
        opts.socket_path.display(),
        opts.owner_uid,
        has_net_admin()
    ));

    // Native-tier egress slot manager (marker-backed, restart-safe).
    let net = std::sync::Arc::new(NetManager::new());
    // Reap orphans left by a previous run: confined processes may have come and
    // gone (or be alive) across a daemon restart, which drops in-memory state.
    net.reap_dead();
    // Background reaper: periodically tear down slots whose confined pid is gone
    // (the confined process exec's its server, so on exit the netns dies but the
    // per-slot dnsmasq + nft table would otherwise leak until the next NetAllow).
    {
        let reaper = std::sync::Arc::clone(&net);
        std::thread::spawn(move || loop {
            std::thread::sleep(super::net::REAP_INTERVAL);
            reaper.reap_dead();
        });
    }

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                if let Err(e) = handle(stream, opts, &net) {
                    audit(&format!("connection error: {e}"));
                }
            }
            Err(e) => audit(&format!("accept error: {e}")),
        }
    }
    Ok(())
}

fn handle(stream: UnixStream, opts: &DaemonOpts, net: &NetManager) -> std::io::Result<()> {
    // Verify the peer is the MIRA user (or root) — kernel-attested, unforgeable.
    if let Some(want) = opts.owner_uid {
        let uid = peer_uid(&stream)?;
        if uid != want && uid != 0 {
            audit(&format!("REJECTED connection from uid {uid} (allowed: {want} or 0)"));
            return Ok(());
        }
    }
    let peer = peer_uid(&stream).unwrap_or(u32::MAX);

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let resp = dispatch(line.trim(), peer, net);

    let mut w = stream;
    let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"ok":false}"#.to_string());
    out.push('\n');
    w.write_all(out.as_bytes())?;
    Ok(())
}

/// Parse + validate + execute a single request. Every accepted op is audited.
fn dispatch(line: &str, peer_uid: u32, net: &NetManager) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            audit(&format!("uid={peer_uid} BAD REQUEST: {e}"));
            return Response::err(format!("bad request: {e}"));
        }
    };
    audit(&format!("uid={peer_uid} op={req:?}"));
    match req {
        Request::Ping => Response::ok(serde_json::json!({
            "pong": true,
            "version": env!("CARGO_PKG_VERSION"),
            "cap_net_admin": has_net_admin(),
            "euid": unsafe { libc::geteuid() },
        })),
        Request::NetAllow { pid, allow, upstream } => {
            if !has_net_admin() {
                return Response::err("daemon lacks CAP_NET_ADMIN — cannot provision egress");
            }
            match net.allow(pid, &allow, upstream.as_deref(), peer_uid) {
                Ok(data) => {
                    audit(&format!("uid={peer_uid} NetAllow pid={pid} OK: {data}"));
                    Response::ok(data)
                }
                Err(e) => {
                    audit(&format!("uid={peer_uid} NetAllow pid={pid} FAILED: {e}"));
                    Response::err(e)
                }
            }
        }
        Request::NetTeardown { pid } => match net.teardown(pid) {
            Ok(data) => Response::ok(data),
            Err(e) => Response::err(e),
        },
    }
}

/// One audit line per event → the journal (the daemon runs under systemd with
/// `StandardError=journal`).
fn audit(msg: &str) {
    eprintln!("mira-helper: {msg}");
}

/// Lock the socket to mode 0600 and (when set) chown it to the MIRA user so only
/// that user can connect.
fn lock_socket(path: &Path, owner_uid: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    if let Some(uid) = owner_uid {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket path has NUL"))?;
        // gid = (gid_t)-1 leaves the group unchanged.
        let r = unsafe { libc::chown(c.as_ptr(), uid, u32::MAX) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Kernel-attested uid of the connected peer (`SO_PEERCRED`).
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Whether this process actually holds `CAP_NET_ADMIN` (root, or granted via
/// systemd `AmbientCapabilities`). Parses the effective capability set.
pub fn has_net_admin() -> bool {
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    const CAP_NET_ADMIN: u64 = 12;
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("CapEff:").map(|v| v.trim().to_string()))
        })
        .and_then(|hex| u64::from_str_radix(&hex, 16).ok())
        .map(|caps| caps & (1 << CAP_NET_ADMIN) != 0)
        .unwrap_or(false)
}
