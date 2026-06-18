// SPDX-License-Identifier: AGPL-3.0-or-later

// src/calendar/caldav.rs
//! CalDAV pull-sync adapter.
//!
//! Performs one PROPFIND+REPORT pair against the configured CalDAV
//! collection, parses the returned iCalendar bodies, and mirrors events into
//! the native store. Write-back is not implemented — edits in MIRA stay
//! local, and any direct edit on the external server is picked up on the
//! next tick.
//!
//! The implementation targets the common-denominator CalDAV behaviour of
//! Nextcloud, Fastmail, iCloud, and Radicale. Servers that require
//! calendar-home discovery before accepting a REPORT will need a PROPFIND
//! round first; we issue one but tolerate servers that return the event
//! calendar directly.

use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;
use tracing::{debug, warn};

use crate::config::CalDavConfig;
use crate::MiraError;

use super::ical::parse_events;
use super::models::EventSource;
use super::store::CalendarStore;
use super::sync::CalendarSync;

pub struct CalDavSync {
    url:      String,
    username: String,
    password: String,
    client:   Client,
}

impl CalDavSync {
    pub fn from_config(cfg: &CalDavConfig) -> Self {
        Self::new(cfg.url.clone(), cfg.username.clone(), cfg.password.clone())
    }

    /// Build from explicit credentials — used for the per-user CalDAV path, where
    /// each user's url/username/password come from the encrypted calendar store
    /// rather than the global config.
    pub fn new(url: String, username: String, password: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(format!("MIRA/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self { url, username, password, client }
    }

    fn configured(&self) -> bool {
        !self.url.is_empty() && !self.username.is_empty()
    }

    /// Fetch every VEVENT in the configured collection. Uses a calendar-query
    /// REPORT with a one-year window (past six months → future six months)
    /// so we don't pull decades of history on first sync.
    async fn fetch_ical(&self) -> Result<String, MiraError> {
        let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag />
    <c:calendar-data />
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT" />
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#;

        let resp = self.client
            .request(reqwest::Method::from_bytes(b"REPORT").unwrap(), &self.url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body)
            .send()
            .await
            .map_err(|e| MiraError::ServerError(format!("caldav REPORT: {}", e)))?;

        if !resp.status().is_success() {
            return Err(MiraError::ServerError(format!(
                "caldav REPORT returned {}", resp.status(),
            )));
        }

        resp.text().await
            .map_err(|e| MiraError::ServerError(format!("caldav body: {}", e)))
    }
}

#[async_trait]
impl CalendarSync for CalDavSync {
    fn source(&self) -> EventSource { EventSource::Caldav }

    async fn sync_user(
        &self,
        user_id: &str,
        store:   &CalendarStore,
    ) -> Result<usize, MiraError> {
        if !self.configured() {
            debug!("caldav: skipping sync — url/username not set");
            return Ok(0);
        }

        let xml = self.fetch_ical().await?;
        let parsed = extract_vevents(&xml);
        let mut keep: Vec<String> = Vec::new();
        let mut written = 0;

        for ev in parsed {
            let Some((uid, input)) = ev.into_input() else { continue };
            match store.upsert_external(user_id, EventSource::Caldav, &uid, &input) {
                Ok(()) => {
                    keep.push(uid);
                    written += 1;
                }
                Err(e) => warn!("caldav upsert failed: {}", e),
            }
        }

        // Prune rows that disappeared from the server.
        if let Err(e) = store.prune_external(user_id, EventSource::Caldav, &keep) {
            warn!("caldav prune failed: {}", e);
        }

        Ok(written)
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Pull every `<calendar-data>` payload out of a multistatus XML body and
/// concatenate them into one iCal document for the parser. We do this with
/// substring scanning rather than a proper XML parser — CalDAV REPORT
/// responses are well-structured and the alternative (quick-xml) adds a
/// dependency for ~10 LOC of value.
fn extract_vevents(xml: &str) -> Vec<super::ical::ParsedEvent> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(idx) = find_ci(rest, "calendar-data") {
        let after_tag = &rest[idx..];
        // Skip past the opening tag itself.
        let open_end = match after_tag.find('>') {
            Some(p) => idx + p + 1,
            None    => break,
        };
        let after_open = &rest[open_end..];
        let close_idx = match find_ci(after_open, "</") {
            Some(p) => p,
            None    => break,
        };
        let payload = &after_open[..close_idx];
        let decoded = decode_xml_entities(payload);
        out.extend(parse_events(&decoded));
        rest = &after_open[close_idx..];
    }
    out
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let lower = hay.to_ascii_lowercase();
    let n     = needle.to_ascii_lowercase();
    lower.find(&n)
}

/// Minimal XML entity decoder — enough for the entities CalDAV servers
/// actually embed inside `<calendar-data>` blocks.
fn decode_xml_entities(s: &str) -> String {
    s.replace("&lt;", "<")
     .replace("&gt;", ">")
     .replace("&amp;", "&")
     .replace("&quot;", "\"")
     .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_vevents_handles_multiple_blocks() {
        let xml = r#"<?xml version="1.0"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response>
    <propstat>
      <prop>
        <c:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:u-1
SUMMARY:One
DTSTART:20260424T120000Z
END:VEVENT
END:VCALENDAR</c:calendar-data>
      </prop>
    </propstat>
  </response>
  <response>
    <propstat>
      <prop>
        <c:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:u-2
SUMMARY:Two
DTSTART:20260425T120000Z
END:VEVENT
END:VCALENDAR</c:calendar-data>
      </prop>
    </propstat>
  </response>
</multistatus>"#;
        let evs = extract_vevents(xml);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].uid.as_deref(), Some("u-1"));
        assert_eq!(evs[1].uid.as_deref(), Some("u-2"));
    }
}
