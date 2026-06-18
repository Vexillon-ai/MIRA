// SPDX-License-Identifier: AGPL-3.0-or-later

// src/security/ip_bans.rs
//! Tower middleware that short-circuits requests from temp-banned IPs.
//!
//! Bans are written by the health-audit auto-action (when an IP trips
//! the failed-login spike threshold) and stored in `auth.db.auth_ip_bans`.
//! This layer caches the active set in memory and refreshes it every
//! `refresh_secs` from the DB so unbans take effect within a bounded
//! window without forcing a per-request DB read.
//!
//! Sits between RateLimitLayer and AuthLayer in the router stack — bans
//! are evaluated AFTER the cheap rate-limit gate (which catches most
//! brute-force traffic) but BEFORE auth (so banned IPs don't waste CPU
//! on JWT validation).

use std::collections::HashSet;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use tower::{Layer, Service};
use tracing::warn;

use crate::auth::AuthDb;

/// In-memory cache of currently-banned IPs. Refreshed on a TTL from
/// the DB. Behind an `RwLock` because reads vastly outnumber writes
/// (one swap-in per refresh interval vs. one read per HTTP request).
#[derive(Clone)]
pub struct IpBanCache {
    inner: Arc<RwLock<Snapshot>>,
    db:    Arc<AuthDb>,
    /// How long a cached snapshot is considered fresh. Lower = bans
    /// take effect faster; higher = fewer DB reads. 30s is a reasonable
    /// middle for the health-audit cadence (auto-actions fire hourly,
    /// so a 30s lag is invisible end-to-end).
    refresh_secs: u64,
}

#[derive(Default)]
struct Snapshot {
    banned: HashSet<IpAddr>,
    fetched_at: Option<Instant>,
}

impl IpBanCache {
    pub fn new(db: Arc<AuthDb>) -> Self {
        Self::with_refresh(db, 30)
    }

    pub fn with_refresh(db: Arc<AuthDb>, refresh_secs: u64) -> Self {
        Self { inner: Arc::new(RwLock::new(Snapshot::default())), db, refresh_secs }
    }

    /// Returns true if `ip` is currently banned. Refreshes the cache
    /// from DB if stale (or never fetched).
    pub fn is_banned(&self, ip: IpAddr) -> bool {
        // Stale? Refresh under a write lock.
        let needs_refresh = {
            let g = self.inner.read().expect("ip-ban cache");
            g.fetched_at
                .map(|t| t.elapsed().as_secs() >= self.refresh_secs)
                .unwrap_or(true)
        };
        if needs_refresh {
            // Distinguish "DB error" from "DB returned empty" — the
            // first should leave the cache alone (transient failure),
            // the second should clear it (legitimate unban).
            match self.db.list_active_bans() {
                Ok(fresh) => {
                    let parsed: HashSet<IpAddr> = fresh.iter()
                        .filter_map(|(s, _, _)| s.parse().ok())
                        .collect();
                    let mut g = self.inner.write().expect("ip-ban cache");
                    g.banned = parsed;
                    g.fetched_at = Some(Instant::now());
                }
                Err(e) => {
                    warn!("ip-ban cache refresh failed (keeping stale set): {e}");
                    // Touch fetched_at so we don't hammer the DB on
                    // every request while it's down. Next refresh
                    // attempt will be after refresh_secs elapse.
                    let mut g = self.inner.write().expect("ip-ban cache");
                    g.fetched_at = Some(Instant::now());
                }
            }
        }
        let g = self.inner.read().expect("ip-ban cache");
        g.banned.contains(&ip)
    }
}

#[derive(Clone)]
pub struct IpBanLayer {
    cache: IpBanCache,
}

impl IpBanLayer {
    pub fn new(cache: IpBanCache) -> Self { Self { cache } }
}

impl<S> Layer<S> for IpBanLayer {
    type Service = IpBanService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        IpBanService { inner, cache: self.cache.clone() }
    }
}

#[derive(Clone)]
pub struct IpBanService<S> {
    inner: S,
    cache: IpBanCache,
}

impl<S> Service<Request<Body>> for IpBanService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error:  Send + 'static,
{
    type Response = Response<Body>;
    type Error    = S::Error;
    type Future   = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let ip = extract_ip(&req);
        if self.cache.is_banned(ip) {
            warn!("ip-ban: rejecting request from banned IP {ip}");
            return Box::pin(std::future::ready(Ok(
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(Body::from("IP temporarily banned"))
                    .unwrap(),
            )));
        }
        let fut = self.inner.call(req);
        Box::pin(async move { fut.await })
    }
}

/// Same extraction logic as RateLimitLayer — keep them in sync if
/// either changes. Inlined rather than re-exported because it's trivial
/// and a re-export would couple the two layers' visibility.
fn extract_ip(req: &Request<Body>) -> IpAddr {
    if let Some(xff) = req.headers().get("x-forwarded-for") {
        if let Ok(v) = xff.to_str() {
            if let Ok(ip) = v.split(',').next().unwrap_or("").trim().parse() {
                return ip;
            }
        }
    }
    if let Some(addr) = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>() {
        return addr.0.ip();
    }
    "127.0.0.1".parse().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_db() -> Arc<AuthDb> {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the file persists for the duration of
        // the test — avoids RAII cleanup mid-call.
        let path = dir.keep().join("auth.db");
        Arc::new(AuthDb::open(&path).unwrap())
    }

    #[test]
    fn cache_returns_false_when_no_bans() {
        let db = open_test_db();
        let cache = IpBanCache::new(db);
        assert!(!cache.is_banned("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn cache_picks_up_ban_after_refresh() {
        let db = open_test_db();
        // 0-second refresh so every is_banned call re-reads DB.
        let cache = IpBanCache::with_refresh(db.clone(), 0);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(!cache.is_banned(ip));
        db.ban_ip(&ip.to_string(), 60, "test").unwrap();
        assert!(cache.is_banned(ip), "ban should be visible after refresh");
    }

    #[test]
    fn unban_takes_effect() {
        let db = open_test_db();
        let cache = IpBanCache::with_refresh(db.clone(), 0);
        let ip = "10.0.0.2";
        db.ban_ip(ip, 60, "x").unwrap();
        assert!(cache.is_banned(ip.parse().unwrap()));
        db.unban_ip(ip).unwrap();
        assert!(!cache.is_banned(ip.parse().unwrap()));
    }
}
