// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native-tier egress allowlist — the privileged daemon's first real consumer.
//!
//! Mirrors the container Tier A model (dnsmasq → nft set → drop-by-default), but
//! applied to a **native confined subprocess's** network namespace. Only this
//! root daemon holds host `CAP_NET_ADMIN`, so it is the one place that can wire a
//! veth into the (user-namespace-owned) child netns and install host-side NAT +
//! filtering. The unprivileged caller asks for it by pid + allowlist; the daemon
//! validates and provisions.
//!
//! Per confined pid (one "slot"):
//! - a veth pair: host end `10.69.<slot>.1`, child end `10.69.<slot>.2` (a /30),
//! - SNAT/masquerade for that /30 out the host's default interface,
//! - a **stateless** nftables forward filter (drop by default; allow egress to
//!   IPs in an `@allowed` set + return traffic to the /30),
//! - a per-slot `dnsmasq` on the host veth IP that resolves *only* allowlisted
//!   names and populates `@allowed` with each resolved address.
//!
//! ## Lifecycle / teardown (restart-safe)
//!
//! The confined process `exec`s its real server, so when it exits the netns dies
//! and the veths auto-remove — but the per-slot dnsmasq + nft table would leak.
//! State is therefore tracked in **marker files** under [`STATE_DIR`] (one per
//! slot, holding the confined pid + dnsmasq pid) rather than only in memory, so a
//! **reaper** can clean up even after the daemon itself restarts (which drops all
//! in-memory state). The reaper runs on daemon start, before each `NetAllow`, and
//! on a periodic timer; it tears down any slot whose confined pid is gone. Slot
//! allocation is derived from the kernel (existing `mira_egr_*` nft tables), so a
//! live plugin's slot is never reused across a restart.
//!
//! Design facts proven by the  spike on the target (WSL2) kernel, encoded
//! deliberately below:
//! * conntrack matches (`ct state …`) **hang** here — the filter is stateless.
//! * nft chain names must avoid reserved words (`fwd` is reserved → `mira_fwd`).
//! * the child resolves via dnsmasq at the host veth IP, which is INPUT to the
//!   host (not forwarded), so it needs no forward rule.
//! * `IFNAMSIZ` is 15 — veth names embed a small slot id, not the pid.
//!
//! Every external command is wrapped in `timeout` so a hung tool can't wedge the
//! single-threaded daemon loop.

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

// Max concurrent confined subprocesses with an egress allowlist. Bounds the
// slot/subnet space (third octet `10.69.<slot>.x`, and `IFNAMSIZ`).
const MAX_SLOTS: usize = 64;
// Hard wall on every external command so a hang fails fast instead of wedging
// the daemon (the spike showed some nft ops can hang on this kernel).
const CMD_TIMEOUT_SECS: &str = "8";
// Where per-slot marker files live: a subdir of the runtime dir, which the unit
// keeps across restarts (`RuntimeDirectoryPreserve=yes`) so markers — and thus
// the ability to reap orphans — survive a daemon restart. Owned by the helper's
// own (non-root) user. Cleared on reboot, by which point no confined process is
// alive anyway.
const STATE_DIR: &str = "/run/mira-helper/slots";
// How often the background reaper sweeps for dead slots.
pub const REAP_INTERVAL: Duration = Duration::from_secs(60);

// Serializes all egress kernel/marker mutations (request handlers + the periodic
// reaper thread). The actual slot state lives in the kernel (nft tables, veths)
// and in marker files — this lock just makes the read-modify-write sequences
// atomic against each other.
#[derive(Default)]
pub struct NetManager {
    lock: Mutex<()>,
}

impl NetManager {
    pub fn new() -> Self {
        if let Err(e) = std::fs::create_dir_all(STATE_DIR) {
            log(&format!("WARNING: could not create state dir {STATE_DIR}: {e} (reaping degraded)"));
        }
        Self { lock: Mutex::new(()) }
    }

    // Provision filtered egress for `pid`. Idempotent: an existing slot for the
    // same pid is torn down first. `peer_uid` is the kernel-attested caller; a
    // non-root caller may only target a pid it owns.
    pub fn allow(
        &self,
        pid: u32,
        allow_raw: &[String],
        upstream: Option<&str>,
        peer_uid: u32,
    ) -> Result<serde_json::Value, String> {
        if !pid_has_netns(pid) {
            return Err(format!("pid {pid} has no network namespace (not alive?)"));
        }
        // A non-root caller may only wrap a process it owns — don't let the MIRA
        // user point the helper at arbitrary pids.
        if peer_uid != 0 {
            match pid_owner_uid(pid) {
                Some(owner) if owner == peer_uid => {}
                Some(owner) => {
                    return Err(format!(
                        "pid {pid} is owned by uid {owner}, not caller uid {peer_uid}"
                    ))
                }
                None => return Err(format!("cannot determine owner of pid {pid}")),
            }
        }

        let mut domains: Vec<String> = Vec::new();
        for d in allow_raw {
            match sanitize_domain(d) {
                Some(s) => {
                    if !domains.contains(&s) {
                        domains.push(s);
                    }
                }
                None => return Err(format!("invalid allowlist host: {d:?}")),
            }
        }
        if domains.is_empty() {
            return Err("allowlist is empty".into());
        }
        if domains.len() > MAX_SLOTS {
            return Err(format!("allowlist too large (max {MAX_SLOTS} hosts)"));
        }
        let upstream = parse_upstream(upstream)?;
        let wan = wan_iface()?;

        let _guard = self.lock.lock().unwrap();
        // Opportunistic cleanup so a freed slot can be reused immediately.
        reap_dead();
        // Idempotency: replace any prior allocation for this pid.
        if let Some(slot) = slot_for_pid(pid) {
            reap_slot(slot);
        }
        let slot = first_free_slot().ok_or_else(|| {
            format!("no free egress slot (max {MAX_SLOTS} concurrent)")
        })?;

        match provision(slot, pid, &domains, &upstream, &wan) {
            Ok(dns_pid) => {
                write_marker(slot, pid, dns_pid);
                Ok(serde_json::json!({
                    "slot": slot,
                    "pid": pid,
                    "subnet": format!("10.69.{slot}.0/30"),
                    "host_ip": host_ip(slot),
                    "child_ip": child_ip(slot),
                    "dns": host_ip(slot),
                    "veth_host": veth_host(slot),
                    "veth_child": veth_child(slot),
                    "upstream": upstream,
                    "allow": domains,
                }))
            }
            Err(e) => {
                cleanup_kernel(slot);
                remove_marker(slot);
                Err(e)
            }
        }
    }

    // Tear down `pid`'s egress slot, if any. A no-op (still `ok`) when there is
    // nothing to tear down, so callers can fire it unconditionally on exit.
    pub fn teardown(&self, pid: u32) -> Result<serde_json::Value, String> {
        let _guard = self.lock.lock().unwrap();
        match slot_for_pid(pid) {
            Some(slot) => {
                reap_slot(slot);
                Ok(serde_json::json!({ "pid": pid, "slot": slot, "torn_down": true }))
            }
            None => Ok(serde_json::json!({
                "pid": pid, "torn_down": false, "note": "no active egress slot for pid"
            })),
        }
    }

    // Sweep: tear down every slot whose confined pid is gone, plus any orphaned
    // kernel state with no marker. Safe to call repeatedly; run on daemon start
    // and on a periodic timer.
    pub fn reap_dead(&self) {
        let _guard = self.lock.lock().unwrap();
        reap_dead();
    }
}

// --- reaping ---------------------------------------------------------------

// Caller must hold the [`NetManager`] lock.
fn reap_dead() {
    // 1. Markers whose confined process is gone → full teardown.
    for (slot, confined_pid, _dns_pid) in read_all_markers() {
        if !pid_alive(confined_pid) {
            log(&format!("reaping slot {slot}: confined pid {confined_pid} is gone"));
            reap_slot(slot);
        }
    }
    // 2. Orphan nft tables with no marker (e.g. a crash between table-create and
    //  marker-write) — no one tracks them and the allocator would skip them
    //  forever. A live plugin across a restart keeps its (preserved) marker, so
    //  this only catches genuine orphans.
    for slot in existing_egress_slots() {
        if read_marker(slot).is_none() {
            log(&format!("reaping orphan slot {slot}: nft table with no marker"));
            reap_slot(slot);
        }
    }
    // 3. Orphan dnsmasq processes whose slot has no live owner — catches the
    //  case where the nft table was already removed but the resolver survived
    //  (e.g. a partial teardown). Reconciles dnsmasq directly, not via tables.
    for (slot, dns_pid) in running_egress_dnsmasq() {
        let owned = matches!(read_marker(slot), Some((cpid, _)) if pid_alive(cpid));
        if !owned {
            log(&format!("reaping slot {slot}: dnsmasq pid {dns_pid} with no live owner"));
            kill_and_reap(dns_pid);
            cleanup_kernel(slot);
            remove_marker(slot);
        }
    }
}

// Tear down one slot: kill its dnsmasq, drop kernel state, remove the marker.
// Caller must hold the [`NetManager`] lock. Never blocks indefinitely.
fn reap_slot(slot: usize) {
    let dns_pid = read_marker(slot).map(|(_c, d)| d).or_else(|| find_dnsmasq_for_slot(slot));
    if let Some(dp) = dns_pid {
        kill_and_reap(dp);
    }
    cleanup_kernel(slot);
    remove_marker(slot);
}

// SIGKILL `pid` (cross-uid: the daemon holds `CAP_KILL`, dnsmasq runs as
// `nobody`) and reap it if it's our child — bounded so a stuck wait can't wedge
// the single-threaded daemon. A reparented dnsmasq (post daemon-restart) is not
// our child; `waitpid` returns `ECHILD` and init reaps it.
fn kill_and_reap(pid: u32) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    let mut status: libc::c_int = 0;
    for _ in 0..100 {
        let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if r != 0 {
            break; // reaped (pid) or not-our-child / error (-1)
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// --- provisioning (root, CAP_NET_ADMIN) -----------------------------------

// Set up the full path for one slot. On success returns the dnsmasq pid (the
// process keeps running independently of this daemon; the reaper kills it later).
fn provision(
    slot: usize,
    pid: u32,
    domains: &[String],
    upstream: &str,
    wan: &str,
) -> Result<u32, String> {
    let h = veth_host(slot);
    let c = veth_child(slot);
    let host = host_ip(slot);
    let host_cidr = format!("{host}/30");
    let child_cidr = format!("{}/30", child_ip(slot));
    let cidr = format!("10.69.{slot}.0/30");
    let table = nft_table(slot);
    let pid_s = pid.to_string();

    // Clear any stale leftovers for this slot (idempotent re-provision).
    let _ = run("ip", &["link", "del", &h]);
    let _ = run("nft", &["delete", "table", "ip", &table]);

    // veth pair, then push the child end into the target netns. Host root holds
    // caps over the user-namespace-owned netns (proven by the spike).
    run("ip", &["link", "add", &h, "type", "veth", "peer", "name", &c])?;
    if let Err(e) = run("ip", &["link", "set", &c, "netns", &pid_s]) {
        let _ = run("ip", &["link", "del", &h]);
        return Err(format!("move veth into netns: {e}"));
    }
    run("ip", &["addr", "add", &host_cidr, "dev", &h])?;
    run("ip", &["link", "set", &h, "up"])?;

    // Configure the child end + default route from outside, via nsenter.
    nsenter(pid, &["ip", "addr", "add", &child_cidr, "dev", &c])?;
    nsenter(pid, &["ip", "link", "set", &c, "up"])?;
    nsenter(pid, &["ip", "link", "set", "lo", "up"])?;
    nsenter(pid, &["ip", "route", "add", "default", "via", &host])?;

    // (IP forwarding is enabled once at install time — net.ipv4.ip_forward is a
    // root-owned sysctl the non-root daemon can't write.)

    // Host nftables: stateless filter (ct hangs here) + SNAT. Built one op at a
    // time (`nft -f` hangs); chain name avoids the reserved `fwd`.
    run("nft", &["add", "table", "ip", &table])?;
    run("nft", &["add", "set", "ip", &table, "allowed", "{ type ipv4_addr; }"])?;
    run("nft", &["add", "chain", "ip", &table, "post", "{ type nat hook postrouting priority 100; }"])?;
    run("nft", &["add", "rule", "ip", &table, "post", "ip", "saddr", &cidr, "oifname", wan, "masquerade"])?;
    run("nft", &["add", "chain", "ip", &table, "mira_fwd", "{ type filter hook forward priority 0; policy drop; }"])?;
    run("nft", &["add", "rule", "ip", &table, "mira_fwd", "ip", "saddr", &cidr, "ip", "daddr", "@allowed", "accept"])?;
    run("nft", &["add", "rule", "ip", &table, "mira_fwd", "ip", "daddr", &cidr, "accept"])?;

    // Per-slot dnsmasq on the host veth IP: resolves only the allowlisted names
    // and populates @allowed with each resolved A record. --no-resolv => every
    // other name is refused.
    let mut cmd = Command::new("dnsmasq");
    cmd.arg("--keep-in-foreground")
        .arg("--conf-file=/dev/null")
        .arg("--no-resolv")
        .arg("--no-hosts")
        .arg("--bind-interfaces")
        .arg(format!("--listen-address={host}"))
        .arg("--filter-AAAA");
    for d in domains {
        cmd.arg(format!("--server=/{d}/{upstream}"));
        cmd.arg(format!("--nftset=/{d}/ip#{table}#allowed"));
    }
    // stderr → the daemon's journal so a dnsmasq failure is diagnosable.
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
    let mut child = cmd.spawn().map_err(|e| format!("spawn dnsmasq: {e}"))?;
    let dns_pid = child.id();
    // dnsmasq drops privileges at startup; if it can't (or can't bind), it exits
    // immediately. Catch that here so NetAllow fails loudly instead of returning
    // a slot backed by a dead resolver.
    std::thread::sleep(Duration::from_millis(300));
    match child.try_wait() {
        Ok(Some(status)) => return Err(format!("dnsmasq exited immediately ({status}); see journal")),
        Ok(None) => {}
        Err(e) => return Err(format!("dnsmasq health check failed: {e}")),
    }
    // Let the Child handle drop without killing — dnsmasq runs on, independent of
    // this daemon's lifetime; the reaper kills + reaps it by pid via the marker.
    drop(child);
    Ok(dns_pid)
}

// Best-effort removal of a slot's kernel objects (idempotent). Deleting the host
// veth removes its peer; deleting the table drops the filter + NAT + set.
fn cleanup_kernel(slot: usize) {
    let _ = run("nft", &["delete", "table", "ip", &nft_table(slot)]);
    let _ = run("ip", &["link", "del", &veth_host(slot)]);
}

// --- marker files (slot → confined pid + dnsmasq pid) ----------------------

fn marker_path(slot: usize) -> String {
    format!("{STATE_DIR}/{slot}")
}

fn write_marker(slot: usize, confined_pid: u32, dns_pid: u32) {
    if let Err(e) = std::fs::write(marker_path(slot), format!("{confined_pid} {dns_pid}\n")) {
        log(&format!("WARNING: could not write marker for slot {slot}: {e}"));
    }
}

fn read_marker(slot: usize) -> Option<(u32, u32)> {
    let s = std::fs::read_to_string(marker_path(slot)).ok()?;
    let mut it = s.split_whitespace();
    let c = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    Some((c, d))
}

fn remove_marker(slot: usize) {
    let _ = std::fs::remove_file(marker_path(slot));
}

// All `(slot, confined_pid, dns_pid)` from the marker directory.
fn read_all_markers() -> Vec<(usize, u32, u32)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(STATE_DIR) else { return out };
    for entry in rd.flatten() {
        if let Some(slot) = entry.file_name().to_str().and_then(|n| n.parse::<usize>().ok()) {
            if let Some((c, d)) = read_marker(slot) {
                out.push((slot, c, d));
            }
        }
    }
    out
}

fn slot_for_pid(pid: u32) -> Option<usize> {
    read_all_markers().into_iter().find(|(_, c, _)| *c == pid).map(|(s, _, _)| s)
}

// --- kernel-derived slot allocation ---------------------------------------

// Slots currently backed by an `mira_egr_<N>` nft table (the kernel is the
// source of truth for occupancy, so a live plugin's slot is never reused).
fn existing_egress_slots() -> Vec<usize> {
    let out = Command::new("nft").args(["list", "tables", "ip"]).output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("table ip mira_egr_"))
        .filter_map(|n| n.trim().parse::<usize>().ok())
        .collect()
}

fn first_free_slot() -> Option<usize> {
    let used = existing_egress_slots();
    (0..MAX_SLOTS).find(|s| !used.contains(s) && read_marker(*s).is_none())
}

// Locate a slot's dnsmasq by its unique nftset reference in the cmdline — used
// only for marker-less orphans (the normal path reads the dnsmasq pid from the
// marker).
fn find_dnsmasq_for_slot(slot: usize) -> Option<u32> {
    running_egress_dnsmasq().into_iter().find(|(s, _)| *s == slot).map(|(_, p)| p)
}

// All running per-slot dnsmasq processes, as `(slot, pid)`, discovered by their
// unique `#mira_egr_<slot>#` nftset argument in the cmdline.
fn running_egress_dnsmasq() -> Vec<(usize, u32)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let cmd = String::from_utf8_lossy(&raw);
        if let Some(rest) = cmd.split("#mira_egr_").nth(1) {
            if let Some(end) = rest.find('#') {
                if let Ok(slot) = rest[..end].parse::<usize>() {
                    out.push((slot, pid));
                }
            }
        }
    }
    out
}

// --- names + addresses (deterministic per slot) ---------------------------

fn veth_host(slot: usize) -> String {
    format!("mira-eg{slot}h") // <= 15 (IFNAMSIZ): "mira-eg63h" = 10
}
fn veth_child(slot: usize) -> String {
    format!("mira-eg{slot}c")
}
fn nft_table(slot: usize) -> String {
    format!("mira_egr_{slot}") // not an nft keyword
}
fn host_ip(slot: usize) -> String {
    format!("10.69.{slot}.1")
}
fn child_ip(slot: usize) -> String {
    format!("10.69.{slot}.2")
}

// --- command helpers ------------------------------------------------------

// One audit/diagnostic line to the daemon's stderr → journal (same prefix as
// `daemon::audit`). Makes reaping/teardown steps visible if anything stalls.
fn log(msg: &str) {
    eprintln!("mira-helper: net: {msg}");
}

// Run `timeout <N> <prog> <args…>`, mapping non-zero/timeout to a message.
fn run(prog: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new("timeout")
        .arg(CMD_TIMEOUT_SECS)
        .arg(prog)
        .args(args)
        .output()
        .map_err(|e| format!("spawn {prog}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    if out.status.code() == Some(124) {
        return Err(format!("{prog} {} timed out (>{CMD_TIMEOUT_SECS}s)", args.join(" ")));
    }
    Err(format!(
        "{prog} {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

// Run a command inside `pid`'s network namespace (`nsenter -t <pid> -n`).
fn nsenter(pid: u32, args: &[&str]) -> Result<(), String> {
    let pid_s = pid.to_string();
    let mut full: Vec<&str> = vec!["-t", &pid_s, "-n", "--"];
    full.extend_from_slice(args);
    run("nsenter", &full)
}

// The host's default-route egress interface (e.g. `eth0`).
fn wan_iface() -> Result<String, String> {
    let out = Command::new("ip")
        .args(["-o", "route", "show", "default"])
        .output()
        .map_err(|e| format!("ip route show default: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let toks: Vec<&str> = text.split_whitespace().collect();
    toks.iter()
        .position(|t| *t == "dev")
        .and_then(|i| toks.get(i + 1))
        .map(|s| s.to_string())
        .ok_or_else(|| "no default route interface found".into())
}

// --- validation -----------------------------------------------------------

// Normalize + validate an allowlist host (mirrors the package-tier
// `egress_host`): strip scheme/path/port, fold a leading `*.`, lowercase, and
// require a plain DNS name. Rejects anything with shell/argv-hostile chars.
fn sanitize_domain(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    let s = s.split('/').next().unwrap_or(s);
    let s = s.split(':').next().unwrap_or(s);
    let s = s.strip_prefix("*.").unwrap_or(s);
    let s = s.trim_matches('.').trim();
    if s.is_empty() || s.len() > 253 {
        return None;
    }
    let ok = s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && s.chars().any(|c| c.is_ascii_alphanumeric())
        && !s.starts_with('-');
    ok.then(|| s.to_ascii_lowercase())
}

// Validate the upstream resolver — must be a literal IPv4 address. Defaults to
// `1.1.1.1` when unset.
fn parse_upstream(opt: Option<&str>) -> Result<String, String> {
    let raw = opt.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("1.1.1.1");
    raw.parse::<std::net::Ipv4Addr>()
        .map(|ip| ip.to_string())
        .map_err(|_| format!("invalid upstream DNS (need an IPv4 address): {raw}"))
}

// Whether `pid` is alive and has a network namespace we can reference. Uses
// `symlink_metadata` (lstat) rather than `exists()` so the check itself doesn't
// *follow* the magic `ns/net` symlink — following it would need ptrace access
// the daemon may lack on the check path (the real ops carry `CAP_SYS_PTRACE`).
fn pid_has_netns(pid: u32) -> bool {
    std::fs::symlink_metadata(format!("/proc/{pid}/ns/net")).is_ok()
}

// Whether `pid` is still a live process (cheap, ptrace-free).
fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).is_dir()
}

// The real uid owning `pid` (from `/proc/<pid>/status`).
fn pid_owner_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|u| u.parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_and_normalizes_domains() {
        assert_eq!(sanitize_domain("https://API.Example.com:443/path"), Some("api.example.com".into()));
        assert_eq!(sanitize_domain("*.foo.com"), Some("foo.com".into()));
        assert_eq!(sanitize_domain("  bar.io  "), Some("bar.io".into()));
        assert_eq!(sanitize_domain("bad host"), None);
        assert_eq!(sanitize_domain("a;rm -rf"), None);
        assert_eq!(sanitize_domain("--conf-file=x"), None);
        assert_eq!(sanitize_domain(""), None);
    }

    #[test]
    fn parses_upstream_or_defaults() {
        assert_eq!(parse_upstream(None).unwrap(), "1.1.1.1");
        assert_eq!(parse_upstream(Some("")).unwrap(), "1.1.1.1");
        assert_eq!(parse_upstream(Some("8.8.8.8")).unwrap(), "8.8.8.8");
        assert!(parse_upstream(Some("nope")).is_err());
        assert!(parse_upstream(Some("1.2.3.4.5")).is_err());
    }

    #[test]
    fn slot_names_fit_ifnamsiz_and_avoid_keywords() {
        for i in 0..MAX_SLOTS {
            assert!(veth_host(i).len() <= 15, "veth host name too long: {}", veth_host(i));
            assert!(veth_child(i).len() <= 15, "veth child name too long: {}", veth_child(i));
            assert_ne!(nft_table(i), "fwd");
        }
        assert_eq!(veth_host(0), "mira-eg0h");
        assert_eq!(veth_child(0), "mira-eg0c");
        assert_eq!(nft_table(7), "mira_egr_7");
        assert_eq!(host_ip(3), "10.69.3.1");
        assert_eq!(child_ip(3), "10.69.3.2");
    }
}
