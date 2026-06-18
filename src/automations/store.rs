// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/store.rs
//! SQLite-backed store for the automations subsystem.
//!
//! covers the time-driven half: `schedules` rows and
//! the unified `automation_runs` audit log. Webhooks and event subs land
//! in their tables live in this same DB so the action
//! dispatcher and history view can see all activations together.

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{debug, info};
use uuid::Uuid;

use crate::MiraError;

use super::types::*;

// ── DB shell ─────────────────────────────────────────────────────────────────

pub struct AutomationsStore {
    conn: Arc<Mutex<Connection>>,
}

impl AutomationsStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("automations dir: {e}"))
            })?;
        }

        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("automations DB open: {e}"))
        })?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        conn.execute_batch(SCHEMA_V1)
            .map_err(|e| MiraError::DatabaseError(format!("automations schema: {e}")))?;

        // Additive column migrations. SQLite has no ADD COLUMN IF NOT
        // EXISTS, so each statement is best-effort — it errors with
        // "duplicate column name" once the column is present, which we
        // swallow. Mirror of the pattern used in src/auth/models.rs.
        for sql in [
            // 0.103.0 — one-shot subscription deletion. Tracked via the
            // `delete_after_fire` flag set by spawn_background_task and
            // friends; the event subscriber tears the row down after a
            // successful dispatch.
            "ALTER TABLE event_subscriptions ADD COLUMN delete_after_fire INTEGER NOT NULL DEFAULT 0",
        ] {
            let _ = conn.execute(sql, []);
        }

        debug!("automations schema ready at {}", path.display());
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, MiraError> {
        self.conn.lock()
            .map_err(|e| MiraError::DatabaseError(format!("automations lock: {e}")))
    }

    // ── Schedules CRUD (subset needed for) ────────────────────────────

    // Insert a new schedule. Computes initial `next_run_at` from the spec
    // using the schedule's own `created_at` as the reference point.
    pub fn create_schedule(&self, new: NewSchedule) -> Result<Schedule, MiraError> {
        let conn = self.lock()?;
        let now = Utc::now().timestamp();
        let id  = Uuid::new_v4().to_string();
        let trigger_json = serde_json::to_string(&new.trigger)
            .map_err(|e| MiraError::DatabaseError(format!("trigger serialise: {e}")))?;
        let action_json  = serde_json::to_string(&new.action)
            .map_err(|e| MiraError::DatabaseError(format!("action serialise: {e}")))?;
        let quiet_json = match &new.quiet_hours {
            Some(q) => Some(serde_json::to_string(q)
                .map_err(|e| MiraError::DatabaseError(format!("quiet serialise: {e}")))?),
            None => None,
        };

        let next_run = super::next_run_at::next_run_at(&new.trigger, &new.timezone, now)?;
        let status = new.status.unwrap_or(ScheduleStatus::Active);

        conn.execute(
            "INSERT INTO schedules (
                id, user_id, owner_kind, name, description, rationale,
                schedule_kind, trigger_spec, timezone, quiet_hours,
                action_kind, action_payload,
                status, created_at, expires_at,
                last_run_at, next_run_at,
                run_count, failure_count, max_failures, last_error
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10,
                ?11, ?12,
                ?13, ?14, ?15,
                NULL, ?16,
                0, 0, 5, NULL
             )",
            params![
                id, new.user_id, new.owner_kind.as_str(),
                new.name, new.description, new.rationale,
                schedule_kind_tag(&new.trigger), trigger_json,
                new.timezone, quiet_json,
                action_kind_tag(&new.action), action_json,
                status.as_str(), now, new.expires_at,
                next_run,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("create_schedule: {e}")))?;

        Ok(Schedule {
            id,
            user_id:        new.user_id,
            owner_kind:     new.owner_kind,
            name:           new.name,
            description:    new.description,
            rationale:      new.rationale,
            trigger:        new.trigger,
            timezone:       new.timezone,
            quiet_hours:    new.quiet_hours,
            action:         new.action,
            status,
            created_at:     now,
            expires_at:     new.expires_at,
            last_run_at:    None,
            next_run_at:    next_run,
            run_count:      0,
            failure_count:  0,
            max_failures:   5,
            last_error:     None,
        })
    }

    // Idempotent upsert keyed on `(user_id, name)` for `system`-owned rows.
    // Used to seed default heartbeats: re-running the seed must not
    // duplicate or clobber user edits to existing rows.
    pub fn ensure_system_schedule(&self, new: NewSchedule) -> Result<(), MiraError> {
        let exists = {
            let conn = self.lock()?;
            conn.query_row(
                "SELECT 1 FROM schedules
                  WHERE owner_kind = 'system' AND user_id = ?1 AND name = ?2",
                params![new.user_id, new.name],
                |_| Ok(()),
            ).optional()
             .map_err(|e| MiraError::DatabaseError(format!("ensure check: {e}")))?
             .is_some()
        };
        if exists { return Ok(()); }
        self.create_schedule(new)?;
        Ok(())
    }

    // Recover system schedules orphaned by a crash/restart *mid-run*.
    //     // [`claim_due`] sets `next_run_at = NULL` when it claims a row so a
    // concurrent tick can't double-fire it; the run's completion then writes
    // the next cron time back. If the process dies between those two steps
    // (e.g. a restart while the hourly `system_audit` is running) the row is
    // left stuck at NULL — `claim_due` only matches `next_run_at IS NOT NULL`,
    // so it's never reclaimed and the job goes silently dormant.
    //     // Call this at startup: for every active `system` schedule missing a
    // `next_run_at`, recompute it from the trigger. Returns the count fixed.
    pub fn requeue_orphaned_system_schedules(&self, now: i64) -> Result<usize, MiraError> {
        let orphans: Vec<Schedule> = {
            let conn = self.lock()?;
            let mut stmt = conn.prepare(
                "SELECT id, user_id, owner_kind, name, description, rationale,
                        schedule_kind, trigger_spec, timezone, quiet_hours,
                        action_kind, action_payload,
                        status, created_at, expires_at,
                        last_run_at, next_run_at,
                        run_count, failure_count, max_failures, last_error
                   FROM schedules
                  WHERE owner_kind = 'system' AND status = 'active'
                    AND next_run_at IS NULL",
            ).map_err(|e| MiraError::DatabaseError(format!("orphan prep: {e}")))?;
            stmt.query_map([], row_to_schedule)
                .map_err(|e| MiraError::DatabaseError(format!("orphan q: {e}")))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| MiraError::DatabaseError(format!("orphan rows: {e}")))?
        };

        let mut fixed = 0usize;
        for s in orphans {
            let next = super::next_run_at::next_run_at(&s.trigger, &s.timezone, now)?;
            if let Some(n) = next {
                let conn = self.lock()?;
                conn.execute(
                    "UPDATE schedules SET next_run_at = ?2 WHERE id = ?1",
                    params![s.id, n],
                ).map_err(|e| MiraError::DatabaseError(format!("orphan fix: {e}")))?;
                fixed += 1;
            }
        }
        Ok(fixed)
    }

    // Atomically claim up to `limit` due schedules. Sets their
    // `next_run_at` to NULL while running so a second worker (or the
    // next tick before this one finishes) can't pick the same row.
    pub fn claim_due(&self, now: i64, limit: usize) -> Result<Vec<Schedule>, MiraError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "UPDATE schedules
                SET next_run_at = NULL
              WHERE id IN (
                SELECT id FROM schedules
                 WHERE status = 'active'
                   AND next_run_at IS NOT NULL
                   AND next_run_at <= ?1
                 ORDER BY next_run_at ASC
                 LIMIT ?2
              )
              RETURNING
                id, user_id, owner_kind, name, description, rationale,
                schedule_kind, trigger_spec, timezone, quiet_hours,
                action_kind, action_payload,
                status, created_at, expires_at,
                last_run_at, next_run_at,
                run_count, failure_count, max_failures, last_error"
        ).map_err(|e| MiraError::DatabaseError(format!("claim_due prep: {e}")))?;

        let rows = stmt.query_map(params![now, limit as i64], row_to_schedule)
            .map_err(|e| MiraError::DatabaseError(format!("claim_due q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("claim_due rows: {e}")))?;

        Ok(rows)
    }

    // Mark a successful run: bump counters, reset failure_count, compute
    // the next fire (or expire one_off rows).
    pub fn record_success(
        &self,
        id:           &str,
        ran_at:       i64,
        next_run_at:  Option<i64>,
        expire:       bool,
    ) -> Result<(), MiraError> {
        let conn = self.lock()?;
        let new_status = if expire { "expired" } else { "active" };
        conn.execute(
            "UPDATE schedules
                SET last_run_at   = ?1,
                    next_run_at   = ?2,
                    run_count     = run_count + 1,
                    failure_count = 0,
                    last_error    = NULL,
                    status        = ?3
              WHERE id = ?4",
            params![ran_at, next_run_at, new_status, id],
        ).map_err(|e| MiraError::DatabaseError(format!("record_success: {e}")))?;
        Ok(())
    }

    // Mark a failed run: bump failure_count, reschedule (so transient
    // errors retry), flip to `failed` if the cap is exceeded, or to
    // `expired` for one-off rows that shouldn't be retried.
    pub fn record_failure(
        &self,
        id:          &str,
        ran_at:      i64,
        next_run_at: Option<i64>,
        error:       &str,
        expire:      bool,
    ) -> Result<(), MiraError> {
        let conn = self.lock()?;
        let truncated: String = error.chars().take(500).collect();
        if expire {
            conn.execute(
                "UPDATE schedules
                    SET last_run_at   = ?1,
                        next_run_at   = NULL,
                        failure_count = failure_count + 1,
                        last_error    = ?2,
                        status        = 'expired'
                  WHERE id = ?3",
                params![ran_at, truncated, id],
            ).map_err(|e| MiraError::DatabaseError(format!("record_failure: {e}")))?;
        } else {
            conn.execute(
                "UPDATE schedules
                    SET last_run_at   = ?1,
                        next_run_at   = ?2,
                        failure_count = failure_count + 1,
                        last_error    = ?3,
                        status = CASE
                            WHEN failure_count + 1 >= max_failures THEN 'failed'
                            ELSE status
                        END
                  WHERE id = ?4",
                params![ran_at, next_run_at, truncated, id],
            ).map_err(|e| MiraError::DatabaseError(format!("record_failure: {e}")))?;
        }
        Ok(())
    }

    // Mark a run as skipped (quiet hours, predicate miss, etc.) without
    // touching `run_count` or `failure_count`. The caller supplies a new
    // `next_run_at` — typically the end of the quiet window — so the
    // worker re-fires as soon as the gate lifts.
    pub fn record_skipped(
        &self,
        id:          &str,
        ran_at:      i64,
        next_run_at: Option<i64>,
    ) -> Result<(), MiraError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE schedules
                SET last_run_at = ?1,
                    next_run_at = ?2
              WHERE id = ?3",
            params![ran_at, next_run_at, id],
        ).map_err(|e| MiraError::DatabaseError(format!("record_skipped: {e}")))?;
        Ok(())
    }

    pub fn get_schedule(&self, id: &str) -> Result<Option<Schedule>, MiraError> {
        let conn = self.lock()?;
        let row = conn.query_row(
            SELECT_SCHEDULE_COLS_BY_ID,
            params![id],
            row_to_schedule,
        ).optional()
         .map_err(|e| MiraError::DatabaseError(format!("get_schedule: {e}")))?;
        Ok(row)
    }

    // List all schedules owned by `user_id` (or all if `None`).
    pub fn list_schedules(&self, user_id: Option<&str>) -> Result<Vec<Schedule>, MiraError> {
        let conn = self.lock()?;
        let rows = match user_id {
            Some(u) => {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, owner_kind, name, description, rationale,
                            schedule_kind, trigger_spec, timezone, quiet_hours,
                            action_kind, action_payload, status, created_at,
                            expires_at, last_run_at, next_run_at, run_count,
                            failure_count, max_failures, last_error
                       FROM schedules
                      WHERE user_id = ?1
                      ORDER BY created_at DESC"
                ).map_err(|e| MiraError::DatabaseError(format!("list prep: {e}")))?;
                stmt.query_map(params![u], row_to_schedule)
                    .map_err(|e| MiraError::DatabaseError(format!("list q: {e}")))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| MiraError::DatabaseError(format!("list rows: {e}")))?
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, owner_kind, name, description, rationale,
                            schedule_kind, trigger_spec, timezone, quiet_hours,
                            action_kind, action_payload, status, created_at,
                            expires_at, last_run_at, next_run_at, run_count,
                            failure_count, max_failures, last_error
                       FROM schedules
                      ORDER BY created_at DESC"
                ).map_err(|e| MiraError::DatabaseError(format!("list prep: {e}")))?;
                stmt.query_map([], row_to_schedule)
                    .map_err(|e| MiraError::DatabaseError(format!("list q: {e}")))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| MiraError::DatabaseError(format!("list rows: {e}")))?
            }
        };
        Ok(rows)
    }

    // Visibility-aware list for the HTTP API: a non-admin user sees their
    // own rows plus `system`-owned rows; admin sees all.
    pub fn list_schedules_visible_to(
        &self,
        user_id:  &str,
        is_admin: bool,
    ) -> Result<Vec<Schedule>, MiraError> {
        if is_admin {
            return self.list_schedules(None);
        }
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, owner_kind, name, description, rationale,
                    schedule_kind, trigger_spec, timezone, quiet_hours,
                    action_kind, action_payload, status, created_at,
                    expires_at, last_run_at, next_run_at, run_count,
                    failure_count, max_failures, last_error
               FROM schedules
              WHERE user_id = ?1 OR owner_kind = 'system'
              ORDER BY created_at DESC"
        ).map_err(|e| MiraError::DatabaseError(format!("list_visible prep: {e}")))?;
        let rows = stmt.query_map(params![user_id], row_to_schedule)
            .map_err(|e| MiraError::DatabaseError(format!("list_visible q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("list_visible rows: {e}")))?;
        Ok(rows)
    }

    // Update mutable fields on a schedule. The action payload, trigger,
    // timezone, and quiet hours are all reserialised; `next_run_at` is
    // recomputed from the new trigger so the next fire honours the edit.
    // Counters and run history are preserved.
    pub fn update_schedule(&self, id: &str, upd: UpdateSchedule) -> Result<Schedule, MiraError> {
        let now = Utc::now().timestamp();
        let trigger_json = serde_json::to_string(&upd.trigger)
            .map_err(|e| MiraError::DatabaseError(format!("trigger serialise: {e}")))?;
        let action_json  = serde_json::to_string(&upd.action)
            .map_err(|e| MiraError::DatabaseError(format!("action serialise: {e}")))?;
        let quiet_json = match &upd.quiet_hours {
            Some(q) => Some(serde_json::to_string(q)
                .map_err(|e| MiraError::DatabaseError(format!("quiet serialise: {e}")))?),
            None => None,
        };
        let next_run = super::next_run_at::next_run_at(&upd.trigger, &upd.timezone, now)?;

        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE schedules
                SET name           = ?1,
                    description    = ?2,
                    rationale      = ?3,
                    schedule_kind  = ?4,
                    trigger_spec   = ?5,
                    timezone       = ?6,
                    quiet_hours    = ?7,
                    action_kind    = ?8,
                    action_payload = ?9,
                    expires_at     = ?10,
                    next_run_at    = ?11
              WHERE id = ?12",
            params![
                upd.name, upd.description, upd.rationale,
                schedule_kind_tag(&upd.trigger), trigger_json,
                upd.timezone, quiet_json,
                action_kind_tag(&upd.action), action_json,
                upd.expires_at, next_run, id,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("update_schedule: {e}")))?;
        if n == 0 {
            return Err(MiraError::ConfigError(format!("schedule {id} not found")));
        }
        drop(conn);
        self.get_schedule(id)?.ok_or_else(|| MiraError::DatabaseError(
            format!("update_schedule: row {id} vanished after update")
        ))
    }

    // Pause an active schedule. Idempotent for already-paused rows. Other
    // terminal states (`expired`, `failed`) reject so a paused-on-failure
    // row is unambiguous.
    pub fn pause_schedule(&self, id: &str) -> Result<Schedule, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE schedules
                SET status = 'paused'
              WHERE id = ?1 AND status IN ('active', 'paused')",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("pause_schedule: {e}")))?;
        drop(conn);
        if n == 0 {
            return Err(MiraError::ConfigError(format!(
                "schedule {id} not found or not pausable"
            )));
        }
        self.get_schedule(id)?.ok_or_else(|| MiraError::DatabaseError(
            "pause_schedule: row vanished".into()
        ))
    }

    // Resume a paused schedule: status flips to `active` and `next_run_at`
    // is recomputed from the trigger so it doesn't fire immediately for
    // a schedule that's been paused for hours.
    pub fn resume_schedule(&self, id: &str) -> Result<Schedule, MiraError> {
        let s = self.get_schedule(id)?
            .ok_or_else(|| MiraError::ConfigError(format!("schedule {id} not found")))?;
        if !matches!(s.status, ScheduleStatus::Paused) {
            return Err(MiraError::ConfigError(format!(
                "schedule {id} is {} — only paused schedules can be resumed", s.status.as_str()
            )));
        }
        let now  = Utc::now().timestamp();
        let next = super::next_run_at::next_run_at(&s.trigger, &s.timezone, now)?;
        let conn = self.lock()?;
        conn.execute(
            "UPDATE schedules
                SET status = 'active', next_run_at = ?1
              WHERE id = ?2",
            params![next, id],
        ).map_err(|e| MiraError::DatabaseError(format!("resume_schedule: {e}")))?;
        drop(conn);
        self.get_schedule(id)?.ok_or_else(|| MiraError::DatabaseError(
            "resume_schedule: row vanished".into()
        ))
    }

    // Snooze: bump `next_run_at` to `until` (clamped to ≥ now). Useful for
    // a quick "remind me in 30m" without pausing/resuming. Status stays
    // active so the worker fires at the new time.
    pub fn snooze_schedule(&self, id: &str, until: i64) -> Result<Schedule, MiraError> {
        let now = Utc::now().timestamp();
        let target = until.max(now);
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE schedules
                SET next_run_at = ?1, status = 'active'
              WHERE id = ?2 AND status IN ('active', 'paused')",
            params![target, id],
        ).map_err(|e| MiraError::DatabaseError(format!("snooze_schedule: {e}")))?;
        drop(conn);
        if n == 0 {
            return Err(MiraError::ConfigError(format!(
                "schedule {id} not found or not snoozable"
            )));
        }
        self.get_schedule(id)?.ok_or_else(|| MiraError::DatabaseError(
            "snooze_schedule: row vanished".into()
        ))
    }

    // Delete a schedule. System-owned rows are protected — those are seeded
    // on every boot, so deletion would just resurrect them on restart. The
    // caller (HTTP handler) reports a 4xx for `system` rows.
    pub fn delete_schedule(&self, id: &str) -> Result<bool, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM schedules WHERE id = ?1 AND owner_kind != 'system'",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("delete_schedule: {e}")))?;
        Ok(n > 0)
    }

    // ── Run audit log ─────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn record_run(
        &self,
        source_kind:    &str,
        source_id:      &str,
        user_id:        &str,
        started_at:     i64,
        finished_at:    Option<i64>,
        outcome:        RunOutcome,
        output_snippet: Option<&str>,
        error:          Option<&str>,
        context_json:   Option<&str>,
    ) -> Result<(), MiraError> {
        let conn = self.lock()?;
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO automation_runs (
                id, source_kind, source_id, user_id,
                started_at, finished_at, outcome,
                output_snippet, error, context
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
             )",
            params![
                id, source_kind, source_id, user_id,
                started_at, finished_at, outcome.as_str(),
                output_snippet, error, context_json,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("record_run: {e}")))?;
        Ok(())
    }

    // Recent runs across all sources, optionally scoped to a user.
    pub fn list_runs(
        &self,
        user_id: Option<&str>,
        limit:   usize,
    ) -> Result<Vec<AutomationRun>, MiraError> {
        self.list_runs_filtered(RunFilter {
            user_id,
            source_kind:    None,
            source_id:      None,
            outcome:        None,
            before_started: None,
            limit,
        })
    }

    // Generic run filter for `/api/automations/runs?source=&id=`. All filter
    // fields are AND-ed; an unset field doesn't restrict.
    pub fn list_runs_filtered(&self, f: RunFilter<'_>) -> Result<Vec<AutomationRun>, MiraError> {
        let mut sql = String::from(
            "SELECT id, source_kind, source_id, user_id, started_at,
                    finished_at, outcome, output_snippet, error, context
               FROM automation_runs
              WHERE 1=1"
        );
        // Build params dynamically; the `?N` placeholders sit in insertion
        // order, so we just count as we go.
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(u) = f.user_id {
            sql.push_str(&format!(" AND user_id = ?{}", args.len() + 1));
            args.push(Box::new(u.to_string()));
        }
        if let Some(k) = f.source_kind {
            sql.push_str(&format!(" AND source_kind = ?{}", args.len() + 1));
            args.push(Box::new(k.to_string()));
        }
        if let Some(id) = f.source_id {
            sql.push_str(&format!(" AND source_id = ?{}", args.len() + 1));
            args.push(Box::new(id.to_string()));
        }
        if let Some(outcome) = f.outcome {
            sql.push_str(&format!(" AND outcome = ?{}", args.len() + 1));
            args.push(Box::new(outcome.to_string()));
        }
        if let Some(before) = f.before_started {
            sql.push_str(&format!(" AND started_at < ?{}", args.len() + 1));
            args.push(Box::new(before));
        }
        sql.push_str(&format!(" ORDER BY started_at DESC LIMIT ?{}", args.len() + 1));
        args.push(Box::new(f.limit as i64));

        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(format!("list_runs prep: {e}")))?;
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), row_to_run)
            .map_err(|e| MiraError::DatabaseError(format!("list_runs q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("list_runs rows: {e}")))?;
        Ok(rows)
    }
}

// ── Webhooks ────────────────────────────────────────────────────────

impl AutomationsStore {
    // Insert a new webhook. Caller supplies everything but the generated
    // `id`, `token`, and `secret`. The returned `Webhook` has `secret`
    // populated (one-time display); subsequent reads omit it.
    pub fn create_webhook(&self, new: NewWebhook) -> Result<Webhook, MiraError> {
        let now    = Utc::now().timestamp();
        let id     = Uuid::new_v4().to_string();
        let token  = gen_token();
        let secret = gen_secret();

        let action_json = serde_json::to_string(&new.action)
            .map_err(|e| MiraError::DatabaseError(format!("action serialise: {e}")))?;
        let predicate_json = match &new.predicate {
            Some(p) => Some(serde_json::to_string(p)
                .map_err(|e| MiraError::DatabaseError(format!("predicate serialise: {e}")))?),
            None => None,
        };
        let rate = new.rate_limit_per_min.unwrap_or(30);
        let status = new.status.unwrap_or(AutomationStatus::Active);

        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO webhooks (
                id, user_id, owner_kind, name, description, rationale,
                token, secret, predicate, payload_template,
                action_kind, action_payload,
                rate_limit_per_min, debounce_secs,
                status, created_at, expires_at, last_seen_at, last_error
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10,
                ?11, ?12,
                ?13, ?14,
                ?15, ?16, ?17, NULL, NULL
             )",
            params![
                id, new.user_id, new.owner_kind.as_str(),
                new.name, new.description, new.rationale,
                token, secret, predicate_json, new.payload_template,
                action_kind_tag(&new.action), action_json,
                rate, new.debounce_secs,
                status.as_str(), now, new.expires_at,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("create_webhook: {e}")))?;

        Ok(Webhook {
            id,
            user_id:            new.user_id,
            owner_kind:         new.owner_kind,
            name:               new.name,
            description:        new.description,
            rationale:          new.rationale,
            token,
            secret:             Some(secret),
            predicate:          new.predicate,
            payload_template:   new.payload_template,
            action:             new.action,
            rate_limit_per_min: rate,
            debounce_secs:      new.debounce_secs,
            status,
            created_at:         now,
            expires_at:         new.expires_at,
            last_seen_at:       None,
            last_error:         None,
        })
    }

    pub fn get_webhook(&self, id: &str) -> Result<Option<Webhook>, MiraError> {
        let conn = self.lock()?;
        conn.query_row(SELECT_WEBHOOK_BY_ID, params![id], row_to_webhook)
            .optional()
            .map_err(|e| MiraError::DatabaseError(format!("get_webhook: {e}")))
    }

    // Token-based lookup for the public POST handler. Returns the row plus
    // the secret needed for HMAC verification — *never* expose this to
    // untrusted consumers.
    pub fn get_webhook_by_token(&self, token: &str) -> Result<Option<(Webhook, String)>, MiraError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, user_id, owner_kind, name, description, rationale,
                    token, secret, predicate, payload_template,
                    action_kind, action_payload,
                    rate_limit_per_min, debounce_secs,
                    status, created_at, expires_at, last_seen_at, last_error
               FROM webhooks
              WHERE token = ?1",
            params![token],
            |r| {
                let secret: String = r.get(7)?;
                let mut wh = row_to_webhook(r)?;
                wh.secret = None;
                Ok((wh, secret))
            },
        ).optional()
         .map_err(|e| MiraError::DatabaseError(format!("get_webhook_by_token: {e}")))
    }

    pub fn list_webhooks(&self, user_id: Option<&str>) -> Result<Vec<Webhook>, MiraError> {
        let conn = self.lock()?;
        let rows = match user_id {
            Some(u) => {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, owner_kind, name, description, rationale,
                            token, secret, predicate, payload_template,
                            action_kind, action_payload,
                            rate_limit_per_min, debounce_secs,
                            status, created_at, expires_at, last_seen_at, last_error
                       FROM webhooks
                      WHERE user_id = ?1
                      ORDER BY created_at DESC"
                ).map_err(|e| MiraError::DatabaseError(format!("list webhooks prep: {e}")))?;
                stmt.query_map(params![u], row_to_webhook)
                    .map_err(|e| MiraError::DatabaseError(format!("list webhooks q: {e}")))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| MiraError::DatabaseError(format!("list webhooks rows: {e}")))?
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, owner_kind, name, description, rationale,
                            token, secret, predicate, payload_template,
                            action_kind, action_payload,
                            rate_limit_per_min, debounce_secs,
                            status, created_at, expires_at, last_seen_at, last_error
                       FROM webhooks
                      ORDER BY created_at DESC"
                ).map_err(|e| MiraError::DatabaseError(format!("list webhooks prep: {e}")))?;
                stmt.query_map([], row_to_webhook)
                    .map_err(|e| MiraError::DatabaseError(format!("list webhooks q: {e}")))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| MiraError::DatabaseError(format!("list webhooks rows: {e}")))?
            }
        };
        Ok(rows)
    }

    pub fn update_webhook(&self, id: &str, upd: UpdateWebhook) -> Result<Webhook, MiraError> {
        let action_json = serde_json::to_string(&upd.action)
            .map_err(|e| MiraError::DatabaseError(format!("action serialise: {e}")))?;
        let predicate_json = match &upd.predicate {
            Some(p) => Some(serde_json::to_string(p)
                .map_err(|e| MiraError::DatabaseError(format!("predicate serialise: {e}")))?),
            None => None,
        };

        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE webhooks
                SET name               = ?1,
                    description        = ?2,
                    rationale          = ?3,
                    predicate          = ?4,
                    payload_template   = ?5,
                    action_kind        = ?6,
                    action_payload     = ?7,
                    rate_limit_per_min = COALESCE(?8, rate_limit_per_min),
                    debounce_secs      = ?9,
                    expires_at         = ?10
              WHERE id = ?11",
            params![
                upd.name, upd.description, upd.rationale,
                predicate_json, upd.payload_template,
                action_kind_tag(&upd.action), action_json,
                upd.rate_limit_per_min, upd.debounce_secs,
                upd.expires_at, id,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("update_webhook: {e}")))?;
        if n == 0 {
            return Err(MiraError::ConfigError(format!("webhook {id} not found")));
        }
        drop(conn);
        self.get_webhook(id)?.ok_or_else(|| MiraError::DatabaseError(
            "update_webhook: row vanished".into()
        ))
    }

    // Generate and persist a fresh secret. Returns the new secret string —
    // shown once to the caller, then zeroised from API responses.
    pub fn rotate_webhook_secret(&self, id: &str) -> Result<String, MiraError> {
        let secret = gen_secret();
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE webhooks SET secret = ?1 WHERE id = ?2",
            params![secret, id],
        ).map_err(|e| MiraError::DatabaseError(format!("rotate_secret: {e}")))?;
        if n == 0 {
            return Err(MiraError::ConfigError(format!("webhook {id} not found")));
        }
        Ok(secret)
    }

    // Generate and persist a fresh public token (URL changes!). The old
    // URL stops resolving immediately; callers must redistribute.
    pub fn rotate_webhook_token(&self, id: &str) -> Result<String, MiraError> {
        let token = gen_token();
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE webhooks SET token = ?1 WHERE id = ?2",
            params![token, id],
        ).map_err(|e| MiraError::DatabaseError(format!("rotate_token: {e}")))?;
        if n == 0 {
            return Err(MiraError::ConfigError(format!("webhook {id} not found")));
        }
        Ok(token)
    }

    pub fn pause_webhook(&self, id: &str) -> Result<Webhook, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE webhooks SET status = 'paused' WHERE id = ?1 AND status IN ('active', 'paused')",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("pause_webhook: {e}")))?;
        drop(conn);
        if n == 0 {
            return Err(MiraError::ConfigError(format!("webhook {id} not pausable")));
        }
        self.get_webhook(id)?.ok_or_else(|| MiraError::DatabaseError("pause_webhook vanished".into()))
    }

    pub fn resume_webhook(&self, id: &str) -> Result<Webhook, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE webhooks SET status = 'active' WHERE id = ?1",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("resume_webhook: {e}")))?;
        drop(conn);
        if n == 0 {
            return Err(MiraError::ConfigError(format!("webhook {id} not found")));
        }
        self.get_webhook(id)?.ok_or_else(|| MiraError::DatabaseError("resume_webhook vanished".into()))
    }

    pub fn delete_webhook(&self, id: &str) -> Result<bool, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM webhooks WHERE id = ?1",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("delete_webhook: {e}")))?;
        Ok(n > 0)
    }

    pub fn touch_webhook(&self, id: &str, when: i64, error: Option<&str>) -> Result<(), MiraError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE webhooks SET last_seen_at = ?1, last_error = ?2 WHERE id = ?3",
            params![when, error, id],
        ).map_err(|e| MiraError::DatabaseError(format!("touch_webhook: {e}")))?;
        Ok(())
    }

    // Append a payload to the per-webhook ring (cap = 5 most recent kept).
    pub fn append_webhook_payload(
        &self,
        webhook_id:   &str,
        received_at:  i64,
        headers_json: &str,
        body:         &str,
        matched:      bool,
    ) -> Result<(), MiraError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO webhook_payloads
                (webhook_id, received_at, headers_json, body, matched)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![webhook_id, received_at, headers_json, body, matched as i64],
        ).map_err(|e| MiraError::DatabaseError(format!("append_payload: {e}")))?;
        // Trim to last 5 entries.
        conn.execute(
            "DELETE FROM webhook_payloads
              WHERE webhook_id = ?1
                AND id NOT IN (
                    SELECT id FROM webhook_payloads
                     WHERE webhook_id = ?1
                     ORDER BY received_at DESC
                     LIMIT 5
                )",
            params![webhook_id],
        ).map_err(|e| MiraError::DatabaseError(format!("trim_payloads: {e}")))?;
        Ok(())
    }

    pub fn list_webhook_payloads(&self, webhook_id: &str) -> Result<Vec<WebhookPayload>, MiraError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, webhook_id, received_at, headers_json, body, matched
               FROM webhook_payloads
              WHERE webhook_id = ?1
              ORDER BY received_at DESC"
        ).map_err(|e| MiraError::DatabaseError(format!("list_payloads prep: {e}")))?;
        let rows = stmt.query_map(params![webhook_id], |r| {
            let matched_i: i64 = r.get(5)?;
            Ok(WebhookPayload {
                id:           r.get(0)?,
                webhook_id:   r.get(1)?,
                received_at:  r.get(2)?,
                headers_json: r.get(3)?,
                body:         r.get(4)?,
                matched:      matched_i != 0,
            })
        }).map_err(|e| MiraError::DatabaseError(format!("list_payloads q: {e}")))?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(|e| MiraError::DatabaseError(format!("list_payloads rows: {e}")))?;
        Ok(rows)
    }

    // ── Event subscriptions ─────────────────────────────────────────────────

    // Idempotent insert keyed by `(owner_kind=system, user_id, name)`.
    // Used by W1 watchdog seeding so re-running boot doesn't duplicate
    // the auto-routing subscription. Re-runs leave the existing row
    // untouched (operator edits like pausing the row survive).
    pub fn ensure_system_event_subscription(
        &self,
        new: NewEventSubscription,
    ) -> Result<(), MiraError> {
        let exists = {
            let conn = self.lock()?;
            conn.query_row(
                "SELECT 1 FROM event_subscriptions
                  WHERE owner_kind = 'system' AND user_id = ?1 AND name = ?2",
                params![new.user_id, new.name],
                |_| Ok(()),
            ).optional()
             .map_err(|e| MiraError::DatabaseError(format!("ensure check: {e}")))?
             .is_some()
        };
        if exists { return Ok(()); }
        self.create_event_subscription(new)?;
        Ok(())
    }

    pub fn create_event_subscription(
        &self,
        new: NewEventSubscription,
    ) -> Result<EventSubscription, MiraError> {
        let now = Utc::now().timestamp();
        let id  = Uuid::new_v4().to_string();
        let action_json = serde_json::to_string(&new.action)
            .map_err(|e| MiraError::DatabaseError(format!("action serialise: {e}")))?;
        let predicate_json = match &new.predicate {
            Some(p) => Some(serde_json::to_string(p)
                .map_err(|e| MiraError::DatabaseError(format!("predicate serialise: {e}")))?),
            None => None,
        };
        let status = new.status.unwrap_or(AutomationStatus::Active);

        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO event_subscriptions (
                id, user_id, owner_kind, name, description, rationale,
                event_name, predicate, action_kind, action_payload,
                status, created_at, expires_at, last_fired_at, last_error,
                delete_after_fire
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, NULL, NULL,
                ?14
             )",
            params![
                id, new.user_id, new.owner_kind.as_str(),
                new.name, new.description, new.rationale,
                new.event_name, predicate_json,
                action_kind_tag(&new.action), action_json,
                status.as_str(), now, new.expires_at,
                new.delete_after_fire as i64,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("create_event_sub: {e}")))?;

        Ok(EventSubscription {
            id,
            user_id:       new.user_id,
            owner_kind:    new.owner_kind,
            name:          new.name,
            description:   new.description,
            rationale:     new.rationale,
            event_name:    new.event_name,
            predicate:     new.predicate,
            action:        new.action,
            status,
            created_at:    now,
            expires_at:    new.expires_at,
            last_fired_at: None,
            last_error:    None,
            delete_after_fire: new.delete_after_fire,
        })
    }

    pub fn get_event_subscription(&self, id: &str) -> Result<Option<EventSubscription>, MiraError> {
        let conn = self.lock()?;
        conn.query_row(SELECT_EVENT_SUB_BY_ID, params![id], row_to_event_sub)
            .optional()
            .map_err(|e| MiraError::DatabaseError(format!("get_event_sub: {e}")))
    }

    pub fn list_event_subscriptions(
        &self,
        user_id: Option<&str>,
    ) -> Result<Vec<EventSubscription>, MiraError> {
        let conn = self.lock()?;
        let rows = match user_id {
            Some(u) => {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, owner_kind, name, description, rationale,
                            event_name, predicate, action_kind, action_payload,
                            status, created_at, expires_at, last_fired_at, last_error,
                            delete_after_fire
                       FROM event_subscriptions
                      WHERE user_id = ?1
                      ORDER BY created_at DESC"
                ).map_err(|e| MiraError::DatabaseError(format!("list ev prep: {e}")))?;
                stmt.query_map(params![u], row_to_event_sub)
                    .map_err(|e| MiraError::DatabaseError(format!("list ev q: {e}")))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| MiraError::DatabaseError(format!("list ev rows: {e}")))?
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, owner_kind, name, description, rationale,
                            event_name, predicate, action_kind, action_payload,
                            status, created_at, expires_at, last_fired_at, last_error,
                            delete_after_fire
                       FROM event_subscriptions
                      ORDER BY created_at DESC"
                ).map_err(|e| MiraError::DatabaseError(format!("list ev prep: {e}")))?;
                stmt.query_map([], row_to_event_sub)
                    .map_err(|e| MiraError::DatabaseError(format!("list ev q: {e}")))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| MiraError::DatabaseError(format!("list ev rows: {e}")))?
            }
        };
        Ok(rows)
    }

    // Active subs filtered by event name — the subscriber loop's hot
    // query. Status filter is applied here so paused rows stay quiet.
    pub fn active_subscriptions_for(
        &self,
        event_name: &str,
    ) -> Result<Vec<EventSubscription>, MiraError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, owner_kind, name, description, rationale,
                    event_name, predicate, action_kind, action_payload,
                    status, created_at, expires_at, last_fired_at, last_error,
                    delete_after_fire
               FROM event_subscriptions
              WHERE event_name = ?1 AND status = 'active'"
        ).map_err(|e| MiraError::DatabaseError(format!("active subs prep: {e}")))?;
        let rows = stmt.query_map(params![event_name], row_to_event_sub)
            .map_err(|e| MiraError::DatabaseError(format!("active subs q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("active subs rows: {e}")))?;
        Ok(rows)
    }

    pub fn update_event_subscription(
        &self,
        id:  &str,
        upd: UpdateEventSubscription,
    ) -> Result<EventSubscription, MiraError> {
        let action_json = serde_json::to_string(&upd.action)
            .map_err(|e| MiraError::DatabaseError(format!("action serialise: {e}")))?;
        let predicate_json = match &upd.predicate {
            Some(p) => Some(serde_json::to_string(p)
                .map_err(|e| MiraError::DatabaseError(format!("predicate serialise: {e}")))?),
            None => None,
        };
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE event_subscriptions
                SET name           = ?1,
                    description    = ?2,
                    rationale      = ?3,
                    event_name     = ?4,
                    predicate      = ?5,
                    action_kind    = ?6,
                    action_payload = ?7,
                    expires_at     = ?8
              WHERE id = ?9",
            params![
                upd.name, upd.description, upd.rationale,
                upd.event_name, predicate_json,
                action_kind_tag(&upd.action), action_json,
                upd.expires_at, id,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("update_event_sub: {e}")))?;
        if n == 0 {
            return Err(MiraError::ConfigError(format!("event subscription {id} not found")));
        }
        drop(conn);
        self.get_event_subscription(id)?.ok_or_else(|| MiraError::DatabaseError(
            "update_event_sub: row vanished".into()
        ))
    }

    pub fn pause_event_subscription(&self, id: &str) -> Result<EventSubscription, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE event_subscriptions SET status = 'paused' WHERE id = ?1",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("pause_event_sub: {e}")))?;
        drop(conn);
        if n == 0 {
            return Err(MiraError::ConfigError(format!("event subscription {id} not found")));
        }
        self.get_event_subscription(id)?.ok_or_else(|| MiraError::DatabaseError("pause_event_sub vanished".into()))
    }

    pub fn resume_event_subscription(&self, id: &str) -> Result<EventSubscription, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE event_subscriptions SET status = 'active' WHERE id = ?1",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("resume_event_sub: {e}")))?;
        drop(conn);
        if n == 0 {
            return Err(MiraError::ConfigError(format!("event subscription {id} not found")));
        }
        self.get_event_subscription(id)?.ok_or_else(|| MiraError::DatabaseError("resume_event_sub vanished".into()))
    }

    pub fn delete_event_subscription(&self, id: &str) -> Result<bool, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM event_subscriptions WHERE id = ?1",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("delete_event_sub: {e}")))?;
        Ok(n > 0)
    }

    pub fn touch_event_subscription(
        &self,
        id:    &str,
        when:  i64,
        error: Option<&str>,
    ) -> Result<(), MiraError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE event_subscriptions
                SET last_fired_at = ?1, last_error = ?2
              WHERE id = ?3",
            params![when, error, id],
        ).map_err(|e| MiraError::DatabaseError(format!("touch_event_sub: {e}")))?;
        Ok(())
    }

    // Find auto-delivery subscriptions registered by `spawn_background_task`
    // whose worker never reached a terminal state — typically because the
    // supervisor process was killed (service restart, crash) before the
    // `agent.worker.completed` event could fire. Identified by:
    // - owner_kind = 'agent'
    // - event_name = `agent.worker.completed`
    // - status     = 'active'
    // - last_fired_at IS NULL
    //     // Subscriptions whose `last_fired_at` is older than `fired_cutoff`
    // AND that are unambiguously one-shot: either they were created
    // with `delete_after_fire=1` (the post-0.103.0 path), or they
    // match the spawn-style legacy shape — agent-owned, on
    // `agent.worker.completed`, with a predicate keyed on a unique
    // `task_id`. That predicate can never match again once the
    // completion event has fired, so the row is dead weight even
    // without the explicit flag.
    //     // Used by the `automations_cleanup` heartbeat to prune both
    // modern flagged rows whose inline tear-down was skipped (e.g.
    // dispatch error path) and pre-0.103.0 rows that never had the
    // flag in the first place.
    pub fn list_dead_after_fire_subscriptions(
        &self,
        fired_cutoff: i64,
    ) -> Result<Vec<EventSubscription>, MiraError> {
        use crate::events::names::AGENT_WORKER_COMPLETED;
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, owner_kind, name, description, rationale,
                    event_name, predicate, action_kind, action_payload,
                    status, created_at, expires_at, last_fired_at, last_error,
                    delete_after_fire
               FROM event_subscriptions
              WHERE last_fired_at IS NOT NULL
                AND last_fired_at < ?1
                AND (
                      delete_after_fire = 1
                      OR (owner_kind = 'agent' AND event_name = ?2)
                    )"
        ).map_err(|e| MiraError::DatabaseError(format!("dead-after-fire prep: {e}")))?;
        let rows = stmt.query_map(params![fired_cutoff, AGENT_WORKER_COMPLETED], row_to_event_sub)
            .map_err(|e| MiraError::DatabaseError(format!("dead-after-fire q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("dead-after-fire rows: {e}")))?;
        Ok(rows)
    }

    // Active agent-owned `agent.worker.completed` subscriptions that
    // were created more than `orphan_cutoff` seconds ago and have
    // never fired. Live-uptime analogue of `list_orphan_completion_
    // subscriptions`, which only runs at boot.
    pub fn list_stuck_completion_subscriptions(
        &self,
        orphan_cutoff: i64,
    ) -> Result<Vec<EventSubscription>, MiraError> {
        use crate::events::names::AGENT_WORKER_COMPLETED;
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, owner_kind, name, description, rationale,
                    event_name, predicate, action_kind, action_payload,
                    status, created_at, expires_at, last_fired_at, last_error,
                    delete_after_fire
               FROM event_subscriptions
              WHERE owner_kind    = 'agent'
                AND event_name    = ?1
                AND status        = 'active'
                AND last_fired_at IS NULL
                AND created_at    < ?2"
        ).map_err(|e| MiraError::DatabaseError(format!("stuck completion prep: {e}")))?;
        let rows = stmt.query_map(params![AGENT_WORKER_COMPLETED, orphan_cutoff], row_to_event_sub)
            .map_err(|e| MiraError::DatabaseError(format!("stuck completion q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("stuck completion rows: {e}")))?;
        Ok(rows)
    }

    // Called once at startup so the user gets a one-shot "task abandoned"
    // notification instead of the subscription waiting forever for an
    // event that will never come.
    pub fn list_orphan_completion_subscriptions(
        &self,
    ) -> Result<Vec<EventSubscription>, MiraError> {
        use crate::events::names::AGENT_WORKER_COMPLETED;
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, owner_kind, name, description, rationale,
                    event_name, predicate, action_kind, action_payload,
                    status, created_at, expires_at, last_fired_at, last_error,
                    delete_after_fire
               FROM event_subscriptions
              WHERE owner_kind    = 'agent'
                AND event_name    = ?1
                AND status        = 'active'
                AND last_fired_at IS NULL"
        ).map_err(|e| MiraError::DatabaseError(format!("orphan subs prep: {e}")))?;
        let rows = stmt.query_map(params![AGENT_WORKER_COMPLETED], row_to_event_sub)
            .map_err(|e| MiraError::DatabaseError(format!("orphan subs q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("orphan subs rows: {e}")))?;
        Ok(rows)
    }

    // Mark a subscription as terminally failed so it won't dispatch again
    // on subsequent restarts. Used by the orphan sweep after the one-shot
    // "abandoned" notification has been delivered.
    pub fn fail_event_subscription(
        &self,
        id:    &str,
        when:  i64,
        error: &str,
    ) -> Result<(), MiraError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE event_subscriptions
                SET status        = 'failed',
                    last_fired_at = ?1,
                    last_error    = ?2
              WHERE id = ?3",
            params![when, error, id],
        ).map_err(|e| MiraError::DatabaseError(format!("fail_event_sub: {e}")))?;
        Ok(())
    }

    // ── Slice W3: watchdog incidents ─────────────────────────────────────

    // Persist one watchdog alert as a referenceable incident before
    // the bus event is emitted, so the auto-routed ChannelMessage
    // can include a stable `/incidents/<id>` link. Returns the new id.
    pub fn create_watchdog_incident(
        &self,
        new: NewWatchdogIncident,
    ) -> Result<String, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO watchdog_incidents
               (id, user_id, fingerprint, severity, source, module,
                message, payload_json, created_at, analysis_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'none')",
            params![
                id, new.user_id, new.fingerprint, new.severity,
                new.source, new.module, new.message,
                new.payload_json, now,
            ],
        ).map_err(|e| MiraError::DatabaseError(format!("create_incident: {e}")))?;
        Ok(id)
    }

    pub fn get_watchdog_incident(
        &self, id: &str,
    ) -> Result<Option<WatchdogIncident>, MiraError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, fingerprint, severity, source, module,
                    message, payload_json, created_at, analysis_status,
                    analysis_started_at, analysis_completed_at,
                    conversation_id, analysis_response
               FROM watchdog_incidents WHERE id = ?1"
        ).map_err(|e| MiraError::DatabaseError(format!("get_incident prep: {e}")))?;
        stmt.query_row(params![id], row_to_incident)
            .optional()
            .map_err(|e| MiraError::DatabaseError(format!("get_incident: {e}")))
    }

    // Most recent N incidents for a user, newest first. Used by the
    // (admin-only) listing endpoint.
    pub fn list_watchdog_incidents(
        &self, user_id: &str, limit: usize,
    ) -> Result<Vec<WatchdogIncident>, MiraError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, fingerprint, severity, source, module,
                    message, payload_json, created_at, analysis_status,
                    analysis_started_at, analysis_completed_at,
                    conversation_id, analysis_response
               FROM watchdog_incidents
              WHERE user_id = ?1
              ORDER BY created_at DESC LIMIT ?2"
        ).map_err(|e| MiraError::DatabaseError(format!("list_incidents prep: {e}")))?;
        let rows = stmt.query_map(
            params![user_id, limit as i64],
            row_to_incident,
        ).map_err(|e| MiraError::DatabaseError(format!("list_incidents q: {e}")))?
         .collect::<rusqlite::Result<Vec<_>>>()
         .map_err(|e| MiraError::DatabaseError(format!("list_incidents rows: {e}")))?;
        Ok(rows)
    }

    // Flip status to `queued` and stamp `analysis_started_at`. Idempotent —
    // re-calling on an already-queued / -completed row no-ops (returns
    // `Ok(false)`) so the analyze endpoint can be safely retried.
    pub fn mark_incident_analysis_queued(
        &self, id: &str, conv_id: &str,
    ) -> Result<bool, MiraError> {
        let now = Utc::now().timestamp();
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE watchdog_incidents
                SET analysis_status = 'queued',
                    analysis_started_at = ?1,
                    conversation_id = ?2
              WHERE id = ?3 AND analysis_status = 'none'",
            params![now, conv_id, id],
        ).map_err(|e| MiraError::DatabaseError(format!("mark_queued: {e}")))?;
        Ok(n > 0)
    }

    // Stamp completion timestamp + the analysis text. Called by the
    // background task that consumed the agent stream.
    pub fn mark_incident_analysis_completed(
        &self, id: &str, response: &str,
    ) -> Result<(), MiraError> {
        let now = Utc::now().timestamp();
        let conn = self.lock()?;
        conn.execute(
            "UPDATE watchdog_incidents
                SET analysis_status = 'completed',
                    analysis_completed_at = ?1,
                    analysis_response = ?2
              WHERE id = ?3",
            params![now, response, id],
        ).map_err(|e| MiraError::DatabaseError(format!("mark_completed: {e}")))?;
        Ok(())
    }

    // How many incidents share `fingerprint` and were created on or
    // after `since`. Used by the health-audit collector to dedup —
    // the watchdog itself dedups in-memory, but health audit fires
    // only hourly, so cross-run dedup needs a persisted lookup.
    pub fn count_incidents_by_fingerprint_since(
        &self, fingerprint: &str, since: i64,
    ) -> Result<usize, MiraError> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM watchdog_incidents
              WHERE fingerprint = ?1 AND created_at >= ?2",
            params![fingerprint, since],
            |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("count_incidents_by_fp: {e}")))?;
        Ok(n as usize)
    }

    // (total_runs, failure_runs) since `since` (unix seconds). Used by
    // the health-audit failure-rate detector — cheaper than pulling
    // every row through `list_runs_filtered`.
    pub fn count_runs_since(&self, since: i64) -> Result<(usize, usize), MiraError> {
        let conn = self.lock()?;
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM automation_runs WHERE started_at >= ?1",
            params![since], |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("count_runs total: {e}")))?;
        let failures: i64 = conn.query_row(
            "SELECT COUNT(*) FROM automation_runs
              WHERE started_at >= ?1 AND outcome = 'failure'",
            params![since], |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("count_runs failures: {e}")))?;
        Ok((total as usize, failures as usize))
    }

    // 0.109.0 — count automation_runs that started in the window
    // `[since, until]` and have no `finished_at` yet. Excludes the
    // last 30s by convention so a run that's actively in flight isn't
    // flagged as orphaned. Used by `automations.runs_with_no_outcome_1h`.
    pub fn count_runs_unfinished_in_window(
        &self, since: i64, until: i64,
    ) -> Result<usize, MiraError> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM automation_runs
              WHERE started_at >= ?1 AND started_at <= ?2 AND finished_at IS NULL",
            params![since, until], |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("count_runs_unfinished: {e}")))?;
        Ok(n as usize)
    }

    // (total_runs, failure_runs) for the named event_subscription since
    // `since`. Used by the watchdog dispatch-failure-rate detector to
    // gauge whether watchdog.alert deliveries are getting through.
    // Matches the subscription via stable name (`watchdog.alert delivery`),
    // since IDs aren't predictable across reseeds.
    pub fn count_runs_for_event_sub_named_since(
        &self, sub_name: &str, since: i64,
    ) -> Result<(usize, usize), MiraError> {
        let conn = self.lock()?;
        // Resolve sub id by name. Multiple matching rows = ambiguous;
        // sum across them so an accidental duplicate seed still gets
        // counted accurately.
        let mut stmt = conn.prepare(
            "SELECT id FROM event_subscriptions WHERE name = ?1",
        ).map_err(|e| MiraError::DatabaseError(format!("count_runs_named prep: {e}")))?;
        let ids: Vec<String> = stmt.query_map(params![sub_name], |r| r.get::<_, String>(0))
            .map_err(|e| MiraError::DatabaseError(format!("count_runs_named ids: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("count_runs_named rows: {e}")))?;
        if ids.is_empty() { return Ok((0, 0)); }
        let placeholders: String = (1..=ids.len()).map(|i| format!("?{}", i + 1)).collect::<Vec<_>>().join(",");
        let total_sql = format!(
            "SELECT COUNT(*) FROM automation_runs
               WHERE started_at >= ?1 AND source_kind = 'event' AND source_id IN ({})",
            placeholders,
        );
        let fail_sql = format!(
            "SELECT COUNT(*) FROM automation_runs
               WHERE started_at >= ?1 AND source_kind = 'event'
                 AND outcome = 'failure' AND source_id IN ({})",
            placeholders,
        );
        // Build the param vec: since + ids.
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(1 + ids.len());
        bind.push(Box::new(since));
        for i in &ids { bind.push(Box::new(i.clone())); }
        let bind_refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
        let total: i64 = conn.query_row(&total_sql, bind_refs.as_slice(), |r| r.get(0))
            .map_err(|e| MiraError::DatabaseError(format!("count_runs_named total: {e}")))?;
        let failures: i64 = conn.query_row(&fail_sql, bind_refs.as_slice(), |r| r.get(0))
            .map_err(|e| MiraError::DatabaseError(format!("count_runs_named fail: {e}")))?;
        Ok((total as usize, failures as usize))
    }

    // Watchdog incidents whose `analysis_status` is `queued` or
    // `in_progress` and whose `analysis_started_at` is older than
    // `cutoff`. Used by the slice-2 stuck-analysis detector. Caller
    // can iterate the result to flip them via
    // `mark_incident_analysis_failed`.
    pub fn list_stuck_incident_analyses(
        &self, cutoff: i64,
    ) -> Result<Vec<WatchdogIncident>, MiraError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, fingerprint, severity, source, module,
                    message, payload_json, created_at, analysis_status,
                    analysis_started_at, analysis_completed_at,
                    conversation_id, analysis_response
               FROM watchdog_incidents
              WHERE analysis_status IN ('queued','in_progress')
                AND analysis_started_at IS NOT NULL
                AND analysis_started_at < ?1
              ORDER BY analysis_started_at ASC",
        ).map_err(|e| MiraError::DatabaseError(format!("list_stuck_analyses prep: {e}")))?;
        let rows = stmt.query_map(params![cutoff], row_to_incident)
            .map_err(|e| MiraError::DatabaseError(format!("list_stuck_analyses q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("list_stuck_analyses rows: {e}")))?;
        Ok(rows)
    }

    // Reset a stuck analysis row to `failed` so the user can re-run
    // it via the analyze endpoint. Used by the slice-2 auto-action.
    pub fn mark_incident_analysis_failed(
        &self, id: &str, reason: &str,
    ) -> Result<bool, MiraError> {
        let now = Utc::now().timestamp();
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE watchdog_incidents
                SET analysis_status        = 'failed',
                    analysis_completed_at  = ?1,
                    analysis_response      = COALESCE(analysis_response, ?2)
              WHERE id = ?3 AND analysis_status IN ('queued','in_progress')",
            params![now, format!("(auto-reset by health audit: {reason})"), id],
        ).map_err(|e| MiraError::DatabaseError(format!("mark_incident_analysis_failed: {e}")))?;
        Ok(n > 0)
    }

    // 0.108.0 — distinct user_ids referenced by user-owned automations
    // (schedules + event_subscriptions + webhooks). Used by the
    // `consistency.automations_for_deleted_users` detector to
    // cross-check against the auth.db users table. System-owned rows
    // (user_id='system') are excluded.
    pub fn distinct_user_ids_referenced(&self) -> Result<Vec<String>, MiraError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT user_id FROM (
                SELECT user_id FROM schedules            WHERE user_id != 'system'
                UNION
                SELECT user_id FROM event_subscriptions  WHERE user_id != 'system'
                UNION
                SELECT user_id FROM webhooks             WHERE user_id != 'system'
            )",
        ).map_err(|e| MiraError::DatabaseError(format!("distinct_user_ids prep: {e}")))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| MiraError::DatabaseError(format!("distinct_user_ids q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("distinct_user_ids rows: {e}")))?;
        Ok(rows)
    }

    // 0.108.0 — count of automation rows owned by `user_id` across
    // all three automation tables. Helper for the orphaned-user
    // detector's payload + the sweep auto-action.
    pub fn count_automations_for_user(&self, user_id: &str) -> Result<usize, MiraError> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM schedules           WHERE user_id = ?1) +
                    (SELECT COUNT(*) FROM event_subscriptions WHERE user_id = ?1) +
                    (SELECT COUNT(*) FROM webhooks            WHERE user_id = ?1)",
            params![user_id], |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("count_automations_for_user: {e}")))?;
        Ok(n as usize)
    }

    // 0.108.0 — wipe every automation row owned by `user_id`. Returns
    // (schedules_deleted, subs_deleted, webhooks_deleted). Used by the
    // orphaned-user sweep auto-action.
    pub fn delete_automations_for_user(
        &self, user_id: &str,
    ) -> Result<(usize, usize, usize), MiraError> {
        let conn = self.lock()?;
        let s = conn.execute(
            "DELETE FROM schedules WHERE user_id = ?1", params![user_id],
        ).map_err(|e| MiraError::DatabaseError(format!("delete schedules: {e}")))?;
        let e = conn.execute(
            "DELETE FROM event_subscriptions WHERE user_id = ?1", params![user_id],
        ).map_err(|e| MiraError::DatabaseError(format!("delete subs: {e}")))?;
        let w = conn.execute(
            "DELETE FROM webhooks WHERE user_id = ?1", params![user_id],
        ).map_err(|e| MiraError::DatabaseError(format!("delete webhooks: {e}")))?;
        Ok((s, e, w))
    }

    // 0.109.0 — pull every system_health-sourced incident filed since
    // `since`. Used by the weekly digest heartbeat to roll up the
    // week's signals.
    pub fn list_health_incidents_since(
        &self, since: i64,
    ) -> Result<Vec<WatchdogIncident>, MiraError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_id, fingerprint, severity, source, module,
                    message, payload_json, created_at, analysis_status,
                    analysis_started_at, analysis_completed_at,
                    conversation_id, analysis_response
               FROM watchdog_incidents
              WHERE source IN ('system_health', 'system_health_digest')
                AND created_at >= ?1
              ORDER BY created_at DESC",
        ).map_err(|e| MiraError::DatabaseError(format!("list_health_incidents prep: {e}")))?;
        let rows = stmt.query_map(params![since], row_to_incident)
            .map_err(|e| MiraError::DatabaseError(format!("list_health_incidents q: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| MiraError::DatabaseError(format!("list_health_incidents rows: {e}")))?;
        Ok(rows)
    }

    // 0.108.0 — last successful run time of a system schedule, by
    // schedule name. Used by `memory.rollup_lag_hours` (last success
    // of `heartbeat.conversation_rollup`). Returns None when the
    // schedule has never run successfully.
    pub fn last_success_at_for_schedule_named(
        &self, schedule_name: &str,
    ) -> Result<Option<i64>, MiraError> {
        let conn = self.lock()?;
        let row: Option<i64> = conn.query_row(
            "SELECT MAX(r.started_at)
               FROM automation_runs r
               JOIN schedules s ON s.id = r.source_id
              WHERE r.outcome = 'success' AND r.source_kind = 'schedule'
                AND s.name = ?1",
            params![schedule_name], |r| r.get(0),
        ).ok();
        Ok(row)
    }

    // Set `next_run_at` to now for the schedule with the given name.
    // Used by the dashboard "Run audit now" button to force the next
    // dispatcher tick to fire that schedule. Returns whether a row
    // was updated (false if no schedule has the requested name).
    pub fn force_schedule_next_run(&self, name: &str) -> Result<bool, MiraError> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE schedules SET next_run_at = ?1 WHERE name = ?2",
            params![now, name],
        ).map_err(|e| MiraError::DatabaseError(format!("force_schedule_next_run: {e}")))?;
        Ok(n > 0)
    }

    // Current `status` of a named `system` schedule (e.g. `heartbeat.watchdog`),
    // or `None` if no such row exists. Used by health detectors to tell a
    // deliberately-disabled subsystem (paused/absent) apart from a genuinely
    // stuck one (active but not running).
    pub fn system_schedule_status_by_name(&self, name: &str) -> Result<Option<String>, MiraError> {
        let conn = self.lock()?;
        let status: Option<String> = conn.query_row(
            "SELECT status FROM schedules WHERE owner_kind = 'system' AND name = ?1",
            params![name],
            |r| r.get::<_, String>(0),
        ).optional().map_err(|e| MiraError::DatabaseError(format!("sched status: {e}")))?;
        Ok(status)
    }

    // Pause a named `system` schedule regardless of its current status (incl.
    // `failed`) and clear its `next_run_at` so `claim_due` stops firing it.
    // Idempotent. Used when a subsystem (e.g. the watchdog) is disabled in
    // config so its seeded heartbeat doesn't keep firing into a hard failure.
    // Returns whether a row was changed.
    pub fn pause_system_schedule_by_name(&self, name: &str) -> Result<bool, MiraError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE schedules SET status = 'paused', next_run_at = NULL
              WHERE owner_kind = 'system' AND name = ?1 AND status != 'paused'",
            params![name],
        ).map_err(|e| MiraError::DatabaseError(format!("pause sched: {e}")))?;
        Ok(n > 0)
    }

    // (fingerprint, count) of the most-frequently-firing incident
    // fingerprint since `since`. Returns None when no incidents in
    // the window. Used by the same-fingerprint detector.
    pub fn top_incident_fingerprint_since(
        &self, since: i64,
    ) -> Result<Option<(String, usize)>, MiraError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fingerprint, COUNT(*) c FROM watchdog_incidents
              WHERE created_at >= ?1
              GROUP BY fingerprint ORDER BY c DESC LIMIT 1",
        ).map_err(|e| MiraError::DatabaseError(format!("top_fp prep: {e}")))?;
        let row: Option<(String, i64)> = stmt.query_row(params![since], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        }).ok();
        Ok(row.map(|(f, c)| (f, c as usize)))
    }

    // ── quota counts ────────────────────────────────────────────

    // Count rows owned by `user_id` (User + Agent owners) toward the quota,
    // in any non-terminal status. `expired`/`failed` schedules don't count
    // since they no longer hold a slot. `pending_approval` *does* count —
    // otherwise an agent could spam pending rows past the cap.
    pub fn count_schedules_for_user(&self, user_id: &str) -> Result<usize, MiraError> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM schedules
              WHERE user_id = ?1
                AND owner_kind IN ('user', 'agent')
                AND status IN ('active', 'paused', 'pending_approval')",
            params![user_id],
            |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("count_schedules: {e}")))?;
        Ok(n as usize)
    }

    pub fn count_webhooks_for_user(&self, user_id: &str) -> Result<usize, MiraError> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM webhooks
              WHERE user_id = ?1
                AND owner_kind IN ('user', 'agent')
                AND status IN ('active', 'paused', 'pending_approval')",
            params![user_id],
            |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("count_webhooks: {e}")))?;
        Ok(n as usize)
    }

    pub fn count_event_subscriptions_for_user(&self, user_id: &str) -> Result<usize, MiraError> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM event_subscriptions
              WHERE user_id = ?1
                AND owner_kind IN ('user', 'agent')
                AND status IN ('active', 'paused', 'pending_approval')",
            params![user_id],
            |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("count_event_subs: {e}")))?;
        Ok(n as usize)
    }

    // ── approve / reject ────────────────────────────────────────

    // Flip a `pending_approval` schedule to `active` and recompute
    // `next_run_at`. Errors if the row isn't pending — keeps the API
    // idempotent-ish (re-approving an active row is a programming bug).
    pub fn approve_schedule(&self, id: &str) -> Result<Schedule, MiraError> {
        let s = self.get_schedule(id)?
            .ok_or_else(|| MiraError::NotFound(format!("schedule {id} not found")))?;
        if !matches!(s.status, ScheduleStatus::PendingApproval) {
            return Err(MiraError::ConfigError(
                format!("approve_schedule: status is {} not pending_approval", s.status.as_str()),
            ));
        }
        let now = chrono::Utc::now().timestamp();
        let next = super::next_run_at::next_run_at(&s.trigger, &s.timezone, now)?;
        let conn = self.lock()?;
        conn.execute(
            "UPDATE schedules SET status = 'active', next_run_at = ?2 WHERE id = ?1",
            params![id, next],
        ).map_err(|e| MiraError::DatabaseError(format!("approve_schedule: {e}")))?;
        drop(conn);
        self.get_schedule(id)?
            .ok_or_else(|| MiraError::NotFound(format!("schedule {id} vanished after approve")))
    }

    pub fn approve_webhook(&self, id: &str) -> Result<Webhook, MiraError> {
        let w = self.get_webhook(id)?
            .ok_or_else(|| MiraError::NotFound(format!("webhook {id} not found")))?;
        if !matches!(w.status, AutomationStatus::PendingApproval) {
            return Err(MiraError::ConfigError(
                format!("approve_webhook: status is {} not pending_approval", w.status.as_str()),
            ));
        }
        let conn = self.lock()?;
        conn.execute(
            "UPDATE webhooks SET status = 'active' WHERE id = ?1",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("approve_webhook: {e}")))?;
        drop(conn);
        self.get_webhook(id)?
            .ok_or_else(|| MiraError::NotFound(format!("webhook {id} vanished after approve")))
    }

    pub fn approve_event_subscription(&self, id: &str) -> Result<EventSubscription, MiraError> {
        let s = self.get_event_subscription(id)?
            .ok_or_else(|| MiraError::NotFound(format!("event subscription {id} not found")))?;
        if !matches!(s.status, AutomationStatus::PendingApproval) {
            return Err(MiraError::ConfigError(
                format!("approve_event_sub: status is {} not pending_approval", s.status.as_str()),
            ));
        }
        let conn = self.lock()?;
        conn.execute(
            "UPDATE event_subscriptions SET status = 'active' WHERE id = ?1",
            params![id],
        ).map_err(|e| MiraError::DatabaseError(format!("approve_event_sub: {e}")))?;
        drop(conn);
        self.get_event_subscription(id)?
            .ok_or_else(|| MiraError::NotFound(format!("event sub {id} vanished after approve")))
    }
}

const SELECT_WEBHOOK_BY_ID: &str = "
    SELECT id, user_id, owner_kind, name, description, rationale,
           token, secret, predicate, payload_template,
           action_kind, action_payload,
           rate_limit_per_min, debounce_secs,
           status, created_at, expires_at, last_seen_at, last_error
      FROM webhooks
     WHERE id = ?1";

const SELECT_EVENT_SUB_BY_ID: &str = "
    SELECT id, user_id, owner_kind, name, description, rationale,
           event_name, predicate, action_kind, action_payload,
           status, created_at, expires_at, last_fired_at, last_error,
           delete_after_fire
      FROM event_subscriptions
     WHERE id = ?1";

fn row_to_webhook(r: &rusqlite::Row) -> rusqlite::Result<Webhook> {
    let action_json: String = r.get(11)?;
    let action: Action = serde_json::from_str(&action_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            11, rusqlite::types::Type::Text, Box::new(e)
        ))?;
    let predicate_str: Option<String> = r.get(8)?;
    let predicate = match predicate_str {
        Some(s) => Some(serde_json::from_str(&s)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                8, rusqlite::types::Type::Text, Box::new(e)
            ))?),
        None => None,
    };
    let owner_s:  String = r.get(2)?;
    let status_s: String = r.get(14)?;
    Ok(Webhook {
        id:                 r.get(0)?,
        user_id:            r.get(1)?,
        owner_kind:         OwnerKind::parse(&owner_s),
        name:               r.get(3)?,
        description:        r.get(4)?,
        rationale:          r.get(5)?,
        token:              r.get(6)?,
        secret:             None,
        predicate,
        payload_template:   r.get(9)?,
        action,
        rate_limit_per_min: r.get(12)?,
        debounce_secs:      r.get(13)?,
        status:             AutomationStatus::parse(&status_s),
        created_at:         r.get(15)?,
        expires_at:         r.get(16)?,
        last_seen_at:       r.get(17)?,
        last_error:         r.get(18)?,
    })
}

// Args to `create_watchdog_incident`.
#[derive(Debug, Clone)]
pub struct NewWatchdogIncident {
    pub user_id:      String,
    pub fingerprint:  String,
    pub severity:     String,
    pub source:       String,
    pub module:       String,
    pub message:      String,
    // Verbatim event payload at the time of detection. JSON string so
    // the analyze endpoint can rehydrate the original context.
    pub payload_json: String,
}

// One row out of `watchdog_incidents`. `analysis_*` fields populate
// over time as the user clicks Analyze and the agent finishes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WatchdogIncident {
    pub id:                    String,
    pub user_id:               String,
    pub fingerprint:           String,
    pub severity:              String,
    pub source:                String,
    pub module:                String,
    pub message:               String,
    pub payload_json:          String,
    pub created_at:            i64,
    // `none` | `queued` | `completed` | `failed`.
    pub analysis_status:       String,
    pub analysis_started_at:   Option<i64>,
    pub analysis_completed_at: Option<i64>,
    pub conversation_id:       Option<String>,
    pub analysis_response:     Option<String>,
}

fn row_to_incident(r: &rusqlite::Row) -> rusqlite::Result<WatchdogIncident> {
    Ok(WatchdogIncident {
        id:                    r.get(0)?,
        user_id:               r.get(1)?,
        fingerprint:           r.get(2)?,
        severity:              r.get(3)?,
        source:                r.get(4)?,
        module:                r.get(5)?,
        message:               r.get(6)?,
        payload_json:          r.get(7)?,
        created_at:            r.get(8)?,
        analysis_status:       r.get(9)?,
        analysis_started_at:   r.get(10)?,
        analysis_completed_at: r.get(11)?,
        conversation_id:       r.get(12)?,
        analysis_response:     r.get(13)?,
    })
}

fn row_to_event_sub(r: &rusqlite::Row) -> rusqlite::Result<EventSubscription> {
    let action_json: String = r.get(9)?;
    let action: Action = serde_json::from_str(&action_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            9, rusqlite::types::Type::Text, Box::new(e)
        ))?;
    let predicate_str: Option<String> = r.get(7)?;
    let predicate = match predicate_str {
        Some(s) => Some(serde_json::from_str(&s)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                7, rusqlite::types::Type::Text, Box::new(e)
            ))?),
        None => None,
    };
    let owner_s:  String = r.get(2)?;
    let status_s: String = r.get(10)?;
    // Column 15 is `delete_after_fire`. Default to false when the
    // SELECT in question doesn't include the column (older queries
    // that haven't been updated yet) — `r.get()` errors with
    // InvalidColumnIndex which we map to false here.
    let delete_after_fire: bool = r.get::<_, Option<i64>>(15).ok().flatten().unwrap_or(0) != 0;
    Ok(EventSubscription {
        id:           r.get(0)?,
        user_id:      r.get(1)?,
        owner_kind:   OwnerKind::parse(&owner_s),
        name:         r.get(3)?,
        description:  r.get(4)?,
        rationale:    r.get(5)?,
        event_name:   r.get(6)?,
        predicate,
        action,
        status:       AutomationStatus::parse(&status_s),
        created_at:   r.get(11)?,
        expires_at:   r.get(12)?,
        last_fired_at: r.get(13)?,
        last_error:   r.get(14)?,
        delete_after_fire,
    })
}

// 64-byte URL-safe token (base64url, no padding) for the public webhook URL.
fn gen_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    base64_url(&buf)
}

// 32-byte HMAC secret (base64url) — distinct from the public token so
// rotating the URL doesn't reveal payload-signing material.
fn gen_secret() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    base64_url(&buf)
}

fn base64_url(b: &[u8]) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(b)
}

// Filter inputs for [`AutomationsStore::list_runs_filtered`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RunFilter<'a> {
    pub user_id:        Option<&'a str>,
    pub source_kind:    Option<&'a str>,
    pub source_id:      Option<&'a str>,
    // Outcome filter (`success` / `failure` / `skipped`). 
    pub outcome:        Option<&'a str>,
    // Cursor: only return runs with `started_at < before_started`. Pairs
    // with `LIMIT` for keyset pagination. 
    pub before_started: Option<i64>,
    pub limit:          usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AutomationRun {
    pub id:             String,
    pub source_kind:    String,
    pub source_id:      String,
    pub user_id:        String,
    pub started_at:     i64,
    pub finished_at:    Option<i64>,
    pub outcome:        String,
    pub output_snippet: Option<String>,
    pub error:          Option<String>,
    pub context:        Option<String>,
}

// ── Migration SQL ────────────────────────────────────────────────────────────

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS schedules (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL,
    owner_kind      TEXT NOT NULL,
    name            TEXT NOT NULL,
    description     TEXT,
    rationale       TEXT,
    schedule_kind   TEXT NOT NULL,
    trigger_spec    TEXT NOT NULL,
    timezone        TEXT NOT NULL DEFAULT 'UTC',
    quiet_hours     TEXT,
    action_kind     TEXT NOT NULL,
    action_payload  TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active',
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER,
    last_run_at     INTEGER,
    next_run_at     INTEGER,
    run_count       INTEGER NOT NULL DEFAULT 0,
    failure_count   INTEGER NOT NULL DEFAULT 0,
    max_failures    INTEGER NOT NULL DEFAULT 5,
    last_error      TEXT
);

-- The hot-path query: "what's due right now?"
CREATE INDEX IF NOT EXISTS idx_schedules_due
    ON schedules(status, next_run_at);

-- Owner lookups for the UI list view.
CREATE INDEX IF NOT EXISTS idx_schedules_owner
    ON schedules(user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS automation_runs (
    id              TEXT PRIMARY KEY,
    source_kind     TEXT NOT NULL,
    source_id       TEXT NOT NULL,
    user_id         TEXT NOT NULL,
    started_at      INTEGER NOT NULL,
    finished_at     INTEGER,
    outcome         TEXT NOT NULL,
    output_snippet  TEXT,
    error           TEXT,
    context         TEXT
);

CREATE INDEX IF NOT EXISTS idx_runs_source
    ON automation_runs(source_kind, source_id, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_runs_user
    ON automation_runs(user_id, started_at DESC);

-- ── Webhooks + event subscriptions ───────────────────────────────
CREATE TABLE IF NOT EXISTS webhooks (
    id                  TEXT PRIMARY KEY,
    user_id             TEXT NOT NULL,
    owner_kind          TEXT NOT NULL,
    name                TEXT NOT NULL,
    description         TEXT,
    rationale           TEXT,
    token               TEXT NOT NULL UNIQUE,
    secret              TEXT NOT NULL,
    predicate           TEXT,
    payload_template    TEXT,
    action_kind         TEXT NOT NULL,
    action_payload      TEXT NOT NULL,
    rate_limit_per_min  INTEGER NOT NULL DEFAULT 30,
    debounce_secs       INTEGER,
    status              TEXT NOT NULL DEFAULT 'active',
    created_at          INTEGER NOT NULL,
    expires_at          INTEGER,
    last_seen_at        INTEGER,
    last_error          TEXT
);

CREATE INDEX IF NOT EXISTS idx_webhooks_owner
    ON webhooks(user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_webhooks_token
    ON webhooks(token);

CREATE TABLE IF NOT EXISTS webhook_payloads (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    webhook_id   TEXT NOT NULL,
    received_at  INTEGER NOT NULL,
    headers_json TEXT NOT NULL,
    body         TEXT NOT NULL,
    matched      INTEGER NOT NULL,
    FOREIGN KEY (webhook_id) REFERENCES webhooks(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_payloads_webhook
    ON webhook_payloads(webhook_id, received_at DESC);

CREATE TABLE IF NOT EXISTS event_subscriptions (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL,
    owner_kind      TEXT NOT NULL,
    name            TEXT NOT NULL,
    description     TEXT,
    rationale       TEXT,
    event_name      TEXT NOT NULL,
    predicate       TEXT,
    action_kind     TEXT NOT NULL,
    action_payload  TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active',
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER,
    last_fired_at   INTEGER,
    last_error      TEXT,
    -- One-shot delivery: when 1, the event subscriber deletes the row
    -- after the first successful dispatch. Default 0 preserves the
    -- pre-existing "subscriptions persist forever" behaviour.
    delete_after_fire INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_event_subs_event
    ON event_subscriptions(event_name, status);
CREATE INDEX IF NOT EXISTS idx_event_subs_owner
    ON event_subscriptions(user_id, created_at DESC);

-- ── Slice W3 — watchdog incidents (LLM analyze opt-in) ─────────────────
-- One row per emitted watchdog.alert. Persisted before the bus event
-- so the auto-routed ChannelMessage template can reference a stable
-- `incident_id` in its "🔍 Analyze with LLM" link. The analyze
-- endpoint flips analysis_status from 'none' through 'queued' to
-- 'completed', stamping the conversation_id where the agent posted
-- its diagnosis.
CREATE TABLE IF NOT EXISTS watchdog_incidents (
    id                    TEXT PRIMARY KEY,
    user_id               TEXT NOT NULL,
    fingerprint           TEXT NOT NULL,
    severity              TEXT NOT NULL,
    source                TEXT NOT NULL,
    module                TEXT NOT NULL,
    message               TEXT NOT NULL,
    payload_json          TEXT NOT NULL,
    created_at            INTEGER NOT NULL,
    analysis_status       TEXT NOT NULL DEFAULT 'none',
    analysis_started_at   INTEGER,
    analysis_completed_at INTEGER,
    conversation_id       TEXT,
    analysis_response     TEXT
);

CREATE INDEX IF NOT EXISTS idx_incidents_user_created
    ON watchdog_incidents(user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_incidents_fingerprint
    ON watchdog_incidents(fingerprint);
"#;

const SELECT_SCHEDULE_COLS_BY_ID: &str = "
    SELECT id, user_id, owner_kind, name, description, rationale,
           schedule_kind, trigger_spec, timezone, quiet_hours,
           action_kind, action_payload, status, created_at,
           expires_at, last_run_at, next_run_at, run_count,
           failure_count, max_failures, last_error
      FROM schedules
     WHERE id = ?1";

// ── Row mappers ──────────────────────────────────────────────────────────────

fn row_to_schedule(r: &rusqlite::Row) -> rusqlite::Result<Schedule> {
    let trigger_json: String = r.get(7)?;
    let trigger: TriggerSpec = serde_json::from_str(&trigger_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            7, rusqlite::types::Type::Text, Box::new(e)
        ))?;

    let action_json: String = r.get(11)?;
    let action: Action = serde_json::from_str(&action_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            11, rusqlite::types::Type::Text, Box::new(e)
        ))?;

    let quiet_str: Option<String> = r.get(9)?;
    let quiet_hours = match quiet_str {
        Some(s) => Some(serde_json::from_str::<QuietHours>(&s)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                9, rusqlite::types::Type::Text, Box::new(e)
            ))?),
        None => None,
    };

    let owner_s: String = r.get(2)?;
    let status_s: String = r.get(12)?;

    Ok(Schedule {
        id:             r.get(0)?,
        user_id:        r.get(1)?,
        owner_kind:     OwnerKind::parse(&owner_s),
        name:           r.get(3)?,
        description:    r.get(4)?,
        rationale:      r.get(5)?,
        trigger,
        timezone:       r.get(8)?,
        quiet_hours,
        action,
        status:         ScheduleStatus::parse(&status_s),
        created_at:     r.get(13)?,
        expires_at:     r.get(14)?,
        last_run_at:    r.get(15)?,
        next_run_at:    r.get(16)?,
        run_count:      r.get(17)?,
        failure_count:  r.get(18)?,
        max_failures:   r.get(19)?,
        last_error:     r.get(20)?,
    })
}

fn row_to_run(r: &rusqlite::Row) -> rusqlite::Result<AutomationRun> {
    Ok(AutomationRun {
        id:             r.get(0)?,
        source_kind:    r.get(1)?,
        source_id:      r.get(2)?,
        user_id:        r.get(3)?,
        started_at:     r.get(4)?,
        finished_at:    r.get(5)?,
        outcome:        r.get(6)?,
        output_snippet: r.get(7)?,
        error:          r.get(8)?,
        context:        r.get(9)?,
    })
}

fn schedule_kind_tag(spec: &TriggerSpec) -> &'static str {
    match spec {
        TriggerSpec::OneOff   { .. } => "one_off",
        TriggerSpec::Interval { .. } => "interval",
        TriggerSpec::Cron     { .. } => "cron",
    }
}

fn action_kind_tag(action: &Action) -> &'static str {
    match action {
        Action::Prompt(_)         => "prompt",
        Action::ToolCall { .. }   => "tool_call",
        Action::Internal { .. }   => "internal",
        Action::HttpPost { .. }   => "http_post",
        Action::ChannelMessage { .. } => "channel_message",
    }
}

// ── Bootstrap helper ─────────────────────────────────────────────────────────

// Open the store at `<data_dir>/automations.db` and seed default
// system heartbeats. Idempotent — safe to call on every boot.
pub fn open_and_seed(data_dir: &Path) -> Result<Arc<AutomationsStore>, MiraError> {
    let store = AutomationsStore::open(&data_dir.join("automations.db"))?;
    super::heartbeats::seed_defaults(&store)?;
    info!("automations store ready");
    Ok(Arc::new(store))
}
