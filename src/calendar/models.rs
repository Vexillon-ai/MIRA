// SPDX-License-Identifier: AGPL-3.0-or-later

// src/calendar/models.rs

//! Canonical calendar event model.
//!
//! MIRA stores events in its own schema regardless of which external source
//! they came from. External sync adapters translate their native format into
//! these fields on pull; UI and agent tools only ever see this shape.

use serde::{Deserialize, Serialize};

/// Where an event came from. Events with `source != Native` are mirrors of
/// an external row (identified by `external_id`) and are replaced wholesale
/// on each sync pull. Native events are created in MIRA and never overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSource {
    Native,
    Caldav,
    Google,
    Outlook,
}

impl EventSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventSource::Native  => "native",
            EventSource::Caldav  => "caldav",
            EventSource::Google  => "google",
            EventSource::Outlook => "outlook",
        }
    }
    pub fn parse(s: &str) -> EventSource {
        match s {
            "caldav"  => EventSource::Caldav,
            "google"  => EventSource::Google,
            "outlook" => EventSource::Outlook,
            _         => EventSource::Native,
        }
    }
}

/// What kind of calendar item this is. Events occupy a time range; notes are
/// attached to a day (or day range) without an implied appointment. Both share
/// the same storage row so listing/sync code only deals with one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Event,
    Note,
}

impl Default for EventKind {
    fn default() -> Self { EventKind::Event }
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Event => "event",
            EventKind::Note  => "note",
        }
    }
    pub fn parse(s: &str) -> EventKind {
        match s {
            "note" => EventKind::Note,
            _      => EventKind::Event,
        }
    }
}

/// One calendar event. All timestamps are stored in UTC as milliseconds since
/// the epoch. Callers that need local-time rendering should apply the user's
/// timezone preference at the UI layer (see `NowTool` for the lookup).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id:              String,
    pub owner_user_id:   String,
    pub summary:         String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description:     Option<String>,
    pub starts_at:       i64,
    pub ends_at:         i64,
    #[serde(default)]
    pub all_day:         bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location:        Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rrule:           Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status:          Option<String>,
    pub source:          EventSource,
    #[serde(default)]
    pub kind:            EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id:     Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at:  Option<i64>,
    pub created_at:      i64,
    pub updated_at:      i64,
}

/// Subset of fields a caller supplies on create / update. `id`, timestamps,
/// and source bookkeeping are filled in by the store.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventInput {
    pub summary:     String,
    #[serde(default)]
    pub description: Option<String>,
    pub starts_at:   i64,
    pub ends_at:     i64,
    #[serde(default)]
    pub all_day:     bool,
    #[serde(default)]
    pub location:    Option<String>,
    #[serde(default)]
    pub rrule:       Option<String>,
    #[serde(default)]
    pub status:      Option<String>,
    #[serde(default)]
    pub kind:        EventKind,
    /// Transient (not stored as a column): when true and the caller is an admin,
    /// the event is created/updated under the shared org owner so every user
    /// sees it. Ignored for non-admins. See `store::SHARED_OWNER`.
    #[serde(default)]
    pub shared:      bool,
    /// Transient: when set (and admin), the event is scoped to this group —
    /// only its members see it (owner = `grp:<group_id>`). Takes precedence over
    /// `shared`. See `store::group_owner`.
    #[serde(default)]
    pub group_id:    Option<String>,
}
