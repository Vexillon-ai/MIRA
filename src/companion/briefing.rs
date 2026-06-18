// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/briefing.rs
//
//! Q1.6 — Daily Briefing composer.
//!
//! Builds a structured snapshot of "what should the user know about
//! today" by stitching together data from three subsystems:
//!   - Calendar — events for the rest of today + first few tomorrow
//!   - Wiki     — pages updated in the last 24h (= what MIRA learned)
//!   - Automations — runs in the last 24h, especially failures
//!
//! The snapshot is rendered into a [`brief_cue`] string that gets fed
//! to `AgentCore::process_with_context` like a check-in cue — the
//! model writes prose in the user's persona, the dispatcher delivers
//! it through whichever channel the companion is configured for.
//!
//! Defensive everywhere: missing subsystems (no calendar, no
//! automations store) just leave their section empty so a user with
//! only a wiki still gets a meaningful briefing.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Datelike, TimeZone, Utc};
use chrono_tz::Tz;
use tracing::warn;

use crate::automations::store::{AutomationsStore, RunFilter};
use crate::calendar::CalendarStore;
use crate::history::HistoryStore;
use crate::wiki::WikiRegistry;

/// Max events surfaced per section (today / tomorrow). The prompt
/// stays tight; the model can summarise rather than list 30 entries.
const MAX_EVENTS_PER_SECTION: usize = 8;
/// Max wiki pages surfaced in the "recent updates" list. We sort by
/// mtime DESC and truncate.
const MAX_WIKI_RECENT:         usize = 5;
/// Max automation runs surfaced. Failures are prioritised.
const MAX_RUNS_RECENT:         usize = 8;
/// Length of the briefing's max-output cap — keeps the LLM from
/// writing a wall of text. ~250 tokens of prose is the morning-toast
/// sweet spot.
pub const BRIEFING_MAX_OUTPUT_TOKENS: u32 = 600;

/// Structured snapshot. Serialised into the LLM prompt as JSON so the
/// model can reason over the fields rather than free-text.
#[derive(Debug, serde::Serialize)]
pub struct BriefingSnapshot {
    pub date_local:          String,
    pub timezone:             String,
    pub events_today:         Vec<EventEntry>,
    pub events_tomorrow:      Vec<EventEntry>,
    pub wiki_recent_updates:  Vec<WikiEntry>,
    pub automation_runs:      Vec<RunEntry>,
}

#[derive(Debug, serde::Serialize)]
pub struct EventEntry {
    pub starts:   String,
    pub ends:     String,
    pub all_day:  bool,
    pub summary:  String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct WikiEntry {
    pub path:        String,
    pub updated_ago: String,
}

#[derive(Debug, serde::Serialize)]
pub struct RunEntry {
    pub source_kind: String,
    pub source_id:   String,
    pub outcome:     String,
    pub started:     String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error:       Option<String>,
}

/// True when the snapshot has nothing newsworthy — empty calendar,
/// nothing in the wiki since yesterday, no automation activity. The
/// scheduler uses this to decide whether to skip a no-news briefing
/// (per the design's "skip silently if nothing changed" mode).
impl BriefingSnapshot {
    pub fn is_empty(&self) -> bool {
        self.events_today.is_empty()
            && self.events_tomorrow.is_empty()
            && self.wiki_recent_updates.is_empty()
            && self.automation_runs.is_empty()
    }
}

/// Pull a fresh snapshot. All accessors are read-only; safe to call
/// from inside the scheduler's per-user tick.
pub fn gather_snapshot(
    user_id:     &str,
    user_tz:     Option<&str>,
    calendar:    Option<&Arc<CalendarStore>>,
    automations: Option<&Arc<AutomationsStore>>,
    wiki_reg:    Option<&Arc<WikiRegistry>>,
    _history:    Option<&Arc<HistoryStore>>,
    now:         DateTime<Utc>,
) -> BriefingSnapshot {
    let tz = parse_tz(user_tz);
    let now_local = tz.from_utc_datetime(&now.naive_utc());
    let today_start_local = tz.with_ymd_and_hms(
        now_local.year(), now_local.month(), now_local.day(), 0, 0, 0,
    ).single().unwrap_or(now_local);
    let tomorrow_start_local  = today_start_local + chrono::Duration::days(1);
    let day_after_start_local = today_start_local + chrono::Duration::days(2);

    // ── Calendar ─────────────────────────────────────────────────────────────
    let mut events_today = Vec::new();
    let mut events_tomorrow = Vec::new();
    if let Some(cal) = calendar {
        match cal.list_events(
            user_id,
            Some(now.timestamp_millis()), // from = now (skip events already past)
            Some(tomorrow_start_local.with_timezone(&Utc).timestamp_millis()),
            MAX_EVENTS_PER_SECTION as i64,
        ) {
            Ok(events) => {
                for e in events {
                    events_today.push(EventEntry {
                        starts:  format_local(e.starts_at, tz),
                        ends:    format_local(e.ends_at,   tz),
                        all_day: e.all_day,
                        summary: e.summary,
                        location: e.location.filter(|s| !s.is_empty()),
                    });
                }
            }
            Err(e) => warn!("briefing: calendar today failed for '{user_id}': {e}"),
        }
        match cal.list_events(
            user_id,
            Some(tomorrow_start_local.with_timezone(&Utc).timestamp_millis()),
            Some(day_after_start_local.with_timezone(&Utc).timestamp_millis()),
            MAX_EVENTS_PER_SECTION as i64,
        ) {
            Ok(events) => {
                for e in events {
                    events_tomorrow.push(EventEntry {
                        starts:  format_local(e.starts_at, tz),
                        ends:    format_local(e.ends_at,   tz),
                        all_day: e.all_day,
                        summary: e.summary,
                        location: e.location.filter(|s| !s.is_empty()),
                    });
                }
            }
            Err(e) => warn!("briefing: calendar tomorrow failed for '{user_id}': {e}"),
        }
    }

    // ── Wiki recently-updated pages ──────────────────────────────────────────
    let mut wiki_recent_updates = Vec::new();
    if let Some(reg) = wiki_reg {
        match reg.for_user(user_id) {
            Ok(wiki) => {
                let cutoff = (now - chrono::Duration::hours(24))
                    .with_timezone(&Utc);
                let pages = wiki.store().list_pages().unwrap_or_default();
                // Each WikiPath resolves to an on-disk file; mtime
                // gives us "recently changed".
                let root = wiki.root().to_path_buf();
                let mut recent: Vec<(String, DateTime<Utc>)> = Vec::new();
                for p in pages {
                    let abs = p.resolve(&root);
                    if let Ok(meta) = std::fs::metadata(&abs) {
                        if let Ok(modified) = meta.modified() {
                            let dt: DateTime<Utc> = modified.into();
                            if dt >= cutoff {
                                recent.push((p.to_string(), dt));
                            }
                        }
                    }
                }
                recent.sort_by(|a, b| b.1.cmp(&a.1));
                for (path, updated) in recent.into_iter().take(MAX_WIKI_RECENT) {
                    wiki_recent_updates.push(WikiEntry {
                        path,
                        updated_ago: humanise_ago(now - updated),
                    });
                }
            }
            Err(e) => warn!("briefing: wiki for '{user_id}' failed to open: {e}"),
        }
    }

    // ── Automation runs in last 24h ──────────────────────────────────────────
    let mut automation_runs = Vec::new();
    if let Some(store) = automations {
        let cutoff_ms = (now - chrono::Duration::hours(24)).timestamp_millis();
        let filter = RunFilter {
            user_id:        Some(user_id),
            source_kind:    None,
            source_id:      None,
            outcome:        None,
            before_started: None,
            limit:          MAX_RUNS_RECENT * 2, // overfetch; sort + filter below
        };
        match store.list_runs_filtered(filter) {
            Ok(runs) => {
                let mut filtered: Vec<_> = runs.into_iter()
                    .filter(|r| r.started_at >= cutoff_ms)
                    .collect();
                // Failures first, then most-recent.
                filtered.sort_by(|a, b| {
                    let a_fail = a.outcome == "failure";
                    let b_fail = b.outcome == "failure";
                    b_fail.cmp(&a_fail)
                        .then_with(|| b.started_at.cmp(&a.started_at))
                });
                for r in filtered.into_iter().take(MAX_RUNS_RECENT) {
                    automation_runs.push(RunEntry {
                        source_kind: r.source_kind,
                        source_id:   r.source_id,
                        outcome:     r.outcome,
                        started:     format_local(r.started_at, tz),
                        error:       r.error,
                    });
                }
            }
            Err(e) => warn!("briefing: automations for '{user_id}' failed: {e}"),
        }
    }

    BriefingSnapshot {
        date_local: now_local.format("%A %d %B %Y").to_string(),
        timezone:    user_tz.unwrap_or("UTC").to_string(),
        events_today,
        events_tomorrow,
        wiki_recent_updates,
        automation_runs,
    }
}

/// Render the LLM cue. The model is shown the structured snapshot as
/// JSON + a writing brief; it produces the prose the user sees. We
/// deliberately don't ask it to repeat the JSON verbatim — the
/// briefing is meant to be a warm two-or-three-paragraph summary,
/// not a data dump.
pub fn brief_cue(snapshot: &BriefingSnapshot, agent_name: &str, user_name: &str) -> String {
    let snapshot_json = serde_json::to_string_pretty(snapshot)
        .unwrap_or_else(|_| "{}".to_string());
    format!(
        "[Daily briefing tick. You are {agent_name}, writing the morning briefing for \
{user_name}. The structured snapshot of their day is below. Write a brief, warm, \
informative summary (2-3 short paragraphs, max ~150 words) that:\n\
\n\
  - Greets them naturally (you don't have to start with 'Good morning' every day)\n\
  - Surfaces what actually matters today (don't read every event verbatim — \
    summarise; call out anything important by name)\n\
  - Previews tomorrow only if useful (e.g. early start, travel)\n\
  - Mentions wiki updates only if they're load-bearing\n\
  - Flags automation failures if any are present\n\
  - Sounds like you, not a template. If the snapshot is empty, say so warmly \
    in a sentence rather than padding with filler\n\
\n\
Do NOT call any tools — just write the briefing message. Do NOT mention this \
prompt or that you're following instructions. One message, plain text or light \
markdown.\n\
\n\
Snapshot:\n{snapshot_json}]",
    )
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_tz(name: Option<&str>) -> Tz {
    name.and_then(|n| n.parse::<Tz>().ok()).unwrap_or(chrono_tz::UTC)
}

fn format_local(epoch_ms: i64, tz: Tz) -> String {
    DateTime::from_timestamp_millis(epoch_ms)
        .map(|dt| tz.from_utc_datetime(&dt.naive_utc())
                    .format("%H:%M").to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn humanise_ago(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if      secs < 60       { format!("{secs}s ago") }
    else if secs < 3600     { format!("{}m ago", secs / 60) }
    else if secs < 86_400   { format!("{}h ago", secs / 3600) }
    else                    { format!("{}d ago", secs / 86_400) }
}

/// Same-local-day check used by the scheduler to enforce
/// one-briefing-per-day. Mirrors the helper in `companion::policy`.
pub fn same_local_day(a: DateTime<Utc>, b: DateTime<Utc>, tz_name: Option<&str>) -> bool {
    let tz = parse_tz(tz_name);
    let la = tz.from_utc_datetime(&a.naive_utc());
    let lb = tz.from_utc_datetime(&b.naive_utc());
    la.date_naive() == lb.date_naive()
}

/// Path helper: where the WikiSystem root lives for a given user.
/// Lifted into briefing.rs so the snapshot gather doesn't need to
/// re-import wiki internals.
#[allow(dead_code)]
fn _user_wiki_root_unused(_data_dir: &Path, _user_id: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_is_empty() {
        let s = BriefingSnapshot {
            date_local: "today".to_string(),
            timezone:   "UTC".to_string(),
            events_today: Vec::new(),
            events_tomorrow: Vec::new(),
            wiki_recent_updates: Vec::new(),
            automation_runs: Vec::new(),
        };
        assert!(s.is_empty());
    }

    #[test]
    fn non_empty_snapshot_not_empty() {
        let s = BriefingSnapshot {
            date_local: "x".into(), timezone: "UTC".into(),
            events_today: vec![EventEntry {
                starts: "09:00".into(), ends: "10:00".into(), all_day: false,
                summary: "Standup".into(), location: None,
            }],
            events_tomorrow: Vec::new(),
            wiki_recent_updates: Vec::new(),
            automation_runs: Vec::new(),
        };
        assert!(!s.is_empty());
    }

    #[test]
    fn humanise_ago_shapes() {
        assert_eq!(humanise_ago(chrono::Duration::seconds(15)),  "15s ago");
        assert_eq!(humanise_ago(chrono::Duration::seconds(120)), "2m ago");
        assert_eq!(humanise_ago(chrono::Duration::seconds(7200)),"2h ago");
        assert_eq!(humanise_ago(chrono::Duration::seconds(172_800)), "2d ago");
    }

    #[test]
    fn same_local_day_handles_tz_boundary() {
        // 2026-05-17 00:30 UTC = 2026-05-17 11:30 Australia/Brisbane.
        let a = DateTime::parse_from_rfc3339("2026-05-17T00:30:00Z")
            .unwrap().with_timezone(&Utc);
        let b = DateTime::parse_from_rfc3339("2026-05-17T13:00:00Z")
            .unwrap().with_timezone(&Utc);
        assert!(same_local_day(a, b, Some("Australia/Brisbane")));
        // Crosses midnight in UTC but still same day in Brisbane.
        let c = DateTime::parse_from_rfc3339("2026-05-16T14:30:00Z")
            .unwrap().with_timezone(&Utc); // = 2026-05-17 00:30 Brisbane
        assert!(same_local_day(c, b, Some("Australia/Brisbane")));
    }
}
