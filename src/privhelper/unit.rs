// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pure rendering of the `mira-helper` systemd **system** unit (root-scoped).
//! Kept separate so it's unit-testable without touching disk or systemctl.

use std::path::Path;

/// Render the root `mira-helper.service`. The daemon runs privileged but is
/// bounded to exactly the capabilities its ops need (not unrestricted root), with
/// filesystem + tmp hardening — a tiny fixed-API daemon should be hard to misuse.
pub fn render(mira_bin: &Path, socket: &Path, owner_uid: u32) -> String {
    format!(
"[Unit]
Description=MIRA privileged helper — least-privilege elevated ops
After=network.target

[Service]
Type=simple
# Runs as a dedicated, unprivileged system user (NOT uid 0) with exactly the
# ambient caps the ops need. The binary is installed to a system path so this
# user can exec it without any access to the operator's home account.
User=mira-helper
ExecStart={bin} helper-daemon --socket {sock} --owner-uid {uid}
# Least privilege: exactly the caps the ops need, granted as AMBIENT (so the
# non-root daemon — and the ip/nft/nsenter/dnsmasq it spawns — actually hold
# them) and bounded to the same set. NET_ADMIN — veth/nft/routing; SYS_ADMIN —
# setns into the child netns; SYS_PTRACE — open another process's
# /proc/<pid>/ns/net to reference its netns; NET_BIND_SERVICE — the per-slot
# dnsmasq binds :53; CHOWN — chown the socket to the MIRA user. (dnsmasq does NOT
# drop privileges when started non-root, so it stays this user — no SETUID/
# SETGID, and killing it on teardown is same-uid, so no KILL.)
CapabilityBoundingSet=CAP_NET_ADMIN CAP_SYS_ADMIN CAP_SYS_PTRACE CAP_NET_BIND_SERVICE CAP_CHOWN
AmbientCapabilities=CAP_NET_ADMIN CAP_SYS_ADMIN CAP_SYS_PTRACE CAP_NET_BIND_SERVICE CAP_CHOWN
# Hardening for a small, fixed-API daemon.
NoNewPrivileges=true
ProtectSystem=full
ProtectHome=tmpfs
PrivateTmp=true
ProtectKernelModules=true
RestrictRealtime=true
RuntimeDirectory=mira-helper
RuntimeDirectoryMode=0755
# Markers (slot → pid) live under the runtime dir; preserve it across restarts so
# the reaper can still recover/clean state after the daemon itself restarts.
RuntimeDirectoryPreserve=yes
# Per-slot dnsmasq resolvers are children of this daemon. KillMode=process
# signals only the main process on stop/restart, so those resolvers SURVIVE a
# daemon restart — a live plugin's egress keeps working across it. The reaper
# (which tracks slots via marker files) cleans up resolvers for dead slots.
KillMode=process
Restart=always
RestartSec=2s

[Install]
WantedBy=multi-user.target
",
        bin = mira_bin.display(),
        sock = socket.display(),
        uid = owner_uid,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn renders_execstart_caps_and_install() {
        let out = render(
            &PathBuf::from("/usr/local/lib/mira/mira-helper"),
            &PathBuf::from("/run/mira-helper/sock"),
            1000,
        );
        assert!(out.contains("ExecStart=/usr/local/lib/mira/mira-helper helper-daemon --socket /run/mira-helper/sock --owner-uid 1000"));
        // Runs as a dedicated non-root user.
        assert!(out.contains("User=mira-helper"));
        // Exactly the caps the ops need, in the ambient set, bounded to the same.
        assert!(out.contains("CapabilityBoundingSet=CAP_NET_ADMIN CAP_SYS_ADMIN CAP_SYS_PTRACE CAP_NET_BIND_SERVICE CAP_CHOWN"));
        assert!(out.contains("AmbientCapabilities=CAP_NET_ADMIN CAP_SYS_ADMIN CAP_SYS_PTRACE CAP_NET_BIND_SERVICE CAP_CHOWN"));
        // dnsmasq doesn't drop privileges when non-root → these aren't needed.
        assert!(!out.contains("CAP_SETUID") && !out.contains("CAP_SETGID") && !out.contains("CAP_KILL"));
        assert!(out.contains("KillMode=process"));
        assert!(out.contains("NoNewPrivileges=true"));
        assert!(out.contains("ProtectHome=tmpfs"));
        assert!(out.contains("RuntimeDirectoryPreserve=yes"));
        assert!(out.contains("WantedBy=multi-user.target"));
        // Must NOT grant blanket root beyond the bounded caps.
        assert!(!out.contains("CapabilityBoundingSet=~"));
    }
}
