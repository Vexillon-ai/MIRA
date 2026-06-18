// SPDX-License-Identifier: AGPL-3.0-or-later

// src/calendar/sync.rs
//! External calendar sync engine.
//!
//! One `SyncEngine` runs on startup when `[calendar].sync_provider != "none"`.
//! It polls the configured provider every `sync_interval_mins`, converts
//! events into MIRA's canonical `EventInput` shape, and upserts them into the
//! native store. External sources own a `(user, source, external_id)` slice
//! of the store — native events are never touched.

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::auth::LocalAuthService;
use crate::config::MiraConfig;
use crate::MiraError;

use super::caldav::CalDavSync;
use super::google::GoogleSync;
use super::outlook::OutlookSync;
use super::store::CalendarStore;
use super::models::EventSource;

/// One external calendar backend. Implementations pull the user's event list
/// from their respective API and hand back `(external_id, EventInput)` pairs.
#[async_trait]
pub trait CalendarSync: Send + Sync {
    fn source(&self) -> EventSource;

    /// Perform one full pull for the given user. Returning `Err` is non-fatal
    /// — the engine logs and tries again on the next tick.
    async fn sync_user(
        &self,
        user_id: &str,
        store:   &CalendarStore,
    ) -> Result<usize, MiraError>;
}

pub struct SyncEngine {
    handle: Option<JoinHandle<()>>,
}

impl SyncEngine {
    /// Start a periodic sync loop. Returns an engine whose handle is aborted
    /// on drop so shutdown stops the background task.
    pub fn start(
        config: Arc<MiraConfig>,
        store:  Arc<CalendarStore>,
        auth:   Arc<LocalAuthService>,
    ) -> Self {
        let provider = config.calendar.sync_provider.clone();
        if provider == "none" {
            info!("calendar sync disabled (sync_provider = none)");
            return Self { handle: None };
        }

        // Google/Outlook use ONE shared backend that reads each user's OAuth
        // tokens from the store internally. CalDAV is per-user: each user's
        // url/username/password live (encrypted) in the store, so there's no
        // shared backend — we build a CalDavSync per user inside the loop.
        let shared: Option<Arc<dyn CalendarSync>> = match provider.as_str() {
            "caldav"  => None,
            "google"  => Some(Arc::new(GoogleSync::new(
                Arc::clone(&store),
                config.calendar.google.client_id.clone(),
                config.calendar.google.client_secret.clone(),
            ))),
            "outlook" => Some(Arc::new(OutlookSync::new(
                Arc::clone(&store),
                config.calendar.outlook.client_id.clone(),
                config.calendar.outlook.client_secret.clone(),
            ))),
            other => {
                warn!("calendar: unknown sync_provider '{}' — syncing disabled", other);
                return Self { handle: None };
            }
        };

        let interval = Duration::from_secs(
            config.calendar.sync_interval_mins.max(5) * 60,
        );

        let h = tokio::spawn(async move {
            // Small initial stagger so the sync doesn't race startup logging.
            tokio::time::sleep(Duration::from_secs(10)).await;
            loop {
                match auth.list_users() {
                    Ok(users) => {
                        for u in users {
                            // Resolve a backend for this user: shared one for
                            // google/outlook, or a per-user CalDavSync built from
                            // their stored creds (skip users who haven't connected).
                            let (src, res) = match &shared {
                                Some(b) => (b.source().as_str().to_string(), b.sync_user(&u.id, &store).await),
                                None => match store.get_caldav(&u.id) {
                                    Ok(Some(c)) => {
                                        let b = CalDavSync::new(c.url, c.username, c.password);
                                        ("caldav".to_string(), b.sync_user(&u.id, &store).await)
                                    }
                                    Ok(None) => continue, // user hasn't connected CalDAV
                                    Err(e) => { warn!("calendar[caldav] creds for {}: {}", u.id, e); continue }
                                },
                            };
                            match res {
                                Ok(n)  => info!("calendar[{}] synced {} events for {}", src, n, u.id),
                                Err(e) => warn!("calendar[{}] sync failed for {}: {}", src, u.id, e),
                            }
                        }
                    }
                    Err(e) => warn!("calendar sync: cannot list users: {}", e),
                }
                tokio::time::sleep(interval).await;
            }
        });

        info!(
            "calendar sync engine started (provider={}, interval={}m)",
            provider, config.calendar.sync_interval_mins.max(5),
        );
        Self { handle: Some(h) }
    }
}

impl Drop for SyncEngine {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}
