// SPDX-License-Identifier: AGPL-3.0-or-later

// src/calendar/outlook.rs
//! Microsoft Graph (Outlook / Office 365) OAuth pull-sync adapter.
//!
//! Mirrors events from the user's default Outlook calendar via the Graph v1
//! `/me/events` endpoint. Tokens live in the calendar DB alongside Google's.
//!
//! Scopes required: `Calendars.Read offline_access`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tracing::{debug, warn};

use crate::MiraError;

use super::models::{EventInput, EventSource};
use super::store::{CalendarStore, OAuthTokens};
use super::sync::CalendarSync;

const MS_API:   &str = "https://graph.microsoft.com/v1.0/me/events?$top=500&$orderby=start/dateTime";
const MS_TOKEN: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
pub const MS_AUTH:   &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
pub const MS_SCOPES: &str = "offline_access Calendars.Read";

pub struct OutlookSync {
    store:         Arc<CalendarStore>,
    client_id:     String,
    client_secret: String,
    http:          Client,
}

impl OutlookSync {
    pub fn new(store: Arc<CalendarStore>, client_id: String, client_secret: String) -> Self {
        let http = Client::builder()
            .timeout(StdDuration::from_secs(30))
            .user_agent(format!("MIRA/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self { store, client_id, client_secret, http }
    }

    fn configured(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }

    async fn ensure_fresh(&self, tokens: &mut OAuthTokens) -> Result<(), MiraError> {
        let now = Utc::now().timestamp_millis();
        let soon = now + 60_000;
        let needs_refresh = tokens.expires_at.map(|e| e <= soon).unwrap_or(false);
        if !needs_refresh { return Ok(()); }

        let Some(ref refresh) = tokens.refresh_token else {
            return Err(MiraError::ServerError(
                "outlook: access token expired and no refresh_token on file".into(),
            ));
        };

        let resp = self.http.post(MS_TOKEN)
            .form(&[
                ("client_id",     self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh.as_str()),
                ("grant_type",    "refresh_token"),
                ("scope",         MS_SCOPES),
            ])
            .send()
            .await
            .map_err(|e| MiraError::ServerError(format!("outlook refresh: {}", e)))?;

        if !resp.status().is_success() {
            return Err(MiraError::ServerError(format!(
                "outlook refresh returned {}", resp.status(),
            )));
        }
        let body: TokenRefreshResp = resp.json().await
            .map_err(|e| MiraError::ServerError(format!("outlook refresh body: {}", e)))?;

        tokens.access_token = body.access_token;
        if let Some(ref r) = body.refresh_token {
            tokens.refresh_token = Some(r.clone());
        }
        if let Some(secs) = body.expires_in {
            tokens.expires_at = Some(now + (secs as i64) * 1000);
        }
        self.store.save_tokens(tokens)?;
        Ok(())
    }
}

#[async_trait]
impl CalendarSync for OutlookSync {
    fn source(&self) -> EventSource { EventSource::Outlook }

    async fn sync_user(
        &self,
        user_id: &str,
        store:   &CalendarStore,
    ) -> Result<usize, MiraError> {
        if !self.configured() {
            debug!("outlook: skipping sync — client_id/secret not set");
            return Ok(0);
        }
        let Some(mut tokens) = self.store.get_tokens(user_id, "outlook")? else {
            debug!("outlook: no tokens on file for {}", user_id);
            return Ok(0);
        };
        self.ensure_fresh(&mut tokens).await?;

        let resp = self.http.get(MS_API)
            .bearer_auth(&tokens.access_token)
            .header("Prefer", "outlook.timezone=\"UTC\"")
            .send()
            .await
            .map_err(|e| MiraError::ServerError(format!("outlook events: {}", e)))?;

        if !resp.status().is_success() {
            return Err(MiraError::ServerError(format!(
                "outlook events returned {}", resp.status(),
            )));
        }

        let payload: ListResp = resp.json().await
            .map_err(|e| MiraError::ServerError(format!("outlook events body: {}", e)))?;

        let mut keep = Vec::new();
        let mut written = 0;
        for raw in payload.value {
            let Some((uid, input)) = raw.into_event_input() else { continue };
            match store.upsert_external(user_id, EventSource::Outlook, &uid, &input) {
                Ok(()) => {
                    keep.push(uid);
                    written += 1;
                }
                Err(e) => warn!("outlook upsert failed: {}", e),
            }
        }

        if let Err(e) = store.prune_external(user_id, EventSource::Outlook, &keep) {
            warn!("outlook prune failed: {}", e);
        }
        Ok(written)
    }
}

// ── Wire formats ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenRefreshResp {
    access_token: String,
    #[serde(default)] refresh_token: Option<String>,
    #[serde(default)] expires_in:    Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ListResp {
    #[serde(default)]
    value: Vec<RawEvent>,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    id:        Option<String>,
    subject:   Option<String>,
    #[serde(rename = "isAllDay", default)]
    is_all_day: bool,
    body:      Option<RawBody>,
    location:  Option<RawLocation>,
    start:     Option<RawWhen>,
    end:       Option<RawWhen>,
    #[serde(rename = "showAs")]
    show_as:   Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawBody {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawLocation {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawWhen {
    #[serde(rename = "dateTime")]
    date_time: String,
    #[serde(rename = "timeZone", default)]
    _tz:       Option<String>,
}

impl RawEvent {
    fn into_event_input(self) -> Option<(String, EventInput)> {
        let id = self.id?;
        let start = parse_when(self.start.as_ref()?)?;
        let end   = self.end.as_ref().and_then(parse_when).unwrap_or(start + 3_600_000);

        Some((
            id,
            EventInput {
                summary:     self.subject.unwrap_or_else(|| "(no title)".into()),
                description: self.body.and_then(|b| b.content),
                starts_at:   start,
                ends_at:     end,
                all_day:     self.is_all_day,
                location:    self.location.and_then(|l| l.display_name),
                rrule:       None,
                status:      self.show_as,
                kind:        crate::calendar::models::EventKind::Event,
                shared:      false,
                group_id:    None,
            },
        ))
    }
}

fn parse_when(w: &RawWhen) -> Option<i64> {
    // Graph returns dateTime in the timezone requested by the Prefer header;
    // we asked for UTC, so treat the string as UTC regardless of TZID echo.
    let s = w.date_time.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).timestamp_millis());
    }
    // Fallback for Graph's non-offset form "YYYY-MM-DDTHH:MM:SS.xxxxxxx".
    let trimmed = s.split('.').next()?;
    let naive = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S").ok()?;
    Some(Utc.from_utc_datetime(&naive).timestamp_millis())
}

use chrono::TimeZone;

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_event_basic_roundtrip() {
        let raw = RawEvent {
            id: Some("o-1".into()),
            subject: Some("Sync 1:1".into()),
            is_all_day: false,
            body: Some(RawBody { content: Some("agenda".into()) }),
            location: Some(RawLocation { display_name: Some("Zoom".into()) }),
            start: Some(RawWhen { date_time: "2026-04-24T12:00:00.0000000".into(), _tz: Some("UTC".into()) }),
            end:   Some(RawWhen { date_time: "2026-04-24T13:00:00.0000000".into(), _tz: Some("UTC".into()) }),
            show_as: Some("busy".into()),
        };
        let (uid, input) = raw.into_event_input().unwrap();
        assert_eq!(uid, "o-1");
        assert_eq!(input.summary, "Sync 1:1");
        assert_eq!(input.location.as_deref(), Some("Zoom"));
        assert_eq!(input.ends_at - input.starts_at, 3_600_000);
    }
}
