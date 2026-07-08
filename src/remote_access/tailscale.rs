// SPDX-License-Identifier: AGPL-3.0-or-later

// src/remote_access/tailscale.rs
//! Best-effort Tailscale tunnel detection by shelling out to the local
//! `tailscale` CLI. **Never crashes:** a missing binary, a non-zero exit, a
//! timeout, or unparseable output all resolve to "not detected" (default /
//! `None` fields). All calls use a fixed argv (no shell, no interpolation of
//! untrusted input) and a wall-clock timeout so a hung CLI can't block request
//! handling. Results are cached briefly so the pairing hot path stays fast.

use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Mutex;

/// How long a detection result is reused before a re-probe.
const CACHE_TTL: Duration = Duration::from_secs(30);
/// Per-command wall-clock cap so a hung CLI never blocks a request.
const CMD_TIMEOUT: Duration = Duration::from_secs(4);

/// A point-in-time view of the local Tailscale state, as far as we can tell.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TailscaleStatus {
    /// The `tailscale` CLI was found and runnable.
    pub installed:     bool,
    /// Tailscale is up (`BackendState == "Running"`).
    pub up:            bool,
    /// MagicDNS name without the trailing dot (e.g. `mira.my-tailnet.ts.net`).
    pub dns_name:      Option<String>,
    /// The node's Tailscale IPs (`100.x` / `fd7a::`).
    pub tailscale_ips: Vec<String>,
    /// `tailscale serve` is fronting our local port over HTTPS.
    pub serving_https: bool,
    /// The best remote URL we could derive, if any.
    pub derived_url:   Option<String>,
    /// Human hint about the next step (e.g. enable `tailscale serve`).
    pub hint:          Option<String>,
}

// ── JSON shape read from `tailscale status --json` (subset). ──
#[derive(Deserialize)]
struct TsStatus {
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
    #[serde(rename = "Self")]
    self_:         Option<TsSelf>,
}
#[derive(Deserialize)]
struct TsSelf {
    #[serde(rename = "DNSName")]
    dns_name:      Option<String>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
}

/// Detect, using a short-lived process-global cache. `mira_port` is the local
/// port MIRA serves on — used to form the http fallback URL and to check
/// `tailscale serve`.
pub async fn detect_cached(mira_port: u16) -> TailscaleStatus {
    static CACHE: OnceLock<Mutex<Option<(Instant, TailscaleStatus)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some((at, status)) = cache.lock().await.as_ref() {
        if at.elapsed() < CACHE_TTL {
            return status.clone();
        }
    }
    let fresh = detect(mira_port).await;
    *cache.lock().await = Some((Instant::now(), fresh.clone()));
    fresh
}

/// Force a fresh detection (bypasses the cache). Used by the admin "re-detect".
pub async fn detect(mira_port: u16) -> TailscaleStatus {
    let mut st = TailscaleStatus::default();

    let Some(status_json) = run(&["status", "--json"]).await else {
        return st; // binary missing / errored / timed out → not installed.
    };
    st.installed = true;

    let Ok(parsed) = serde_json::from_slice::<TsStatus>(&status_json) else {
        return st; // installed but unparseable output.
    };
    st.up = parsed.backend_state.as_deref() == Some("Running");
    if let Some(s) = parsed.self_ {
        if let Some(dns) = s.dns_name.as_deref().map(|d| d.trim_end_matches('.').trim()) {
            if !dns.is_empty() {
                st.dns_name = Some(dns.to_string());
            }
        }
        st.tailscale_ips = s.tailscale_ips.unwrap_or_default();
    }

    st.serving_https = serve_targets_local_port(mira_port).await;

    // Derive the best URL we can, preferring HTTPS via MagicDNS.
    if let (Some(dns), true) = (st.dns_name.as_deref(), st.serving_https) {
        st.derived_url = Some(format!("https://{dns}"));
    } else if let Some(ip) = st.tailscale_ips.first() {
        // Up but no HTTPS front → raw IP:port, and point the operator at
        // `tailscale serve` for a real HTTPS URL.
        st.derived_url = Some(format!("http://{}:{mira_port}", bracket_if_v6(ip)));
        if let Some(dns) = st.dns_name.as_deref() {
            st.hint = Some(format!(
                "Tailscale is up but not serving HTTPS. Run \
                 `tailscale serve --bg http://localhost:{mira_port}` (and enable MagicDNS + HTTPS \
                 certs in the Tailscale admin console) for a valid https://{dns} URL."
            ));
        }
    }
    st
}

/// Heuristic: does `tailscale serve` front our local port over HTTPS? We read
/// `serve status --json` and look for a proxy to our loopback port alongside an
/// HTTPS/:443 front. Best-effort — a false negative only downgrades us to the
/// http-IP fallback + a hint, never a crash.
async fn serve_targets_local_port(port: u16) -> bool {
    let Some(out) = run(&["serve", "status", "--json"]).await else {
        return false;
    };
    let text = String::from_utf8_lossy(&out);
    let proxies_port = text.contains(&format!("127.0.0.1:{port}"))
        || text.contains(&format!("localhost:{port}"));
    let https = text.contains(":443") || text.contains("\"HTTPS\"");
    proxies_port && https
}

/// Wrap an IPv6 literal in `[]` for use in a URL authority; pass IPv4 through.
fn bracket_if_v6(ip: &str) -> String {
    if ip.contains(':') { format!("[{ip}]") } else { ip.to_string() }
}

/// Run `tailscale <args>` with a fixed argv + timeout. Returns captured stdout
/// on a clean exit, or `None` for missing-binary / non-zero exit / timeout.
async fn run(args: &[&str]) -> Option<Vec<u8>> {
    let fut = Command::new("tailscale")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(CMD_TIMEOUT, fut).await {
        Ok(Ok(out)) if out.status.success() => Some(out.stdout),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_bracketing() {
        assert_eq!(bracket_if_v6("100.101.102.103"), "100.101.102.103");
        assert_eq!(bracket_if_v6("fd7a:115c:a1e0::1"), "[fd7a:115c:a1e0::1]");
    }

    #[tokio::test]
    async fn missing_binary_is_graceful() {
        // On a box without tailscale (or where it errors), detection must not
        // panic and must report "not installed" with no derived URL.
        let st = detect(8080).await;
        if !st.installed {
            assert!(st.derived_url.is_none());
            assert!(!st.up);
        }
    }

    #[test]
    fn parses_status_json_shape() {
        let json = br#"{"BackendState":"Running","Self":{"DNSName":"mira.my-tailnet.ts.net.","TailscaleIPs":["100.64.0.1","fd7a:115c:a1e0::1"]}}"#;
        let p: TsStatus = serde_json::from_slice(json).unwrap();
        assert_eq!(p.backend_state.as_deref(), Some("Running"));
        let s = p.self_.unwrap();
        assert_eq!(s.dns_name.as_deref(), Some("mira.my-tailnet.ts.net."));
        assert_eq!(s.tailscale_ips.unwrap().len(), 2);
    }
}
