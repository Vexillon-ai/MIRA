// SPDX-License-Identifier: AGPL-3.0-or-later

// src/calendar/mod.rs
//! Calendar subsystem.
//!
//! MIRA-native event storage is always available. External providers
//! (CalDAV, Google, Microsoft) are opt-in and mirror their events into the
//! native store on a background sync loop. Write-back to external providers
//! is out of scope for this iteration — UI / agent tools write to the
//! native store only.

pub mod models;
pub mod store;
pub mod sync;
pub mod caldav;
pub mod google;
pub mod outlook;
pub mod ical;

pub use models::{CalendarEvent, EventInput, EventKind, EventSource};
pub use store::{CalendarStore, OAuthTokens};
pub use sync::{CalendarSync, SyncEngine};
