// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/calendar.rs
//! Agent-callable calendar tools.
//!
//! Four thin tools over `CalendarStore`:
//! - `calendar_list_events`  — range-filtered read
//! - `calendar_create_event` — always writes to the native store
//! - `calendar_update_event` — mutates native events only (external mirrors
//!                             are read-only; see store docstring)
//! - `calendar_delete_event` — same native-only constraint
//!
//! Each tool resolves the caller via the trusted `_user_id` key the chat
//! handler injects into every tool-call payload (same mechanism as
//! `ToolRegistry::execute` uses for audit-actor attribution).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Utc};
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::calendar::{CalendarStore, EventInput};
use crate::calendar::models::EventKind;
use crate::MiraError;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn caller(args: &ToolArgs) -> Result<String, MiraError> {
    args.get("_user_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| MiraError::ToolError(
            "calendar tool called without caller identity".into(),
        ))
}

/// Parse an ISO-8601 / epoch-ms value into milliseconds since epoch. Accepts:
/// - integer (epoch ms)
/// - RFC 3339 timestamp with offset (`"2026-04-24T12:00:00Z"` / `+10:00`) — an exact instant
/// - **zone-less datetime** (`"2026-06-19T15:00:00"`, also space-separated, optional seconds) —
///   the model commonly emits these; interpreted as wall-clock time in the
///   caller's timezone (`tz`, an IANA name) when known, else UTC
/// - `"YYYY-MM-DD"` date → UTC midnight
fn parse_time(v: &Value, tz: Option<&str>) -> Result<i64, String> {
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    let Some(s) = v.as_str() else {
        return Err("time must be ISO-8601 string or epoch ms integer".into());
    };
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() { return Ok(n); }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc).timestamp_millis());
    }
    // Zone-less datetime → interpret as the caller's local wall-clock time.
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(naive_local_to_utc_ms(ndt, tz));
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let ndt = d.and_hms_opt(0, 0, 0).ok_or("invalid date")?;
        return Ok(Utc.from_utc_datetime(&ndt).timestamp_millis());
    }
    Err(format!("unrecognised time: '{}'", s))
}

/// Interpret a zone-less datetime as wall-clock time in `tz` (IANA name) and
/// return the UTC instant in epoch ms. Falls back to UTC when `tz` is absent or
/// unparseable; a DST gap (no such local time) falls back to the UTC reading.
fn naive_local_to_utc_ms(ndt: NaiveDateTime, tz: Option<&str>) -> i64 {
    if let Some(z) = tz.and_then(|n| n.parse::<chrono_tz::Tz>().ok()) {
        return match z.from_local_datetime(&ndt) {
            LocalResult::Single(dt)       => dt.with_timezone(&Utc).timestamp_millis(),
            LocalResult::Ambiguous(dt, _) => dt.with_timezone(&Utc).timestamp_millis(),
            LocalResult::None             => Utc.from_utc_datetime(&ndt).timestamp_millis(),
        };
    }
    Utc.from_utc_datetime(&ndt).timestamp_millis()
}

/// The caller's IANA timezone, injected by the chat handler as `_user_tz`.
fn caller_tz(args: &ToolArgs) -> Option<&str> {
    args.get("_user_tz").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

fn read_input(args: &ToolArgs) -> Result<EventInput, String> {
    let summary = args.get("summary")
        .and_then(|v| v.as_str())
        .ok_or("summary (string) is required")?;
    if summary.trim().is_empty() {
        return Err("summary is required".into());
    }
    let tz = caller_tz(args);
    let starts_at = match args.get("starts_at") {
        Some(v) => parse_time(v, tz)?,
        None    => return Err("starts_at is required".into()),
    };
    let ends_at = match args.get("ends_at") {
        Some(v) => parse_time(v, tz)?,
        None    => starts_at + 3_600_000,
    };
    if ends_at < starts_at {
        return Err("ends_at must be >= starts_at".into());
    }
    let kind = args.get("kind").and_then(|v| v.as_str())
        .map(EventKind::parse)
        .unwrap_or_default();
    Ok(EventInput {
        summary:     summary.to_string(),
        description: args.get("description").and_then(|v| v.as_str()).map(|s| s.to_string()),
        starts_at,
        ends_at,
        all_day:     args.get("all_day").and_then(|v| v.as_bool()).unwrap_or(false),
        location:    args.get("location").and_then(|v| v.as_str()).map(|s| s.to_string()),
        rrule:       args.get("rrule").and_then(|v| v.as_str()).map(|s| s.to_string()),
        status:      args.get("status").and_then(|v| v.as_str()).map(|s| s.to_string()),
        kind,
        shared:      false,
        group_id:    None,
    })
}

// ── calendar_list_events ─────────────────────────────────────────────────────

pub struct CalendarListEventsTool { store: Arc<CalendarStore> }

impl CalendarListEventsTool {
    pub fn new(store: Arc<CalendarStore>) -> Self { Self { store } }
}

#[async_trait]
impl Tool for CalendarListEventsTool {
    fn name(&self) -> &str { "calendar_list_events" }
    fn description(&self) -> &str {
        "List the caller's calendar events in a given time range. \
         Returns native MIRA events plus any events mirrored from an \
         external calendar (CalDAV / Google / Outlook) when sync is \
         configured. Times must be ISO-8601 strings or epoch ms; defaults \
         to the next 30 days when no range is given."
    }
    fn tier(&self) -> Tier { Tier::Pure }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from":  { "type": "string", "description": "Earliest start time (ISO-8601 or epoch ms)." },
                "to":    { "type": "string", "description": "Latest start time (ISO-8601 or epoch ms)." },
                "limit": { "type": "integer", "description": "Max events to return (default 100, max 500)." }
            }
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let caller = caller(&args)?;

        let tz = caller_tz(&args);
        let from = match args.get("from") {
            Some(v) => match parse_time(v, tz) { Ok(n) => Some(n), Err(e) => return Ok(ToolResult::failure(e)) },
            None    => Some(Utc::now().timestamp_millis()),
        };
        let to = match args.get("to") {
            Some(v) => match parse_time(v, tz) { Ok(n) => Some(n), Err(e) => return Ok(ToolResult::failure(e)) },
            None    => Some(Utc::now().timestamp_millis() + 30 * 24 * 3_600_000),
        };
        let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(100).clamp(1, 500);

        let events = self.store.list_events(&caller, from, to, limit)?;
        let body = json!({
            "from":   from,
            "to":     to,
            "count":  events.len(),
            "events": events,
        });
        Ok(ToolResult::success(body.to_string()))
    }
}

// ── calendar_create_event ────────────────────────────────────────────────────

pub struct CalendarCreateEventTool { store: Arc<CalendarStore> }

impl CalendarCreateEventTool {
    pub fn new(store: Arc<CalendarStore>) -> Self { Self { store } }
}

#[async_trait]
impl Tool for CalendarCreateEventTool {
    fn name(&self) -> &str { "calendar_create_event" }
    fn description(&self) -> &str {
        "Create a new calendar event for the caller. Always writes to the \
         MIRA-native store — external calendar write-back is out of scope. \
         Times accept ISO-8601 with an offset ('2026-04-24T12:00:00Z'), a \
         zone-less local datetime ('2026-04-24T12:00:00', interpreted in the \
         user's timezone), a date ('2026-04-24'), or epoch ms. `ends_at` \
         defaults to one hour after `starts_at`."
    }
    fn tier(&self) -> Tier { Tier::Pure }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["summary", "starts_at"],
            "properties": {
                "summary":     { "type": "string" },
                "description": { "type": "string" },
                "starts_at":   { "type": "string" },
                "ends_at":     { "type": "string" },
                "all_day":     { "type": "boolean" },
                "location":    { "type": "string" },
                "rrule":       { "type": "string", "description": "RFC 5545 RRULE without the leading 'RRULE:' tag." },
                "status":      { "type": "string", "enum": ["tentative", "confirmed", "cancelled"] },
                "kind":        { "type": "string", "enum": ["event", "note"], "description": "'event' (default) for an appointment, 'note' for a day-attached reminder." }
            }
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let caller = caller(&args)?;
        let input = match read_input(&args) {
            Ok(i)  => i,
            Err(e) => return Ok(ToolResult::failure(e)),
        };
        // Dedup guard: a flaky model can re-issue the same create call (we've
        // seen one birthday created 4×). If an identical native event already
        // exists (same summary + start), return it instead of inserting a
        // duplicate, and tell the model so it doesn't keep retrying.
        if let Some(existing) = self.store.find_duplicate_native(&caller, &input.summary, input.starts_at)? {
            return Ok(ToolResult::success(format!(
                "An identical event already exists (\"{}\", id {}) — not creating a duplicate. {}",
                existing.summary, existing.id, serde_json::to_string(&existing)?,
            )));
        }
        let ev = self.store.create_event(&caller, &input)?;
        Ok(ToolResult::success(serde_json::to_string(&ev)?))
    }
}

// ── calendar_update_event ────────────────────────────────────────────────────

pub struct CalendarUpdateEventTool { store: Arc<CalendarStore> }

impl CalendarUpdateEventTool {
    pub fn new(store: Arc<CalendarStore>) -> Self { Self { store } }
}

#[async_trait]
impl Tool for CalendarUpdateEventTool {
    fn name(&self) -> &str { "calendar_update_event" }
    fn description(&self) -> &str {
        "Update a native calendar event. External (synced) events are \
         read-only — attempting to edit them returns not-found. All fields \
         except `id` are replaced; pass the full event shape."
    }
    fn tier(&self) -> Tier { Tier::Pure }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["id", "summary", "starts_at"],
            "properties": {
                "id":          { "type": "string" },
                "summary":     { "type": "string" },
                "description": { "type": "string" },
                "starts_at":   { "type": "string" },
                "ends_at":     { "type": "string" },
                "all_day":     { "type": "boolean" },
                "location":    { "type": "string" },
                "rrule":       { "type": "string" },
                "status":      { "type": "string" },
                "kind":        { "type": "string", "enum": ["event", "note"] }
            }
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let caller = caller(&args)?;
        let id = args.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError("id is required".into()))?
            .to_string();
        let input = match read_input(&args) {
            Ok(i)  => i,
            Err(e) => return Ok(ToolResult::failure(e)),
        };
        match self.store.update_event(&caller, &id, &input)? {
            Some(ev) => Ok(ToolResult::success(serde_json::to_string(&ev)?)),
            None     => Ok(ToolResult::failure(
                "event not found, or it's an external mirror (read-only)",
            )),
        }
    }
}

// ── calendar_delete_event ────────────────────────────────────────────────────

pub struct CalendarDeleteEventTool { store: Arc<CalendarStore> }

impl CalendarDeleteEventTool {
    pub fn new(store: Arc<CalendarStore>) -> Self { Self { store } }
}

#[async_trait]
impl Tool for CalendarDeleteEventTool {
    fn name(&self) -> &str { "calendar_delete_event" }
    fn description(&self) -> &str {
        "Delete a native calendar event by id. External (synced) events \
         are read-only and cannot be deleted from MIRA."
    }
    fn tier(&self) -> Tier { Tier::Pure }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "string" } }
        })
    }
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let caller = caller(&args)?;
        let id = args.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError("id is required".into()))?;
        let deleted = self.store.delete_event(&caller, id)?;
        Ok(ToolResult::success(json!({ "deleted": deleted, "id": id }).to_string()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup() -> (Arc<CalendarStore>, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = Arc::new(CalendarStore::open(&dir.path().join("cal.db")).unwrap());
        (store, dir)
    }

    fn ms(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().timestamp_millis()
    }

    #[test]
    fn parse_time_accepts_zoneless_datetime_in_user_tz() {
        // The exact input that regressed ("unrecognised time") must now parse.
        let v = json!("2026-06-19T15:00:00");
        // No tz → interpreted as UTC.
        assert_eq!(parse_time(&v, None).unwrap(), ms("2026-06-19T15:00:00Z"));
        // Melbourne is UTC+10 in June (AEST, no DST) → 15:00 local == 05:00 UTC.
        assert_eq!(parse_time(&v, Some("Australia/Melbourne")).unwrap(), ms("2026-06-19T05:00:00Z"));
        // An offset-aware RFC 3339 stays an exact instant regardless of tz.
        assert_eq!(
            parse_time(&json!("2026-06-19T15:00:00Z"), Some("Australia/Melbourne")).unwrap(),
            ms("2026-06-19T15:00:00Z"),
        );
        // Space-separated + minute precision also parse; date-only still works.
        assert!(parse_time(&json!("2026-06-19 15:00"), Some("Australia/Melbourne")).is_ok());
        assert!(parse_time(&json!("2026-06-19"), None).is_ok());
        assert!(parse_time(&json!(1_750_000_000_000_i64), None).is_ok());
        // Genuinely garbage still errors.
        assert!(parse_time(&json!("next tuesday"), None).is_err());
    }

    #[tokio::test]
    async fn create_list_update_delete_flow() {
        let (store, _dir) = setup();

        let created = CalendarCreateEventTool::new(Arc::clone(&store))
            .execute(json!({
                "_user_id": "u1",
                "summary":  "Lunch",
                "starts_at": "2026-04-24T12:00:00Z",
                "ends_at":   "2026-04-24T13:00:00Z",
            }))
            .await.unwrap();
        assert!(created.success, "{:?}", created.error);
        let ev: Value = serde_json::from_str(&created.output).unwrap();
        let id = ev["id"].as_str().unwrap().to_string();

        let listed = CalendarListEventsTool::new(Arc::clone(&store))
            .execute(json!({
                "_user_id": "u1",
                "from": "2026-04-23T00:00:00Z",
                "to":   "2026-04-25T00:00:00Z",
            }))
            .await.unwrap();
        assert!(listed.success);
        let body: Value = serde_json::from_str(&listed.output).unwrap();
        assert_eq!(body["count"], 1);

        let updated = CalendarUpdateEventTool::new(Arc::clone(&store))
            .execute(json!({
                "_user_id": "u1",
                "id": id,
                "summary": "Lunch (moved)",
                "starts_at": "2026-04-24T13:00:00Z",
            }))
            .await.unwrap();
        assert!(updated.success, "{:?}", updated.error);

        let deleted = CalendarDeleteEventTool::new(Arc::clone(&store))
            .execute(json!({ "_user_id": "u1", "id": id }))
            .await.unwrap();
        assert!(deleted.success);
        let body: Value = serde_json::from_str(&deleted.output).unwrap();
        assert_eq!(body["deleted"], true);
    }

    #[tokio::test]
    async fn create_event_dedups_identical_calls() {
        let (store, _dir) = setup();
        let tool = CalendarCreateEventTool::new(Arc::clone(&store));
        let args = json!({
            "_user_id": "u1",
            "summary":  "Tarek's Birthday",
            "starts_at": "2026-02-09",
            "all_day":  true,
        });
        // First create succeeds.
        let first = tool.execute(args.clone()).await.unwrap();
        assert!(first.success);
        // A second identical call is deduped — reported success, but no insert.
        let second = tool.execute(args.clone()).await.unwrap();
        assert!(second.success);
        assert!(second.output.contains("already exists"), "got: {}", second.output);
        // A third for good measure (the model retried up to 4× in the wild).
        let _ = tool.execute(args).await.unwrap();

        // Exactly one event in the store.
        let listed = CalendarListEventsTool::new(Arc::clone(&store))
            .execute(json!({ "_user_id": "u1", "from": "2026-02-01", "to": "2026-02-28" }))
            .await.unwrap();
        let body: Value = serde_json::from_str(&listed.output).unwrap();
        assert_eq!(body["count"], 1, "dedup should keep exactly one event");

        // A different start time is NOT a duplicate (legitimately distinct).
        let other = tool.execute(json!({
            "_user_id": "u1", "summary": "Tarek's Birthday", "starts_at": "2026-02-08", "all_day": true,
        })).await.unwrap();
        assert!(other.success);
        assert!(!other.output.contains("already exists"), "different date must not dedup");
    }

    #[tokio::test]
    async fn rejects_missing_user_id() {
        let (store, _dir) = setup();
        let out = CalendarListEventsTool::new(store)
            .execute(json!({})).await;
        match out {
            Err(MiraError::ToolError(msg)) => assert!(msg.contains("caller identity")),
            other => panic!("expected ToolError, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn rejects_bad_time_format() {
        let (store, _dir) = setup();
        let out = CalendarCreateEventTool::new(store)
            .execute(json!({
                "_user_id": "u1",
                "summary":  "x",
                "starts_at": "tomorrow",
            }))
            .await.unwrap();
        assert!(!out.success);
    }
}
