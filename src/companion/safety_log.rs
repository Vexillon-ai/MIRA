// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/safety_log.rs
//! Append-only audit log of safety-floor events.
//!
//! Privacy-sensitive: the safety log records that an event happened,
//! who was affected, what the event class was, and whether an
//! escalation went out — **but not the full conversation transcript**.
//! The conversation history already lives in `history.db` and follows
//! the user's normal retention; the safety log is a separate, scoped
//! record so a reviewer / family member can audit "did the safety
//! floor work as expected this week?" without trawling the user's
//! private chats.
//!
//! Stored in the same `companion.db` SQLite file as the settings +
//! engagement tables. Append-only by design: there is no UPDATE or
//! DELETE method — every row is forensic evidence the system worked.
//!
//! ships three event kinds:
//! - `Distress` — engagement assessor labelled a turn as Distressed.
//! - `MissedCheckin` — N consecutive check-ins went unanswered.
//! - `RefusedHarmRequest` — reserved for hard-refusal hook (//! if it lands); recorded now so the schema is stable.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::companion::Result;

// What kind of safety event was recorded. Stored as a snake_case
// string in the `kind` column so a future variant doesn't need a
// migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyEventKind {
    // Engagement assessor produced a `Distressed` label.
    Distress,
    // N consecutive missed check-ins crossed the threshold.
    MissedCheckin,
    // Reserved for a hard-refusal hook (+).
    RefusedHarmRequest,
}

impl SafetyEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SafetyEventKind::Distress           => "distress",
            SafetyEventKind::MissedCheckin      => "missed_checkin",
            SafetyEventKind::RefusedHarmRequest => "refused_harm_request",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "distress"             => Some(SafetyEventKind::Distress),
            "missed_checkin"       => Some(SafetyEventKind::MissedCheckin),
            "refused_harm_request" => Some(SafetyEventKind::RefusedHarmRequest),
            _ => None,
        }
    }
}

// Was the configured safety contact actually notified?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationOutcome {
    // Contact received the notice (delivered to their channel).
    Delivered,
    // We tried but the delivery itself failed (channel offline,
    // recipient missing, etc.). Details in `note`.
    DeliveryFailed,
    // No safety contact is configured — nothing to escalate to.
    // Setup-incomplete users never reach the safety floor (the
    // scheduler excludes them), but a user with a contact removed
    // post-enable falls here.
    NoContact,
    // Event happened but the policy didn't escalate (e.g. the same
    // distress signal already escalated within the dedup window).
    Suppressed,
}

impl EscalationOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            EscalationOutcome::Delivered      => "delivered",
            EscalationOutcome::DeliveryFailed => "delivery_failed",
            EscalationOutcome::NoContact      => "no_contact",
            EscalationOutcome::Suppressed     => "suppressed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "delivered"       => Some(EscalationOutcome::Delivered),
            "delivery_failed" => Some(EscalationOutcome::DeliveryFailed),
            "no_contact"      => Some(EscalationOutcome::NoContact),
            "suppressed"      => Some(EscalationOutcome::Suppressed),
            _ => None,
        }
    }
}

// One row in `companion_safety_log`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyEvent {
    pub id: i64,
    pub user_id: String,
    pub kind: SafetyEventKind,
    pub outcome: EscalationOutcome,
    // User id of the safety contact who was notified (or who would
    // have been). `None` when outcome = NoContact.
    pub contact_user_id: Option<String>,
    // A redacted summary of what triggered the event. NOT the full
    // transcript — typically 1–2 sentences. Stored so the user (or
    // the safety contact) can review the audit log meaningfully
    // without leaking deep conversation content.
    pub summary: String,
    // Optional free-form note from the safety module (delivery error
    // message, suppression reason, etc.).
    pub note: Option<String>,
    // Concern severity as a lowercase string ("acute" / "concerning"), or
    // `None` for non-distress events (missed-checkin, etc.). The dedup
    // window is severity-aware — an Acute signal must never be suppressed by a
    // milder prior alert — so severity is a first-class column, not buried in
    // `note`.
    pub severity: Option<String>,
    pub created_at: DateTime<Utc>,
}

// New-row input.
#[derive(Debug, Clone)]
pub struct NewSafetyEvent {
    pub user_id: String,
    pub kind: SafetyEventKind,
    pub outcome: EscalationOutcome,
    pub contact_user_id: Option<String>,
    pub summary: String,
    pub note: Option<String>,
    // "acute" / "concerning" for distress events; `None` otherwise.
    pub severity: Option<String>,
}

pub struct SafetyLog {
    conn: Mutex<Connection>,
}

impl SafetyLog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init_schema(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS companion_safety_log (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id         TEXT NOT NULL,
                kind            TEXT NOT NULL,
                outcome         TEXT NOT NULL,
                contact_user_id TEXT,
                summary         TEXT NOT NULL,
                note            TEXT,
                severity        TEXT,
                created_at      INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_safety_user_time
                ON companion_safety_log(user_id, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_safety_contact_time
                ON companion_safety_log(contact_user_id, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_safety_kind_time
                ON companion_safety_log(kind, created_at DESC);
        "#)?;
        // migration for DBs created before the `severity` column existed.
        // ALTER TABLE has no IF NOT EXISTS in SQLite, so swallow the
        // "duplicate column" error on an already-migrated DB.
        let _ = conn.execute(
            "ALTER TABLE companion_safety_log ADD COLUMN severity TEXT",
            [],
        );
        Ok(())
    }

    // Append a single event. Returns the auto-assigned id so the
    // caller can correlate logs.
    pub fn record(&self, ev: &NewSafetyEvent) -> Result<i64> {
        let conn = self.conn.lock().expect("safety log poisoned");
        conn.execute(
            "INSERT INTO companion_safety_log
                (user_id, kind, outcome, contact_user_id, summary, note, severity, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                ev.user_id,
                ev.kind.as_str(),
                ev.outcome.as_str(),
                ev.contact_user_id,
                ev.summary,
                ev.note,
                ev.severity,
                Utc::now().timestamp_millis(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    // Most-recent `limit` events for a user, newest first. Used by
    // the user themselves (or an admin) to audit "what's the
    // safety floor done for me?".
    pub fn list_recent_for_user(&self, user_id: &str, limit: usize) -> Result<Vec<SafetyEvent>> {
        let conn = self.conn.lock().expect("safety log poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, user_id, kind, outcome, contact_user_id, summary, note, created_at, severity
             FROM companion_safety_log
             WHERE user_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![user_id, limit as i64], row_to_event)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    // Most-recent events where the `contact_user_id` was notified.
    // Used by family members to audit "what alerts have I received
    // from people who named me as their safety contact?".
    pub fn list_recent_for_contact(&self, contact_user_id: &str, limit: usize) -> Result<Vec<SafetyEvent>> {
        let conn = self.conn.lock().expect("safety log poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, user_id, kind, outcome, contact_user_id, summary, note, created_at, severity
             FROM companion_safety_log
             WHERE contact_user_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![contact_user_id, limit as i64], row_to_event)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    // True if there's a `kind=Distress` event for `user_id` within
    // the last `dedup_secs` seconds with outcome=Delivered. Used to
    // suppress duplicate distress escalations when the user emits a
    // distress signal twice in quick succession — the contact has
    // already been notified.
    pub fn has_recent_delivered_distress(&self, user_id: &str, dedup_secs: i64) -> Result<bool> {
        // Severity-agnostic form: min_rank = 0 matches any delivered distress.
        self.has_recent_delivered_distress_at_or_above(user_id, 0, dedup_secs)
    }

    // True if a `kind=MissedCheckin` escalation for `user_id` was DELIVERED
    // within the last `dedup_secs` seconds. lets the scheduler escalate on
    // `count >= threshold` (not exact `==`) — a delivered escalation dedups
    // repeats so the contact isn't spammed, while a FAILED/NoContact one leaves
    // this false so the next qualifying tick retries instead of being skipped
    // forever. Also gives an ongoing silence a periodic (per-window) re-alert.
    pub fn has_recent_delivered_missed_checkin(&self, user_id: &str, dedup_secs: i64) -> Result<bool> {
        let conn = self.conn.lock().expect("safety log poisoned");
        let cutoff = Utc::now().timestamp_millis() - dedup_secs * 1000;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM companion_safety_log
             WHERE user_id = ?1
               AND kind    = 'missed_checkin'
               AND outcome = 'delivered'
               AND created_at >= ?2",
            params![user_id, cutoff],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    // true if there's a delivered `kind=Distress` event for `user_id`
    // within the window whose severity rank is **>= `min_rank`**. Callers pass
    // the incoming signal's rank (see [`severity_rank`]) so that a milder prior
    // alert never suppresses a more urgent one: an incoming Acute (rank 2) is
    // suppressed only by a delivered Acute; an incoming Concerning (rank 1) is
    // suppressed by a delivered Concerning *or* Acute. A NULL/old severity ranks
    // 0 and so never suppresses anything but an already-rank-0 caller.
    pub fn has_recent_delivered_distress_at_or_above(
        &self,
        user_id:    &str,
        min_rank:   i64,
        dedup_secs: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().expect("safety log poisoned");
        let cutoff = Utc::now().timestamp_millis() - dedup_secs * 1000;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM companion_safety_log
             WHERE user_id = ?1
               AND kind    = 'distress'
               AND outcome = 'delivered'
               AND created_at >= ?2
               AND (CASE severity
                        WHEN 'acute'      THEN 2
                        WHEN 'concerning' THEN 1
                        ELSE 0 END) >= ?3",
            params![user_id, cutoff, min_rank],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<SafetyEvent> {
    let kind_s: String = row.get(2)?;
    let outcome_s: String = row.get(3)?;
    let created_ms: i64 = row.get(7)?;
    Ok(SafetyEvent {
        id: row.get(0)?,
        user_id: row.get(1)?,
        kind: SafetyEventKind::parse(&kind_s).unwrap_or(SafetyEventKind::Distress),
        outcome: EscalationOutcome::parse(&outcome_s).unwrap_or(EscalationOutcome::Suppressed),
        contact_user_id: row.get(4)?,
        summary: row.get(5)?,
        note: row.get(6)?,
        severity: row.get(8)?,
        created_at: DateTime::from_timestamp_millis(created_ms).unwrap_or_else(Utc::now),
    })
}

// Rank a stored severity string for the dedup comparison. Higher = more
// urgent. Unknown / NULL ranks lowest so an old severity-less row can never
// suppress a new higher-severity signal. Mirrors the SQL `CASE` in
// `has_recent_delivered_distress_at_or_above` + `ConcernSeverity::rank`; used by
// the tests to express those ranks readably (test-only — the production callers
// pass `ConcernSeverity::rank()` or rely on the SQL CASE).
#[cfg(test)]
fn severity_rank(s: Option<&str>) -> i64 {
    match s {
        Some("acute")      => 2,
        Some("concerning") => 1,
        _                  => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh_log() -> (tempfile::TempDir, SafetyLog) {
        let dir = tempdir().unwrap();
        let log = SafetyLog::open(&dir.path().join("companion.db")).unwrap();
        (dir, log)
    }

    fn sample(user_id: &str, kind: SafetyEventKind, outcome: EscalationOutcome) -> NewSafetyEvent {
        NewSafetyEvent {
            user_id: user_id.into(),
            kind,
            outcome,
            contact_user_id: Some("david".into()),
            summary: "user mentioned feeling overwhelmed".into(),
            note: None,
            severity: None,
        }
    }

    fn sample_sev(user_id: &str, outcome: EscalationOutcome, severity: &str) -> NewSafetyEvent {
        NewSafetyEvent { severity: Some(severity.into()), ..sample(user_id, SafetyEventKind::Distress, outcome) }
    }

    #[test]
    fn kind_and_outcome_round_trip() {
        for k in [SafetyEventKind::Distress, SafetyEventKind::MissedCheckin,
                  SafetyEventKind::RefusedHarmRequest] {
            assert_eq!(SafetyEventKind::parse(k.as_str()), Some(k));
        }
        for o in [EscalationOutcome::Delivered, EscalationOutcome::DeliveryFailed,
                  EscalationOutcome::NoContact, EscalationOutcome::Suppressed] {
            assert_eq!(EscalationOutcome::parse(o.as_str()), Some(o));
        }
    }

    #[test]
    fn record_and_list_round_trips() {
        let (_dir, log) = fresh_log();
        let id = log.record(&sample("alice", SafetyEventKind::Distress, EscalationOutcome::Delivered)).unwrap();
        assert!(id > 0);
        let events = log.list_recent_for_user("alice", 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, SafetyEventKind::Distress);
        assert_eq!(events[0].outcome, EscalationOutcome::Delivered);
        assert_eq!(events[0].contact_user_id.as_deref(), Some("david"));
    }

    #[test]
    fn list_recent_for_user_is_scoped() {
        let (_dir, log) = fresh_log();
        log.record(&sample("alice", SafetyEventKind::Distress, EscalationOutcome::Delivered)).unwrap();
        log.record(&sample("bob", SafetyEventKind::Distress, EscalationOutcome::Delivered)).unwrap();
        assert_eq!(log.list_recent_for_user("alice", 10).unwrap().len(), 1);
        assert_eq!(log.list_recent_for_user("bob", 10).unwrap().len(), 1);
        assert_eq!(log.list_recent_for_user("ghost", 10).unwrap().len(), 0);
    }

    #[test]
    fn list_recent_for_contact_is_scoped() {
        let (_dir, log) = fresh_log();
        // alice's safety contact is david
        log.record(&sample("alice", SafetyEventKind::Distress, EscalationOutcome::Delivered)).unwrap();
        // bob's safety contact is sarah
        let mut bob = sample("bob", SafetyEventKind::Distress, EscalationOutcome::Delivered);
        bob.contact_user_id = Some("sarah".into());
        log.record(&bob).unwrap();

        assert_eq!(log.list_recent_for_contact("david", 10).unwrap().len(), 1);
        assert_eq!(log.list_recent_for_contact("sarah", 10).unwrap().len(), 1);
        assert_eq!(log.list_recent_for_contact("ghost", 10).unwrap().len(), 0);
    }

    #[test]
    fn dedup_lookup_detects_recent_delivered_distress() {
        let (_dir, log) = fresh_log();
        log.record(&sample("alice", SafetyEventKind::Distress, EscalationOutcome::Delivered)).unwrap();
        assert!(log.has_recent_delivered_distress("alice", 600).unwrap(),
            "freshly-recorded distress should be within 10-minute dedup window");
        // 0-second window → still counts because the row was created moments ago
        // and 0-second cutoff is "now" — events with created_at >= now would match.
        // We assert positively against a long window only.
    }

    #[test]
    fn dedup_lookup_ignores_failed_or_suppressed_events() {
        let (_dir, log) = fresh_log();
        log.record(&sample("alice", SafetyEventKind::Distress, EscalationOutcome::DeliveryFailed)).unwrap();
        log.record(&sample("alice", SafetyEventKind::Distress, EscalationOutcome::Suppressed)).unwrap();
        log.record(&sample("alice", SafetyEventKind::Distress, EscalationOutcome::NoContact)).unwrap();
        assert!(!log.has_recent_delivered_distress("alice", 3600).unwrap(),
            "non-delivered outcomes should NOT trigger dedup — the contact wasn't actually told");
    }

    // an Acute signal is NEVER suppressed by a milder prior alert.
    #[test]
    fn acute_is_not_suppressed_by_prior_concerning() {
        let (_dir, log) = fresh_log();
        // A Concerning alert was delivered moments ago.
        log.record(&sample_sev("alice", EscalationOutcome::Delivered, "concerning")).unwrap();
        let acute_rank = severity_rank(Some("acute"));         // 2
        let concerning_rank = severity_rank(Some("concerning")); // 1
        // An incoming Concerning IS deduped (contact already told).
        assert!(log.has_recent_delivered_distress_at_or_above("alice", concerning_rank, 3600).unwrap());
        // An incoming Acute is NOT — it must escalate.
        assert!(!log.has_recent_delivered_distress_at_or_above("alice", acute_rank, 3600).unwrap());
    }

    // acute-after-acute still debounces (no spam), and a delivered Acute
    // also suppresses a later Concerning (contact already knows it's bad).
    #[test]
    fn acute_dedups_acute_and_covers_later_concerning() {
        let (_dir, log) = fresh_log();
        log.record(&sample_sev("alice", EscalationOutcome::Delivered, "acute")).unwrap();
        assert!(log.has_recent_delivered_distress_at_or_above("alice", severity_rank(Some("acute")), 3600).unwrap());
        assert!(log.has_recent_delivered_distress_at_or_above("alice", severity_rank(Some("concerning")), 3600).unwrap());
    }

    // a DELIVERED missed-checkin escalation dedups repeats; a FAILED /
    // no-contact one does not (so the next tick retries).
    #[test]
    fn missed_checkin_dedup_tracks_only_delivered() {
        let (_dir, log) = fresh_log();
        // A failed escalation does NOT start the dedup window.
        log.record(&sample("alice", SafetyEventKind::MissedCheckin, EscalationOutcome::DeliveryFailed)).unwrap();
        assert!(!log.has_recent_delivered_missed_checkin("alice", 3600).unwrap());
        // A delivered one does.
        log.record(&sample("alice", SafetyEventKind::MissedCheckin, EscalationOutcome::Delivered)).unwrap();
        assert!(log.has_recent_delivered_missed_checkin("alice", 3600).unwrap());
        // Distress delivery doesn't count as a missed-checkin.
        assert!(!log.has_recent_delivered_missed_checkin("bob", 3600).unwrap());
    }

    #[test]
    fn dedup_lookup_distinguishes_kinds() {
        let (_dir, log) = fresh_log();
        // Missed-checkin doesn't dedup distress.
        log.record(&sample("alice", SafetyEventKind::MissedCheckin, EscalationOutcome::Delivered)).unwrap();
        assert!(!log.has_recent_delivered_distress("alice", 3600).unwrap());
    }

    #[test]
    fn schema_idempotent_across_opens() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("companion.db");
        let _l1 = SafetyLog::open(&path).unwrap();
        let _l2 = SafetyLog::open(&path).unwrap();
        // No panic = good.
    }
}
