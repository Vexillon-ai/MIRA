// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/actions.rs
//! Auto-cleanup actions the collector can run when a detector trips
//! AND the user's per-signal policy is `auto_cleanup`.
//!
//! ships exactly one action: marking stranded
//! `agent.worker.completed` subscriptions as failed (the safe one,
//! identical to what the boot orphan sweep does). Other auto-actions
//! from the plan (chmod master key, kill orphan workers, etc.) are
//! intentionally deferred — each carries its own blast-radius
//! considerations and deserves a separate slice.

use std::sync::Arc;

use tracing::warn;

use crate::automations::AutomationsStore;
use crate::MiraError;

use super::DetectorContext;

// Outcome of attempting an auto-action. `NotImplemented` is the
// expected return for any detector without a registered action — the
// collector logs at debug and moves on.
#[derive(Debug, Clone)]
pub enum AutoAction {
    NotImplemented,
    Ran { summary: String },
}

// Dispatch by detector name. Returns `NotImplemented` for detectors
// without an action; returns Err for actions that ran but failed.
pub fn run_action_for(
    detector_name: &str,
    ctx:           &DetectorContext,
    automations:   Option<&Arc<AutomationsStore>>,
) -> Result<AutoAction, MiraError> {
    match detector_name {
        "automations.subscriptions_stranded_completion" => {
            let Some(store) = automations else {
                return Err(MiraError::ConfigError(
                    "auto-action requires automations store".into(),
                ));
            };
            sweep_stranded_completion_subs(store)
        }
        // 0.106.0 — slice 2 actions.
        "db.wal_size_mb" => {
            let dbs = super::db_paths::all_dbs(&ctx.data_dir);
            wal_checkpoint_all(&dbs)
        }
        "auth.failed_logins_per_ip_1h" => {
            let Some(db) = ctx.auth_db.as_ref() else {
                return Err(MiraError::ConfigError(
                    "auth db not wired — cannot temp-ban".into(),
                ));
            };
            ban_top_failed_login_ip(db)
        }
        "watchdog.analysis_stuck_in_progress_30m" => {
            let Some(store) = automations else {
                return Err(MiraError::ConfigError(
                    "auto-action requires automations store".into(),
                ));
            };
            reset_stuck_incident_analyses(store)
        }
        "auth.master_key_perms" => chmod_master_key_to_0600(&ctx.data_dir),
        // 0.108.0 — slice 3b actions.
        "channel.signal.daemon_alive" => restart_dead_signal_daemons(ctx),
        "skills.bundled_drift" => force_refresh_bundled_skills(&ctx.data_dir),
        "skills.dangling_secrets_count" => sweep_dangling_skill_secrets(ctx),
        "consistency.automations_for_deleted_users" => sweep_automations_for_deleted_users(ctx, automations),
        _ => Ok(AutoAction::NotImplemented),
    }
}

// Find every dead signal account and restart it via the ChannelManager.
// Identical mutex semantics to the detector — brief blocking write
// lock. Each restart is a separate call so one failure doesn't abort
// the others.
fn restart_dead_signal_daemons(ctx: &DetectorContext) -> Result<AutoAction, MiraError> {
    let Some(mgr) = ctx.channel_manager.as_ref() else {
        return Err(MiraError::ConfigError("channel manager not wired".into()));
    };
    // Snapshot under a brief write lock.
    let dead_ids: Vec<String> = {
        let mut guard = match mgr.try_write() {
            Ok(g)  => g,
            Err(_) => return Err(MiraError::ConfigError(
                "channel manager busy — try again next tick".into(),
            )),
        };
        guard.signal_account_aliveness().into_iter()
            .filter_map(|(id, alive)| if !alive { Some(id) } else { None })
            .collect()
    };
    if dead_ids.is_empty() {
        return Ok(AutoAction::Ran { summary: "no dead signal daemons".into() });
    }
    // The restart is async — the heartbeat task is already on a tokio
    // runtime, so we can use Handle::current().block_on for each.
    let rt = tokio::runtime::Handle::current();
    let mut restarted = 0usize;
    let mut errors:   Vec<String> = Vec::new();
    for id in &dead_ids {
        let mgr_c = Arc::clone(mgr);
        let id_c = id.clone();
        let res: Result<(), String> = rt.block_on(async move {
            let mut guard = mgr_c.write().await;
            guard.restart_account(&id_c).await
        });
        match res {
            Ok(())  => { restarted += 1; }
            Err(e)  => { errors.push(format!("{id}: {e}")); warn!("restart {id} failed: {e}"); }
        }
    }
    Ok(AutoAction::Ran {
        summary: format!(
            "restarted {restarted}/{} signal daemon(s){}",
            dead_ids.len(),
            if errors.is_empty() { String::new() } else { format!(" — {} error(s): {:?}", errors.len(), errors) },
        ),
    })
}

// Re-run the boot-time bundled-skill extract with `force=false` —
// only newer-versioned bundled skills overwrite their on-disk
// counterparts. Same code path the gateway uses on startup; safe
// to call repeatedly.
fn force_refresh_bundled_skills(data_dir: &std::path::Path) -> Result<AutoAction, MiraError> {
    let skills_dir = crate::skills::default_skills_dir(data_dir);
    let outcomes = crate::skills::bundled::extract_or_refresh(&skills_dir, false)
        .map_err(|e| MiraError::ConfigError(format!("extract_or_refresh: {e}")))?;
    let refreshed: Vec<String> = outcomes.iter()
        .filter_map(|(id, o)| match o {
            crate::skills::bundled::RefreshOutcome::Refreshed { from, to } =>
                Some(format!("{id}: {from} → {to}")),
            crate::skills::bundled::RefreshOutcome::Extracted =>
                Some(format!("{id}: extracted")),
            _ => None,
        })
        .collect();
    Ok(AutoAction::Ran {
        summary: if refreshed.is_empty() {
            "no bundled skills needed refresh".into()
        } else {
            format!("refreshed {}: {}", refreshed.len(), refreshed.join(", "))
        },
    })
}

// Purge skill-secret rows for skills no longer in the registry.
// Recomputes the dangling set rather than trusting the detector's
// payload — keeps the action self-contained and idempotent.
fn sweep_dangling_skill_secrets(ctx: &DetectorContext) -> Result<AutoAction, MiraError> {
    let Some(secrets) = ctx.secrets_store.as_ref() else {
        return Err(MiraError::ConfigError("secrets store not wired".into()));
    };
    let secret_skill_ids = secrets.list_distinct_skill_ids()
        .map_err(|e| MiraError::DatabaseError(format!("list secrets: {e}")))?;
    let skills_dir = crate::skills::default_skills_dir(&ctx.data_dir);
    let registry = crate::skills::loader::load_dir(&skills_dir, &ctx.mira_version);
    let installed: std::collections::HashSet<String> = registry.iter()
        .map(|s| s.manifest.skill.id.clone())
        .collect();
    let mut purged = 0usize;
    let mut purged_ids: Vec<String> = Vec::new();
    for id in &secret_skill_ids {
        if installed.contains(id) { continue; }
        match secrets.purge_skill(id) {
            Ok(n) if n > 0 => { purged += n; purged_ids.push(id.clone()); }
            Ok(_)          => {}
            Err(e)         => warn!("purge_skill({id}) failed: {e}"),
        }
    }
    Ok(AutoAction::Ran {
        summary: if purged == 0 {
            "no dangling secrets to purge".into()
        } else {
            format!("purged {purged} secret row(s) across {} skill(s): {:?}", purged_ids.len(), purged_ids)
        },
    })
}

// For every automation row whose user_id isn't in the auth.db users
// table, delete the row. Uses the orphan-detection logic from the
// detector — recomputed here for the same self-contained-action reason.
fn sweep_automations_for_deleted_users(
    ctx: &DetectorContext,
    automations: Option<&Arc<AutomationsStore>>,
) -> Result<AutoAction, MiraError> {
    let Some(autos) = automations else {
        return Err(MiraError::ConfigError("automations store not wired".into()));
    };
    let Some(auth) = ctx.auth_db.as_ref() else {
        return Err(MiraError::ConfigError("auth db not wired".into()));
    };
    let referenced = autos.distinct_user_ids_referenced()?;
    let users = auth.list_users()?;
    let valid: std::collections::HashSet<String> = users.iter().map(|u| u.id.clone()).collect();
    let mut total_s = 0usize;
    let mut total_e = 0usize;
    let mut total_w = 0usize;
    for uid in &referenced {
        if valid.contains(uid) { continue; }
        match autos.delete_automations_for_user(uid) {
            Ok((s, e, w)) => { total_s += s; total_e += e; total_w += w; }
            Err(err)      => warn!("delete_automations_for_user({uid}) failed: {err}"),
        }
    }
    let total = total_s + total_e + total_w;
    Ok(AutoAction::Ran {
        summary: if total == 0 {
            "no orphan automations to sweep".into()
        } else {
            format!(
                "deleted {total} row(s) — schedules={total_s}, subs={total_e}, webhooks={total_w}",
            )
        },
    })
}

// Tighten `<data_dir>/master.key` to 0600. Idempotent (no-op when
// already 0600) and conservative (only touches mode bits — owner /
// group untouched). Unix-only; non-Unix returns NotImplemented since
// the perms model is different.
#[cfg(target_family = "unix")]
fn chmod_master_key_to_0600(data_dir: &std::path::Path) -> Result<AutoAction, MiraError> {
    use std::os::unix::fs::PermissionsExt;
    let path = data_dir.join("master.key");
    if !path.exists() {
        return Ok(AutoAction::Ran {
            summary: format!("master.key not found at {} — nothing to chmod", path.display()),
        });
    }
    let meta = std::fs::metadata(&path)
        .map_err(|e| MiraError::ConfigError(format!("stat master.key: {e}")))?;
    let actual = meta.permissions().mode() & 0o777;
    if actual == 0o600 {
        return Ok(AutoAction::Ran {
            summary: "master.key already 0600 — no change".into(),
        });
    }
    let mut p = meta.permissions();
    p.set_mode(0o600);
    std::fs::set_permissions(&path, p)
        .map_err(|e| MiraError::ConfigError(format!("chmod master.key: {e}")))?;
    Ok(AutoAction::Ran {
        summary: format!("chmod master.key from {actual:o} → 0600"),
    })
}

#[cfg(not(target_family = "unix"))]
fn chmod_master_key_to_0600(_data_dir: &std::path::Path) -> Result<AutoAction, MiraError> {
    Ok(AutoAction::NotImplemented)
}

// Mark every active `agent.worker.completed` subscription that's been
// waiting >6h as `failed`. Same shape as the boot orphan sweep but
// scoped to the same age threshold the detector uses.
fn sweep_stranded_completion_subs(
    store: &Arc<AutomationsStore>,
) -> Result<AutoAction, MiraError> {
    const STRANDED_AGE_SECS: i64 = 6 * 60 * 60;
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - STRANDED_AGE_SECS;
    let stuck = store.list_stuck_completion_subscriptions(cutoff)?;
    if stuck.is_empty() {
        return Ok(AutoAction::Ran {
            summary: "no stranded subscriptions to sweep".into(),
        });
    }
    let mut marked = 0usize;
    for sub in &stuck {
        if let Err(e) = store.fail_event_subscription(
            &sub.id, now, "abandoned (stranded >6h, swept by health-audit)",
        ) {
            warn!("sweep_stranded: mark {} failed: {e}", sub.id);
        } else {
            marked += 1;
        }
    }
    Ok(AutoAction::Ran {
        summary: format!("marked {marked}/{} stranded subscription(s) as failed", stuck.len()),
    })
}

// Run `wal_checkpoint(TRUNCATE)` against every DB whose WAL exists.
// Cheap and idempotent — silently no-ops on DBs without an active WAL.
// We don't filter by size because the detector's threshold already
// gated this action; if we got here, at least one WAL was Yellow+.
fn wal_checkpoint_all(dbs: &[super::db_paths::DbEntry]) -> Result<AutoAction, MiraError> {
    use rusqlite::Connection;
    let mut checkpointed: Vec<String> = Vec::new();
    let mut errors:       Vec<String> = Vec::new();
    for entry in dbs {
        if !entry.path.exists() { continue }
        // Need a writeable open for wal_checkpoint to truncate the
        // file. Open and immediately close — the DB doesn't need to
        // stay open here.
        let conn = match Connection::open(&entry.path) {
            Ok(c)  => c,
            Err(e) => { errors.push(format!("{}: open: {e}", entry.name)); continue; }
        };
        // SQLite returns three integers: (busy, log_pages, checkpointed).
        let res: rusqlite::Result<(i64, i64, i64)> = conn.query_row(
            "PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        );
        match res {
            Ok((busy, _, _)) if busy == 0 => checkpointed.push(entry.name.into()),
            Ok((busy, _, _))              => errors.push(format!("{}: busy={busy} (reader holding snapshot)", entry.name)),
            Err(e)                        => errors.push(format!("{}: {e}", entry.name)),
        }
    }
    if checkpointed.is_empty() && errors.is_empty() {
        return Ok(AutoAction::Ran { summary: "no DBs needed checkpointing".into() });
    }
    Ok(AutoAction::Ran {
        summary: format!(
            "checkpointed {} DB(s){}",
            checkpointed.len(),
            if errors.is_empty() { String::new() } else { format!(" — {} issue(s): {:?}", errors.len(), errors) },
        ),
    })
}

// Look up the IP with the most failed logins in the last hour and
// ban it for 30 minutes. Detector already gated to fire only at Red
// (>50 failures). Idempotent — re-banning the same IP extends the
// existing window via `MAX(banned_until,...)`.
fn ban_top_failed_login_ip(db: &Arc<crate::auth::AuthDb>) -> Result<AutoAction, MiraError> {
    const BAN_SECS: i64 = 30 * 60;
    let since = chrono::Utc::now().timestamp() - 3600;
    let (_total, top_count, top_ip) = db.count_failed_logins_since(since)?;
    let Some(ip) = top_ip else {
        return Ok(AutoAction::Ran {
            summary: "no IP recorded on failed-login rows — skipping ban".into(),
        });
    };
    let until = db.ban_ip(&ip, BAN_SECS, &format!(
        "auto-ban: {top_count} failed login(s) in last hour",
    ))?;
    Ok(AutoAction::Ran {
        summary: format!("banned {ip} until unix {until} ({BAN_SECS}s window)"),
    })
}

// Flip every stuck-in-progress watchdog incident analysis to `failed`
// so the analyze button works again. Cheap iteration — analyses
// don't accumulate unless the LLM call is hung repeatedly.
fn reset_stuck_incident_analyses(
    store: &Arc<AutomationsStore>,
) -> Result<AutoAction, MiraError> {
    let cutoff = chrono::Utc::now().timestamp() - 30 * 60;
    let stuck = store.list_stuck_incident_analyses(cutoff)?;
    if stuck.is_empty() {
        return Ok(AutoAction::Ran { summary: "no stuck analyses to reset".into() });
    }
    let mut reset = 0usize;
    for inc in &stuck {
        match store.mark_incident_analysis_failed(&inc.id, "stuck >30min") {
            Ok(true)  => reset += 1,
            Ok(false) => {/* race — another process already moved it */},
            Err(e)    => warn!("reset stuck incident {} failed: {e}", inc.id),
        }
    }
    Ok(AutoAction::Ran {
        summary: format!("reset {reset}/{} stuck analysis row(s) to failed", stuck.len()),
    })
}
