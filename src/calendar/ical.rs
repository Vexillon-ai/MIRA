// SPDX-License-Identifier: AGPL-3.0-or-later

// src/calendar/ical.rs
//! Minimal iCalendar (RFC 5545) reader.
//!
//! Only the subset we need to mirror CalDAV events into MIRA: SUMMARY,
//! DESCRIPTION, DTSTART, DTEND, LOCATION, UID, RRULE, STATUS. Full RFC
//! compliance is an explicit non-goal — we tolerate common servers
//! (Nextcloud, Fastmail, iCloud, Radicale) and fall back gracefully on
//! fields we don't recognise.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};

use super::models::EventInput;

#[derive(Debug, Default, Clone)]
pub struct ParsedEvent {
    pub uid:         Option<String>,
    pub summary:     Option<String>,
    pub description: Option<String>,
    pub location:    Option<String>,
    pub rrule:       Option<String>,
    pub status:      Option<String>,
    pub dtstart_ms:  Option<i64>,
    pub dtend_ms:    Option<i64>,
    pub all_day:     bool,
}

impl ParsedEvent {
    pub fn into_input(self) -> Option<(String, EventInput)> {
        let uid   = self.uid?;
        let start = self.dtstart_ms?;
        let end   = self.dtend_ms.unwrap_or(start + 3_600_000);
        Some((
            uid,
            EventInput {
                summary:     self.summary.unwrap_or_else(|| "(no title)".into()),
                description: self.description,
                starts_at:   start,
                ends_at:     end,
                all_day:     self.all_day,
                location:    self.location,
                rrule:       self.rrule,
                status:      self.status,
                kind:        crate::calendar::models::EventKind::Event,
                shared:      false,
                group_id:    None,
            },
        ))
    }
}

/// Unfold long iCal lines. Per RFC 5545 §3.1, continuation lines start with
/// a space or tab and must be joined to the previous line with the
/// leading whitespace stripped.
fn unfold(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            let last = out.last_mut().unwrap();
            last.push_str(&line[1..]);
        } else {
            out.push(line.trim_end_matches('\r').to_string());
        }
    }
    out
}

/// Unescape iCal text fields (RFC 5545 §3.3.11): `\\`, `\,`, `\;`, `\N`, `\n`.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some(',')  => out.push(','),
                Some(';')  => out.push(';'),
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => { out.push('\\'); out.push(other); }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split a property line into `(name, params, value)`. The name may carry
/// semicolon-separated parameters before the first `:`. Params are returned
/// as a flat `name=value` vec so callers can scan for `VALUE=DATE`, `TZID=...`.
fn split_property(line: &str) -> Option<(String, Vec<(String, String)>, String)> {
    let colon = line.find(':')?;
    let (head, rest) = line.split_at(colon);
    let value = rest[1..].to_string();

    let mut parts = head.split(';');
    let name = parts.next()?.to_ascii_uppercase();
    let mut params = Vec::new();
    for p in parts {
        if let Some(eq) = p.find('=') {
            let (k, v) = p.split_at(eq);
            params.push((k.to_ascii_uppercase(), v[1..].to_string()));
        }
    }
    Some((name, params, value))
}

/// Parse an iCal `DTSTART` / `DTEND` value.
///
/// Supports:
/// - Date-only forms (`VALUE=DATE` param) → returned as UTC midnight, flagged
///   as `all_day`.
/// - Floating local time without zone (`19960401T120000`) → treated as UTC.
/// - UTC-marked timestamps (`19960401T120000Z`).
/// - `TZID=...` parameter → falls back to UTC (zone databases are out of
///   scope; we log in DEBUG and keep the instant in the file).
fn parse_datetime(params: &[(String, String)], value: &str) -> Option<(i64, bool)> {
    let value_type = params.iter()
        .find(|(k, _)| k == "VALUE").map(|(_, v)| v.as_str());

    if value_type == Some("DATE") || value.len() == 8 {
        let d = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        let dt = d.and_hms_opt(0, 0, 0)?;
        let utc = Utc.from_utc_datetime(&dt);
        return Some((utc.timestamp_millis(), true));
    }

    let is_utc = value.ends_with('Z');
    let trimmed = value.trim_end_matches('Z');
    let naive = NaiveDateTime::parse_from_str(trimmed, "%Y%m%dT%H%M%S").ok()?;
    if is_utc {
        let utc: DateTime<Utc> = Utc.from_utc_datetime(&naive);
        Some((utc.timestamp_millis(), false))
    } else {
        let utc: DateTime<Utc> = Utc.from_utc_datetime(&naive);
        Some((utc.timestamp_millis(), false))
    }
}

/// Scan an iCal document and return every VEVENT we can assemble. Malformed
/// events are skipped silently so one bad entry doesn't poison a whole sync.
pub fn parse_events(raw: &str) -> Vec<ParsedEvent> {
    let lines = unfold(raw);
    let mut out = Vec::new();
    let mut current: Option<ParsedEvent> = None;

    for line in lines {
        let upper = line.trim().to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" {
            current = Some(ParsedEvent::default());
            continue;
        }
        if upper == "END:VEVENT" {
            if let Some(ev) = current.take() {
                if ev.uid.is_some() && ev.dtstart_ms.is_some() {
                    out.push(ev);
                }
            }
            continue;
        }
        let Some(ev) = current.as_mut() else { continue; };
        let Some((name, params, value)) = split_property(&line) else { continue; };

        match name.as_str() {
            "UID"         => ev.uid = Some(value),
            "SUMMARY"     => ev.summary = Some(unescape(&value)),
            "DESCRIPTION" => ev.description = Some(unescape(&value)),
            "LOCATION"    => ev.location = Some(unescape(&value)),
            "RRULE"       => ev.rrule = Some(value),
            "STATUS"      => ev.status = Some(value.to_ascii_lowercase()),
            "DTSTART" => {
                if let Some((ms, all_day)) = parse_datetime(&params, &value) {
                    ev.dtstart_ms = Some(ms);
                    ev.all_day    = all_day;
                }
            }
            "DTEND" => {
                if let Some((ms, _)) = parse_datetime(&params, &value) {
                    ev.dtend_ms = Some(ms);
                }
            }
            _ => {}
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:abc-123\r\n\
SUMMARY:Lunch with Dana\r\n\
DESCRIPTION:Talk about\\nQ2 goals\\, obviously\r\n\
LOCATION:Cafe Oto\r\n\
DTSTART:20260424T120000Z\r\n\
DTEND:20260424T130000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn parses_basic_vevent() {
        let evs = parse_events(SAMPLE);
        assert_eq!(evs.len(), 1);
        let e = &evs[0];
        assert_eq!(e.uid.as_deref(), Some("abc-123"));
        assert_eq!(e.summary.as_deref(), Some("Lunch with Dana"));
        assert_eq!(e.description.as_deref().unwrap(), "Talk about\nQ2 goals, obviously");
        assert_eq!(e.location.as_deref(), Some("Cafe Oto"));
        assert!(e.dtstart_ms.is_some());
        assert!(e.dtend_ms.is_some());
        assert!(!e.all_day);
    }

    #[test]
    fn all_day_event_flags_all_day() {
        let src = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:day-1\r\n\
SUMMARY:Bday\r\n\
DTSTART;VALUE=DATE:20260425\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let evs = parse_events(src);
        assert_eq!(evs.len(), 1);
        assert!(evs[0].all_day);
    }

    #[test]
    fn unfolds_continuation_lines() {
        let src = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:long-1\r\n\
SUMMARY:This is a very\r\n long summary\r\n\
DTSTART:20260424T120000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let evs = parse_events(src);
        assert_eq!(evs[0].summary.as_deref(), Some("This is a verylong summary"));
    }

    #[test]
    fn broken_event_does_not_poison_batch() {
        let src = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:missing uid and dtstart\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:good-1\r\n\
SUMMARY:Good\r\n\
DTSTART:20260424T120000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let evs = parse_events(src);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].uid.as_deref(), Some("good-1"));
    }
}
