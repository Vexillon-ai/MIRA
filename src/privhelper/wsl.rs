// SPDX-License-Identifier: AGPL-3.0-or-later

//! WSL host-alias setup — a root-time, install-time privileged operation.
//!
//! On WSL2 in **NAT mode** (the default), a MIRA-in-WSL guest cannot reach its
//! own Windows host's *LAN* IP — only the WSL **NAT gateway** IP works, and that
//! IP is reassigned on every `wsl --shutdown` / Windows reboot. So Windows-host
//! services (LM Studio, a TTS server, SearXNG, …) configured by LAN IP break
//! intermittently and after every reboot.
//!
//! This installs a tiny boot hook that maps a stable hostname (`windows-host`)
//! to *whatever the gateway IP currently is*, refreshed on every boot — so the
//! operator can point MIRA at `http://windows-host:PORT` once and never touch it
//! again.
//!
//! Like [`super::install`], this is a **root one-shot run at install time**, not
//! a runtime daemon RPC: it writes `/etc/`-owned files and drives `systemctl`,
//! which the unprivileged `mira-helper` daemon deliberately can't do (it has no
//! `CAP_DAC_OVERRIDE` and no systemd access). Keeping it out of the daemon means
//! the persistent helper's capability set stays untouched.

use serde_json::json;

/// The stable hostname pointed at the Windows host. Resolvable via `/etc/hosts`,
/// which the standard system resolver (and thus reqwest's default getaddrinfo
/// path) honours.
pub const DEFAULT_ALIAS: &str = "windows-host";

const SCRIPT_PATH: &str = "/usr/local/bin/wsl-host-alias.sh";
const UNIT_PATH: &str = "/etc/systemd/system/wsl-host-alias.service";
const UNIT_NAME: &str = "wsl-host-alias.service";

/// Reject anything but a simple DNS-label-ish alias — it gets baked into a shell
/// script and a `sed` pattern, so no shell/sed metacharacters are allowed.
fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() || alias.len() > 63 {
        return Err("alias must be 1–63 characters".into());
    }
    if !alias.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err("alias may contain only [a-z0-9-]".into());
    }
    if alias.starts_with('-') || alias.ends_with('-') {
        return Err("alias must not start or end with '-'".into());
    }
    Ok(())
}

/// The boot-refresh script: write `<gateway-ip> <alias>` into /etc/hosts. WSL
/// regenerates /etc/hosts on every boot, so this re-adds the line each boot
/// (and drops any stale one first). Idempotent.
fn render_script(alias: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Managed by MIRA (privhelper::wsl). Maps `{alias}` -> the WSL2 NAT gateway\n\
         # (the Windows host) in /etc/hosts, refreshed on every boot. Do not edit.\n\
         set -eu\n\
         GW=\"$(ip route show default | awk '{{print $3; exit}}')\"\n\
         [ -n \"${{GW:-}}\" ] || exit 0\n\
         sed -i '/[[:space:]]{alias}$/d' /etc/hosts\n\
         printf '%s\\t{alias}\\n' \"$GW\" >> /etc/hosts\n"
    )
}

/// The systemd oneshot that runs the script at boot (after the network is up).
fn render_unit(alias: &str) -> String {
    format!(
        "[Unit]\n\
         Description=MIRA: map {alias} to the WSL2 gateway (Windows host) in /etc/hosts\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={SCRIPT_PATH}\n\
         RemainAfterExit=yes\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Current WSL2 NAT gateway (the Windows host) from the default route.
fn gateway_ip() -> Option<String> {
    let out = std::process::Command::new("ip").args(["route", "show", "default"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    // "default via 198.51.100.10 dev eth0 proto kernel"
    s.split_whitespace().skip_while(|t| *t != "via").nth(1).map(str::to_string)
}

/// The current `/etc/hosts` line for `alias`, if present.
fn hosts_entry(alias: &str) -> Option<String> {
    let hosts = std::fs::read_to_string("/etc/hosts").ok()?;
    hosts.lines().find(|l| {
        l.split_whitespace().skip(1).any(|h| h == alias) && !l.trim_start().starts_with('#')
    }).map(|l| l.trim().to_string())
}

/// Install (or refresh) the WSL host-alias boot hook. Must run as **root**
/// (`sudo mira wsl-host-alias-install`, or folded into `sudo mira helper-install`).
/// Idempotent: re-running redeploys the script/unit and re-populates /etc/hosts.
/// Returns a small status object for the caller to print/audit.
pub fn install_host_alias(alias: &str) -> Result<serde_json::Value, String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("wsl-host-alias-install must run as root — try: sudo mira wsl-host-alias-install".into());
    }
    validate_alias(alias)?;
    if !crate::install::is_wsl() {
        return Err("not running under WSL — the host alias is only needed inside WSL".into());
    }

    // 1. Refresh script (0755).
    std::fs::write(SCRIPT_PATH, render_script(alias))
        .map_err(|e| format!("write {SCRIPT_PATH}: {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(SCRIPT_PATH, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {SCRIPT_PATH}: {e}"))?;
    }

    // 2. systemd oneshot (0644).
    std::fs::write(UNIT_PATH, render_unit(alias))
        .map_err(|e| format!("write {UNIT_PATH}: {e}"))?;

    // 3. Enable + run now (ExecStart populates /etc/hosts immediately).
    super::systemctl(&["daemon-reload"])?;
    super::systemctl(&["enable", "--now", UNIT_NAME])?;

    Ok(json!({
        "alias":        alias,
        "gateway":      gateway_ip(),
        "hosts_entry":  hosts_entry(alias),
        "unit":         UNIT_PATH,
        "script":       SCRIPT_PATH,
        "use_url_form": format!("http://{alias}:<PORT>"),
    }))
}

/// Read-only status of the host-alias hook (no privilege needed) — for
/// `helper-status` and the startup self-check.
pub fn host_alias_status(alias: &str) -> serde_json::Value {
    json!({
        "is_wsl":       crate::install::is_wsl(),
        "alias":        alias,
        "unit_present": std::path::Path::new(UNIT_PATH).exists(),
        "gateway":      gateway_ip(),
        "hosts_entry":  hosts_entry(alias),
    })
}
