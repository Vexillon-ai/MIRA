// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/process.rs
//! Current-process resource usage for the Guardian health detectors.
//!
//! Memory (RSS/VSZ), CPU%, and uptime come from `sysinfo`, so these detectors
//! run on **Linux, macOS, and Windows** alike — no more Linux-only `/proc`
//! dependency and no "detector unavailable" warnings off Linux.
//!
//! Thread count and open-FD count have no portable source in `sysinfo`, so they
//! stay Linux-only (`/proc`). On other platforms they're left `None` and their
//! two detectors report "not applicable on this platform" rather than failing.
//!
//! CPU% is a delta between two `sysinfo` refreshes of this process, so we keep a
//! process-static `System` alive across calls. The first sample after boot
//! reads ~0% (no prior refresh to diff) — the detector treats that as
//! informational.

use std::sync::{Mutex, OnceLock};

use sysinfo::{get_current_pid, ProcessRefreshKind, ProcessesToUpdate, System};

// `/proc` readers for the two metrics sysinfo can't provide portably.
#[cfg(target_os = "linux")]
use std::fs;

/// Snapshot of current-process resource usage. All fields are best-effort —
/// any individual read failure leaves that field as `None` rather than
/// failing the whole snapshot.
#[derive(Debug, Clone, Default)]
pub struct ProcessSnapshot {
    pub rss_kb:        Option<u64>,
    pub vsz_kb:        Option<u64>,
    pub thread_count:  Option<u64>,
    pub fd_count:      Option<u64>,
    pub fd_soft_limit: Option<u64>,
    /// Approximated CPU utilisation as a fraction (1.0 ≈ one core fully used;
    /// can exceed 1.0 on multicore) over the interval since the previous call.
    pub cpu_pct:       Option<f64>,
    /// Wall-clock seconds since the process started.
    pub uptime_secs:   Option<u64>,
}

// Persistent `System` so `cpu_usage()` has a prior sample to diff against —
// sysinfo computes a process's CPU% between consecutive refreshes of the same
// `System`. A fresh `System` per call would always report 0%.
fn system() -> &'static Mutex<System> {
    static SYS: OnceLock<Mutex<System>> = OnceLock::new();
    SYS.get_or_init(|| Mutex::new(System::new()))
}

pub fn snapshot() -> ProcessSnapshot {
    let mut snap = ProcessSnapshot::default();

    // Portable metrics: memory, CPU, uptime via sysinfo (Linux/macOS/Windows).
    if let Ok(pid) = get_current_pid() {
        if let Ok(mut sys) = system().lock() {
            // CPU% is a delta vs the previous refresh of the same process, and
            // sysinfo computes it relative to total system CPU time — so we must
            // refresh the system CPU too, then the process (CPU + memory). The
            // persistent `System` preserves the prior sample across calls; the
            // first sample after boot reads ~0% (no prior to diff).
            sys.refresh_cpu_all();
            let kind = ProcessRefreshKind::nothing().with_cpu().with_memory();
            sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind);
            if let Some(p) = sys.process(pid) {
                // sysinfo reports memory in bytes.
                snap.rss_kb = Some(p.memory() / 1024);
                snap.vsz_kb = Some(p.virtual_memory() / 1024);
                // cpu_usage(): percent where 100.0 == one core fully used (can
                // exceed 100 on multicore). The detector wants a "cores used"
                // fraction, so divide by 100.
                snap.cpu_pct = Some((p.cpu_usage() as f64 / 100.0).max(0.0));
                snap.uptime_secs = Some(p.run_time());
            }
        }
    }

    // Thread + FD counts: Linux `/proc` only (no portable source). Other
    // platforms leave these `None`; their detectors report "not applicable".
    #[cfg(target_os = "linux")]
    {
        snap.thread_count  = read_status_field("Threads");
        snap.fd_count      = count_fds();
        snap.fd_soft_limit = read_fd_soft_limit();
    }

    snap
}

/// Free + total space (MB) on the filesystem holding `path`. Cross-platform via
/// sysinfo. Picks the mounted volume whose mount point is the longest prefix of
/// `path` (the most specific mount), so a data dir on its own volume is measured
/// against that volume — not `/` or `C:\`. `(None, None)` if no match.
pub fn disk_space_mb(path: &std::path::Path) -> (Option<u64>, Option<u64>) {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    let mut best: Option<(usize, u64, u64)> = None;
    for d in disks.list() {
        let mp = d.mount_point();
        if path.starts_with(mp) {
            let specificity = mp.as_os_str().len();
            if best.is_none_or(|(len, _, _)| specificity >= len) {
                best = Some((specificity, d.available_space(), d.total_space()));
            }
        }
    }
    match best {
        Some((_, free, total)) => (Some(free / 1024 / 1024), Some(total / 1024 / 1024)),
        None => (None, None),
    }
}

/// Host-level machine metrics for the Status page: overall CPU, host memory,
/// MIRA's own resident memory, and the data partition's disk usage.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MachineMetrics {
    /// Overall host CPU usage, 0..100 (%). `None`/0 on the first sample.
    pub cpu_pct:       Option<f64>,
    pub mem_total_mb:  Option<u64>,
    pub mem_used_mb:   Option<u64>,
    /// MIRA's own resident memory (MB).
    pub proc_rss_mb:   Option<u64>,
    pub disk_free_mb:  Option<u64>,
    pub disk_total_mb: Option<u64>,
}

/// Gather host CPU + memory, MIRA's RSS, and the data partition's disk usage.
/// Reuses the persistent `System` so the CPU% has a prior sample to diff.
pub fn machine_metrics(data_dir: &std::path::Path) -> MachineMetrics {
    let mut m = MachineMetrics::default();
    if let Ok(mut sys) = system().lock() {
        sys.refresh_cpu_all();
        sys.refresh_memory();
        m.cpu_pct      = Some(sys.global_cpu_usage() as f64);
        m.mem_total_mb = Some(sys.total_memory() / 1024 / 1024);
        m.mem_used_mb  = Some(sys.used_memory()  / 1024 / 1024);
        if let Ok(pid) = get_current_pid() {
            let kind = ProcessRefreshKind::nothing().with_memory();
            sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind);
            if let Some(p) = sys.process(pid) {
                m.proc_rss_mb = Some(p.memory() / 1024 / 1024);
            }
        }
    }
    let (free, total) = disk_space_mb(data_dir);
    m.disk_free_mb  = free;
    m.disk_total_mb = total;
    m
}

#[cfg(target_os = "linux")]
fn read_status_field(field: &str) -> Option<u64> {
    let s = fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        let Some(rest) = line.strip_prefix(field) else { continue };
        let rest = rest.trim_start_matches(':').trim();
        // Format: `<num> kB` for memory, bare `<num>` for thread count.
        let val = rest.split_whitespace().next()?;
        return val.parse().ok();
    }
    None
}

#[cfg(target_os = "linux")]
fn count_fds() -> Option<u64> {
    let entries = fs::read_dir("/proc/self/fd").ok()?;
    Some(entries.filter_map(|e| e.ok()).count() as u64)
}

#[cfg(target_os = "linux")]
fn read_fd_soft_limit() -> Option<u64> {
    let s = fs::read_to_string("/proc/self/limits").ok()?;
    for line in s.lines() {
        // Format: `Max open files            <soft>   <hard>   files`
        if let Some(rest) = line.strip_prefix("Max open files") {
            let mut parts = rest.split_whitespace();
            // First numeric column is the soft limit.
            if let Some(soft) = parts.next() {
                if let Ok(n) = soft.parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_returns_portable_basics_everywhere() {
        // RSS + uptime come from sysinfo and must work on every platform.
        let snap = snapshot();
        assert!(snap.rss_kb.unwrap_or(0) > 0, "rss should be non-zero: {snap:?}");
        assert!(snap.vsz_kb.unwrap_or(0) > 0, "vsz should be non-zero: {snap:?}");
        // cpu_pct is Some from the first sample (0% with no prior refresh).
        let _ = snapshot();
        let third = snapshot();
        assert!(third.cpu_pct.is_some(), "expected cpu_pct: {third:?}");
    }

    #[test]
    fn snapshot_captures_real_cpu_under_load() {
        // A busy loop between two snapshots must surface as non-zero CPU — the
        // whole point of the detector. Guards the sysinfo refresh recipe
        // (system CPU + process, persistent System) against regressions.
        // Retried a few times so the shared global `System` + sysinfo's minimum
        // CPU-update interval can't make it flake under parallel test load.
        let mut best = 0.0_f64;
        for _ in 0..4 {
            let _ = snapshot(); // establish the prior sample
            let start = std::time::Instant::now();
            let mut x: u64 = 0;
            while start.elapsed().as_millis() < 350 { x = x.wrapping_add(1).wrapping_mul(2654435761); }
            std::hint::black_box(x);
            best = best.max(snapshot().cpu_pct.unwrap_or(0.0));
            if best > 0.0 { break; }
        }
        assert!(best > 0.0, "expected real CPU after a busy loop, got {best}");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn snapshot_returns_linux_thread_and_fd_counts() {
        let snap = snapshot();
        // FD count includes stdin/stdout/stderr so >= 3.
        assert!(snap.fd_count.unwrap_or(0) >= 3, "fd_count too low: {snap:?}");
        assert!(snap.thread_count.unwrap_or(0) >= 1);
        assert!(snap.fd_soft_limit.unwrap_or(0) > 0);
    }
}
