// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/engagement_log.rs
//! Per-turn engagement labels for companion-mode users.
//!
//! records one row per turn for users with companion mode
//! active: was the turn `engaged` (long-ish back-and-forth, follow-up
//! questions), `brief` (one-or-two-word replies), `declined` (user
//! told the companion to back off), or `distressed` (safety-floor
//! signal —  enforces it; we just label here).
//!
//! Downstream consumers:
//! - **Scheduler cadence adjustment** (this slice): reads recent
//! labels to slow down when the user's been brief/declining.
//! - **Wiki routines.md updates** (a follow-up): aggregates by
//! hour-of-day to fill in the "best check-in windows" section.
//! - ** safety floor**: a `distressed` label triggers the
//! escalation flow.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::companion::Result;

// Outcome labels the assessor emits per turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngagementLabel {
    Engaged,
    Brief,
    Declined,
    // Reserved for  The assessor MAY emit this when 
    // ships, but the safety-floor enforcement comes later. For now
    // the label just lands in the log.
    Distressed,
}

impl EngagementLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            EngagementLabel::Engaged    => "engaged",
            EngagementLabel::Brief      => "brief",
            EngagementLabel::Declined   => "declined",
            EngagementLabel::Distressed => "distressed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "engaged"    => Some(EngagementLabel::Engaged),
            "brief"      => Some(EngagementLabel::Brief),
            "declined"   => Some(EngagementLabel::Declined),
            "distressed" => Some(EngagementLabel::Distressed),
            _ => None,
        }
    }
}

// One row in `companion_engagement_log`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementEntry {
    pub user_id: String,
    pub conversation_id: Option<String>,
    pub turn_id: Option<String>,
    pub label: EngagementLabel,
    // 0..23 in the user's local tz at the time of the turn.
    pub hour_of_day: u8,
    // 0=Monday..6=Sunday, per chrono's `Weekday::num_days_from_monday`.
    pub day_of_week: u8,
    pub created_at: DateTime<Utc>,
}

pub struct EngagementLog {
    conn: Mutex<Connection>,
}

impl EngagementLog {
    // Open or attach at `<data_dir>/companion.db`. The schema
    // includes only the engagement table; the settings table lives
    // in the same file and is created by `settings::CompanionStore`.
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
            CREATE TABLE IF NOT EXISTS companion_engagement_log (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id         TEXT NOT NULL,
                conversation_id TEXT,
                turn_id         TEXT,
                label           TEXT NOT NULL,
                hour_of_day     INTEGER NOT NULL,
                day_of_week     INTEGER NOT NULL,
                created_at      INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_engagement_user_time
                ON companion_engagement_log(user_id, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_engagement_user_hour
                ON companion_engagement_log(user_id, hour_of_day);
        "#)?;
        Ok(())
    }

    pub fn insert(&self, e: &EngagementEntry) -> Result<()> {
        let conn = self.conn.lock().expect("engagement log poisoned");
        conn.execute(
            "INSERT INTO companion_engagement_log
                (user_id, conversation_id, turn_id, label, hour_of_day, day_of_week, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                e.user_id,
                e.conversation_id,
                e.turn_id,
                e.label.as_str(),
                e.hour_of_day as i64,
                e.day_of_week as i64,
                e.created_at.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    // Most-recent `limit` entries for the user, newest first.
    // Used by the scheduler's cadence adjustment.
    pub fn list_recent(&self, user_id: &str, limit: usize) -> Result<Vec<EngagementEntry>> {
        let conn = self.conn.lock().expect("engagement log poisoned");
        let mut stmt = conn.prepare(
            "SELECT user_id, conversation_id, turn_id, label,
                    hour_of_day, day_of_week, created_at
             FROM companion_engagement_log
             WHERE user_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![user_id, limit as i64], row_to_entry)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    // (label → count) tally for `user_id` over the last `since`
    // epoch ms. Convenient for the cadence adjuster — it consumes
    // "of the last N labels in the trailing window, what's the
    // distribution?".
    pub fn tally_since(
        &self,
        user_id: &str,
        since: DateTime<Utc>,
    ) -> Result<EngagementTally> {
        let conn = self.conn.lock().expect("engagement log poisoned");
        let mut stmt = conn.prepare(
            "SELECT label, COUNT(*)
             FROM companion_engagement_log
             WHERE user_id = ?1 AND created_at >= ?2
             GROUP BY label",
        )?;
        let mut tally = EngagementTally::default();
        let rows = stmt.query_map(
            params![user_id, since.timestamp_millis()],
            |row| {
                let label: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((label, count))
            },
        )?;
        for r in rows {
            let (label, count) = r?;
            let count = count as u32;
            match EngagementLabel::parse(&label) {
                Some(EngagementLabel::Engaged)    => tally.engaged    = count,
                Some(EngagementLabel::Brief)      => tally.brief      = count,
                Some(EngagementLabel::Declined)   => tally.declined   = count,
                Some(EngagementLabel::Distressed) => tally.distressed = count,
                None => {}
            }
        }
        Ok(tally)
    }
}

// Counts of each label over a trailing window. Provided to the
// cadence adjuster.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngagementTally {
    pub engaged: u32,
    pub brief: u32,
    pub declined: u32,
    pub distressed: u32,
}

impl EngagementTally {
    pub fn total(&self) -> u32 {
        self.engaged + self.brief + self.declined + self.distressed
    }

    // Fraction (0..=1) of the tally that signals the user was
    // disengaged (brief or declined). Returns 0 when total is zero.
    pub fn disengaged_fraction(&self) -> f32 {
        let total = self.total();
        if total == 0 { return 0.0; }
        (self.brief + self.declined) as f32 / total as f32
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<EngagementEntry> {
    let label_s: String = row.get(3)?;
    let label = EngagementLabel::parse(&label_s)
        .unwrap_or(EngagementLabel::Brief);
    let created_ms: i64 = row.get(6)?;
    Ok(EngagementEntry {
        user_id: row.get(0)?,
        conversation_id: row.get(1)?,
        turn_id: row.get(2)?,
        label,
        hour_of_day: row.get::<_, i64>(4)? as u8,
        day_of_week: row.get::<_, i64>(5)? as u8,
        created_at: DateTime::from_timestamp_millis(created_ms).unwrap_or_else(Utc::now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use tempfile::tempdir;

    fn fresh_log() -> (tempfile::TempDir, EngagementLog) {
        let dir = tempdir().unwrap();
        let log = EngagementLog::open(&dir.path().join("companion.db")).unwrap();
        (dir, log)
    }

    fn sample(user_id: &str, label: EngagementLabel, ago: Duration) -> EngagementEntry {
        EngagementEntry {
            user_id: user_id.into(),
            conversation_id: Some("c1".into()),
            turn_id: Some("t1".into()),
            label,
            hour_of_day: 10,
            day_of_week: 2,
            created_at: Utc::now() - ago,
        }
    }

    #[test]
    fn label_round_trips() {
        for l in [EngagementLabel::Engaged, EngagementLabel::Brief,
                  EngagementLabel::Declined, EngagementLabel::Distressed] {
            assert_eq!(EngagementLabel::parse(l.as_str()), Some(l));
        }
        assert_eq!(EngagementLabel::parse("garbage"), None);
    }

    #[test]
    fn insert_and_list_recent() {
        let (_dir, log) = fresh_log();
        log.insert(&sample("alice", EngagementLabel::Engaged,  Duration::minutes(60))).unwrap();
        log.insert(&sample("alice", EngagementLabel::Brief,    Duration::minutes(30))).unwrap();
        log.insert(&sample("alice", EngagementLabel::Declined, Duration::minutes(5))).unwrap();

        let entries = log.list_recent("alice", 10).unwrap();
        assert_eq!(entries.len(), 3);
        // Newest first.
        assert_eq!(entries[0].label, EngagementLabel::Declined);
        assert_eq!(entries[1].label, EngagementLabel::Brief);
        assert_eq!(entries[2].label, EngagementLabel::Engaged);
    }

    #[test]
    fn list_recent_scopes_by_user() {
        let (_dir, log) = fresh_log();
        log.insert(&sample("alice", EngagementLabel::Engaged, Duration::minutes(5))).unwrap();
        log.insert(&sample("bob",   EngagementLabel::Brief,   Duration::minutes(5))).unwrap();
        assert_eq!(log.list_recent("alice", 10).unwrap().len(), 1);
        assert_eq!(log.list_recent("bob",   10).unwrap().len(), 1);
        assert_eq!(log.list_recent("ghost", 10).unwrap().len(), 0);
    }

    #[test]
    fn tally_since_distinguishes_labels() {
        let (_dir, log) = fresh_log();
        for _ in 0..3 {
            log.insert(&sample("alice", EngagementLabel::Engaged, Duration::minutes(60))).unwrap();
        }
        log.insert(&sample("alice", EngagementLabel::Brief,    Duration::minutes(30))).unwrap();
        log.insert(&sample("alice", EngagementLabel::Declined, Duration::minutes(10))).unwrap();

        let cutoff = Utc::now() - Duration::days(1);
        let t = log.tally_since("alice", cutoff).unwrap();
        assert_eq!(t.engaged, 3);
        assert_eq!(t.brief, 1);
        assert_eq!(t.declined, 1);
        assert_eq!(t.total(), 5);
    }

    #[test]
    fn tally_since_excludes_old_entries() {
        let (_dir, log) = fresh_log();
        log.insert(&sample("alice", EngagementLabel::Engaged, Duration::days(10))).unwrap();
        log.insert(&sample("alice", EngagementLabel::Brief,    Duration::hours(1))).unwrap();

        let cutoff = Utc::now() - Duration::days(2);
        let t = log.tally_since("alice", cutoff).unwrap();
        assert_eq!(t.engaged, 0); // outside the window
        assert_eq!(t.brief, 1);
    }

    #[test]
    fn disengaged_fraction_math() {
        let t = EngagementTally { engaged: 1, brief: 2, declined: 1, distressed: 0 };
        assert!((t.disengaged_fraction() - 0.75).abs() < 1e-6);

        let empty = EngagementTally::default();
        assert_eq!(empty.disengaged_fraction(), 0.0);
        assert_eq!(empty.total(), 0);
    }

    #[test]
    fn schema_idempotent_across_opens() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("companion.db");
        let _l1 = EngagementLog::open(&path).unwrap();
        let _l2 = EngagementLog::open(&path).unwrap();
        let _l3 = EngagementLog::open(&path).unwrap();
        // No panic = good.
    }
}
