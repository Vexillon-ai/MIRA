// SPDX-License-Identifier: AGPL-3.0-or-later

//! App health-check poller (apps framework).
//!
//! An app can declare a `health_check` in its spec (see [`super::apps::AppHealthCheck`]):
//! a URL to poll (templated `${config.*}`) plus a declared *issue* event to emit
//! when the service can't be reached. This single background task polls every
//! such installed app on its interval and, on a **transition to unhealthy**,
//! emits the app's declared event onto the shared bus — where the Guardian's
//! app-issue triage ([`crate::agent::guardian_app_events`]) picks it up. This is
//! how an app becomes a Guardian *detection source* without shipping its own
//! backend.
//!
//! Edge-triggered: it emits once when a service goes down (or is down the first
//! time it's seen), logs recovery, and does not re-emit while it stays down —
//! consistent with the Guardian's own dedup. Every poll goes through the shared
//! SSRF-guarded [`HttpPolicy`], so a health check can never be pointed at an
//! internal address either.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::events::{Event, EventBus};
use crate::skills::secrets::SecretsStore;
use crate::tools::http_policy::{HttpPolicy, RequestContext};

use super::apps;
use super::store::PackageStore;

/// How often the poller wakes to check which apps are due. Individual apps are
/// checked no more often than their own `interval_secs` (floored at this tick).
const BASE_TICK_SECS: u64 = 30;
/// Floor on a health check's interval, so a misdeclared app can't hammer a service.
const MIN_INTERVAL_SECS: i64 = 30;

/// Spawn the app health-check poller. `http`/`secrets` are the shared handles
/// (from the gateway builder); `data_dir` locates the package store. Returns a
/// no-op task when `http` is absent (nothing can be polled).
pub fn spawn_app_health_pollers(
    bus:      Arc<EventBus>,
    http:     Option<Arc<HttpPolicy>>,
    secrets:  Option<Arc<SecretsStore>>,
    data_dir: PathBuf,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(http) = http else {
            debug!("apps health poller: no HTTP policy — disabled");
            return;
        };
        let auth_db = data_dir.join("auth.db");
        // app_id → last-poll unix ts, and app_id → last-known health.
        let mut last_check: HashMap<String, i64> = HashMap::new();
        let mut healthy:    HashMap<String, bool> = HashMap::new();

        let mut tick = tokio::time::interval(Duration::from_secs(BASE_TICK_SECS));
        loop {
            tick.tick().await;

            let Ok(store) = PackageStore::open(&auth_db) else { continue };
            let pkgs = store.list().unwrap_or_default();
            let mut seen: HashSet<String> = HashSet::new();

            for pkg in &pkgs {
                let Some(target) = apps::app_health_target(pkg) else { continue };
                seen.insert(pkg.id.clone());

                let now = chrono::Utc::now().timestamp();
                let interval = (target.check.interval_secs as i64).max(MIN_INTERVAL_SECS);
                if last_check.get(&pkg.id).is_some_and(|&t| now - t < interval) {
                    continue; // not due yet
                }

                // Resolve the check URL + headers from config. If the app isn't
                // configured yet (missing config), skip quietly — that's not an
                // outage.
                let config = pkg.config.as_object().cloned().unwrap_or_default();
                let url = match apps::render_config(&target.check.url, &config, secrets.as_ref(), &pkg.id) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                let mut headers: Vec<(String, String)> = Vec::new();
                let mut header_ok = true;
                for (k, v) in &target.check.headers {
                    match apps::render_config(v, &config, secrets.as_ref(), &pkg.id) {
                        Ok(rv) => headers.push((k.clone(), rv)),
                        Err(_) => { header_ok = false; break; }
                    }
                }
                if !header_ok { continue; }

                last_check.insert(pkg.id.clone(), now);

                let hdr_refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                // LAN egress: relax the private-network SSRF block only for the
                // app's declared, config-resolved egress hosts (e.g. a LAN HA box).
                let allow = apps::app_egress_hosts(&target.egress, &config, secrets.as_ref(), &pkg.id);
                let ctx = RequestContext::user_only(String::new()).with_private_hosts(allow);
                let is_healthy = match http
                    .request_with_context(reqwest::Method::GET, &url, &hdr_refs, None, &ctx).await
                {
                    Ok(resp) => (200..400).contains(&resp.status),
                    Err(_)   => false,
                };

                let prev = healthy.get(&pkg.id).copied();
                if !is_healthy && prev != Some(false) {
                    // Transition to (or first-seen) unhealthy → emit the declared
                    // issue event. The Guardian's app-issue triage takes it from here.
                    info!(
                        "apps: health check FAILED for '{}' → emitting {} (severity {})",
                        pkg.id, target.event_name, target.event_severity,
                    );
                    bus.emit(Event::new_app(
                        target.event_name.clone(),
                        None,
                        target.event_domain.clone(),
                        target.event_severity.clone(),
                        pkg.id.clone(),
                        serde_json::json!({ "check_url": url, "reason": "health check did not return a 2xx/3xx" }),
                    ));
                } else if is_healthy && prev == Some(false) {
                    info!("apps: health check RECOVERED for '{}'", pkg.id);
                }
                healthy.insert(pkg.id.clone(), is_healthy);
            }

            // Drop state for apps that are gone (uninstalled/disabled).
            last_check.retain(|id, _| seen.contains(id));
            healthy.retain(|id, _| seen.contains(id));
            if !seen.is_empty() {
                debug!("apps health poller: {} app(s) with a health_check", seen.len());
            }
        }
    })
}
