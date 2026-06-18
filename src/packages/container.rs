// SPDX-License-Identifier: AGPL-3.0-or-later

//! Container-tier runtime for packaged components.
//!
//! The native launcher ([`super::launcher`]) confines an *arbitrary host
//! subprocess*; the container tier is the higher-isolation ceiling for
//! less-trusted code — the component runs inside an OCI container the host's
//! engine (docker/podman) supervises, with a read-only rootfs, all capabilities
//! dropped, and only the declared volumes/ports/secrets wired in.
//!
//! Integration is deliberately identical to native: a `runtime: container`
//! `mcp_server` becomes a `docker run --rm -i … <image>` command that MIRA's MCP
//! host spawns and speaks **MCP-over-stdio** to — the image's entrypoint is the
//! server. The image comes from `spec.image` (a registry ref; the 10 MB bundle
//! cap rules out shipping image blobs), pulled at install so a bad/unreachable
//! image fails fast with a clear message rather than at first tool call.
//!
//! ## Egress allowlist (Tier A)
//!
//! When a component declares specific hosts in `network_egress`, MIRA enforces a
//! **per-host egress allowlist** instead of all-or-nothing networking. The plugin
//! runs in the network namespace of a MIRA-controlled **sidecar** ([`EGRESS_*`])
//! that holds `CAP_NET_ADMIN` (granted by the Docker daemon — no *host* privilege,
//! so it works even on WSL2 Docker Desktop) and runs an `nftables` + `dnsmasq`
//! filter: dnsmasq resolves *only* the allowlisted hostnames and adds each
//! resolved IP into an nft set; nftables drops everything not in that set. The
//! result is hostname-accurate, all-protocol, and CDN/round-robin safe. The
//! plugin itself keeps `--cap-drop ALL`. Orchestrated by [`run_egress_confined`]
//! (the `mira ctr-run` wrapper), which brackets the sidecar's lifecycle around the
//! plugin's. Declared-empty egress → `--network none`; no allowlist needed.

use std::process::Command;

// How a containerised component reaches the network.
#[derive(Debug, Clone, Default)]
pub enum NetworkMode {
    // `--network none` — fully offline (declared no egress).
    #[default]
    None,
    // The engine's default bridge — unrestricted egress.
    Bridge,
    // Join another container's network namespace (`--network container:<id>`) —
    // used to put the plugin behind the Tier-A egress-allowlist sidecar.
    Join(String),
    // Attach to a named (internal) network — used for the Tier-B proxy fallback,
    // where the plugin has no direct egress and reaches allowed hosts via a proxy.
    Named(String),
}

// Hardening + capability flags for a containerised component. Pure data; turned
// into engine args by [`build_run_args`].
#[derive(Debug, Clone, Default)]
pub struct ContainerSpec {
    // Registry image reference (e.g. `ghcr.io/acme/foo:1.2`).
    pub image: String,
    // How the container reaches the network.
    pub network: NetworkMode,
    // `(host_path, container_path)` writable bind mounts.
    pub volumes: Vec<(String, String)>,
    // Env var names to forward from the spawned engine process into the
    // container (`-e NAME`, value-less so secrets stay out of argv / `ps`).
    pub env_keys: Vec<String>,
    // Explicit `KEY=VALUE` env (`-e KEY=VALUE`) — for non-secret values like the
    // Tier-B proxy URL. Never used for secrets (those go through `env_keys`).
    pub env_set: Vec<(String, String)>,
    // Optional inbound port to publish on loopback only. Ignored when joining
    // another netns (a published port must live on the netns owner).
    pub listen_port: Option<u16>,
    // Memory ceiling (e.g. `512m`).
    pub memory: String,
    // Max PIDs (fork-bomb guard).
    pub pids_limit: u32,
}

impl ContainerSpec {
    pub fn new(image: impl Into<String>, network: NetworkMode) -> Self {
        Self {
            image: image.into(),
            network,
            volumes: Vec::new(),
            env_keys: Vec::new(),
            env_set: Vec::new(),
            listen_port: None,
            memory: "512m".to_string(),
            pids_limit: 256,
        }
    }
}

// Build the `run …` argument vector for a container engine. Pure —
// unit-testable without an engine. The caller spawns `<engine> <these args>`;
// stdin/stdout are inherited so MCP-over-stdio works.
pub fn build_run_args(spec: &ContainerSpec) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "-i".into(),
        // Hardening — read-only rootfs with a writable scratch tmpfs, no added
        // caps, no privilege escalation, bounded pids + memory.
        "--read-only".into(),
        "--tmpfs".into(),
        "/tmp:rw,nosuid,nodev".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--pids-limit".into(),
        spec.pids_limit.to_string(),
        "--memory".into(),
        spec.memory.clone(),
    ];
    match &spec.network {
        NetworkMode::None => {
            a.push("--network".into());
            a.push("none".into());
        }
        NetworkMode::Bridge => {}
        NetworkMode::Join(id) => {
            a.push("--network".into());
            a.push(format!("container:{id}"));
        }
        NetworkMode::Named(net) => {
            a.push("--network".into());
            a.push(net.clone());
        }
    }
    for (host, cont) in &spec.volumes {
        a.push("-v".into());
        a.push(format!("{host}:{cont}"));
    }
    // Explicit non-secret env (e.g. the proxy URL) — value in argv is fine here.
    for (k, v) in &spec.env_set {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    // Value-less `-e NAME` forwards the value from the engine process's own
    // environment (which MIRA sets on the spawned server) — keeps secrets out
    // of the command line.
    for k in &spec.env_keys {
        a.push("-e".into());
        a.push(k.clone());
    }
    // A published port can't coexist with joining another netns / an internal
    // network — skip it in those modes.
    let netns_bound = matches!(spec.network, NetworkMode::Join(_) | NetworkMode::Named(_));
    if let (Some(p), false) = (spec.listen_port, netns_bound) {
        a.push("-p".into());
        a.push(format!("127.0.0.1:{p}:{p}"));
    }
    a.push(spec.image.clone());
    a
}

// First available container engine binary (docker preferred, then podman), or
// `None` if neither is on `PATH`. Only checks the CLI exists — daemon
// reachability is surfaced by [`pull`].
pub fn detect_engine() -> Option<String> {
    for engine in ["docker", "podman"] {
        if Command::new(engine)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(engine.to_string());
        }
    }
    None
}

// Pull an image so install fails fast (and guides) when the engine's daemon is
// down or the image is unreachable, rather than at first tool call.
pub fn pull(engine: &str, image: &str) -> Result<(), String> {
    let out = Command::new(engine)
        .args(["pull", image])
        .output()
        .map_err(|e| format!("could not run `{engine} pull`: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(stderr.trim().lines().last().unwrap_or("pull failed").to_string())
}

// ── Egress-allowlist sidecar (Tier A) ───────────────────────────────────────

// Tag of the MIRA egress sidecar image. Versioned so a MIRA upgrade that changes
// the recipe rebuilds it.
pub fn egress_image_tag() -> String {
    format!("mira-egress:{}", env!("CARGO_PKG_VERSION"))
}

// The sidecar's entrypoint. Built around two Docker-Desktop-kernel realities
// found while prototyping: `nft -f <file>` and a `ct state` expression both hang
// there, so rules are added one `nft add` at a time and the allow is purely by
// destination IP (the `@allowed` set, populated by dnsmasq's `--nftset`, plus
// loopback + the upstream resolver). `dnsmasq` answers ONLY the allowlisted
// domains and drops AAAA (IPv4-only this slice).
const EGRESS_ENTRYPOINT: &str = r#"#!/bin/sh
set -e
UPSTREAM="${UPSTREAM:-1.1.1.1}"
nft add table inet egress
nft add set inet egress allowed '{ type ipv4_addr; flags timeout; }'
nft add chain inet egress out '{ type filter hook output priority 0; policy drop; }'
nft add rule inet egress out ip daddr 127.0.0.0/8 accept
nft add rule inet egress out ip daddr "$UPSTREAM" udp dport 53 accept
nft add rule inet egress out ip daddr "$UPSTREAM" tcp dport 53 accept
nft add rule inet egress out ip daddr @allowed accept
set -- -k --listen-address=127.0.0.1 --no-resolv --filter-AAAA
for d in $ALLOW; do
  set -- "$@" "--server=/$d/$UPSTREAM" "--nftset=/$d/inet#egress#allowed"
done
echo "egress-sidecar ready: allow=[$ALLOW] upstream=$UPSTREAM"
exec dnsmasq "$@"
"#;

const EGRESS_DOCKERFILE: &str = "FROM debian:stable-slim\n\
RUN apt-get update && apt-get install -y --no-install-recommends nftables dnsmasq \
&& rm -rf /var/lib/apt/lists/*\n\
COPY entrypoint.sh /entrypoint.sh\n\
RUN chmod +x /entrypoint.sh\n\
ENTRYPOINT [\"/entrypoint.sh\"]\n";

// Ensure the egress sidecar image exists, building it (once) if not. Returns the
// image tag. Idempotent — a present image is a fast no-op.
pub fn ensure_egress_image(engine: &str) -> Result<String, String> {
    let tag = egress_image_tag();
    let present = Command::new(engine)
        .args(["image", "inspect", &tag])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if present {
        return Ok(tag);
    }
    // Write a tiny build context to a temp dir and build.
    let dir = std::env::temp_dir().join(format!("mira-egress-build-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("egress build dir: {e}"))?;
    std::fs::write(dir.join("Dockerfile"), EGRESS_DOCKERFILE).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("entrypoint.sh"), EGRESS_ENTRYPOINT).map_err(|e| e.to_string())?;
    let out = Command::new(engine)
        .args(["build", "-t", &tag])
        .arg(&dir)
        .output()
        .map_err(|e| format!("could not run `{engine} build`: {e}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "building the egress sidecar image failed: {}",
            stderr.trim().lines().last().unwrap_or("build failed")
        ));
    }
    Ok(tag)
}

// Whether the engine can run a `CAP_NET_ADMIN` container that programs nftables —
// the requirement for the Tier-A egress allowlist. A quick probe; the result
// decides Tier A vs the best-effort fallback at install.
pub fn supports_net_admin(engine: &str) -> bool {
    let tag = match ensure_egress_image(engine) {
        Ok(t) => t,
        Err(_) => return false,
    };
    // Run the sidecar's own nft setup with an empty allowlist; success means the
    // kernel + cap path work here.
    Command::new(engine)
        .args(["run", "--rm", "--cap-add", "NET_ADMIN", "-e", "ALLOW=", "--entrypoint", "sh", &tag, "-c",
            "nft add table inet probe && nft add chain inet probe c '{ type filter hook output priority 0; }' && echo ok"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("ok"))
        .unwrap_or(false)
}

// Inputs for the `mira ctr-run` egress wrapper.
#[derive(Debug, Clone)]
pub struct EgressRunOpts {
    pub engine: String,
    // Upstream DNS the sidecar forwards allowlisted lookups to.
    pub upstream: String,
    // Allowlisted hostnames (exact).
    pub allow: Vec<String>,
    // The plugin container spec (its `network` is overridden to join the sidecar).
    pub plugin: ContainerSpec,
}

// Start the egress sidecar, run the plugin inside its filtered network namespace
// with stdio inherited (so MCP-over-stdio works), and tear the sidecar down when
// the plugin exits. Returns the process exit code to propagate. Used by the
// hidden `mira ctr-run` subcommand.
pub fn run_egress_confined(opts: &EgressRunOpts) -> i32 {
    let tag = match ensure_egress_image(&opts.engine) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mira ctr-run: {e}");
            return 127;
        }
    };
    let sidecar_name = format!("mira-egress-{}", std::process::id());
    let allow = opts.allow.join(" ");
    let start = Command::new(&opts.engine)
        .args([
            "run", "-d", "--rm", "--cap-add", "NET_ADMIN",
            "--label", "mira-egress=1",
            "--name", &sidecar_name,
            "-e", &format!("ALLOW={allow}"),
            "-e", &format!("UPSTREAM={}", opts.upstream),
            &tag,
        ])
        .output();
    let _sid = match start {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            eprintln!("mira ctr-run: egress sidecar failed to start: {}", String::from_utf8_lossy(&o.stderr).trim());
            return 127;
        }
        Err(e) => {
            eprintln!("mira ctr-run: egress sidecar start: {e}");
            return 127;
        }
    };
    let teardown = |id: &str| {
        let _ = Command::new(&opts.engine).args(["rm", "-f", id]).output();
    };
    if !egress_sidecar_ready(&opts.engine, &sidecar_name, 20) {
        eprintln!("mira ctr-run: egress sidecar did not become ready");
        teardown(&sidecar_name);
        return 127;
    }

    // The plugin needs the sidecar's dnsmasq as its resolver.
    let resolv = std::env::temp_dir().join(format!("mira-egress-{}-resolv.conf", std::process::id()));
    if let Err(e) = std::fs::write(&resolv, "nameserver 127.0.0.1\n") {
        eprintln!("mira ctr-run: resolv.conf: {e}");
        teardown(&sidecar_name);
        return 127;
    }

    let mut spec = opts.plugin.clone();
    spec.network = NetworkMode::Join(sidecar_name.clone());
    let mut args = build_run_args(&spec);
    // Insert the read-only resolv.conf mount just before the image (last arg).
    let img_pos = args.len() - 1;
    args.insert(img_pos, format!("{}:/etc/resolv.conf:ro", resolv.display()));
    args.insert(img_pos, "-v".into());

    let status = Command::new(&opts.engine)
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    teardown(&sidecar_name);
    let _ = std::fs::remove_file(&resolv);

    match status {
        Ok(s) => s.code().unwrap_or(0),
        Err(e) => {
            eprintln!("mira ctr-run: plugin container: {e}");
            127
        }
    }
}

// Poll the sidecar's logs for its readiness line, up to `secs` seconds.
fn egress_sidecar_ready(engine: &str, name: &str, secs: u64) -> bool {
    for _ in 0..(secs * 5) {
        let out = Command::new(engine).args(["logs", name]).output();
        if let Ok(o) = out {
            let logs = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            if logs.contains("egress-sidecar ready") {
                return true;
            }
        }
        // Bail early if the container already died.
        let alive = Command::new(engine)
            .args(["inspect", "-f", "{{.State.Running}}", name])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
            .unwrap_or(false);
        if !alive {
            // Give logs one last read before giving up.
            if let Ok(o) = Command::new(engine).args(["logs", name]).output() {
                if String::from_utf8_lossy(&o.stdout).contains("egress-sidecar ready") {
                    return true;
                }
            }
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

// ── Egress allowlist (Tier B — best-effort HTTP/S proxy) ─────────────────────

// The Tier-B proxy image tag.
pub fn proxy_image_tag() -> String {
    format!("mira-proxy:{}", env!("CARGO_PKG_VERSION"))
}

// tinyproxy entrypoint: build a hostname allowlist filter from `$ALLOW`
// (default-deny), then run. Matches a domain and its subdomains; applies to both
// plain HTTP and HTTPS CONNECT.
const PROXY_ENTRYPOINT: &str = r#"#!/bin/sh
set -e
CONF=/etc/tinyproxy/tinyproxy.conf
FILTER=/etc/tinyproxy/filter
: > "$FILTER"
for d in $ALLOW; do
  esc=$(echo "$d" | sed 's/\./\\./g')
  printf '(^|\.)%s$\n' "$esc" >> "$FILTER"
done
cat > "$CONF" <<CONFEOF
Port 8888
Listen 0.0.0.0
Timeout 600
Allow 0.0.0.0/0
FilterDefaultDeny Yes
Filter "$FILTER"
FilterExtended On
FilterURLs Off
CONFEOF
echo "mira-proxy ready: allow=[$ALLOW]"
exec tinyproxy -d -c "$CONF"
"#;

const PROXY_DOCKERFILE: &str = "FROM debian:stable-slim\n\
RUN apt-get update && apt-get install -y --no-install-recommends tinyproxy \
&& rm -rf /var/lib/apt/lists/*\n\
COPY entrypoint.sh /entrypoint.sh\n\
RUN chmod +x /entrypoint.sh\n\
ENTRYPOINT [\"/entrypoint.sh\"]\n";

// Ensure the Tier-B proxy image exists, building it once if not.
pub fn ensure_proxy_image(engine: &str) -> Result<String, String> {
    let tag = proxy_image_tag();
    let present = Command::new(engine)
        .args(["image", "inspect", &tag])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if present {
        return Ok(tag);
    }
    let dir = std::env::temp_dir().join(format!("mira-proxy-build-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("proxy build dir: {e}"))?;
    std::fs::write(dir.join("Dockerfile"), PROXY_DOCKERFILE).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("entrypoint.sh"), PROXY_ENTRYPOINT).map_err(|e| e.to_string())?;
    let out = Command::new(engine)
        .args(["build", "-t", &tag])
        .arg(&dir)
        .output()
        .map_err(|e| format!("could not run `{engine} build`: {e}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "building the egress proxy image failed: {}",
            stderr.trim().lines().last().unwrap_or("build failed")
        ));
    }
    Ok(tag)
}

// Tier-B best-effort egress allowlist: run the plugin on an **internal** docker
// network (no direct egress, daemon-enforced — no cap needed) and force its
// HTTP/S traffic through a dual-homed `tinyproxy` that allows only the declared
// hostnames. HTTP(S)-only by nature; raw-socket egress is denied entirely (safe).
// Used by `mira ctr-run --mode proxy`.
pub fn run_proxy_confined(opts: &EgressRunOpts) -> i32 {
    let tag = match ensure_proxy_image(&opts.engine) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mira ctr-run: {e}");
            return 127;
        }
    };
    let token = std::process::id();
    let net = format!("mira-int-{token}");
    let proxy_name = format!("mira-proxy-{token}");
    let teardown = |proxy: &str, net: &str| {
        let _ = Command::new(&opts.engine).args(["rm", "-f", proxy]).output();
        let _ = Command::new(&opts.engine).args(["network", "rm", net]).output();
    };

    // Internal network (no external connectivity); labelled for the reaper.
    let mk = Command::new(&opts.engine)
        .args(["network", "create", "--internal", "--label", "mira-egress=1", &net])
        .output();
    if !mk.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("mira ctr-run: could not create internal network");
        return 127;
    }
    // Proxy on the internal network, then a second leg on the default bridge for
    // its own egress.
    let allow = opts.allow.join(" ");
    let start = Command::new(&opts.engine)
        .args([
            "run", "-d", "--rm",
            "--label", "mira-egress=1",
            "--name", &proxy_name,
            "--network", &net,
            "-e", &format!("ALLOW={allow}"),
            &tag,
        ])
        .output();
    if !start.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("mira ctr-run: proxy sidecar failed to start");
        teardown(&proxy_name, &net);
        return 127;
    }
    let _ = Command::new(&opts.engine)
        .args(["network", "connect", "bridge", &proxy_name])
        .output();
    if !proxy_ready(&opts.engine, &proxy_name, 20) {
        eprintln!("mira ctr-run: proxy sidecar did not become ready");
        teardown(&proxy_name, &net);
        return 127;
    }

    let proxy_url = format!("http://{proxy_name}:8888");
    let mut spec = opts.plugin.clone();
    spec.network = NetworkMode::Named(net.clone());
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        spec.env_set.push((k.to_string(), proxy_url.clone()));
    }
    spec.env_set.push(("NO_PROXY".into(), "localhost,127.0.0.1".into()));
    spec.env_set.push(("no_proxy".into(), "localhost,127.0.0.1".into()));
    let args = build_run_args(&spec);

    let status = Command::new(&opts.engine)
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();
    teardown(&proxy_name, &net);
    match status {
        Ok(s) => s.code().unwrap_or(0),
        Err(e) => {
            eprintln!("mira ctr-run: plugin container: {e}");
            127
        }
    }
}

// Poll the proxy sidecar's logs for its readiness line.
fn proxy_ready(engine: &str, name: &str, secs: u64) -> bool {
    for _ in 0..(secs * 5) {
        if let Ok(o) = Command::new(engine).args(["logs", name]).output() {
            if String::from_utf8_lossy(&o.stdout).contains("mira-proxy ready")
                || String::from_utf8_lossy(&o.stderr).contains("mira-proxy ready")
            {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

// Remove orphan egress sidecars / proxies / internal networks left by a
// hard-killed `ctr-run`. Best-effort. **PID-aware**: every resource is named
// `mira-{proxy,egress,int}-<ctr-run-pid>`; a resource is removed only if its
// owning `ctr-run` process is no longer alive. This makes the sweep safe to run
// concurrently with live `ctr-run`s (it never nukes a running session's
// sidecar/network — the bug that an unconditional label sweep would cause).
pub fn reap_orphan_sidecars(engine: &str) {
    // A resource whose trailing -<pid> belongs to a live `ctr-run` is in use.
    let owner_alive = |name: &str| -> bool {
        let Some(pid) = name.rsplit('-').next().and_then(|s| s.parse::<u32>().ok()) else {
            return false;
        };
        std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .map(|c| c.contains("ctr-run"))
            .unwrap_or(false)
    };
    if let Ok(o) = Command::new(engine)
        .args(["ps", "-a", "--filter", "label=mira-egress=1", "--format", "{{.Names}}"])
        .output()
    {
        for name in String::from_utf8_lossy(&o.stdout).split_whitespace() {
            if !owner_alive(name) {
                let _ = Command::new(engine).args(["rm", "-f", name]).output();
            }
        }
    }
    if let Ok(o) = Command::new(engine)
        .args(["network", "ls", "--filter", "label=mira-egress=1", "--format", "{{.Name}}"])
        .output()
    {
        for name in String::from_utf8_lossy(&o.stdout).split_whitespace() {
            if !owner_alive(name) {
                let _ = Command::new(engine).args(["network", "rm", name]).output();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_spec_drops_network_and_hardens() {
        let args = build_run_args(&ContainerSpec::new("img:1", NetworkMode::None));
        for f in ["--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges"] {
            assert!(args.iter().any(|a| a == f), "missing {f}");
        }
        let i = args.iter().position(|a| a == "--network").expect("has --network");
        assert_eq!(args[i + 1], "none");
        assert_eq!(args.last().unwrap(), "img:1");
    }

    #[test]
    fn bridge_spec_has_no_network_flag() {
        let args = build_run_args(&ContainerSpec::new("img:2", NetworkMode::Bridge));
        assert!(!args.iter().any(|a| a == "--network"));
    }

    #[test]
    fn join_spec_joins_the_sidecar_netns_and_skips_port() {
        let mut spec = ContainerSpec::new("img:3", NetworkMode::Join("sidecar123".into()));
        spec.listen_port = Some(8099); // must be dropped in join mode
        let args = build_run_args(&spec);
        let i = args.iter().position(|a| a == "--network").unwrap();
        assert_eq!(args[i + 1], "container:sidecar123");
        assert!(!args.iter().any(|a| a == "-p"), "port must not be published when joining a netns");
    }

    #[test]
    fn volumes_env_and_ports_map_to_flags() {
        let mut spec = ContainerSpec::new("acme/srv:9", NetworkMode::Bridge);
        spec.volumes = vec![("/host/data".into(), "/data".into())];
        spec.env_keys = vec!["NC_APP_PASS".into(), "NC_USER".into()];
        spec.listen_port = Some(8099);
        let args = build_run_args(&spec);
        let vi = args.iter().position(|a| a == "-v").unwrap();
        assert_eq!(args[vi + 1], "/host/data:/data");
        let mut es: Vec<&str> = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if a == "-e" {
                es.push(&args[i + 1]);
            }
        }
        assert_eq!(es, vec!["NC_APP_PASS", "NC_USER"]);
        assert!(args.iter().all(|a| !a.contains("NC_APP_PASS=")), "secret value must not be in argv");
        let pi = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[pi + 1], "127.0.0.1:8099:8099");
    }

    #[test]
    fn egress_tag_is_versioned() {
        assert!(egress_image_tag().starts_with("mira-egress:"));
    }
}
