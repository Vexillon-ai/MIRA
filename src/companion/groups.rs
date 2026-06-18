// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/groups.rs
//! Companion-aware extensions to MIRA's existing groups feature
//!.
//!
//! Two new tables in `companion.db`, sitting alongside the auth-db
//! `groups` + `group_members`:
//!
//! - `companion_group_settings` — per-group companion policy
//! (which signal classes the group relays, which topics stay
//! private even when a covered signal fires).
//! - `companion_group_members` — per-member flags within
//! companion-enabled groups (`contactable_for`, channel
//! preference, mute hours, daily cap, `opt_in`).
//!
//! The companion-config tables don't have FK constraints back to
//! the auth-db groups/group_members tables — they live in a
//! different file. The companion module owns its data; rows for
//! removed groups / members linger until pruned (cheap; the
//! routing gateway filters them out at read time anyway).
//!
//! Design ref: `design-docs/companion/design-proposal.md` §9 + §11.11.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::companion::Result;

// Signal classes a companion-enabled group can relay between
// members. Stored as a snake_case JSON array; an empty array means
// the group is technically companion-enabled but won't relay
// anything (admins can use this state to disable the group
// temporarily without removing the row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    // Distress signals from another member's companion. Highest
    // priority — overrides mute_hours / daily_message_cap.
    Distress,
    // "Haven't replied to last 3 check-ins" notices. Respects
    // mute_hours + daily_cap.
    MissedCheckin,
    // User explicitly asked their companion to contact someone.
    // (Reserved for a follow-up — the safety floor doesn't emit
    // these yet, but the field is here so the schema is stable.)
    HelpRequest,
    // Daily / weekly summary updates ("dad had a nice chat
    // today"). Off by default; opt-in per member.
    General,
}

impl SignalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalKind::Distress      => "distress",
            SignalKind::MissedCheckin => "missed_checkin",
            SignalKind::HelpRequest   => "help_request",
            SignalKind::General       => "general",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "distress"       => Some(SignalKind::Distress),
            "missed_checkin" => Some(SignalKind::MissedCheckin),
            "help_request"   => Some(SignalKind::HelpRequest),
            "general"        => Some(SignalKind::General),
            _ => None,
        }
    }
}

// Per-group policy. `allowed_signals` gates which classes are
// eligible to flow at all; per-member `contactable_for` further
// narrows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupCompanionPolicy {
    pub group_id: String,
    // Signal classes this group relays. An empty list means
    // "companion-enabled but currently dormant" — useful for
    // temporary pauses without removing the row.
    pub allowed_signals: Vec<SignalKind>,
    // Topics that NEVER appear in cross-user notices, even when a
    // covered signal fires. Free-form strings (e.g.
    // "health_details", "finances") — the safety floor's notice
    // builder strips any sentence containing these substrings
    // before forwarding.
    pub privacy_topics: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Per-member flags inside a companion-enabled group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupCompanionMember {
    pub group_id: String,
    pub user_id: String,
    // Which signal classes THIS member wants to receive. Subset of
    // the group's `allowed_signals`; the routing gateway
    // intersects the two.
    pub contactable_for: Vec<SignalKind>,
    // Ordered channel preference for delivery to this member.
    // Empty = fall back to web (the only channel implemented in
    // 's safety delivery).
    pub channel_preference: Vec<String>,
    // Pairs of "HH:MM"–"HH:MM" interpreted in the member's tz.
    // During these windows the routing gateway suppresses
    // `MissedCheckin` / `General` / `HelpRequest` (not
    // `Distress` — distress always delivers).
    pub mute_hours: Vec<(String, String)>,
    // Maximum signals to deliver to this member per local day.
    // `Distress` bypasses this cap (it's the whole point of the
    // safety floor); other classes respect it.
    pub daily_message_cap: u32,
    // Hard gate — defaults to `false`. The user must explicitly
    // say "yes I want to receive these notifications" before any
    // signal reaches them. Admins setting up a group can flip
    // this for users they configure on their behalf (a son
    // setting up companion mode for a father can opt the son
    // himself in).
    pub opt_in: bool,
    pub joined_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl GroupCompanionMember {
    // Returns true if THIS member would accept a `signal_kind` —
    // ignoring mute_hours and daily_cap (those are time-of-call
    // checks done by the routing gateway). Just an opt-in +
    // contactable_for intersection check.
    pub fn accepts_signal(&self, signal_kind: SignalKind) -> bool {
        self.opt_in && self.contactable_for.contains(&signal_kind)
    }
}

// What CRUD operations the safety floor + admin endpoints need.
pub struct CompanionGroupStore {
    conn: Mutex<Connection>,
}

impl CompanionGroupStore {
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
            CREATE TABLE IF NOT EXISTS companion_group_settings (
                group_id            TEXT PRIMARY KEY,
                allowed_signals_json TEXT NOT NULL DEFAULT '[]',
                privacy_topics_json  TEXT NOT NULL DEFAULT '[]',
                created_at          INTEGER NOT NULL,
                updated_at          INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS companion_group_members (
                group_id              TEXT NOT NULL,
                user_id               TEXT NOT NULL,
                contactable_for_json  TEXT NOT NULL DEFAULT '[]',
                channel_preference_json TEXT NOT NULL DEFAULT '[]',
                mute_hours_json       TEXT NOT NULL DEFAULT '[]',
                daily_message_cap     INTEGER NOT NULL DEFAULT 3,
                opt_in                INTEGER NOT NULL DEFAULT 0,
                joined_at             INTEGER NOT NULL,
                updated_at            INTEGER NOT NULL,
                PRIMARY KEY (group_id, user_id)
            );
            CREATE INDEX IF NOT EXISTS idx_cgm_user ON companion_group_members(user_id);
        "#)?;
        Ok(())
    }

    // ── Policy CRUD ──────────────────────────────────────────────────────

    // Fetch a group's companion policy, or `None` if the group
    // isn't companion-enabled.
    pub fn get_policy(&self, group_id: &str) -> Result<Option<GroupCompanionPolicy>> {
        let conn = self.conn.lock().expect("companion group store poisoned");
        let p: Option<GroupCompanionPolicy> = conn.query_row(
            "SELECT group_id, allowed_signals_json, privacy_topics_json, created_at, updated_at
             FROM companion_group_settings WHERE group_id = ?1",
            params![group_id],
            row_to_policy,
        ).optional()?;
        Ok(p)
    }

    // Upsert a group's policy (also "companion-enables" the group
    // creating a row at all is the enable gesture).
    pub fn upsert_policy(&self, p: &GroupCompanionPolicy) -> Result<()> {
        let signals: Vec<&str> = p.allowed_signals.iter().map(|s| s.as_str()).collect();
        let signals_json = serde_json::to_string(&signals)?;
        let topics_json  = serde_json::to_string(&p.privacy_topics)?;
        let conn = self.conn.lock().expect("companion group store poisoned");
        conn.execute(
            "INSERT INTO companion_group_settings
                (group_id, allowed_signals_json, privacy_topics_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(group_id) DO UPDATE SET
                allowed_signals_json = excluded.allowed_signals_json,
                privacy_topics_json  = excluded.privacy_topics_json,
                updated_at           = excluded.updated_at",
            params![
                p.group_id,
                signals_json,
                topics_json,
                p.created_at.timestamp_millis(),
                p.updated_at.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    // Disable companion notifications on a group. Removes the
    // policy + every member row. (Doesn't touch the auth-db
    // groups/group_members rows — those remain.)
    pub fn delete_group(&self, group_id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("companion group store poisoned");
        conn.execute(
            "DELETE FROM companion_group_settings WHERE group_id = ?1",
            params![group_id],
        )?;
        conn.execute(
            "DELETE FROM companion_group_members WHERE group_id = ?1",
            params![group_id],
        )?;
        Ok(())
    }

    // List all companion-enabled groups (just policies, no members).
    pub fn list_policies(&self) -> Result<Vec<GroupCompanionPolicy>> {
        let conn = self.conn.lock().expect("companion group store poisoned");
        let mut stmt = conn.prepare(
            "SELECT group_id, allowed_signals_json, privacy_topics_json, created_at, updated_at
             FROM companion_group_settings ORDER BY group_id ASC",
        )?;
        let rows = stmt.query_map([], row_to_policy)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    // ── Member CRUD ──────────────────────────────────────────────────────

    // Upsert a single member's flags within a companion-enabled
    // group. The group must already have a policy row (i.e. be
    // companion-enabled); this call doesn't auto-enable the group
    // that's the admin's deliberate decision.
    pub fn upsert_member(&self, m: &GroupCompanionMember) -> Result<()> {
        let contactable: Vec<&str> = m.contactable_for.iter().map(|s| s.as_str()).collect();
        let conn = self.conn.lock().expect("companion group store poisoned");
        conn.execute(
            "INSERT INTO companion_group_members
                (group_id, user_id, contactable_for_json, channel_preference_json,
                 mute_hours_json, daily_message_cap, opt_in, joined_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(group_id, user_id) DO UPDATE SET
                contactable_for_json     = excluded.contactable_for_json,
                channel_preference_json  = excluded.channel_preference_json,
                mute_hours_json          = excluded.mute_hours_json,
                daily_message_cap        = excluded.daily_message_cap,
                opt_in                   = excluded.opt_in,
                updated_at               = excluded.updated_at",
            params![
                m.group_id,
                m.user_id,
                serde_json::to_string(&contactable)?,
                serde_json::to_string(&m.channel_preference)?,
                serde_json::to_string(&m.mute_hours)?,
                m.daily_message_cap as i64,
                if m.opt_in { 1i64 } else { 0i64 },
                m.joined_at.timestamp_millis(),
                m.updated_at.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    pub fn delete_member(&self, group_id: &str, user_id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("companion group store poisoned");
        conn.execute(
            "DELETE FROM companion_group_members
             WHERE group_id = ?1 AND user_id = ?2",
            params![group_id, user_id],
        )?;
        Ok(())
    }

    pub fn get_member(&self, group_id: &str, user_id: &str)
        -> Result<Option<GroupCompanionMember>>
    {
        let conn = self.conn.lock().expect("companion group store poisoned");
        let m: Option<GroupCompanionMember> = conn.query_row(
            "SELECT group_id, user_id, contactable_for_json, channel_preference_json,
                    mute_hours_json, daily_message_cap, opt_in, joined_at, updated_at
             FROM companion_group_members
             WHERE group_id = ?1 AND user_id = ?2",
            params![group_id, user_id],
            row_to_member,
        ).optional()?;
        Ok(m)
    }

    // All members of a companion-enabled group.
    pub fn list_members(&self, group_id: &str) -> Result<Vec<GroupCompanionMember>> {
        let conn = self.conn.lock().expect("companion group store poisoned");
        let mut stmt = conn.prepare(
            "SELECT group_id, user_id, contactable_for_json, channel_preference_json,
                    mute_hours_json, daily_message_cap, opt_in, joined_at, updated_at
             FROM companion_group_members
             WHERE group_id = ?1
             ORDER BY user_id ASC",
        )?;
        let rows = stmt.query_map(params![group_id], row_to_member)?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    // Every companion-enabled group that `user_id` is a member of.
    // Used by the safety floor to enumerate who might receive a
    // notice when a user emits a signal.
    pub fn list_groups_for_user(&self, user_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().expect("companion group store poisoned");
        let mut stmt = conn.prepare(
            "SELECT m.group_id FROM companion_group_members m
             INNER JOIN companion_group_settings g ON g.group_id = m.group_id
             WHERE m.user_id = ?1
             ORDER BY m.group_id ASC",
        )?;
        let rows = stmt.query_map(params![user_id], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }
}

// ── Row mappers ──────────────────────────────────────────────────────────────

fn row_to_policy(row: &rusqlite::Row<'_>) -> rusqlite::Result<GroupCompanionPolicy> {
    let signals_json: String = row.get(1)?;
    let topics_json:  String = row.get(2)?;
    let created_ms:   i64    = row.get(3)?;
    let updated_ms:   i64    = row.get(4)?;

    let signal_strs: Vec<String> = serde_json::from_str(&signals_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            1, rusqlite::types::Type::Text, Box::new(e),
        ))?;
    let allowed_signals: Vec<SignalKind> = signal_strs.iter()
        .filter_map(|s| SignalKind::parse(s))
        .collect();

    let privacy_topics: Vec<String> = serde_json::from_str(&topics_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            2, rusqlite::types::Type::Text, Box::new(e),
        ))?;

    Ok(GroupCompanionPolicy {
        group_id: row.get(0)?,
        allowed_signals,
        privacy_topics,
        created_at: DateTime::from_timestamp_millis(created_ms).unwrap_or_else(Utc::now),
        updated_at: DateTime::from_timestamp_millis(updated_ms).unwrap_or_else(Utc::now),
    })
}

fn row_to_member(row: &rusqlite::Row<'_>) -> rusqlite::Result<GroupCompanionMember> {
    let contactable_json: String = row.get(2)?;
    let channel_json:     String = row.get(3)?;
    let mute_json:        String = row.get(4)?;
    let cap_i:            i64    = row.get(5)?;
    let opt_in_i:         i64    = row.get(6)?;
    let joined_ms:        i64    = row.get(7)?;
    let updated_ms:       i64    = row.get(8)?;

    let contactable_strs: Vec<String> = serde_json::from_str(&contactable_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            2, rusqlite::types::Type::Text, Box::new(e),
        ))?;
    let contactable_for: Vec<SignalKind> = contactable_strs.iter()
        .filter_map(|s| SignalKind::parse(s))
        .collect();
    let channel_preference: Vec<String> = serde_json::from_str(&channel_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            3, rusqlite::types::Type::Text, Box::new(e),
        ))?;
    let mute_hours: Vec<(String, String)> = serde_json::from_str(&mute_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            4, rusqlite::types::Type::Text, Box::new(e),
        ))?;

    Ok(GroupCompanionMember {
        group_id: row.get(0)?,
        user_id:  row.get(1)?,
        contactable_for,
        channel_preference,
        mute_hours,
        daily_message_cap: cap_i.max(0) as u32,
        opt_in: opt_in_i != 0,
        joined_at:  DateTime::from_timestamp_millis(joined_ms).unwrap_or_else(Utc::now),
        updated_at: DateTime::from_timestamp_millis(updated_ms).unwrap_or_else(Utc::now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh_store() -> (tempfile::TempDir, CompanionGroupStore) {
        let dir = tempdir().unwrap();
        let store = CompanionGroupStore::open(&dir.path().join("companion.db")).unwrap();
        (dir, store)
    }

    fn sample_policy(group_id: &str) -> GroupCompanionPolicy {
        let now = Utc::now();
        GroupCompanionPolicy {
            group_id: group_id.into(),
            allowed_signals: vec![SignalKind::Distress, SignalKind::MissedCheckin],
            privacy_topics: vec!["health_details".into()],
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_member(group_id: &str, user_id: &str) -> GroupCompanionMember {
        let now = Utc::now();
        GroupCompanionMember {
            group_id: group_id.into(),
            user_id: user_id.into(),
            contactable_for: vec![SignalKind::Distress, SignalKind::MissedCheckin],
            channel_preference: vec!["web".into()],
            mute_hours: vec![],
            daily_message_cap: 3,
            opt_in: false,
            joined_at: now,
            updated_at: now,
        }
    }

    // ── Signal-kind round-trips ──────────────────────────────────

    #[test]
    fn signal_kind_round_trips() {
        for s in [SignalKind::Distress, SignalKind::MissedCheckin,
                  SignalKind::HelpRequest, SignalKind::General] {
            assert_eq!(SignalKind::parse(s.as_str()), Some(s));
        }
        assert_eq!(SignalKind::parse("nope"), None);
    }

    // ── Policy CRUD ──────────────────────────────────────────────

    #[test]
    fn policy_round_trip() {
        let (_dir, store) = fresh_store();
        let p = sample_policy("family");
        store.upsert_policy(&p).unwrap();
        let back = store.get_policy("family").unwrap().unwrap();
        assert_eq!(back.allowed_signals, p.allowed_signals);
        assert_eq!(back.privacy_topics, p.privacy_topics);
    }

    #[test]
    fn get_policy_returns_none_for_non_companion_group() {
        let (_dir, store) = fresh_store();
        assert!(store.get_policy("not-enabled").unwrap().is_none());
    }

    #[test]
    fn upsert_policy_updates_existing() {
        let (_dir, store) = fresh_store();
        let mut p = sample_policy("family");
        store.upsert_policy(&p).unwrap();
        p.allowed_signals = vec![SignalKind::Distress]; // narrow scope
        p.privacy_topics.push("finances".into());
        p.updated_at = Utc::now();
        store.upsert_policy(&p).unwrap();
        let back = store.get_policy("family").unwrap().unwrap();
        assert_eq!(back.allowed_signals, vec![SignalKind::Distress]);
        assert!(back.privacy_topics.contains(&"finances".to_string()));
    }

    #[test]
    fn delete_group_removes_policy_and_members() {
        let (_dir, store) = fresh_store();
        store.upsert_policy(&sample_policy("family")).unwrap();
        store.upsert_member(&sample_member("family", "alice")).unwrap();
        store.upsert_member(&sample_member("family", "bob")).unwrap();
        store.delete_group("family").unwrap();
        assert!(store.get_policy("family").unwrap().is_none());
        assert_eq!(store.list_members("family").unwrap().len(), 0);
    }

    #[test]
    fn list_policies_returns_all() {
        let (_dir, store) = fresh_store();
        store.upsert_policy(&sample_policy("family")).unwrap();
        store.upsert_policy(&sample_policy("work")).unwrap();
        let all = store.list_policies().unwrap();
        let ids: Vec<&str> = all.iter().map(|p| p.group_id.as_str()).collect();
        assert!(ids.contains(&"family"));
        assert!(ids.contains(&"work"));
    }

    // ── Member CRUD ──────────────────────────────────────────────

    #[test]
    fn member_round_trip() {
        let (_dir, store) = fresh_store();
        store.upsert_policy(&sample_policy("family")).unwrap();
        let mut m = sample_member("family", "alice");
        m.opt_in = true;
        m.mute_hours = vec![("22:00".into(), "07:00".into())];
        store.upsert_member(&m).unwrap();
        let back = store.get_member("family", "alice").unwrap().unwrap();
        assert!(back.opt_in);
        assert_eq!(back.mute_hours, vec![("22:00".to_string(), "07:00".to_string())]);
        assert_eq!(back.daily_message_cap, 3);
    }

    #[test]
    fn list_members_returns_all() {
        let (_dir, store) = fresh_store();
        store.upsert_policy(&sample_policy("family")).unwrap();
        store.upsert_member(&sample_member("family", "alice")).unwrap();
        store.upsert_member(&sample_member("family", "bob")).unwrap();
        let members = store.list_members("family").unwrap();
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn list_groups_for_user_returns_only_companion_enabled() {
        let (_dir, store) = fresh_store();
        store.upsert_policy(&sample_policy("family")).unwrap();
        store.upsert_member(&sample_member("family", "alice")).unwrap();
        // Alice is also a member in some other (non-companion) group
        // represented by a member row without a policy row — the
        // INNER JOIN should filter it out.
        store.upsert_member(&sample_member("orphan", "alice")).unwrap();

        let groups = store.list_groups_for_user("alice").unwrap();
        assert_eq!(groups, vec!["family".to_string()]);
    }

    #[test]
    fn delete_member_removes_only_that_row() {
        let (_dir, store) = fresh_store();
        store.upsert_policy(&sample_policy("family")).unwrap();
        store.upsert_member(&sample_member("family", "alice")).unwrap();
        store.upsert_member(&sample_member("family", "bob")).unwrap();
        store.delete_member("family", "alice").unwrap();
        assert!(store.get_member("family", "alice").unwrap().is_none());
        assert!(store.get_member("family", "bob").unwrap().is_some());
    }

    #[test]
    fn member_accepts_signal_requires_opt_in_and_contactable_for() {
        let mut m = sample_member("family", "alice");
        // Default: opt_in is false → never accepts.
        assert!(!m.accepts_signal(SignalKind::Distress));
        m.opt_in = true;
        assert!(m.accepts_signal(SignalKind::Distress));
        // Removing the kind from contactable_for blocks it.
        m.contactable_for = vec![SignalKind::MissedCheckin];
        assert!(!m.accepts_signal(SignalKind::Distress));
        assert!(m.accepts_signal(SignalKind::MissedCheckin));
    }

    #[test]
    fn schema_idempotent_across_opens() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("companion.db");
        let _s1 = CompanionGroupStore::open(&path).unwrap();
        let _s2 = CompanionGroupStore::open(&path).unwrap();
        let _s3 = CompanionGroupStore::open(&path).unwrap();
    }
}
