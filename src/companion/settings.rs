// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/settings.rs
//! SQLite storage for per-user companion-mode settings.
//!
//! One row per user. The settings DB is global (single file at
//! `<data_dir>/companion.db`) rather than per-user because the scheduler
//! needs to scan "which users have companion enabled" on
//! every tick. Per-user SQLite would force the scheduler to enumerate
//! every wiki dir on every tick — wasteful.
//!
//! Wiki pages still carry the *content* (persona, learned routines,
//! safety contacts); this DB carries only the operational flags + the
//! hot-path safety contact for v1.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::companion::Result;

// Per-user overrides for the global check-in cadence knobs
// (`companion.*` in mira_config.json). `None` on a field means "inherit
// the instance default"; the scheduler falls back to the global value.
// Stored as JSON in `cadence_json` so adding a knob needs no new column.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompanionCadence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unanswered_checkins: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_day: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_gap_minutes: Option<i64>,
}

// Per-user companion-mode state. JSON fields (`quiet_hours`,
// `preferred_channels`) are stored as text and parsed on read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionSettings {
    pub user_id: String,
    pub enabled: bool,
    // Epoch ms; `None` means not paused. `enabled && paused_until > now`
    // is the "paused" state — set `paused_until = None` (or past time) to
    // resume.
    pub paused_until: Option<DateTime<Utc>>,
    // Pairs of "HH:MM"–"HH:MM" strings, interpreted in the user's tz.
    // Empty = no quiet hours; the scheduler treats anything missing as
    // "always OK".
    pub quiet_hours: Vec<(String, String)>,
    // Ordered preference list. e.g. `["signal","telegram","web"]`. The
    // scheduler picks the first reachable one.
    pub preferred_channels: Vec<String>,
    // For v1 this is a single user id; v2 swaps in a group id (§9).
    // `None` while waiting for setup.
    pub safety_contact_user_id: Option<String>,
    // Epoch ms when the minimum bootstrap completed. `None` = setup not
    // done yet; the chit-chat detector / engagement assessor stay off
    // until this is stamped.
    pub setup_completed_at: Option<DateTime<Utc>>,
    // Last time the scheduler fired a check-in for this user.
    // `None` = never fired. Used by the policy to enforce `min_gap`
    // and `daily_cap`.
    pub last_checkin_at: Option<DateTime<Utc>>,
    // consecutive check-ins fired since the user last
    // sent a message. Incremented by the scheduler after each fire;
    // reset to 0 by the chat handler on any user message. The
    // safety floor watches this for "user hasn't replied in a
    // while" → soft escalation.
    pub consecutive_missed_checkins: u32,
    // Q1.6 — daily-briefing toggle. Off by default per the design
    // (existing companion users shouldn't get surprised by a new
    // daily message they didn't ask for). When true, the scheduler
    // fires a structured briefing at `daily_briefing_hour` local
    // time each day, separate from the warm-opener check-ins.
    pub daily_briefing_enabled: bool,
    // Hour of day (0-23) in the user's local tz when the daily
    // briefing fires. Default 7 = 07:00 local. Honoured even when
    // quiet_hours covers the same window (the briefing is opt-in
    // content, not noise).
    pub daily_briefing_hour: u8,
    // Last time the scheduler fired a daily briefing for this user.
    // Used by the briefing pass to enforce one-per-local-day.
    pub last_briefing_at: Option<DateTime<Utc>>,
    // Per-user overrides for the global check-in cadence knobs. Empty
    // (all None) = inherit the instance defaults. Set via
    // `companion_configure`.
    #[serde(default)]
    pub cadence: CompanionCadence,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CompanionSettings {
    // Convenience: is the user currently in the "enabled and not
    // paused" state? Returns false when setup isn't done — even an
    // enabled-but-unconfigured account should not have hooks fire.
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled { return false; }
        if self.setup_completed_at.is_none() { return false; }
        match self.paused_until {
            Some(until) => until <= now,
            None => true,
        }
    }
}

pub struct CompanionStore {
    conn: Mutex<Connection>,
}

impl CompanionStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init_schema(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        // Step 1 — fresh-DB schema (covers all known columns).
        conn.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS companion_settings (
                user_id                       TEXT PRIMARY KEY,
                enabled                       INTEGER NOT NULL DEFAULT 0,
                paused_until                  INTEGER,
                quiet_hours_json              TEXT NOT NULL DEFAULT '[]',
                preferred_channels_json       TEXT NOT NULL DEFAULT '[]',
                safety_contact_user_id        TEXT,
                setup_completed_at            INTEGER,
                last_checkin_at               INTEGER,
                consecutive_missed_checkins   INTEGER NOT NULL DEFAULT 0,
                daily_briefing_enabled        INTEGER NOT NULL DEFAULT 0,
                daily_briefing_hour           INTEGER NOT NULL DEFAULT 7,
                last_briefing_at              INTEGER,
                checkins_today_count          INTEGER NOT NULL DEFAULT 0,
                checkins_today_day            TEXT,
                cadence_json                  TEXT NOT NULL DEFAULT '{}',
                created_at                    INTEGER NOT NULL,
                updated_at                    INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_companion_enabled
                ON companion_settings(enabled, paused_until);
        "#)?;
        // Step 2 — idempotent column adds for DBs created by earlier
        // slices. SQLite returns "duplicate column name" when the
        // column already exists; swallow that and surface real
        // failures.
        let add = |sql: &str| -> Result<()> {
            match conn.execute(sql, []) {
                Ok(_) => Ok(()),
                Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
                    if msg.contains("duplicate column") => Ok(()),
                Err(e) => Err(e.into()),
            }
        };
        add("ALTER TABLE companion_settings ADD COLUMN last_checkin_at INTEGER")?;
        add("ALTER TABLE companion_settings \
             ADD COLUMN consecutive_missed_checkins INTEGER NOT NULL DEFAULT 0")?;
        // Q1.6 — daily-briefing columns. Off by default; existing
        // rows get briefing_enabled=0 / hour=7 implicitly via the
        // column defaults.
        add("ALTER TABLE companion_settings \
             ADD COLUMN daily_briefing_enabled INTEGER NOT NULL DEFAULT 0")?;
        add("ALTER TABLE companion_settings \
             ADD COLUMN daily_briefing_hour INTEGER NOT NULL DEFAULT 7")?;
        add("ALTER TABLE companion_settings ADD COLUMN last_briefing_at INTEGER")?;
        // 0.270.0 — real per-day check-in counter (the policy's `max_per_day`
        // cap was previously fed a 0-or-1 approximation). `checkins_today_day`
        // holds the user-local date ("YYYY-MM-DD") the count applies to; the
        // count rolls over when the local day changes.
        add("ALTER TABLE companion_settings \
             ADD COLUMN checkins_today_count INTEGER NOT NULL DEFAULT 0")?;
        add("ALTER TABLE companion_settings ADD COLUMN checkins_today_day TEXT")?;
        // 0.271.0 — per-user cadence overrides (JSON: max_unanswered_checkins,
        // max_per_day, min_gap_minutes). Empty '{}' = inherit global defaults.
        add("ALTER TABLE companion_settings \
             ADD COLUMN cadence_json TEXT NOT NULL DEFAULT '{}'")?;
        Ok(())
    }

    // Real count of check-ins fired so far on the user-local day `day`
    // ("YYYY-MM-DD"). Returns 0 when the stored counter is for a different
    // day (i.e. the day has rolled over) or the user has no row yet. Scheduler-
    // owned; feeds the policy's `max_per_day` cap.
    pub fn checkins_today(&self, user_id: &str, day: &str) -> Result<u32> {
        let conn = self.conn.lock().expect("companion store poisoned");
        let count: i64 = conn.query_row(
            "SELECT CASE WHEN checkins_today_day = ?2 THEN checkins_today_count ELSE 0 END
             FROM companion_settings WHERE user_id = ?1",
            params![user_id, day],
            |row| row.get(0),
        ).optional()?.unwrap_or(0);
        Ok(count.max(0) as u32)
    }

    // Record a check-in fired on the user-local day `day`, returning the new
    // count. Atomic read-modify-write: increments when the stored day matches,
    // otherwise resets to 1 and stamps the new day (local-day rollover).
    pub fn bump_checkins_today(&self, user_id: &str, day: &str) -> Result<u32> {
        let conn = self.conn.lock().expect("companion store poisoned");
        conn.execute(
            "UPDATE companion_settings
             SET checkins_today_count =
                   CASE WHEN checkins_today_day = ?1 THEN checkins_today_count + 1 ELSE 1 END,
                 checkins_today_day = ?1,
                 updated_at = ?2
             WHERE user_id = ?3",
            params![day, Utc::now().timestamp_millis(), user_id],
        )?;
        let count: i64 = conn.query_row(
            "SELECT checkins_today_count FROM companion_settings WHERE user_id = ?1",
            params![user_id],
            |row| row.get(0),
        ).unwrap_or(0);
        Ok(count.max(0) as u32)
    }

    // Fetch settings for `user_id`. Returns `Ok(None)` if no row.
    pub fn get(&self, user_id: &str) -> Result<Option<CompanionSettings>> {
        let conn = self.conn.lock().expect("companion store poisoned");
        let row: Option<CompanionSettings> = conn.query_row(
            "SELECT user_id, enabled, paused_until,
                    quiet_hours_json, preferred_channels_json,
                    safety_contact_user_id, setup_completed_at,
                    last_checkin_at, consecutive_missed_checkins,
                    daily_briefing_enabled, daily_briefing_hour, last_briefing_at,
                    created_at, updated_at, cadence_json
             FROM companion_settings WHERE user_id = ?1",
            params![user_id],
            row_to_settings,
        ).optional()?;
        Ok(row)
    }

    // Upsert. Used by `enable` / `configure` / `pause` / `resume`.
    // `created_at` is only set when the row is new; otherwise preserved.
    // **Note:** `last_checkin_at` and `consecutive_missed_checkins`
    // are intentionally NOT in the upsert path — they're scheduler-
    // owned columns updated by targeted helpers
    // (`mark_checkin` / `increment_missed` / `reset_missed`) so a
    // configure call can't accidentally clear them.
    pub fn upsert(&self, s: &CompanionSettings) -> Result<()> {
        let quiet_json = serde_json::to_string(&s.quiet_hours)?;
        let chan_json  = serde_json::to_string(&s.preferred_channels)?;
        let cadence_json = serde_json::to_string(&s.cadence)?;
        let conn = self.conn.lock().expect("companion store poisoned");
        conn.execute(
            "INSERT INTO companion_settings (
                user_id, enabled, paused_until,
                quiet_hours_json, preferred_channels_json,
                safety_contact_user_id, setup_completed_at,
                last_checkin_at, consecutive_missed_checkins,
                daily_briefing_enabled, daily_briefing_hour,
                cadence_json,
                created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(user_id) DO UPDATE SET
                enabled                 = excluded.enabled,
                paused_until            = excluded.paused_until,
                quiet_hours_json        = excluded.quiet_hours_json,
                preferred_channels_json = excluded.preferred_channels_json,
                safety_contact_user_id  = excluded.safety_contact_user_id,
                setup_completed_at      = excluded.setup_completed_at,
                daily_briefing_enabled  = excluded.daily_briefing_enabled,
                daily_briefing_hour     = excluded.daily_briefing_hour,
                cadence_json            = excluded.cadence_json,
                updated_at              = excluded.updated_at",
            params![
                s.user_id,
                if s.enabled { 1i64 } else { 0i64 },
                s.paused_until.map(|d| d.timestamp_millis()),
                quiet_json,
                chan_json,
                s.safety_contact_user_id,
                s.setup_completed_at.map(|d| d.timestamp_millis()),
                s.last_checkin_at.map(|d| d.timestamp_millis()),
                s.consecutive_missed_checkins as i64,
                if s.daily_briefing_enabled { 1i64 } else { 0i64 },
                s.daily_briefing_hour as i64,
                cadence_json,
                s.created_at.timestamp_millis(),
                s.updated_at.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    // Q1.6 — stamp `last_briefing_at` for `user_id`. Targeted update
    // so the scheduler's briefing pass doesn't race with the
    // check-in / configure paths; only the column it owns gets
    // touched. Mirrors `mark_checkin`.
    pub fn mark_briefing(&self, user_id: &str, at: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.lock().expect("companion store poisoned");
        conn.execute(
            "UPDATE companion_settings
             SET last_briefing_at = ?1, updated_at = ?2
             WHERE user_id = ?3",
            params![at.timestamp_millis(), Utc::now().timestamp_millis(), user_id],
        )?;
        Ok(())
    }

    // Stamp `last_checkin_at` for `user_id` to `at`. Targeted update so
    // the scheduler doesn't race with other writers; only the column
    // it owns gets touched.
    pub fn mark_checkin(&self, user_id: &str, at: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.lock().expect("companion store poisoned");
        conn.execute(
            "UPDATE companion_settings
             SET last_checkin_at = ?1, updated_at = ?2
             WHERE user_id = ?3",
            params![at.timestamp_millis(), Utc::now().timestamp_millis(), user_id],
        )?;
        Ok(())
    }

    // Increment `consecutive_missed_checkins` by 1 (atomic). Called
    // by the scheduler after a fire that completed delivery but
    // before the user has had a chance to respond. The next user
    // message resets this via [`Self::reset_missed_checkins`].
    pub fn increment_missed_checkins(&self, user_id: &str) -> Result<u32> {
        let conn = self.conn.lock().expect("companion store poisoned");
        let now = Utc::now().timestamp_millis();
        conn.execute(
            "UPDATE companion_settings
             SET consecutive_missed_checkins = consecutive_missed_checkins + 1,
                 updated_at = ?1
             WHERE user_id = ?2",
            params![now, user_id],
        )?;
        // Read back so callers can act on the new value.
        let count: i64 = conn.query_row(
            "SELECT consecutive_missed_checkins
             FROM companion_settings WHERE user_id = ?1",
            params![user_id],
            |row| row.get(0),
        ).unwrap_or(0);
        Ok(count as u32)
    }

    // Reset `consecutive_missed_checkins` to 0. Called by the chat
    // handler when the user sends any message — proves the
    // scheduler's previous check-ins were received.
    pub fn reset_missed_checkins(&self, user_id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("companion store poisoned");
        conn.execute(
            "UPDATE companion_settings
             SET consecutive_missed_checkins = 0, updated_at = ?1
             WHERE user_id = ?2",
            params![Utc::now().timestamp_millis(), user_id],
        )?;
        Ok(())
    }

    // All users currently in the "enabled AND past their paused_until"
    // state. 's scheduler uses this to find candidates for a
    // check-in tick. Setup-incomplete users are excluded — they don't
    // have a safety contact yet, so the safety floor can't kick in.
    pub fn list_active(&self, now: DateTime<Utc>) -> Result<Vec<CompanionSettings>> {
        let conn = self.conn.lock().expect("companion store poisoned");
        let mut stmt = conn.prepare(
            "SELECT user_id, enabled, paused_until,
                    quiet_hours_json, preferred_channels_json,
                    safety_contact_user_id, setup_completed_at,
                    last_checkin_at, consecutive_missed_checkins,
                    daily_briefing_enabled, daily_briefing_hour, last_briefing_at,
                    created_at, updated_at, cadence_json
             FROM companion_settings
             WHERE enabled = 1
               AND setup_completed_at IS NOT NULL
               AND (paused_until IS NULL OR paused_until <= ?1)
             ORDER BY user_id ASC",
        )?;
        let rows = stmt.query_map(params![now.timestamp_millis()], row_to_settings)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    // Delete the row entirely. Used when a user is deleted upstream;
    // `companion_disable` only flips `enabled = 0`, preserving the
    // settings so re-enable doesn't lose the safety contact / persona.
    pub fn delete(&self, user_id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("companion store poisoned");
        conn.execute(
            "DELETE FROM companion_settings WHERE user_id = ?1",
            params![user_id],
        )?;
        Ok(())
    }
}

fn row_to_settings(row: &rusqlite::Row<'_>) -> rusqlite::Result<CompanionSettings> {
    let user_id: String = row.get(0)?;
    let enabled_i: i64  = row.get(1)?;
    let paused_ms: Option<i64> = row.get(2)?;
    let quiet_json: String = row.get(3)?;
    let chan_json:  String = row.get(4)?;
    let safety:     Option<String> = row.get(5)?;
    let setup_ms:   Option<i64>    = row.get(6)?;
    let last_ck_ms: Option<i64>    = row.get(7)?;
    let missed_i:        i64         = row.get(8)?;
    let brief_en_i:      i64         = row.get(9)?;
    let brief_hour_i:    i64         = row.get(10)?;
    let last_brief_ms:   Option<i64> = row.get(11)?;
    let created_ms:      i64         = row.get(12)?;
    let updated_ms:      i64         = row.get(13)?;
    let cadence_json:    String      = row.get(14)?;

    // A malformed cadence blob falls back to "inherit defaults" rather than
    // failing the whole row read.
    let cadence: CompanionCadence = serde_json::from_str(&cadence_json).unwrap_or_default();

    let quiet_hours: Vec<(String, String)> = serde_json::from_str(&quiet_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            3, rusqlite::types::Type::Text, Box::new(e),
        ))?;
    let preferred_channels: Vec<String> = serde_json::from_str(&chan_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            4, rusqlite::types::Type::Text, Box::new(e),
        ))?;

    Ok(CompanionSettings {
        user_id,
        enabled: enabled_i != 0,
        paused_until: paused_ms.and_then(DateTime::from_timestamp_millis),
        quiet_hours,
        preferred_channels,
        safety_contact_user_id: safety,
        setup_completed_at: setup_ms.and_then(DateTime::from_timestamp_millis),
        last_checkin_at: last_ck_ms.and_then(DateTime::from_timestamp_millis),
        consecutive_missed_checkins: missed_i.max(0) as u32,
        daily_briefing_enabled: brief_en_i != 0,
        // Clamp to 0..=23 to defend against a bad ALTER default or a
        // hand-edited row.
        daily_briefing_hour:    brief_hour_i.clamp(0, 23) as u8,
        last_briefing_at:       last_brief_ms.and_then(DateTime::from_timestamp_millis),
        cadence,
        created_at: DateTime::from_timestamp_millis(created_ms).unwrap_or_else(Utc::now),
        updated_at: DateTime::from_timestamp_millis(updated_ms).unwrap_or_else(Utc::now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh_store() -> (tempfile::TempDir, CompanionStore) {
        let dir = tempdir().unwrap();
        let store = CompanionStore::open(&dir.path().join("companion.db")).unwrap();
        (dir, store)
    }

    fn sample(user_id: &str) -> CompanionSettings {
        let now = Utc::now();
        CompanionSettings {
            user_id: user_id.to_string(),
            enabled: false,
            paused_until: None,
            quiet_hours: vec![],
            preferred_channels: vec![],
            safety_contact_user_id: None,
            setup_completed_at: None,
            last_checkin_at: None,
            consecutive_missed_checkins: 0,
            daily_briefing_enabled: false,
            daily_briefing_hour: 7,
            last_briefing_at: None,
            cadence: CompanionCadence::default(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn get_returns_none_for_unseeded_user() {
        let (_dir, store) = fresh_store();
        assert!(store.get("nobody").unwrap().is_none());
    }

    #[test]
    fn upsert_round_trips() {
        let (_dir, store) = fresh_store();
        let mut s = sample("alice");
        s.enabled = true;
        s.quiet_hours = vec![("22:00".into(), "06:30".into())];
        s.preferred_channels = vec!["signal".into(), "telegram".into()];
        s.safety_contact_user_id = Some("david".into());
        s.setup_completed_at = Some(Utc::now());
        store.upsert(&s).unwrap();

        let back = store.get("alice").unwrap().unwrap();
        assert!(back.enabled);
        assert_eq!(back.quiet_hours, vec![("22:00".to_string(), "06:30".to_string())]);
        assert_eq!(back.preferred_channels, vec!["signal".to_string(), "telegram".to_string()]);
        assert_eq!(back.safety_contact_user_id.as_deref(), Some("david"));
        assert!(back.setup_completed_at.is_some());
    }

    #[test]
    fn upsert_preserves_created_at_on_update() {
        // The schema's ON CONFLICT clause excludes `created_at` from the
        // update, so an upsert with a different `created_at` doesn't
        // clobber the original. We don't rely on this in the facade
        // (the facade reads-then-writes), but the constraint is worth
        // protecting in case a future caller upserts a fresh struct.
        let (_dir, store) = fresh_store();
        let first = sample("alice");
        store.upsert(&first).unwrap();
        let mut second = sample("alice");
        second.created_at = first.created_at + chrono::Duration::hours(1);
        store.upsert(&second).unwrap();
        let back = store.get("alice").unwrap().unwrap();
        assert_eq!(back.created_at.timestamp_millis(), first.created_at.timestamp_millis());
    }

    #[test]
    fn list_active_filters_by_enabled_and_setup() {
        let (_dir, store) = fresh_store();
        let now = Utc::now();

        // Enabled + setup done → listed
        let mut alice = sample("alice");
        alice.enabled = true;
        alice.setup_completed_at = Some(now);
        store.upsert(&alice).unwrap();

        // Enabled but setup not done → NOT listed (no safety floor yet)
        let mut bob = sample("bob");
        bob.enabled = true;
        store.upsert(&bob).unwrap();

        // Disabled → NOT listed
        let charlie = sample("charlie");
        store.upsert(&charlie).unwrap();

        // Enabled + setup done + paused in the future → NOT listed
        let mut dora = sample("dora");
        dora.enabled = true;
        dora.setup_completed_at = Some(now);
        dora.paused_until = Some(now + chrono::Duration::hours(1));
        store.upsert(&dora).unwrap();

        let active = store.list_active(now).unwrap();
        let names: Vec<&str> = active.iter().map(|s| s.user_id.as_str()).collect();
        assert_eq!(names, vec!["alice"]);
    }

    #[test]
    fn is_active_respects_setup_and_pause() {
        let now = Utc::now();
        let mut s = sample("u");
        assert!(!s.is_active(now));
        s.enabled = true;
        assert!(!s.is_active(now), "setup not done");
        s.setup_completed_at = Some(now);
        assert!(s.is_active(now));
        s.paused_until = Some(now + chrono::Duration::minutes(30));
        assert!(!s.is_active(now), "paused into the future");
        s.paused_until = Some(now - chrono::Duration::minutes(1));
        assert!(s.is_active(now), "pause expired");
    }

    #[test]
    fn mark_checkin_stamps_only_that_column() {
        let (_dir, store) = fresh_store();
        let mut s = sample("alice");
        s.enabled = true;
        s.safety_contact_user_id = Some("david".into());
        s.setup_completed_at = Some(Utc::now());
        store.upsert(&s).unwrap();

        // Should round-trip as None initially.
        assert!(store.get("alice").unwrap().unwrap().last_checkin_at.is_none());

        let at = Utc::now();
        store.mark_checkin("alice", at).unwrap();
        let back = store.get("alice").unwrap().unwrap();
        assert!(back.last_checkin_at.is_some());
        // Round-trip is within 1s (ms precision; bounded).
        let diff = (back.last_checkin_at.unwrap() - at).num_milliseconds().abs();
        assert!(diff < 1000, "diff {diff}ms");

        // Other fields untouched.
        assert!(back.enabled);
        assert_eq!(back.safety_contact_user_id.as_deref(), Some("david"));
    }

    #[test]
    fn increment_and_reset_missed_checkins_round_trip() {
        let (_dir, store) = fresh_store();
        let mut s = sample("alice");
        s.enabled = true;
        s.safety_contact_user_id = Some("david".into());
        s.setup_completed_at = Some(Utc::now());
        store.upsert(&s).unwrap();

        // Three consecutive fires without a reset → count is 3.
        assert_eq!(store.increment_missed_checkins("alice").unwrap(), 1);
        assert_eq!(store.increment_missed_checkins("alice").unwrap(), 2);
        assert_eq!(store.increment_missed_checkins("alice").unwrap(), 3);
        let back = store.get("alice").unwrap().unwrap();
        assert_eq!(back.consecutive_missed_checkins, 3);

        // Reset (user sent a message).
        store.reset_missed_checkins("alice").unwrap();
        let back = store.get("alice").unwrap().unwrap();
        assert_eq!(back.consecutive_missed_checkins, 0);

        // Subsequent fires resume from 0.
        assert_eq!(store.increment_missed_checkins("alice").unwrap(), 1);
    }

    #[test]
    fn increment_missed_on_missing_user_is_safe() {
        let (_dir, store) = fresh_store();
        // No row, no panic; UPDATE matches zero rows. Reading back
        // returns 0 (the unwrap_or in the helper).
        assert_eq!(store.increment_missed_checkins("ghost").unwrap(), 0);
    }

    #[test]
    fn mark_checkin_is_idempotent_on_missing_user() {
        // Mark before enable shouldn't crash; it's just a no-op
        // (rusqlite returns 0 rows affected).
        let (_dir, store) = fresh_store();
        store.mark_checkin("ghost", Utc::now()).unwrap();
        assert!(store.get("ghost").unwrap().is_none());
    }

    #[test]
    fn checkins_today_counts_and_rolls_over_per_local_day() {
        let (_dir, store) = fresh_store();
        let mut s = sample("alice");
        s.enabled = true;
        s.setup_completed_at = Some(Utc::now());
        store.upsert(&s).unwrap();

        // No fires yet.
        assert_eq!(store.checkins_today("alice", "2026-06-16").unwrap(), 0);

        // Two fires on the same local day accumulate.
        assert_eq!(store.bump_checkins_today("alice", "2026-06-16").unwrap(), 1);
        assert_eq!(store.bump_checkins_today("alice", "2026-06-16").unwrap(), 2);
        assert_eq!(store.checkins_today("alice", "2026-06-16").unwrap(), 2);

        // New local day → counter rolls over (reads 0, next bump resets to 1).
        assert_eq!(store.checkins_today("alice", "2026-06-17").unwrap(), 0);
        assert_eq!(store.bump_checkins_today("alice", "2026-06-17").unwrap(), 1);
        // The prior day now reads 0 — the stored day has moved on.
        assert_eq!(store.checkins_today("alice", "2026-06-16").unwrap(), 0);
    }

    #[test]
    fn delete_removes_row() {
        let (_dir, store) = fresh_store();
        let s = sample("alice");
        store.upsert(&s).unwrap();
        assert!(store.get("alice").unwrap().is_some());
        store.delete("alice").unwrap();
        assert!(store.get("alice").unwrap().is_none());
    }
}
