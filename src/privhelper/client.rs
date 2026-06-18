// SPDX-License-Identifier: AGPL-3.0-or-later

//! Client side of the privileged helper — used by the unprivileged main MIRA to
//! call the daemon, and to **detect** whether the helper is available (the
//! capability-tier probe that decides "proper" vs best-effort + notify).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use super::protocol::{Request, Response};

/// Send one request and read one response. Short-lived connection per call.
pub fn call(socket: &Path, req: &Request) -> Result<Response, String> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');
    (&stream).write_all(line.as_bytes()).map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).map_err(|e| e.to_string())?;
    serde_json::from_str(resp.trim()).map_err(|e| format!("bad response: {e}"))
}

/// Whether the privileged helper is reachable + healthy. Returns the `Ping`
/// payload (version, `cap_net_admin`, …) on success, or `None` if the helper is
/// absent/unreachable — the caller then degrades to best-effort + a notification.
pub fn probe(socket: &Path) -> Option<serde_json::Value> {
    match call(socket, &Request::Ping) {
        Ok(r) if r.ok => r.data,
        _ => None,
    }
}

/// Does the helper exist *and* actually hold `CAP_NET_ADMIN`? The gate for
/// routing a privileged-tier operation to it.
pub fn has_net_admin(socket: &Path) -> bool {
    probe(socket)
        .and_then(|d| d.get("cap_net_admin").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

/// Ask the helper to provision native-tier egress filtering for `pid`. Returns
/// the provisioning details (slot, subnet, dns IP, …) on success.
pub fn net_allow(
    socket: &Path,
    pid: u32,
    allow: &[String],
    upstream: Option<&str>,
) -> Result<serde_json::Value, String> {
    let req = Request::NetAllow {
        pid,
        allow: allow.to_vec(),
        upstream: upstream.map(str::to_string),
    };
    let r = call(socket, &req)?;
    if r.ok {
        Ok(r.data.unwrap_or(serde_json::Value::Null))
    } else {
        Err(r.error.unwrap_or_else(|| "net_allow failed".into()))
    }
}

/// Ask the helper to tear down `pid`'s egress slot. Idempotent on the daemon
/// side, so safe to call unconditionally on subprocess exit.
pub fn net_teardown(socket: &Path, pid: u32) -> Result<(), String> {
    let r = call(socket, &Request::NetTeardown { pid })?;
    if r.ok {
        Ok(())
    } else {
        Err(r.error.unwrap_or_else(|| "net_teardown failed".into()))
    }
}
