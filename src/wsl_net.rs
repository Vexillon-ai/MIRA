// SPDX-License-Identifier: AGPL-3.0-or-later

//! WSL host-URL misrouting detection (the unprivileged half of the WSL story).
//!
//! On WSL2 NAT, service URLs pointed at the Windows host's *LAN* IP can't be
//! reached from the guest — only the `windows-host` alias (see
//! [`crate::privhelper::wsl`]) works. This scans the live config for service
//! URLs whose host is a literal private IP, and **empirically** flags the ones
//! that are misrouted: the configured address fails to connect *and*
//! `windows-host:<same-port>` succeeds. That proof-by-probe means we never guess
//! — we only suggest the swap when it demonstrably fixes the connection (so a URL
//! pointing at a *different* LAN box is left alone).

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

use crate::config::MiraConfig;

/// The alias the WSL host-alias hook maps to the Windows host.
const ALIAS: &str = "windows-host";
const PROBE_TIMEOUT: Duration = Duration::from_millis(600);

/// A configured service URL that can't be reached at its current address but
/// would work via `windows-host`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MisroutedUrl {
    /// Dotted JSON path into the config (e.g. `providers.lmstudio.url`).
    pub path:      String,
    pub current:   String,
    pub suggested: String,
}

/// True when running inside WSL (reuses the install-time probe).
pub fn is_wsl() -> bool {
    crate::install::is_wsl()
}

/// Scan the config for misrouted Windows-host URLs. Returns empty off-WSL, when
/// nothing is misrouted, or when the `windows-host` alias isn't set up (so we
/// never suggest a name that won't resolve). Does short blocking TCP probes —
/// call off the hot path.
pub fn scan_misrouted(cfg: &MiraConfig) -> Vec<MisroutedUrl> {
    if !is_wsl() || !alias_resolves() {
        return Vec::new();
    }
    let Ok(v) = serde_json::to_value(cfg) else { return Vec::new() };
    let mut urls = Vec::new();
    collect_urls(&v, String::new(), &mut urls);

    let mut out = Vec::new();
    for (path, url) in urls {
        let Some((host, port)) = parse_host_port(&url) else { continue };
        // Only literal private IPv4 hosts are candidates (external services use
        // public hostnames; loopback is always reachable).
        let Ok(ip) = host.parse::<std::net::Ipv4Addr>() else { continue };
        if !ip.is_private() || ip.is_loopback() {
            continue;
        }
        // Proof-by-probe: misrouted only if the IP is dead AND windows-host works.
        if !probe(&host, port) && probe(ALIAS, port) {
            out.push(MisroutedUrl {
                path,
                suggested: url.replacen(&host, ALIAS, 1),
                current: url,
            });
        }
    }
    out
}

/// Apply the suggested swaps to a config, returning the rewritten config for the
/// caller to persist via the safe `LiveConfig::update`. Exact-string match on
/// the URL value, so only the flagged URLs change.
pub fn apply_fixes(cfg: &MiraConfig, fixes: &[MisroutedUrl]) -> Result<MiraConfig, String> {
    let mut v = serde_json::to_value(cfg).map_err(|e| e.to_string())?;
    for f in fixes {
        set_at_path(&mut v, &f.path, Value::String(f.suggested.clone()));
    }
    let mut new_cfg: MiraConfig =
        serde_json::from_value(v).map_err(|e| format!("rewrite produced invalid config: {e}"))?;
    // `config_path` is #[serde(skip)] — restore it from the source so the
    // subsequent save() has a real path to write to (mirrors put_config).
    new_cfg.config_path = cfg.config_path.clone();
    Ok(new_cfg)
}

// ── internals ──────────────────────────────────────────────────────────────

/// Does `windows-host` resolve (i.e. the boot-hook alias is set up)? If not,
/// suggesting it would be useless, so we stay quiet.
fn alias_resolves() -> bool {
    (ALIAS, 0u16).to_socket_addrs().map(|mut a| a.next().is_some()).unwrap_or(false)
}

/// Recursively collect (dotted-path, value) for every http(s) URL string leaf.
fn collect_urls(v: &Value, path: String, out: &mut Vec<(String, String)>) {
    match v {
        Value::String(s) if s.starts_with("http://") || s.starts_with("https://") => {
            out.push((path, s.clone()));
        }
        Value::Object(map) => {
            for (k, val) in map {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                collect_urls(val, p, out);
            }
        }
        Value::Array(arr) => {
            for (i, val) in arr.iter().enumerate() {
                collect_urls(val, format!("{path}.{i}"), out);
            }
        }
        _ => {}
    }
}

/// Pull `host` + `port` out of an `http(s)://host:port/...` URL. Requires an
/// explicit port (host-service URLs always have one).
fn parse_host_port(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?; // drop any userinfo
    let (host, port) = authority.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

/// True if a TCP connect to `host:port` succeeds within the probe timeout.
fn probe(host: &str, port: u16) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else { return false };
    for addr in addrs {
        if TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok() {
            return true;
        }
    }
    false
}

/// Set a dotted-path leaf in a JSON object to `val` (no-op if the path is absent).
fn set_at_path(v: &mut Value, path: &str, val: Value) {
    let mut cur = v;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if let Value::Object(m) = cur {
                m.insert(part.to_string(), val);
            }
            return;
        }
        match cur {
            Value::Object(m) => match m.get_mut(part) {
                Some(next) => cur = next,
                None => return,
            },
            _ => return,
        }
    }
}
