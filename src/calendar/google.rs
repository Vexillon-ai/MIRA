// SPDX-License-Identifier: AGPL-3.0-or-later

// src/calendar/google.rs
//! Google Calendar OAuth pull-sync adapter.
//!
//! Uses stored OAuth tokens (written by the /api/calendar/oauth/callback
//! handler) to fetch the user's primary calendar via the v3 REST API. Tokens
//! are refreshed automatically when they're about to expire.
//!
//! Scopes required: `https://www.googleapis.com/auth/calendar.readonly`.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tracing::{debug, warn};

use crate::MiraError;

use super::models::{EventInput, EventSource};
use super::store::{CalendarStore, OAuthTokens};
use super::sync::CalendarSync;

const GOOGLE_API:   &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events";
const GOOGLE_TOKEN: &str = "https://oauth2.googleapis.com/token";
pub const GOOGLE_AUTH:  &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_SCOPES: &str = "https://www.googleapis.com/auth/calendar.readonly";

pub struct GoogleSync {
    store:         Arc<CalendarStore>,
    client_id:     String,
    client_secret: String,
    http:          Client,
}

impl GoogleSync {
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

    /// Refresh the stored access token if it's within 60s of expiry.
    async fn ensure_fresh(&self, tokens: &mut OAuthTokens) -> Result<(), MiraError> {
        let now = Utc::now().timestamp_millis();
        let soon = now + 60_000;
        let needs_refresh = tokens.expires_at.map(|e| e <= soon).unwrap_or(false);
        if !needs_refresh { return Ok(()); }

        let Some(ref refresh) = tokens.refresh_token else {
            return Err(MiraError::ServerError(
                "google: access token expired and no refresh_token on file".into(),
            ));
        };

        let resp = self.http.post(GOOGLE_TOKEN)
            .form(&[
                ("client_id",     self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh.as_str()),
                ("grant_type",    "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| MiraError::ServerError(format!("google refresh: {}", e)))?;

        if !resp.status().is_success() {
            return Err(MiraError::ServerError(format!(
                "google refresh returned {}", resp.status(),
            )));
        }
        let body: TokenRefreshResp = resp.json().await
            .map_err(|e| MiraError::ServerError(format!("google refresh body: {}", e)))?;

        tokens.access_token = body.access_token;
        if let Some(secs) = body.expires_in {
            tokens.expires_at = Some(now + (secs as i64) * 1000);
        }
        self.store.save_tokens(tokens)?;
        Ok(())
    }
}

#[async_trait]
impl CalendarSync for GoogleSync {
    fn source(&self) -> EventSource { EventSource::Google }

    async fn sync_user(
        &self,
        user_id: &str,
        store:   &CalendarStore,
    ) -> Result<usize, MiraError> {
        if !self.configured() {
            debug!("google: skipping sync — client_id/secret not set");
            return Ok(0);
        }
        let Some(mut tokens) = self.store.get_tokens(user_id, "google")? else {
            debug!("google: no tokens on file for {}", user_id);
            return Ok(0);
        };
        self.ensure_fresh(&mut tokens).await?;

        let time_min = (Utc::now() - Duration::days(180)).to_rfc3339();
        let time_max = (Utc::now() + Duration::days(180)).to_rfc3339();

        let resp = self.http.get(GOOGLE_API)
            .bearer_auth(&tokens.access_token)
            .query(&[
                ("singleEvents", "true"),
                ("maxResults",   "500"),
                ("timeMin",      &time_min),
                ("timeMax",      &time_max),
                ("orderBy",      "startTime"),
            ])
            .send()
            .await
            .map_err(|e| MiraError::ServerError(format!("google events: {}", e)))?;

        if !resp.status().is_success() {
            return Err(MiraError::ServerError(format!(
                "google events returned {}", resp.status(),
            )));
        }

        let payload: ListResp = resp.json().await
            .map_err(|e| MiraError::ServerError(format!("google events body: {}", e)))?;

        let mut keep = Vec::new();
        let mut written = 0;
        for raw in payload.items {
            let Some((uid, input)) = raw.into_event_input() else { continue };
            match store.upsert_external(user_id, EventSource::Google, &uid, &input) {
                Ok(()) => {
                    keep.push(uid);
                    written += 1;
                }
                Err(e) => warn!("google upsert failed: {}", e),
            }
        }

        if let Err(e) = store.prune_external(user_id, EventSource::Google, &keep) {
            warn!("google prune failed: {}", e);
        }
        Ok(written)
    }
}

// ── Wire formats ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenRefreshResp {
    access_token: String,
    #[serde(default)]
    expires_in:   Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ListResp {
    #[serde(default)]
    items: Vec<RawEvent>,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    id:          Option<String>,
    status:      Option<String>,
    summary:     Option<String>,
    description: Option<String>,
    location:    Option<String>,
    start:       Option<RawWhen>,
    end:         Option<RawWhen>,
    #[serde(default)]
    recurrence:  Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawWhen {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    date:      Option<String>,
}

impl RawEvent {
    fn into_event_input(self) -> Option<(String, EventInput)> {
        let id    = self.id?;
        let start = self.start.as_ref()?;
        let end   = self.end.as_ref();

        let (start_ms, all_day) = parse_when(start)?;
        let end_ms = end.and_then(parse_when).map(|(ms, _)| ms)
            .unwrap_or(start_ms + 3_600_000);

        let rrule = self.recurrence.into_iter().find(|s| s.starts_with("RRULE:"))
            .map(|s| s[6..].to_string());

        Some((
            id,
            EventInput {
                summary:     self.summary.unwrap_or_else(|| "(no title)".into()),
                description: self.description,
                starts_at:   start_ms,
                ends_at:     end_ms,
                all_day,
                location:    self.location,
                rrule,
                status:      self.status,
                kind:        crate::calendar::models::EventKind::Event,
                shared:      false,
                group_id:    None,
            },
        ))
    }
}

fn parse_when(w: &RawWhen) -> Option<(i64, bool)> {
    if let Some(ref dt) = w.date_time {
        let parsed = DateTime::parse_from_rfc3339(dt).ok()?;
        return Some((parsed.with_timezone(&Utc).timestamp_millis(), false));
    }
    if let Some(ref d) = w.date {
        let date = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?;
        let dt = date.and_hms_opt(0, 0, 0)?;
        return Some((Utc.from_utc_datetime(&dt).timestamp_millis(), true));
    }
    None
}

use chrono::TimeZone;

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_event_datetime_roundtrip() {
        let raw = RawEvent {
            id: Some("g-1".into()),
            status: Some("confirmed".into()),
            summary: Some("Hello".into()),
            description: None,
            location: None,
            start: Some(RawWhen {
                date_time: Some("2026-04-24T12:00:00+00:00".into()),
                date: None,
            }),
            end: Some(RawWhen {
                date_time: Some("2026-04-24T13:00:00+00:00".into()),
                date: None,
            }),
            recurrence: vec![],
        };
        let (uid, input) = raw.into_event_input().unwrap();
        assert_eq!(uid, "g-1");
        assert_eq!(input.summary, "Hello");
        assert!(!input.all_day);
        assert_eq!(input.ends_at - input.starts_at, 3_600_000);
    }

    #[test]
    fn raw_event_all_day_sets_flag() {
        let raw = RawEvent {
            id: Some("g-2".into()),
            status: None, summary: Some("bday".into()),
            description: None, location: None,
            start: Some(RawWhen { date_time: None, date: Some("2026-04-25".into()) }),
            end:   Some(RawWhen { date_time: None, date: Some("2026-04-26".into()) }),
            recurrence: vec![],
        };
        let (_, input) = raw.into_event_input().unwrap();
        assert!(input.all_day);
    }
}
