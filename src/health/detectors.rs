// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/detectors.rs
//! All slice-1 health detectors.
//!
//! Each detector implements [`super::Detector`] and is run once per
//! audit. Failure modes:
//! - Probe success → Green/Yellow/Red report.
//! - Probe error (couldn't read DB, file gone, etc.) → Yellow report
//!   describing what failed. The detector itself being broken
//!   shouldn't kill the audit; the operator still wants the rest
//!   of the snapshot.
//!
//! Naming: `<domain>.<thing>` (dotted, snake_case). The name doubles as
//! the watchdog incident fingerprint suffix (`health:<name>`) which is
//! used for cross-restart dedup.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::{Detector, DetectorContext, DetectorReport, HealthLevel};

// ── Helpers shared across detectors ─────────────────────────────────────────

fn err_yellow(name: &str, msg: impl Into<String>) -> DetectorReport {
    DetectorReport {
        name: name.into(),
        level: HealthLevel::Yellow,
        message: format!("detector unavailable: {}", msg.into()),
        value: None,
        payload: serde_json::Value::Null,
        auto_action_eligible: false,
        analytics: None,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 1. automations.subscriptions_stranded_completion
// ────────────────────────────────────────────────────────────────────────────

// Active `agent.worker.completed` deliveries that have been waiting
// without firing for >6h — these are workers whose completion event
// will never arrive, just like the bug surfaced in the 2026-05-09
// "neon-pong" incident. Threshold:
// - 0 stuck    → Green
// - 1–2 stuck  → Yellow ("a worker may have abandoned its parent")
// - 3+ stuck   → Red    ("multiple stranded — scheduler / worker pool sick")
// // Auto-cleanup eligible at Yellow+: each stranded sub gets marked
// `failed` so it stops occupying the active set.
pub struct StrandedCompletionSubsDetector;

const STRANDED_AGE_SECS: i64 = 6 * 60 * 60;

impl Detector for StrandedCompletionSubsDetector {
    fn name(&self) -> &'static str { "automations.subscriptions_stranded_completion" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(store) = ctx.automations.as_ref() else {
            return err_yellow(self.name(), "automations store not wired");
        };
        let now = chrono::Utc::now().timestamp();
        let cutoff = now - STRANDED_AGE_SECS;
        let stuck = match store.list_stuck_completion_subscriptions(cutoff) {
            Ok(v)  => v,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let count = stuck.len();
        let ids: Vec<&str> = stuck.iter().map(|s| s.id.as_str()).collect();
        let payload = json!({ "count": count, "ids": ids, "cutoff_age_secs": STRANDED_AGE_SECS });
        match count {
            0   => DetectorReport::green(self.name(), "no stranded completion subscriptions"),
            1..=2 => DetectorReport {
                name: self.name().into(),
                level: HealthLevel::Yellow,
                message: format!(
                    "{count} agent.worker.completed subscription(s) waiting >6h with no event"
                ),
                value: Some(count as f64),
                payload,
                auto_action_eligible: true,
                analytics: None,
            },
            _ => DetectorReport {
                name: self.name().into(),
                level: HealthLevel::Red,
                message: format!(
                    "{count} stranded completion subscriptions — workers are abandoning parents"
                ),
                value: Some(count as f64),
                payload,
                auto_action_eligible: true,
                analytics: None,
            },
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 2. automations.scheduler_tick_lag_secs
// ────────────────────────────────────────────────────────────────────────────

// Are the system heartbeat schedules firing on time? Pulls every
// system-owned schedule and checks whether `next_run_at` is past due
// by more than 2× its declared cadence. Signals "scheduler thread is
// stuck" before any single heartbeat alone would.
// // Note: cron-based schedules are exempt because their cadence isn't a
// single number; only `interval` triggers are checked. The most
// safety-critical interval schedule is the watchdog itself (60s) —
// if that's lagging, this detector fires.
pub struct SchedulerTickLagDetector;

impl Detector for SchedulerTickLagDetector {
    fn name(&self) -> &'static str { "automations.scheduler_tick_lag_secs" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(store) = ctx.automations.as_ref() else {
            return err_yellow(self.name(), "automations store not wired");
        };
        let now = chrono::Utc::now().timestamp();
        let schedules = match store.list_schedules(Some("system")) {
            Ok(v)  => v,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let mut worst_lag_secs: i64 = 0;
        let mut laggy: Vec<serde_json::Value> = Vec::new();
        for s in &schedules {
            // Only interval triggers — cron has no fixed cadence to compare against.
            let every = match &s.trigger {
                crate::automations::types::TriggerSpec::Interval { every_secs } => *every_secs as i64,
                _ => continue,
            };
            // Only active schedules — paused/expired aren't expected to fire.
            if !matches!(s.status, crate::automations::types::ScheduleStatus::Active) {
                continue;
            }
            let Some(next) = s.next_run_at else { continue };
            let lag = now - next;
            // Allow up to 2× the cadence as routine drift; past that
            // means the worker missed at least one full tick.
            if lag > 2 * every {
                if lag > worst_lag_secs { worst_lag_secs = lag; }
                laggy.push(json!({ "name": s.name, "lag_secs": lag, "cadence_secs": every }));
            }
        }
        let payload = json!({ "worst_lag_secs": worst_lag_secs, "laggy_schedules": laggy });
        if worst_lag_secs == 0 {
            return DetectorReport::green(self.name(), "all interval schedules ticking on cadence");
        }
        let level = if worst_lag_secs > 600 { HealthLevel::Red } else { HealthLevel::Yellow };
        DetectorReport {
            name: self.name().into(),
            level,
            message: format!(
                "{} schedule(s) lagging — worst {}s past expected next_run_at",
                laggy.len(), worst_lag_secs,
            ),
            value: Some(worst_lag_secs as f64),
            payload,
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 3. automations.runs_failure_rate_1h
// ────────────────────────────────────────────────────────────────────────────

// Failure rate of `automation_runs` rows in the last hour. Catches
// "an automation is firing repeatedly and failing every time"
// before anyone notices the silent breakage.
// // Substitutes for the originally-planned `channel.web.sse_5xx_rate_1h`
// signal — same domain (transient automation failures), but reads from
// a clean structured source rather than scraping logs.
pub struct AutomationsFailureRateDetector;

impl Detector for AutomationsFailureRateDetector {
    fn name(&self) -> &'static str { "automations.runs_failure_rate_1h" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(store) = ctx.automations.as_ref() else {
            return err_yellow(self.name(), "automations store not wired");
        };
        let now = chrono::Utc::now().timestamp();
        let since = now - 60 * 60;
        let (total, failures) = match store.count_runs_since(since) {
            Ok(t)  => t,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        if total == 0 {
            return DetectorReport::green(self.name(), "no automation runs in last hour");
        }
        let rate = failures as f64 / total as f64;
        let payload = json!({ "total": total, "failures": failures, "rate": rate });
        let level = if rate >= 0.5 { HealthLevel::Red }
                    else if rate >= 0.2 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        let message = format!(
            "{}/{} automation run(s) failed in last hour ({:.0}%)",
            failures, total, rate * 100.0,
        );
        DetectorReport {
            name: self.name().into(),
            level,
            message,
            value: Some(rate),
            payload,
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 4. agent.scratch_dirs_orphaned
// ────────────────────────────────────────────────────────────────────────────

// Scratch dirs left behind by abandoned worker subprocesses.
// // `code_run` allocates per-call scratch via `tempfile::Builder::new()
// .prefix("mira-coderun-").tempdir()`, which lands in `$TMPDIR`
// (usually `/tmp/`). Healthy operation removes the dir on RAII drop;
// abandoned workers leave the dir behind and the daily `tmp_cleanup`
// heartbeat is supposed to sweep it. Anything matching the prefix
// that's older than 7 days means *both* paths failed.
// // Important: we deliberately do NOT scan `<data_dir>/sandbox/` —
// `sandbox/rootfs/` and `sandbox/cache/` are long-lived infra dirs
// (python rootfs, model download cache) that would always trip a
// blanket age filter. And we don't scan the user's home for
// agent-produced output (e.g. `~/neon-pong-2024/`); we can't
// distinguish those from the user's own files.
pub struct ScratchDirsOrphanedDetector;

const SCRATCH_AGE_DAYS: u64 = 7;
// Prefixes that mark a per-call scratch dir from MIRA. Keep this list
// tight — anything matched here is assumed safe to flag/eventually
// auto-remove. New scratch consumers should use one of these prefixes
// or be added explicitly.
const SCRATCH_PREFIXES: &[&str] = &["mira-coderun-", "mira-skill-", ".mira-"];

impl Detector for ScratchDirsOrphanedDetector {
    fn name(&self) -> &'static str { "agent.scratch_dirs_orphaned" }

    fn run(&self, _ctx: &DetectorContext) -> DetectorReport {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(SCRATCH_AGE_DAYS * 24 * 60 * 60))
            .unwrap_or(std::time::UNIX_EPOCH);
        let mut orphans: Vec<String> = Vec::new();
        // $TMPDIR (default /tmp) is the only root we scan — see the
        // comment above on why we skip <data_dir>/sandbox/.
        let tmpdir = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        scan_tmp_for_scratch(&tmpdir, cutoff, &mut orphans);
        let count = orphans.len();
        let payload = json!({
            "count": count, "paths": orphans, "age_days": SCRATCH_AGE_DAYS,
            "scanned_root": tmpdir.display().to_string(),
            "matched_prefixes": SCRATCH_PREFIXES,
        });
        match count {
            0     => DetectorReport::green(self.name(), "no orphan scratch dirs"),
            1..=4 => DetectorReport {
                name: self.name().into(),
                level: HealthLevel::Yellow,
                message: format!("{count} scratch dir(s) older than {SCRATCH_AGE_DAYS}d"),
                value: Some(count as f64),
                payload,
                auto_action_eligible: false,
                analytics: None,
            },
            _ => DetectorReport {
                name: self.name().into(),
                level: HealthLevel::Red,
                message: format!(
                    "{count} orphan scratch dirs — tmp_cleanup heartbeat may be broken"
                ),
                value: Some(count as f64),
                payload,
                auto_action_eligible: false,
                analytics: None,
            },
        }
    }
}

fn scan_tmp_for_scratch(root: &Path, cutoff: std::time::SystemTime, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() { continue }
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if !SCRATCH_PREFIXES.iter().any(|p| s.starts_with(p)) { continue; }
        let Ok(modified) = meta.modified() else { continue };
        if modified < cutoff {
            out.push(entry.path().display().to_string());
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 5. llm.embedding.provider_unreachable
// ────────────────────────────────────────────────────────────────────────────

// When the configured embedding provider is `internal` (fastembed),
// verify the ONNX runtime is loadable. Catches the silent breakage
// from the 2026-05-04 incident where a missing libonnxruntime made
// every embed fall back to NoopEmbeddingProvider.
// // Other providers (lmstudio, openai) are skipped — their reachability
// is best tested via an actual embed call, which is heavier than
// this hourly probe should do.
pub struct EmbeddingProviderReachableDetector;

impl Detector for EmbeddingProviderReachableDetector {
    fn name(&self) -> &'static str { "llm.embedding.provider_unreachable" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        if ctx.embedding_provider_kind != "internal" {
            return DetectorReport::green(
                self.name(),
                format!("embedding provider is `{}` — skipping local-runtime probe", ctx.embedding_provider_kind),
            );
        }
        if crate::install::deps::is_onnxruntime_available() {
            return DetectorReport::green(self.name(), "onnxruntime loadable");
        }
        DetectorReport {
            name: self.name().into(),
            level: HealthLevel::Red,
            message: "embedding provider=internal but libonnxruntime is not loadable; \
                      embeds will silently no-op".into(),
            value: None,
            payload: json!({ "provider": "internal", "remediation": "POST /api/v1/admin/deps/install onnxruntime" }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 6. skills.broken_manifest_count
// ────────────────────────────────────────────────────────────────────────────

// Count of skill directories that failed to load (broken `skill.toml`,
// missing fields, unsigned-when-required, etc.). Matches the
// `.bundled-uninstalled` bug class fixed in 0.100.4 and any future
// manifest regressions.
pub struct BrokenSkillManifestsDetector;

impl Detector for BrokenSkillManifestsDetector {
    fn name(&self) -> &'static str { "skills.broken_manifest_count" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let skills_dir = crate::skills::default_skills_dir(&ctx.data_dir);
        let registry = crate::skills::loader::load_dir(&skills_dir, &ctx.mira_version);
        let count = registry.errors.len();
        let paths: Vec<String> = registry.errors.iter()
            .map(|e| format!("{}: {}", e.path.display(), e.error))
            .collect();
        let payload = json!({ "count": count, "errors": paths });
        match count {
            0     => DetectorReport::green(self.name(), "all installed skills loaded cleanly"),
            1..=2 => DetectorReport {
                name: self.name().into(),
                level: HealthLevel::Yellow,
                message: format!("{count} skill(s) failed to load"),
                value: Some(count as f64),
                payload,
                auto_action_eligible: false,
                analytics: None,
            },
            _ => DetectorReport {
                name: self.name().into(),
                level: HealthLevel::Red,
                message: format!("{count} broken skill manifests"),
                value: Some(count as f64),
                payload,
                auto_action_eligible: false,
                analytics: None,
            },
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 7. disk.mira_data_free_mb
// ────────────────────────────────────────────────────────────────────────────

// Free space on the partition holding `data_dir`. SQLite writes start
// failing well before 0; we want runway warning. Thresholds:
// - >2 GB free → Green
// - 500 MB – 2 GB → Yellow
// - <500 MB → Red
pub struct DiskFreeDetector;

impl Detector for DiskFreeDetector {
    fn name(&self) -> &'static str { "disk.mira_data_free_mb" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let free_mb = match statvfs_free_mb(&ctx.data_dir) {
            Ok(v)  => v,
            Err(e) => return err_yellow(self.name(), e),
        };
        let payload = json!({ "free_mb": free_mb, "data_dir": ctx.data_dir.display().to_string() });
        let level = if free_mb < 500 { HealthLevel::Red }
                    else if free_mb < 2048 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        let message = format!("{free_mb} MB free on data partition");
        DetectorReport {
            name: self.name().into(),
            level,
            message,
            value: Some(free_mb as f64),
            payload,
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

#[cfg(target_family = "unix")]
fn statvfs_free_mb(path: &Path) -> Result<u64, String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| format!("path -> CString: {e}"))?;
    // SAFETY: libc::statvfs writes into a stack-allocated struct on
    // success and returns -1 on failure. cpath outlives the call.
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut sv) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("statvfs({}) failed: {err}", path.display()));
    }
    // f_bavail is blocks available to non-root, which is what we want
    // (root can technically still write to a fully-reserved partition).
    let bytes_free = (sv.f_bavail as u64) * (sv.f_frsize as u64);
    Ok(bytes_free / 1024 / 1024)
}

#[cfg(not(target_family = "unix"))]
fn statvfs_free_mb(_path: &Path) -> Result<u64, String> {
    Err("statvfs unsupported on this platform".into())
}

// ────────────────────────────────────────────────────────────────────────────
// 8. audit.hmac_chain_breaks
// ────────────────────────────────────────────────────────────────────────────

// Verify the agent_audit HMAC chain. A break means either tampering
// or a deletion — both critical because the audit log is the
// security record for every spawn / budget / policy decision.
// // Cost: walks the entire `agent_audit` table per fire. At a few
// hundred rows that's microseconds; at millions it would be slower.
// We accept that — security audits warrant the cost.
pub struct AuditChainIntegrityDetector;

impl Detector for AuditChainIntegrityDetector {
    fn name(&self) -> &'static str { "audit.hmac_chain_breaks" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(audit) = ctx.audit_store.as_ref() else {
            return err_yellow(self.name(), "agent audit store not wired");
        };
        match audit.verify_chain() {
            Ok(()) => DetectorReport::green(self.name(), "agent_audit chain verifies clean"),
            Err(crate::agent::audit::AuditError::ChainBroken { row, reason }) => DetectorReport {
                name: self.name().into(),
                level: HealthLevel::Red,
                message: format!("agent_audit chain broken at row {row}"),
                value: Some(row as f64),
                payload: json!({ "broken_row_id": row, "reason": reason }),
                auto_action_eligible: false,
                analytics: None,
            },
            Err(e) => err_yellow(self.name(), e.to_string()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 9. auth.master_key_perms
// ────────────────────────────────────────────────────────────────────────────

// Master key file at `<data_dir>/master.key` must be 0600. Anything
// looser means another local user could read it and decrypt the
// skill secrets vault.
pub struct MasterKeyPermsDetector;

impl Detector for MasterKeyPermsDetector {
    fn name(&self) -> &'static str { "auth.master_key_perms" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let path = ctx.data_dir.join("master.key");
        if !path.exists() {
            // Missing-key is its own detector; this one only checks
            // perms. Green here so we don't double-fire.
            return DetectorReport::green(self.name(), "master.key absent (see auth.master_key_present)");
        }
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = match std::fs::metadata(&path) {
                Ok(m)  => m,
                Err(e) => return err_yellow(self.name(), e.to_string()),
            };
            // mode() returns the full st_mode — mask to the perm bits.
            let perms = meta.permissions().mode() & 0o777;
            let payload = json!({
                "path": path.display().to_string(),
                "actual_perms_octal": format!("{:o}", perms),
                "expected_perms_octal": "600",
            });
            if perms == 0o600 {
                return DetectorReport::green(self.name(), "master.key permissions are 0600");
            }
            DetectorReport {
                name: self.name().into(),
                level: HealthLevel::Red,
                message: format!(
                    "master.key has perms {:o} — should be 0600 (other users may read it)",
                    perms,
                ),
                value: Some(perms as f64),
                payload,
                auto_action_eligible: true,
                analytics: None,
            }
        }
        #[cfg(not(target_family = "unix"))]
        {
            let _ = path;
            DetectorReport::green(self.name(), "perms check skipped on non-unix platform")
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 10. auth.master_key_present
// ────────────────────────────────────────────────────────────────────────────

// The master key file must exist. Missing it means the skill secrets
// store can't decrypt anything — every secret read returns garbage.
// This is unrecoverable without a backup (the AES key is the only
// way to read existing ciphertext).
pub struct MasterKeyPresentDetector;

impl Detector for MasterKeyPresentDetector {
    fn name(&self) -> &'static str { "auth.master_key_present" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let path = ctx.data_dir.join("master.key");
        let payload = json!({ "path": path.display().to_string() });
        if path.exists() {
            return DetectorReport::green(self.name(), "master.key present");
        }
        DetectorReport {
            name: self.name().into(),
            level: HealthLevel::Red,
            message: "master.key is missing — skill secrets vault unreadable".into(),
            value: None,
            payload,
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║  (0.106.0) detectors                                              ║
// ║                                                                          ║
// ║ Five themes: A) process runtime, B) database integrity, C) auth attack   ║
// ║ signals, D) watchdog self-monitoring, E) agent worker runtime.           ║
// ╚══════════════════════════════════════════════════════════════════════════╝

// ── Theme A: process runtime ────────────────────────────────────────────────

// Resident set size in MB. Memory leak warning before the OOM killer
// gets involved. Y >1500, R >2500.
pub struct ProcessRssDetector;
impl Detector for ProcessRssDetector {
    fn name(&self) -> &'static str { "process.rss_mb" }
    fn run(&self, _ctx: &DetectorContext) -> DetectorReport {
        let snap = super::process::snapshot();
        let Some(rss_kb) = snap.rss_kb else {
            return err_yellow(self.name(), "could not read /proc/self/status");
        };
        let mb = rss_kb / 1024;
        let level = if mb > 2500 { HealthLevel::Red }
                    else if mb > 1500 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("RSS = {mb} MB"),
            value: Some(mb as f64),
            payload: json!({ "rss_mb": mb, "rss_kb": rss_kb }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Virtual size in MB. Tokio + axum + rusqlite + reqwest + every
// shared-lib mapping easily reach ~5 GB of address space on Linux
// without any actual memory pressure (mmap reserves, thread guard
// pages, lazy-allocated heaps). Thresholds tuned for "something has
// genuinely run away" rather than ordinary growth: Y >12 GB, R >24 GB.
pub struct ProcessVszDetector;
impl Detector for ProcessVszDetector {
    fn name(&self) -> &'static str { "process.vsz_mb" }
    fn run(&self, _ctx: &DetectorContext) -> DetectorReport {
        let snap = super::process::snapshot();
        let Some(vsz_kb) = snap.vsz_kb else {
            return err_yellow(self.name(), "could not read /proc/self/status");
        };
        let mb = vsz_kb / 1024;
        let level = if mb > 24_000 { HealthLevel::Red }
                    else if mb > 12_000 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("VSZ = {mb} MB"),
            value: Some(mb as f64),
            payload: json!({ "vsz_mb": mb }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// CPU% over the interval since the last snapshot. The first call after
// process start returns Green / informational because there's no
// prior reading to diff. Y >50% sustained, R >85%.
pub struct ProcessCpuDetector;
impl Detector for ProcessCpuDetector {
    fn name(&self) -> &'static str { "process.cpu_pct_5m" }
    fn run(&self, _ctx: &DetectorContext) -> DetectorReport {
        let snap = super::process::snapshot();
        let Some(cpu) = snap.cpu_pct else {
            return DetectorReport::green(self.name(), "no prior sample yet (first call since boot)");
        };
        // Multi-core systems can legitimately go above 1.0 — clamp the
        // displayed % to per-core terms by dividing by num_cpus before
        // comparison would change semantics; leave as raw fraction and
        // adjust thresholds. 0.85 ≈ "one core pinned" on a 1-core box.
        let pct = (cpu * 100.0).round();
        let level = if cpu >= 0.85 { HealthLevel::Red }
                    else if cpu >= 0.50 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("CPU = {pct}%"),
            value: Some(pct),
            payload: json!({ "cpu_fraction": cpu }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Open FDs vs the process's soft ulimit. The "FD leak that takes the
// server down at 3am" classic. Y >70% of soft, R >90%.
pub struct ProcessFdCountDetector;
impl Detector for ProcessFdCountDetector {
    fn name(&self) -> &'static str { "process.fd_count" }
    fn run(&self, _ctx: &DetectorContext) -> DetectorReport {
        let snap = super::process::snapshot();
        let Some(count) = snap.fd_count else {
            return err_yellow(self.name(), "could not enumerate /proc/self/fd");
        };
        let Some(soft) = snap.fd_soft_limit else {
            // Fall back to absolute thresholds if we can't read the limit.
            let level = if count > 4000 { HealthLevel::Red }
                        else if count > 1000 { HealthLevel::Yellow }
                        else { HealthLevel::Green };
            return DetectorReport {
                name: self.name().into(), level,
                message: format!("{count} open FDs (soft limit unknown)"),
                value: Some(count as f64),
                payload: json!({ "fd_count": count }),
                auto_action_eligible: false,
                analytics: None,
            };
        };
        let pct = (count as f64 / soft as f64).clamp(0.0, 1.0);
        let level = if pct >= 0.90 { HealthLevel::Red }
                    else if pct >= 0.70 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count}/{soft} open FDs ({:.0}%)", pct * 100.0),
            value: Some(count as f64),
            payload: json!({ "fd_count": count, "fd_soft_limit": soft, "pct": pct }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Thread count. Tokio runtimes spawn workers liberally but a >500
// count usually means a thread leak somewhere. Y >300, R >500.
pub struct ProcessThreadCountDetector;
impl Detector for ProcessThreadCountDetector {
    fn name(&self) -> &'static str { "process.thread_count" }
    fn run(&self, _ctx: &DetectorContext) -> DetectorReport {
        let snap = super::process::snapshot();
        let Some(n) = snap.thread_count else {
            return err_yellow(self.name(), "could not read /proc/self/status");
        };
        let level = if n > 500 { HealthLevel::Red }
                    else if n > 300 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{n} threads"),
            value: Some(n as f64),
            payload: json!({ "thread_count": n }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Wall-clock seconds since process start. Always Green — purely
// informational, surfaced for the LLM analyst's context window.
pub struct ProcessUptimeDetector;
impl Detector for ProcessUptimeDetector {
    fn name(&self) -> &'static str { "process.uptime_secs" }
    fn run(&self, _ctx: &DetectorContext) -> DetectorReport {
        let snap = super::process::snapshot();
        let secs = snap.uptime_secs.unwrap_or(0);
        DetectorReport {
            name: self.name().into(), level: HealthLevel::Green,
            message: format!("uptime {secs}s ({:.1}h)", secs as f64 / 3600.0),
            value: Some(secs as f64),
            payload: json!({ "uptime_secs": secs }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Restart count in last 24h. Crash-loop signal. Reads
// `<data_dir>/boot_history.json` (written by the gateway on each
// startup). Y ≥2, R ≥5.
pub struct ProcessRestartCountDetector;
impl Detector for ProcessRestartCountDetector {
    fn name(&self) -> &'static str { "process.restart_count_24h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let now = chrono::Utc::now().timestamp();
        let since = now - 24 * 60 * 60;
        let count = match super::boot::count_boots_since(&ctx.data_dir, since) {
            Ok(n)  => n,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let level = if count >= 5 { HealthLevel::Red }
                    else if count >= 2 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count} restart(s) in last 24h"),
            value: Some(count as f64),
            payload: json!({ "restart_count_24h": count }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ── Theme B: database integrity ──────────────────────────────────────────────

// Run `PRAGMA quick_check` on every DB. Reports the worst result
// across all of them. quick_check (vs full integrity_check) skips
// out-of-order index checks but catches the common corruption modes
// and is fast enough to run every hour. The slower full check
// belongs on a future weekly heartbeat.
pub struct DbIntegrityDetector;
impl Detector for DbIntegrityDetector {
    fn name(&self) -> &'static str { "db.integrity_check" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        use rusqlite::{Connection, OpenFlags};
        let mut bad: Vec<serde_json::Value> = Vec::new();
        let mut checked = 0usize;
        for entry in super::db_paths::all_dbs(&ctx.data_dir) {
            if !entry.path.exists() { continue }
            checked += 1;
            let conn = match Connection::open_with_flags(&entry.path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
                Ok(c)  => c,
                Err(e) => { bad.push(json!({ "name": entry.name, "error": e.to_string() })); continue; }
            };
            let result: rusqlite::Result<String> = conn.query_row(
                "PRAGMA quick_check", [], |r| r.get(0),
            );
            match result {
                Ok(s) if s.eq_ignore_ascii_case("ok") => {}
                Ok(s)  => bad.push(json!({ "name": entry.name, "result": s })),
                Err(e) => bad.push(json!({ "name": entry.name, "error": e.to_string() })),
            }
        }
        let payload = json!({ "checked": checked, "issues": bad });
        if bad.is_empty() {
            return DetectorReport::green(self.name(), format!("{checked} DB(s) pass quick_check"));
        }
        DetectorReport {
            name: self.name().into(), level: HealthLevel::Red,
            message: format!("{} DB(s) failed integrity check", bad.len()),
            value: Some(bad.len() as f64),
            payload,
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Largest WAL sidecar across all DBs. A growing WAL means
// checkpoints aren't happening — usually fixed by a long-lived
// reader holding the page snapshot, but sometimes a config issue.
// Y >50 MB, R >200 MB. Auto-action: wal_checkpoint(TRUNCATE).
pub struct DbWalSizeDetector;
impl Detector for DbWalSizeDetector {
    fn name(&self) -> &'static str { "db.wal_size_mb" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let mut max_mb: u64 = 0;
        let mut max_name: &'static str = "";
        let mut details: Vec<serde_json::Value> = Vec::new();
        for entry in super::db_paths::all_dbs(&ctx.data_dir) {
            let wal = entry.wal_path();
            let Ok(meta) = std::fs::metadata(&wal) else { continue };
            let mb = meta.len() / 1024 / 1024;
            details.push(json!({ "name": entry.name, "wal_mb": mb }));
            if mb > max_mb { max_mb = mb; max_name = entry.name; }
        }
        let payload = json!({ "worst_name": max_name, "worst_mb": max_mb, "all": details });
        let level = if max_mb > 200 { HealthLevel::Red }
                    else if max_mb > 50 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        let message = if max_mb == 0 {
            "no WAL sidecars present (nothing to report)".to_string()
        } else {
            format!("largest WAL = {max_mb} MB ({max_name})")
        };
        DetectorReport {
            name: self.name().into(), level, message,
            value: Some(max_mb as f64), payload,
            // Eligible at Yellow+ — wal_checkpoint(TRUNCATE) is safe
            // and idempotent. Action filters by `worst_name` to avoid
            // checkpointing healthy DBs.
            auto_action_eligible: !matches!(level, HealthLevel::Green),
            analytics: None,
        }
    }
}

// ── Theme C: auth attack signals ────────────────────────────────────────────

// Failed-login spike from a single IP in the last hour. Y >10, R >50.
// Auto-action eligible at Red — temp-bans the IP for 30 min.
pub struct FailedLoginsPerIpDetector;
impl Detector for FailedLoginsPerIpDetector {
    fn name(&self) -> &'static str { "auth.failed_logins_per_ip_1h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(db) = ctx.auth_db.as_ref() else {
            return err_yellow(self.name(), "auth db not wired");
        };
        let since = chrono::Utc::now().timestamp() - 3600;
        let (_total, top_count, top_ip) = match db.count_failed_logins_since(since) {
            Ok(t)  => t,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let level = if top_count > 50 { HealthLevel::Red }
                    else if top_count > 10 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        let payload = json!({ "top_ip": top_ip, "top_count": top_count });
        let message = match (&top_ip, top_count) {
            (_, 0) => "no failed logins in last hour".into(),
            (Some(ip), n) => format!("{n} failed login(s) from {ip} in last hour"),
            (None, n)     => format!("{n} failed login(s) (no IP recorded)"),
        };
        DetectorReport {
            name: self.name().into(), level, message,
            value: Some(top_count as f64), payload,
            auto_action_eligible: matches!(level, HealthLevel::Red),
            analytics: None,
        }
    }
}

// Total failed logins in the last hour, across all IPs. Catches
// distributed brute force attempts that single-IP banning won't help.
// Y >100, R >500. Notification-only.
pub struct FailedLoginsTotalDetector;
impl Detector for FailedLoginsTotalDetector {
    fn name(&self) -> &'static str { "auth.failed_logins_total_1h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(db) = ctx.auth_db.as_ref() else {
            return err_yellow(self.name(), "auth db not wired");
        };
        let since = chrono::Utc::now().timestamp() - 3600;
        let (total, _, _) = match db.count_failed_logins_since(since) {
            Ok(t)  => t,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let level = if total > 500 { HealthLevel::Red }
                    else if total > 100 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{total} failed login(s) total in last hour"),
            value: Some(total as f64),
            payload: json!({ "total": total }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Count of `WARN` lines from `mira::auth::middleware` in the last
// hour. A spike usually means the JWT signing key rotated without a
// matching client update, or someone's poking the API with bad
// tokens. Y >20, R >200.
pub struct JwtValidationFailuresDetector;
impl Detector for JwtValidationFailuresDetector {
    fn name(&self) -> &'static str { "auth.jwt_validation_failures_1h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(path) = ctx.log_path.as_ref() else {
            return err_yellow(self.name(), "log path not configured");
        };
        let count = count_log_pattern_since(
            path, &["mira::auth::middleware", "JWT", "jwt"], 3600,
        );
        let level = if count > 200 { HealthLevel::Red }
                    else if count > 20 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count} JWT-validation WARN line(s) in last hour"),
            value: Some(count as f64),
            payload: json!({ "count": count }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ── Theme D: watchdog self-monitoring ───────────────────────────────────────

// Watchdog incidents stuck in `analysis_status='queued'` or
// `'in_progress'` for >30 min. Means the analyze flow hung —
// usually because the LLM call crashed without updating the row.
// Auto-action: flip those rows to `failed` so the Analyze button
// works again.
pub struct WatchdogStuckAnalysisDetector;
impl Detector for WatchdogStuckAnalysisDetector {
    fn name(&self) -> &'static str { "watchdog.analysis_stuck_in_progress_30m" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(store) = ctx.automations.as_ref() else {
            return err_yellow(self.name(), "automations store not wired");
        };
        let cutoff = chrono::Utc::now().timestamp() - 30 * 60;
        let (count, ids) = match store.list_stuck_incident_analyses(cutoff) {
            Ok(v) => {
                let ids: Vec<String> = v.iter().map(|i| i.id.clone()).collect();
                (v.len(), ids)
            }
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let level = if count > 3 { HealthLevel::Red }
                    else if count > 0 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count} incident analysis(es) stuck >30min"),
            value: Some(count as f64),
            payload: json!({ "count": count, "ids": ids }),
            auto_action_eligible: count > 0,
            analytics: None,
        }
    }
}

// One fingerprint repeating heavily in the last 24h means dedup
// is working but the underlying error isn't getting fixed. Y >20,
// R >100. The user should add an `ignore_patterns` regex if they've
// decided the noise is acceptable; we surface it so they actually
// notice.
pub struct WatchdogSameFingerprintDetector;
impl Detector for WatchdogSameFingerprintDetector {
    fn name(&self) -> &'static str { "watchdog.same_fingerprint_count_24h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(store) = ctx.automations.as_ref() else {
            return err_yellow(self.name(), "automations store not wired");
        };
        let since = chrono::Utc::now().timestamp() - 24 * 3600;
        let top = match store.top_incident_fingerprint_since(since) {
            Ok(v)  => v,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let (fp, count) = top.unwrap_or_default();
        let level = if count > 100 { HealthLevel::Red }
                    else if count > 20 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        let message = if count == 0 {
            "no repeating watchdog fingerprints in last 24h".into()
        } else {
            format!("fingerprint `{fp}` fired {count}× in last 24h")
        };
        DetectorReport {
            name: self.name().into(), level, message,
            value: Some(count as f64),
            payload: json!({ "fingerprint": fp, "count": count }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Watchdog `last_scanned_at` lag — read from the on-disk state file.
// If >5 min stale (5× the default 60s tick), the watchdog heartbeat
// itself is stuck. Y >300s, R >1800s.
pub struct WatchdogLogLagDetector;
impl Detector for WatchdogLogLagDetector {
    fn name(&self) -> &'static str { "watchdog.last_log_offset_lag_secs" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        // A deliberately-disabled watchdog is paused (or never seeded), not
        // running — don't flag its stale state file Red. Only an *active*
        // schedule that isn't scanning is a genuine stall. (When the watchdog
        // is disabled in config, `seed_watchdog_schedule` pauses this row.)
        if let Some(store) = ctx.automations.as_ref() {
            match store.system_schedule_status_by_name("heartbeat.watchdog") {
                Ok(Some(s)) if s != "active" =>
                    return DetectorReport::green(
                        self.name(),
                        format!("watchdog schedule {s} (disabled/not running)"),
                    ),
                Ok(None) =>
                    return DetectorReport::green(
                        self.name(), "watchdog not scheduled (disabled)",
                    ),
                _ => {} // active (or store error) — fall through to the lag check
            }
        }
        let path = ctx.data_dir.join("watchdog_state.json");
        if !path.exists() {
            return DetectorReport::green(
                self.name(),
                "watchdog state file absent (watchdog likely disabled)",
            );
        }
        // Minimal struct — we only need one field. Dropping anything
        // else lets the format evolve in watchdog.rs without
        // recompiling here.
        #[derive(serde::Deserialize)]
        struct Fragment { last_scanned_at: i64 }
        let bytes = match std::fs::read(&path) {
            Ok(b)  => b,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let frag: Fragment = match serde_json::from_slice(&bytes) {
            Ok(f)  => f,
            Err(e) => return err_yellow(self.name(), format!("parse state file: {e}")),
        };
        let now = chrono::Utc::now().timestamp();
        let lag = now - frag.last_scanned_at;
        let level = if lag > 1800 { HealthLevel::Red }
                    else if lag > 300 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("watchdog last scan {lag}s ago"),
            value: Some(lag as f64),
            payload: json!({ "lag_secs": lag, "last_scanned_at": frag.last_scanned_at }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Failure rate of the seeded `watchdog.alert delivery` subscription
// in the last hour. >50% failure means the admin's channel is
// broken (Signal token expired, web channel offline, etc.) — alerts
// being filed but never delivered.
pub struct WatchdogDispatchFailureRateDetector;
impl Detector for WatchdogDispatchFailureRateDetector {
    fn name(&self) -> &'static str { "watchdog.alert_dispatch_failure_rate_1h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(store) = ctx.automations.as_ref() else {
            return err_yellow(self.name(), "automations store not wired");
        };
        let since = chrono::Utc::now().timestamp() - 3600;
        let (total, failures) = match store.count_runs_for_event_sub_named_since(
            "watchdog.alert delivery", since,
        ) {
            Ok(t)  => t,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        if total == 0 {
            return DetectorReport::green(
                self.name(), "no watchdog dispatches in last hour",
            );
        }
        let rate = failures as f64 / total as f64;
        let level = if rate >= 0.5 { HealthLevel::Red }
                    else if rate >= 0.2 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!(
                "{}/{} watchdog dispatches failed last hour ({:.0}%)",
                failures, total, rate * 100.0,
            ),
            value: Some(rate),
            payload: json!({ "total": total, "failures": failures, "rate": rate }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ── Theme E: agent worker runtime ───────────────────────────────────────────

// Live count of agents in the registry. Y >10, R >25. Concurrency
// creep — usually means workers aren't being deregistered cleanly.
pub struct AgentWorkersRunningDetector;
impl Detector for AgentWorkersRunningDetector {
    fn name(&self) -> &'static str { "agent.workers_running_count" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(reg) = ctx.agent_registry.as_ref() else {
            return DetectorReport::green(self.name(), "agent registry not wired (no workers possible)");
        };
        let n = reg.len();
        let level = if n > 25 { HealthLevel::Red }
                    else if n > 10 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{n} agent(s) currently in registry"),
            value: Some(n as f64),
            payload: json!({ "count": n }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// `agent_budget_exceeded` events in the last hour. Supervisor already
// kills the offender; we surface the trend so cost spikes get noticed.
// Y ≥1, R ≥5.
pub struct AgentOverBudgetDetector;
impl Detector for AgentOverBudgetDetector {
    fn name(&self) -> &'static str { "agent.workers_over_budget_count" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(audit) = ctx.audit_store.as_ref() else {
            return err_yellow(self.name(), "agent audit not wired");
        };
        let since_ms = (chrono::Utc::now().timestamp() - 3600) * 1000;
        let filter = crate::agent::audit::AuditFilter {
            kinds:    vec!["agent_budget_exceeded"],
            since_ms: Some(since_ms),
            limit:    Some(100),
            ..Default::default()
        };
        let rows = match audit.query(&filter) {
            Ok(v)  => v,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let count = rows.len();
        let level = if count >= 5 { HealthLevel::Red }
                    else if count >= 1 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count} agent_budget_exceeded event(s) in last hour"),
            value: Some(count as f64),
            payload: json!({ "count": count }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Tool-loop hit max_rounds — the model thrashed and the loop bailed
// out. The recent LMStudio incident produced exactly this signal in
// the log. Last 24h count, Y ≥3, R ≥10.
pub struct AgentMaxRoundsHitDetector;
impl Detector for AgentMaxRoundsHitDetector {
    fn name(&self) -> &'static str { "agent.tool_loop_max_rounds_hit_24h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(path) = ctx.log_path.as_ref() else {
            return err_yellow(self.name(), "log path not configured");
        };
        let count = count_log_pattern_since(path, &["Tool loop hit max_rounds"], 24 * 3600);
        let level = if count >= 10 { HealthLevel::Red }
                    else if count >= 3 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count} max_rounds hits in last 24h"),
            value: Some(count as f64),
            payload: json!({ "count": count }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Duplicate tool call blocked count. Same incident as max_rounds
// thrashy model behavior — but the hot-path defense surfaces it
// per-call. Last 24h, Y ≥10, R ≥50.
pub struct AgentDuplicateToolCallDetector;
impl Detector for AgentDuplicateToolCallDetector {
    fn name(&self) -> &'static str { "agent.duplicate_tool_call_blocks_24h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(path) = ctx.log_path.as_ref() else {
            return err_yellow(self.name(), "log path not configured");
        };
        let count = count_log_pattern_since(path, &["duplicate", "call blocked"], 24 * 3600);
        let level = if count >= 50 { HealthLevel::Red }
                    else if count >= 10 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count} duplicate-tool-call blocks in last 24h"),
            value: Some(count as f64),
            payload: json!({ "count": count }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ── Log scan helper ─────────────────────────────────────────────────────────

// Read up to the last MAX_TAIL_BYTES of `path`, count lines that
// contain ALL of `needles` (substring match, not regex) AND whose
// timestamp is within `window_secs`. Returns 0 on any read failure
// (silent — log scan is best-effort).
// // Bounded scan: only the last 16 MB of log are read. For an hourly
// tick that's easily enough capacity unless something is logging
// enormously, in which case the watchdog should be noisier.
const MAX_TAIL_BYTES: u64 = 16 * 1024 * 1024;

fn count_log_pattern_since(path: &Path, needles: &[&str], window_secs: i64) -> usize {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return 0 };
    let len = match f.metadata() { Ok(m) => m.len(), Err(_) => return 0 };
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() { return 0 }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() { return 0 }
    let cutoff_secs = chrono::Utc::now().timestamp() - window_secs;
    let mut hits = 0usize;
    for line in buf.lines() {
        if !needles.iter().all(|n| line.contains(n)) { continue }
        // Tracing format: `2026-05-09T12:34:56.789Z  WARN...`. Parse
        // the leading ISO-8601 timestamp; skip lines we can't parse
        // (partial first line after seek into mid-line, etc.).
        let Some(ts_str) = line.split_whitespace().next() else { continue };
        let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str) else { continue };
        if dt.timestamp() >= cutoff_secs {
            hits += 1;
        }
    }
    hits
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║  (0.108.0) detectors                                             ║
// ║                                                                          ║
// ║ Five themes: F) LLM providers, G) channels, H) memory, I) skills, J)     ║
// ║ cross-table consistency. Plus one DB-contention detector tucked in.      ║
// ╚══════════════════════════════════════════════════════════════════════════╝

// ── F. LLM providers ────────────────────────────────────────────────────────

// Aggregate WARN+ rate from any `mira::providers::*` module in the
// last hour. Single detector across all providers — per-provider
// breakdowns can wait until the dashboard supports drilldown.
// Y >0.1 (rough), R >0.3.
pub struct LlmErrorRateDetector;
impl Detector for LlmErrorRateDetector {
    fn name(&self) -> &'static str { "llm.error_rate_1h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(path) = ctx.log_path.as_ref() else {
            return err_yellow(self.name(), "log path not configured");
        };
        // Count WARN+ERROR lines + total provider lines as the
        // denominator. "mira::providers::" is the universal prefix
        // for every provider module.
        let warn_or_error = count_log_pattern_since_any(
            path, &["mira::providers::"], &[" WARN ", " ERROR "], 3600,
        );
        let total = count_log_pattern_since(path, &["mira::providers::"], 3600);
        if total == 0 {
            return DetectorReport::green(self.name(), "no provider activity in last hour");
        }
        let rate = warn_or_error as f64 / total as f64;
        let level = if rate >= 0.3 { HealthLevel::Red }
                    else if rate >= 0.1 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!(
                "{}/{} provider log lines were WARN+ in last hour ({:.0}%)",
                warn_or_error, total, rate * 100.0,
            ),
            value: Some(rate),
            payload: json!({ "warn_or_error": warn_or_error, "total": total, "rate": rate }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// 0.110.0 — sum from the `llm_charges` ledger over the last 24h.
// Replaces the running-burn snapshot (which only saw currently-running
// agents). Falls back to the running-budget proxy when the ledger has
// no entries yet (fresh install). Y >$10/day, R >$50/day.
pub struct LlmCostBurn24hDetector;
impl Detector for LlmCostBurn24hDetector {
    fn name(&self) -> &'static str { "llm.cost_burn_24h_usd" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        // Prefer ledger sum when health.db is available — but the
        // detector context doesn't carry HealthStore directly. Use the
        // data_dir to open the ledger DB read-only.
        use rusqlite::{Connection, OpenFlags};
        let path = ctx.data_dir.join("health.db");
        let total_24h: Option<f64> = Connection::open_with_flags(
            &path, OpenFlags::SQLITE_OPEN_READ_ONLY,
        ).ok().and_then(|conn| {
            let since = chrono::Utc::now().timestamp() - 24 * 3600;
            conn.query_row(
                "SELECT COALESCE(SUM(usd), 0.0) FROM llm_charges WHERE charged_at >= ?1",
                rusqlite::params![since], |r| r.get::<_, f64>(0),
            ).ok()
        });
        let total = total_24h.unwrap_or(0.0);
        let level = if total >= 50.0 { HealthLevel::Red }
                    else if total >= 10.0 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("LLM spend last 24h: ${total:.2}"),
            value: Some(total),
            payload: json!({"usd_24h": total, "ledger_present": total_24h.is_some()}),
            auto_action_eligible: false, analytics: None,
        }
    }
}

// Sum of `spent_usd` across every agent currently in the registry.
// Captures concurrent-burn spikes. Y >$5 (peak), R >$25.
pub struct LlmCurrentCostBurnDetector;
impl Detector for LlmCurrentCostBurnDetector {
    fn name(&self) -> &'static str { "llm.current_cost_burn_usd" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(reg) = ctx.agent_registry.as_ref() else {
            return DetectorReport::green(self.name(), "agent registry not wired");
        };
        let agents = reg.list();
        let mut total_usd = 0.0_f64;
        let mut by_agent: Vec<serde_json::Value> = Vec::new();
        for handle in &agents {
            if let Ok(a) = handle.read() {
                if a.budget.spent_usd > 0.0 {
                    total_usd += a.budget.spent_usd;
                    by_agent.push(json!({
                        "agent_id": a.id.0.to_string(),
                        "spent_usd": a.budget.spent_usd,
                    }));
                }
            }
        }
        let level = if total_usd >= 25.0 { HealthLevel::Red }
                    else if total_usd >= 5.0 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("running agents have ${:.2} accumulated spend", total_usd),
            value: Some(total_usd),
            payload: json!({
                "total_usd": total_usd,
                "running_agent_count": agents.len(),
                "agents": by_agent,
            }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Count of "embed*failed" log lines in the last hour. Catches silent
// embed failures the slice-1 ONNX probe misses — e.g. an in-flight
// LM Studio outage. Y >5, R >50.
pub struct LlmEmbeddingFailuresDetector;
impl Detector for LlmEmbeddingFailuresDetector {
    fn name(&self) -> &'static str { "llm.embedding_failures_1h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(path) = ctx.log_path.as_ref() else {
            return err_yellow(self.name(), "log path not configured");
        };
        // Match either "embed* failed" patterns. The actual log lines
        // typically come from `mira::memory::embedding`. Match across
        // the module + a "fail" word substring.
        let count = count_log_pattern_since_any(
            path, &["mira::memory::"], &["embed", "fail"], 3600,
        );
        let level = if count >= 50 { HealthLevel::Red }
                    else if count >= 5 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count} embed-failure log line(s) in last hour"),
            value: Some(count as f64),
            payload: json!({ "count": count }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ── G. Channels ─────────────────────────────────────────────────────────────

// Per-account signal-cli daemon liveness via the ChannelManager's own
// `is_running` probe (a `try_wait` on the child). Any account whose
// daemon is dead → Red. Auto-action: restart the dead daemon.
pub struct SignalDaemonAliveDetector;
impl Detector for SignalDaemonAliveDetector {
    fn name(&self) -> &'static str { "channel.signal.daemon_alive" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(mgr) = ctx.channel_manager.as_ref() else {
            return DetectorReport::green(self.name(), "channel manager not wired");
        };
        // Brief blocking write lock — `is_running` requires &mut. The
        // tokio::sync::RwLock blocking_write is fine here because the
        // health audit runs from a tokio::spawn'd task, not the main
        // poll loop. Heartbeat dispatch already accepts blocking.
        let mut guard = match mgr.try_write() {
            Ok(g)  => g,
            Err(_) => return DetectorReport::green(
                self.name(), "channel manager busy — skipping this tick",
            ),
        };
        let snapshot = guard.signal_account_aliveness();
        drop(guard);
        if snapshot.is_empty() {
            return DetectorReport::green(self.name(), "no signal accounts configured");
        }
        let dead: Vec<&str> = snapshot.iter()
            .filter_map(|(id, alive)| if !*alive { Some(id.as_str()) } else { None })
            .collect();
        let payload = json!({ "accounts": snapshot, "dead": dead });
        if dead.is_empty() {
            return DetectorReport {
                name: self.name().into(), level: HealthLevel::Green,
                message: format!("{} signal account(s) alive", snapshot.len()),
                value: Some(snapshot.len() as f64),
                payload, auto_action_eligible: false,
                analytics: None,
            };
        }
        DetectorReport {
            name: self.name().into(), level: HealthLevel::Red,
            message: format!("{}/{} signal daemon(s) dead", dead.len(), snapshot.len()),
            value: Some(dead.len() as f64),
            payload,
            auto_action_eligible: true,
            analytics: None,
        }
    }
}

// 5xx rate from the request-log middleware. The format is
// `← 5XX METHOD /path  [Nms]`. Y >0.05, R >0.2.
pub struct WebFiveXxRateDetector;
impl Detector for WebFiveXxRateDetector {
    fn name(&self) -> &'static str { "channel.web.5xx_rate_1h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(path) = ctx.log_path.as_ref() else {
            return err_yellow(self.name(), "log path not configured");
        };
        // Count any line containing "← " (response log marker). The
        // 5xx subset matches "← 5". Crude but works against the
        // actual format from src/security/log.rs.
        let total = count_log_pattern_since(path, &["mira::security::log:", "←"], 3600);
        let fives = count_log_pattern_since(path, &["mira::security::log:", "← 5"], 3600);
        if total == 0 {
            return DetectorReport::green(self.name(), "no HTTP traffic logged in last hour");
        }
        let rate = fives as f64 / total as f64;
        let level = if rate >= 0.2 { HealthLevel::Red }
                    else if rate >= 0.05 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{fives}/{total} HTTP responses were 5xx ({:.1}%)", rate * 100.0),
            value: Some(rate),
            payload: json!({ "total": total, "five_xx": fives, "rate": rate }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ── H. Memory subsystem ─────────────────────────────────────────────────────

// Hours since `heartbeat.conversation_rollup` last completed
// successfully. The schedule fires daily at 03:15; >36h means at
// least one day was skipped. Y >36h, R >72h.
pub struct MemoryRollupLagDetector;
impl Detector for MemoryRollupLagDetector {
    fn name(&self) -> &'static str { "memory.rollup_lag_hours" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(store) = ctx.automations.as_ref() else {
            return err_yellow(self.name(), "automations store not wired");
        };
        let last = match store.last_success_at_for_schedule_named("heartbeat.conversation_rollup") {
            Ok(v)  => v,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let now = chrono::Utc::now().timestamp();
        let (level, message, value, payload) = match last {
            None => (
                HealthLevel::Yellow,
                "rollup heartbeat has never run successfully".to_string(),
                None, json!({"last_success_at": null}),
            ),
            Some(ts) => {
                let lag_hours = (now - ts) as f64 / 3600.0;
                let level = if lag_hours > 72.0 { HealthLevel::Red }
                            else if lag_hours > 36.0 { HealthLevel::Yellow }
                            else { HealthLevel::Green };
                (level,
                 format!("rollup last succeeded {:.1}h ago", lag_hours),
                 Some(lag_hours),
                 json!({"last_success_at": ts, "lag_hours": lag_hours}))
            }
        };
        DetectorReport {
            name: self.name().into(), level, message, value, payload,
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ── I. Skills ───────────────────────────────────────────────────────────────

// Bundled-skill version drift — when the binary embeds a newer
// version than what's on disk. Trips after a binary upgrade if the
// boot-time `extract_or_refresh` failed (rare, usually a permissions
// issue). Y on any drift. Auto-action: re-run extract_or_refresh.
pub struct BundledSkillDriftDetector;
impl Detector for BundledSkillDriftDetector {
    fn name(&self) -> &'static str { "skills.bundled_drift" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let skills_dir = crate::skills::default_skills_dir(&ctx.data_dir);
        let drifts = crate::skills::bundled::check_drift(&skills_dir);
        let drifted: Vec<&crate::skills::bundled::BundledDrift> =
            drifts.iter().filter(|d| d.drift).collect();
        let count = drifted.len();
        let payload = json!({
            "drifted_count": count,
            "drifted": drifted,
            "all_count": drifts.len(),
        });
        if count == 0 {
            return DetectorReport::green(self.name(), "all bundled skills up-to-date on disk");
        }
        DetectorReport {
            name: self.name().into(), level: HealthLevel::Yellow,
            message: format!("{count} bundled skill(s) older on disk than the binary"),
            value: Some(count as f64),
            payload,
            auto_action_eligible: true,
            analytics: None,
        }
    }
}

// Skill-secret rows for skills that aren't installed. Cleanup target
// old secrets shouldn't accumulate after an uninstall (the uninstall
// flow doesn't sweep them today). Y >0. Auto-action: purge orphans.
pub struct DanglingSecretsDetector;
impl Detector for DanglingSecretsDetector {
    fn name(&self) -> &'static str { "skills.dangling_secrets_count" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(secrets) = ctx.secrets_store.as_ref() else {
            return DetectorReport::green(self.name(), "secrets store not wired");
        };
        let secret_skill_ids = match secrets.list_distinct_skill_ids() {
            Ok(v)  => v,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        // Build the installed-skill set via the loader (cheap, just
        // reads manifest tomls — same call the broken_manifest detector
        // uses).
        let skills_dir = crate::skills::default_skills_dir(&ctx.data_dir);
        let registry = crate::skills::loader::load_dir(&skills_dir, &ctx.mira_version);
        let installed: std::collections::HashSet<String> = registry.iter()
            .map(|s| s.manifest.skill.id.clone())
            .collect();
        let dangling: Vec<&String> = secret_skill_ids.iter()
            .filter(|id| !installed.contains(*id))
            .collect();
        let count = dangling.len();
        let payload = json!({
            "count": count,
            "dangling": dangling,
            "installed_count": installed.len(),
        });
        match count {
            0 => DetectorReport::green(self.name(), "no orphaned skill secrets"),
            _ => DetectorReport {
                name: self.name().into(), level: HealthLevel::Yellow,
                message: format!("{count} skill(s) have secrets but aren't installed"),
                value: Some(count as f64),
                payload,
                auto_action_eligible: true,
                analytics: None,
            },
        }
    }
}

// ── J. Cross-table consistency ──────────────────────────────────────────────

// Automations (schedules / event_subscriptions / webhooks) whose
// user_id refers to a deleted user. Happens when a user is removed
// while still owning automations — none of those tables are CASCADE-
// deleted from auth.db. Y >0. Auto-action: delete orphan rows.
pub struct AutomationsForDeletedUsersDetector;
impl Detector for AutomationsForDeletedUsersDetector {
    fn name(&self) -> &'static str { "consistency.automations_for_deleted_users" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(autos) = ctx.automations.as_ref() else {
            return err_yellow(self.name(), "automations store not wired");
        };
        let Some(auth) = ctx.auth_db.as_ref() else {
            return err_yellow(self.name(), "auth db not wired");
        };
        let referenced = match autos.distinct_user_ids_referenced() {
            Ok(v)  => v,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let users = match auth.list_users() {
            Ok(u)  => u,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let valid: std::collections::HashSet<String> = users.iter().map(|u| u.id.clone()).collect();
        let mut orphans: Vec<serde_json::Value> = Vec::new();
        let mut total_rows = 0usize;
        for uid in &referenced {
            if !valid.contains(uid) {
                let row_count = autos.count_automations_for_user(uid).unwrap_or(0);
                total_rows += row_count;
                orphans.push(json!({"user_id": uid, "rows": row_count}));
            }
        }
        let payload = json!({ "orphan_user_count": orphans.len(), "total_rows": total_rows, "orphans": orphans });
        let level = if total_rows >= 100 { HealthLevel::Red }
                    else if !orphans.is_empty() { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        let message = if orphans.is_empty() {
            "no automations reference deleted users".into()
        } else {
            format!("{} automation row(s) owned by {} deleted user(s)", total_rows, orphans.len())
        };
        DetectorReport {
            name: self.name().into(), level, message,
            value: Some(orphans.len() as f64),
            payload,
            auto_action_eligible: !orphans.is_empty(),
            analytics: None,
        }
    }
}

// ── Bonus: DB contention ────────────────────────────────────────────────────

// Count of "database is locked" log lines in the last hour.
// SQLite contention is silent until it isn't — this surfaces it
// before the user sees a 500. Y ≥3, R ≥20.
pub struct DbLockedInLogDetector;
impl Detector for DbLockedInLogDetector {
    fn name(&self) -> &'static str { "db.locked_in_log_1h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(path) = ctx.log_path.as_ref() else {
            return err_yellow(self.name(), "log path not configured");
        };
        let count = count_log_pattern_since(path, &["database is locked"], 3600);
        let level = if count >= 20 { HealthLevel::Red }
                    else if count >= 3 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count} 'database is locked' log line(s) in last hour"),
            value: Some(count as f64),
            payload: json!({"count": count}),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║  (0.109.0) detectors                                              ║
// ╚══════════════════════════════════════════════════════════════════════════╝

// Provider activity in the last 2h that's *all* WARN+ — i.e. lots of
// errors but no INFO success lines. Catches silent provider outages
// the slice-3b error-rate detector misses (because that one needs a
// non-zero ratio, which doesn't fire when total INFO collapses to 0).
// Y when warn+ ≥5 and successes = 0; R when warn+ ≥20 and successes = 0.
pub struct LlmNoSuccessfulCallDetector;
impl Detector for LlmNoSuccessfulCallDetector {
    fn name(&self) -> &'static str { "llm.no_successful_call_2h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(path) = ctx.log_path.as_ref() else {
            return err_yellow(self.name(), "log path not configured");
        };
        // INFO lines from a provider mean the request reached the
        // provider (the providers log INFO on completion). Absence
        // means either total silence (Green) or all-failure (Red).
        let info_lines = count_log_pattern_since_any(
            path, &["mira::providers::"], &[" INFO "], 2 * 3600,
        );
        let warn_lines = count_log_pattern_since_any(
            path, &["mira::providers::"], &[" WARN ", " ERROR "], 2 * 3600,
        );
        let payload = json!({
            "info_lines": info_lines, "warn_lines": warn_lines,
        });
        let level = if info_lines == 0 && warn_lines >= 20 { HealthLevel::Red }
                    else if info_lines == 0 && warn_lines >= 5 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        let message = match level {
            HealthLevel::Green if warn_lines == 0 && info_lines == 0
                => "no provider traffic in last 2h".into(),
            HealthLevel::Green
                => format!("provider traffic healthy ({info_lines} INFO, {warn_lines} WARN+)"),
            _   => format!("provider traffic all-failure: 0 INFO, {warn_lines} WARN+ in last 2h"),
        };
        DetectorReport {
            name: self.name().into(), level, message,
            value: Some(warn_lines as f64),
            payload, auto_action_eligible: false,
            analytics: None,
        }
    }
}

// Automation runs that started but have no `finished_at` and aren't
// in `running` state — usually means the worker process died
// mid-call. The boot-time orphan sweep cleans these up at restart;
// this detector flags ones accumulated during a long uptime. Y ≥1, R ≥5.
pub struct AutomationsRunsWithNoOutcomeDetector;
impl Detector for AutomationsRunsWithNoOutcomeDetector {
    fn name(&self) -> &'static str { "automations.runs_with_no_outcome_1h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(store) = ctx.automations.as_ref() else {
            return err_yellow(self.name(), "automations store not wired");
        };
        let now = chrono::Utc::now().timestamp();
        // Started in the last hour but never finished. Restrict the
        // window so a single boot-time sweep doesn't artificially
        // inflate the count.
        let count = match store.count_runs_unfinished_in_window(now - 3600, now - 30) {
            Ok(n)  => n,
            Err(e) => return err_yellow(self.name(), e.to_string()),
        };
        let level = if count >= 5 { HealthLevel::Red }
                    else if count >= 1 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        DetectorReport {
            name: self.name().into(), level,
            message: format!("{count} automation run(s) started but never finished"),
            value: Some(count as f64),
            payload: json!({"count": count, "window_secs": 3600}),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

// ── Log scan helper extension ───────────────────────────────────────────────

// Like [`count_log_pattern_since`] but matches lines that contain ALL
// of `must_have` AND ANY ONE OF `any_of`. Used by the LLM/embedding
// detectors which need a module prefix + a severity word.
fn count_log_pattern_since_any(
    path: &Path, must_have: &[&str], any_of: &[&str], window_secs: i64,
) -> usize {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return 0 };
    let len = match f.metadata() { Ok(m) => m.len(), Err(_) => return 0 };
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() { return 0 }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() { return 0 }
    let cutoff = chrono::Utc::now().timestamp() - window_secs;
    let mut hits = 0usize;
    for line in buf.lines() {
        if !must_have.iter().all(|n| line.contains(n)) { continue }
        if !any_of.is_empty() && !any_of.iter().any(|n| line.contains(n)) { continue }
        let Some(ts_str) = line.split_whitespace().next() else { continue };
        let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str) else { continue };
        if dt.timestamp() >= cutoff { hits += 1; }
    }
    hits
}

// ── Registry ────────────────────────────────────────────────────────────────

// Original slice-1 detector set, kept as a public constant so older
// call sites and tests have a stable reference. New code should
// prefer [`default_registry`] which is the full slice-1 + slice-2 set.
// `subsystem.degraded` — surfaces silent fallbacks (TTS → Piper, embedding
// server → internal, reasoning provider failed to build, …) from the live
// [`DegradationTracker`]. Yellow while any subsystem is on a degraded path.
pub struct SubsystemDegradationDetector;

impl Detector for SubsystemDegradationDetector {
    fn name(&self) -> &'static str { "subsystem.degraded" }

    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(tracker) = ctx.degradations.as_ref() else {
            return DetectorReport::green(self.name(), "subsystem-fallback tracker not wired");
        };
        let active = tracker.active();
        if active.is_empty() {
            return DetectorReport::green(self.name(), "all subsystems on their primary path");
        }
        let summary = active
            .iter()
            .map(|d| format!("{} → {}", d.label, d.to))
            .collect::<Vec<_>>()
            .join(", ");
        DetectorReport {
            name: self.name().into(),
            level: HealthLevel::Yellow,
            message: format!("{} subsystem(s) degraded: {}", active.len(), summary),
            value: Some(active.len() as f64),
            payload: json!({ "count": active.len(), "degradations": active }),
            auto_action_eligible: false,
            analytics: None,
        }
    }
}

pub fn slice_1_registry() -> Vec<Box<dyn Detector>> {
    vec![
        Box::new(SubsystemDegradationDetector),
        Box::new(StrandedCompletionSubsDetector),
        Box::new(SchedulerTickLagDetector),
        Box::new(AutomationsFailureRateDetector),
        Box::new(ScratchDirsOrphanedDetector),
        Box::new(EmbeddingProviderReachableDetector),
        Box::new(BrokenSkillManifestsDetector),
        Box::new(DiskFreeDetector),
        Box::new(AuditChainIntegrityDetector),
        Box::new(MasterKeyPermsDetector),
        Box::new(MasterKeyPresentDetector),
    ]
}

// Slice-2 additions only. Combined with slice 1 by [`default_registry`].
pub fn slice_2_registry() -> Vec<Box<dyn Detector>> {
    vec![
        // A: process runtime
        Box::new(ProcessRssDetector),
        Box::new(ProcessVszDetector),
        Box::new(ProcessCpuDetector),
        Box::new(ProcessFdCountDetector),
        Box::new(ProcessThreadCountDetector),
        Box::new(ProcessUptimeDetector),
        Box::new(ProcessRestartCountDetector),
        // B: database integrity
        Box::new(DbIntegrityDetector),
        Box::new(DbWalSizeDetector),
        // C: auth attack
        Box::new(FailedLoginsPerIpDetector),
        Box::new(FailedLoginsTotalDetector),
        Box::new(JwtValidationFailuresDetector),
        // D: watchdog self-monitoring
        Box::new(WatchdogStuckAnalysisDetector),
        Box::new(WatchdogSameFingerprintDetector),
        Box::new(WatchdogLogLagDetector),
        Box::new(WatchdogDispatchFailureRateDetector),
        // E: agent worker runtime
        Box::new(AgentWorkersRunningDetector),
        Box::new(AgentOverBudgetDetector),
        Box::new(AgentMaxRoundsHitDetector),
        Box::new(AgentDuplicateToolCallDetector),
    ]
}

// Slice-3b additions only.
pub fn slice_3b_registry() -> Vec<Box<dyn Detector>> {
    vec![
        // F: LLM providers
        Box::new(LlmErrorRateDetector),
        Box::new(LlmCurrentCostBurnDetector),
        Box::new(LlmEmbeddingFailuresDetector),
        // G: channels
        Box::new(SignalDaemonAliveDetector),
        Box::new(WebFiveXxRateDetector),
        // H: memory
        Box::new(MemoryRollupLagDetector),
        // I: skills
        Box::new(BundledSkillDriftDetector),
        Box::new(DanglingSecretsDetector),
        // J: cross-table consistency
        Box::new(AutomationsForDeletedUsersDetector),
        // Bonus: DB contention
        Box::new(DbLockedInLogDetector),
    ]
}

// 0.109.0 — slice 4 detectors that complete the deferred set.
pub fn slice_4_registry() -> Vec<Box<dyn Detector>> {
    vec![
        Box::new(LlmNoSuccessfulCallDetector),
        Box::new(AutomationsRunsWithNoOutcomeDetector),
    ]
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║ 2 (0.110.0) — channel reachability detectors                    ║
// ╚══════════════════════════════════════════════════════════════════════════╝

// Telegram bot reachability via the lightweight `getMe` endpoint.
// Caches the last good probe for 30 min so the hourly audit doesn't
// hammer Telegram's API. R when an enabled bot's last probe was an
// error, Y when no probe yet but enabled accounts exist.
pub struct TelegramReachableDetector;

use std::sync::Mutex as StdMutex;

// (account_id, last_probe_unix, last_ok). One entry per enabled
// telegram bot. Never blocks the detector — when locked, we just
// return the previous Green-or-cached state.
static TELEGRAM_PROBE_CACHE: StdMutex<Vec<(String, i64, bool, Option<String>)>> = StdMutex::new(Vec::new());
const TELEGRAM_PROBE_TTL_SECS: i64 = 30 * 60;

impl Detector for TelegramReachableDetector {
    fn name(&self) -> &'static str { "channel.telegram.reachable" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(accounts) = ctx.channel_accounts.as_ref() else {
            return DetectorReport::green(self.name(), "channel accounts store not wired");
        };
        let enabled = match accounts.list_enabled() {
            Ok(rows) => rows,
            Err(e)   => return err_yellow(self.name(), e.to_string()),
        };
        let telegram: Vec<&crate::channel_accounts::ChannelAccount> = enabled.iter()
            .filter(|a| matches!(a.channel, crate::channel_accounts::ChannelKind::Telegram))
            .collect();
        if telegram.is_empty() {
            return DetectorReport::green(self.name(), "no telegram accounts configured");
        }

        let now = chrono::Utc::now().timestamp();
        let mut by_id: std::collections::HashMap<String, (i64, bool, Option<String>)> =
            TELEGRAM_PROBE_CACHE.lock().ok().map(|guard| {
                guard.iter().map(|(id, ts, ok, err)| (id.clone(), (*ts, *ok, err.clone()))).collect()
            }).unwrap_or_default();

        // Re-probe stale or missing entries. Each probe is short
        // (timeout 8s) and runs sequentially — fine for the typical
        // count of one telegram account.
        for acct in &telegram {
            let needs_refresh = by_id.get(&acct.id)
                .map(|(ts, _, _)| now - ts > TELEGRAM_PROBE_TTL_SECS)
                .unwrap_or(true);
            if needs_refresh {
                let token = parse_telegram_token(&acct.config_json);
                let (ok, err) = match token {
                    Some(t) => probe_telegram_blocking(t),
                    None    => (false, Some("missing bot_token in config_json".into())),
                };
                by_id.insert(acct.id.clone(), (now, ok, err));
            }
        }
        // Persist back to the cache.
        if let Ok(mut guard) = TELEGRAM_PROBE_CACHE.lock() {
            *guard = by_id.iter()
                .map(|(id, (ts, ok, err))| (id.clone(), *ts, *ok, err.clone()))
                .collect();
        }

        // Aggregate: any unreachable → Red, all OK → Green.
        let mut unreachable: Vec<serde_json::Value> = Vec::new();
        for acct in &telegram {
            if let Some((_, ok, err)) = by_id.get(&acct.id) {
                if !*ok {
                    unreachable.push(json!({
                        "account_id": acct.id,
                        "label": acct.account_label,
                        "error": err,
                    }));
                }
            }
        }
        let payload = json!({
            "checked": telegram.len(), "unreachable": unreachable,
        });
        if unreachable.is_empty() {
            return DetectorReport {
                name: self.name().into(), level: HealthLevel::Green,
                message: format!("{} telegram bot(s) reachable", telegram.len()),
                value: Some(telegram.len() as f64),
                payload, auto_action_eligible: false, analytics: None,
            };
        }
        DetectorReport {
            name: self.name().into(), level: HealthLevel::Red,
            message: format!("{}/{} telegram bot(s) unreachable", unreachable.len(), telegram.len()),
            value: Some(unreachable.len() as f64),
            payload, auto_action_eligible: false, analytics: None,
        }
    }
}

fn parse_telegram_token(config_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(config_json).ok()?;
    v.get("bot_token").and_then(|t| t.as_str()).map(|s| s.to_string())
}

// Run [`probe_telegram`] to completion from a *synchronous* detector.
// // Detectors execute on a Tokio runtime worker thread, so calling
// `Handle::current().block_on(...)` panics with "Cannot start a runtime from
// within a runtime" — and because the release profile is `panic = "abort"`,
// the collector's `catch_unwind` is ineffective and that panic **aborts the
// whole MIRA process** (this froze the Health page at the last good snapshot
// and caused a restart loop once a Telegram account was configured). Running
// the probe on a dedicated OS thread with its own current-thread runtime
// sidesteps the nesting entirely, independent of the caller's runtime flavor.
// Probes are cached for 30 min, so a fresh thread here is rare and cheap.
fn probe_telegram_blocking(token: String) -> (bool, Option<String>) {
    std::thread::spawn(move || {
        match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(probe_telegram(&token)),
            Err(e) => (false, Some(format!("probe runtime build failed: {e}"))),
        }
    })
    .join()
    .unwrap_or((false, Some("telegram probe thread panicked".into())))
}

async fn probe_telegram(token: &str) -> (bool, Option<String>) {
    let url = format!("https://api.telegram.org/bot{token}/getMe");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8)).build()
    {
        Ok(c)  => c,
        Err(e) => return (false, Some(format!("client build: {e}"))),
    };
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => (true, None),
        Ok(resp) => (false, Some(format!("HTTP {}", resp.status().as_u16()))),
        // `without_url()` keeps the bot token (in the request URL) out of the
        // health-report message.
        Err(e)   => (false, Some(e.without_url().to_string())),
    }
}

// Hours since signal-cli last logged an inbound message. Log-scan based
// the `mira::providers::signal_cli` modules log INFO when a message
// is received. >24h with active accounts → Yellow; >72h → Red.
pub struct SignalNoReceivedDetector;
impl Detector for SignalNoReceivedDetector {
    fn name(&self) -> &'static str { "channel.signal.no_received_24h" }
    fn run(&self, ctx: &DetectorContext) -> DetectorReport {
        let Some(path) = ctx.log_path.as_ref() else {
            return err_yellow(self.name(), "log path not configured");
        };
        // Skip when no signal accounts are configured.
        let mut have_signal = false;
        if let Some(accounts) = ctx.channel_accounts.as_ref() {
            if let Ok(rows) = accounts.list_enabled() {
                have_signal = rows.iter().any(|a| matches!(a.channel, crate::channel_accounts::ChannelKind::Signal));
            }
        }
        if !have_signal {
            return DetectorReport::green(self.name(), "no signal accounts configured");
        }

        // Last 72h scan for any inbound-shaped log line. Looking for
        // either "received" or "incoming" patterns from the signal_cli
        // module.
        let recent_24h = count_log_pattern_since_any(
            path, &["mira::providers::signal_cli"], &["received", "incoming"], 24 * 3600,
        );
        let recent_72h = count_log_pattern_since_any(
            path, &["mira::providers::signal_cli"], &["received", "incoming"], 72 * 3600,
        );
        let payload = json!({
            "received_lines_24h": recent_24h,
            "received_lines_72h": recent_72h,
        });
        let level = if recent_72h == 0 { HealthLevel::Red }
                    else if recent_24h == 0 { HealthLevel::Yellow }
                    else { HealthLevel::Green };
        let message = match level {
            HealthLevel::Red    => "no signal inbound traffic in last 72h".into(),
            HealthLevel::Yellow => "no signal inbound traffic in last 24h (last 72h: present)".into(),
            HealthLevel::Green  => format!("{recent_24h} signal inbound log line(s) in last 24h"),
        };
        DetectorReport {
            name: self.name().into(), level, message,
            value: Some(recent_24h as f64),
            payload, auto_action_eligible: false, analytics: None,
        }
    }
}

pub fn slice_5d_registry() -> Vec<Box<dyn Detector>> {
    vec![
        Box::new(TelegramReachableDetector),
        Box::new(SignalNoReceivedDetector),
    ]
}

// Full set the heartbeat runs each fire.  first (most stable),
// then 2, 3b, 4, 5d in order so the snapshot's display order is stable
// over time. Plus the 24h cost detector that supersedes the slice-3b
// running-burn one.
pub fn default_registry() -> Vec<Box<dyn Detector>> {
    let mut v = slice_1_registry();
    v.extend(slice_2_registry());
    v.extend(slice_3b_registry());
    v.extend(slice_4_registry());
    v.extend(slice_5d_registry());
    // 0.110.0 — ledger-based cost detector lives alongside the
    // running-burn snapshot; both are useful (one historical, one peak).
    v.push(Box::new(LlmCostBurn24hDetector));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_ctx(dir: &Path) -> DetectorContext {
        DetectorContext {
            data_dir: dir.to_path_buf(),
            automations: None,
            audit_store: None,
            embedding_provider_kind: "lmstudio".into(),
            mira_version: semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
            agent_registry: None,
            auth_db: None,
            log_path: None,
            channel_manager: None,
            secrets_store: None,
            channel_accounts: None,
            degradations: None,        }
    }

    #[test]
    fn master_key_present_red_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let r = MasterKeyPresentDetector.run(&make_ctx(dir.path()));
        assert_eq!(r.level, HealthLevel::Red);
    }

    #[test]
    fn master_key_present_green_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("master.key"), b"x").unwrap();
        let r = MasterKeyPresentDetector.run(&make_ctx(dir.path()));
        assert_eq!(r.level, HealthLevel::Green);
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn master_key_perms_red_when_loose() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        std::fs::write(&path, b"x").unwrap();
        // chmod 0644 — looser than 0600.
        let mut p = std::fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o644);
        std::fs::set_permissions(&path, p).unwrap();
        let r = MasterKeyPermsDetector.run(&make_ctx(dir.path()));
        assert_eq!(r.level, HealthLevel::Red, "got: {r:?}");
        assert!(r.message.contains("644") || r.payload["actual_perms_octal"] == "644");
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn master_key_perms_green_when_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        std::fs::write(&path, b"x").unwrap();
        let mut p = std::fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o600);
        std::fs::set_permissions(&path, p).unwrap();
        let r = MasterKeyPermsDetector.run(&make_ctx(dir.path()));
        assert_eq!(r.level, HealthLevel::Green, "got: {r:?}");
    }

    #[test]
    fn embedding_detector_skips_non_internal_providers() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = make_ctx(dir.path());
        ctx.embedding_provider_kind = "lmstudio".into();
        let r = EmbeddingProviderReachableDetector.run(&ctx);
        assert_eq!(r.level, HealthLevel::Green);
        assert!(r.message.contains("lmstudio"));
    }

    #[test]
    fn disk_free_detector_returns_some_value() {
        // Smoke test — just ensure statvfs returns a sane number for /tmp.
        let dir = tempfile::tempdir().unwrap();
        let r = DiskFreeDetector.run(&make_ctx(dir.path()));
        // Whatever the value is, it should be non-Green only if the
        // disk really is full. On a CI / dev box assume there's >2GB
        // free, but accept Yellow as a non-failure either way.
        assert!(matches!(r.level, HealthLevel::Green | HealthLevel::Yellow | HealthLevel::Red));
        assert!(r.value.is_some());
    }

    #[test]
    fn audit_detector_yellow_when_no_store() {
        let dir = tempfile::tempdir().unwrap();
        let r = AuditChainIntegrityDetector.run(&make_ctx(dir.path()));
        assert_eq!(r.level, HealthLevel::Yellow);
    }

    #[test]
    fn audit_detector_green_on_clean_chain() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::agent::AuditStore::open_in_memory());
        // Record a few clean rows.
        let aid = crate::agent::instance::AgentId::new();
        for r in 0..3 {
            store.record(aid, crate::agent::audit::AuditEvent::Interrupted {
                reason: format!("r{r}"),
            }).unwrap();
        }
        let mut ctx = make_ctx(dir.path());
        ctx.audit_store = Some(store);
        let report = AuditChainIntegrityDetector.run(&ctx);
        assert_eq!(report.level, HealthLevel::Green, "got: {report:?}");
    }

    // Uses Unix `PermissionsExt` + the `#[cfg(target_family = "unix")]`
    // `filetime_set` helper below, so gate it to keep the Windows test build green.
    #[test]
    #[cfg(target_family = "unix")]
    fn scan_tmp_ignores_recent_and_unprefixed_dirs() {
        // Direct test on the inner scan fn so we don't have to mutate
        // process-global TMPDIR. Build a tempdir holding 3 entries:
        // - mira-coderun-fresh   → matched prefix, recent → ignored
        // - mira-coderun-stale   → matched prefix, mtime backdated → flagged
        // - random-dir           → unmatched prefix → ignored regardless
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let fresh = root.path().join("mira-coderun-fresh");
        let stale = root.path().join("mira-coderun-stale");
        let other = root.path().join("random-dir");
        for d in [&fresh, &stale, &other] { std::fs::create_dir(d).unwrap(); }
        // Backdate `stale` 30 days into the past via filetime (set both
        // mtime and atime; we only check mtime but be consistent).
        let backdate = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
        let _ = filetime_set(&stale, backdate);
        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 24 * 3600);
        let mut out = Vec::new();
        scan_tmp_for_scratch(root.path(), cutoff, &mut out);
        assert_eq!(out.len(), 1, "expected one stale scratch hit, got: {out:?}");
        assert!(out[0].contains("mira-coderun-stale"));
        // Quiet the unused-import lint when the platform-specific
        // mode-bits import isn't needed here.
        let _ = std::fs::metadata(&other).unwrap().permissions().mode();
    }

    // Set both mtime and atime on `path` to `t`. Done via `utimensat`
    // instead of pulling the `filetime` crate just for this one test.
    #[cfg(target_family = "unix")]
    fn filetime_set(path: &Path, t: std::time::SystemTime) -> std::io::Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let cpath = CString::new(path.as_os_str().as_bytes()).unwrap();
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let times = [
            libc::timespec { tv_sec: secs, tv_nsec: 0 },
            libc::timespec { tv_sec: secs, tv_nsec: 0 },
        ];
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) };
        if rc != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }
}
