// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/process.rs
//! `/proc/self/*` reader — current process resource usage.
//!
//! Linux only — `/proc` is the convention for runtime introspection on
//! Linux. The non-Linux build returns `Unsupported` for every call and
//! the corresponding detectors degrade to Yellow with a clear message.
//! That's better than fabricating numbers or pulling in `sysinfo` just
//! for cross-platform support a personal-server tool will rarely need.
//!
//! CPU% needs delta math across two samples. We keep the previous
//! reading in a process-static atomic so the detector doesn't have to
//! plumb state through `DetectorContext`. The first sample after boot
//! returns 0% — fine, the detector reports informational.

// All `/proc` reads + the CPU-delta atomics below are Linux-only (every reader
// fn is `#[cfg(target_os = "linux")]`); gate the imports to match so non-Linux
// builds don't warn on unused `fs` / atomics.
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

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
    /// Approximated cpu utilisation as a fraction (0.0–1.0) over the
    /// interval since the previous call. None on the first sample
    /// after process start (no prior reading to diff).
    pub cpu_pct:       Option<f64>,
    /// Wall-clock seconds since the process started.
    pub uptime_secs:   Option<u64>,
}

#[cfg(target_os = "linux")]
pub fn snapshot() -> ProcessSnapshot {
    ProcessSnapshot {
        rss_kb:        read_status_field("VmRSS"),
        vsz_kb:        read_status_field("VmSize"),
        thread_count:  read_status_field("Threads"),
        fd_count:      count_fds(),
        fd_soft_limit: read_fd_soft_limit(),
        cpu_pct:       compute_cpu_pct(),
        uptime_secs:   compute_uptime_secs(),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn snapshot() -> ProcessSnapshot { ProcessSnapshot::default() }

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

// CPU% delta state. Stored as raw atomics so we don't need a Mutex on
// the read path. `prev_total_jiffies` tracks the sum of utime + stime
// at the last sample; `prev_wall_ms` is the wall clock in millis at
// the same point.
#[cfg(target_os = "linux")]
static PREV_TOTAL_JIFFIES: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "linux")]
static PREV_WALL_MS:       AtomicI64 = AtomicI64::new(0);

#[cfg(target_os = "linux")]
fn compute_cpu_pct() -> Option<f64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // Field 14 is utime, 15 is stime (1-indexed per proc(5)). The
    // `comm` field can contain spaces and parentheses, so split after
    // the closing paren.
    let close = stat.rfind(')')?;
    let after: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // After `)` we're at field 3 (state), so utime/stime are at indices
    // 14-3 = 11 and 12 in `after`.
    let utime: u64 = after.get(11)?.parse().ok()?;
    let stime: u64 = after.get(12)?.parse().ok()?;
    let total = utime + stime;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let prev_total = PREV_TOTAL_JIFFIES.swap(total, Ordering::SeqCst);
    let prev_wall  = PREV_WALL_MS.swap(now_ms, Ordering::SeqCst);

    // Only PREV_WALL_MS distinguishes "first call" from "subsequent
    // call". PREV_TOTAL_JIFFIES might legitimately stay 0 for many
    // calls in a row (bursty single-jiffy clock + idle process), so
    // gating on it would suppress reporting indefinitely.
    if prev_wall == 0 { return None; }

    // Floor at 1ms — back-to-back calls in tests can land in the same
    // millisecond. Returning None there would force callers to wait.
    let elapsed_ms = ((now_ms - prev_wall) as f64).max(1.0);

    // Read the system clock-tick rate from sysconf. Most Linuxes use
    // 100 Hz, but we shouldn't assume — `sysconf(_SC_CLK_TCK)` is the
    // canonical value.
    let hz = clk_tck();
    // total_jiffies is monotonic so the diff is non-negative — but
    // saturating_sub guards against any future quirk (e.g. overflow on
    // a long-running process).
    let cpu_secs = total.saturating_sub(prev_total) as f64 / hz;
    let elapsed_secs = elapsed_ms / 1000.0;
    Some((cpu_secs / elapsed_secs).clamp(0.0, num_cpus()))
}

#[cfg(target_os = "linux")]
fn clk_tck() -> f64 {
    // SAFETY: sysconf is async-signal-safe and takes a constant int.
    let n = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if n > 0 { n as f64 } else { 100.0 }
}

#[cfg(target_os = "linux")]
fn num_cpus() -> f64 {
    // SAFETY: sysconf with _SC_NPROCESSORS_ONLN.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 { n as f64 } else { 1.0 }
}

#[cfg(target_os = "linux")]
fn compute_uptime_secs() -> Option<u64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let close = stat.rfind(')')?;
    let after: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // start_time is field 22 (1-indexed) → index 22-3 = 19 in `after`.
    let start_jiffies: u64 = after.get(19)?.parse().ok()?;
    let uptime_secs = read_proc_uptime()?;
    let proc_start_secs = start_jiffies as f64 / clk_tck();
    Some((uptime_secs as f64 - proc_start_secs).max(0.0) as u64)
}

#[cfg(target_os = "linux")]
fn read_proc_uptime() -> Option<u64> {
    let s = fs::read_to_string("/proc/uptime").ok()?;
    let first = s.split_whitespace().next()?;
    Some(first.parse::<f64>().ok()? as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn snapshot_returns_some_basics() {
        let snap = snapshot();
        // RSS should be Some > 0 — we're running.
        assert!(snap.rss_kb.unwrap_or(0) > 0, "rss should be non-zero: {snap:?}");
        // FD count includes stdin/stdout/stderr so >= 3.
        assert!(snap.fd_count.unwrap_or(0) >= 3, "fd_count too low: {snap:?}");
        assert!(snap.thread_count.unwrap_or(0) >= 1);
        // Soft limit should be > 0 (typically 1024+).
        assert!(snap.fd_soft_limit.unwrap_or(0) > 0);
        // First call returns no cpu_pct (no prior sample).
        // Don't assert on the first cpu_pct value because tests in a
        // module can run in any order; just ensure subsequent calls
        // produce *some* number.
        let _second = snapshot();
        let third = snapshot();
        assert!(third.cpu_pct.is_some(), "expected cpu_pct on second+ call");
    }
}
