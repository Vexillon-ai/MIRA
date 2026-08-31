// SPDX-License-Identifier: AGPL-3.0-or-later

//! Ephemeral guest sessions for Restricted Mode.
//!
//! A guest is a throwaway `Role::User` account with its own **isolated** memory,
//! wiki, and chat history, seeded from a baseline and **wiped** on a TTL (and by
//! a periodic global-reset backstop, and unconditionally on startup — guests
//! never survive a restart). This is the generic mechanism behind the anonymous
//! "try it" endpoint; the public configuration + seed content live in the demo
//! layer.
//!
//! **Fail-closed:** a guest is only ever minted when Restricted Mode is active
//! (a profile is set) AND `restricted_mode.guest.enabled` is true. A guest
//! session is never handed out on an unrestricted instance, even by
//! misconfiguration.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::Utc;
use tracing::{info, warn};

use crate::auth::local::LocalAuthService;
use crate::auth::models::User;
use crate::config::GuestConfig;
use crate::history::HistoryStore;
use crate::memory::MemorySystem;
use crate::wiki::WikiRegistry;

/// A freshly minted guest session, returned to the mint endpoint.
pub struct GuestSession {
    pub user:             User,
    pub access_token:     String,
    pub expires_at_ms:    i64,
    pub session_ttl_secs: u64,
}

/// Why a guest could not be minted.
#[derive(Debug)]
pub enum GuestMintError {
    /// Guest sessions are disabled, or — fail-closed — no restriction profile is
    /// active. Anonymous access is refused.
    Disabled,
    /// At the `max_active` cap; the caller should degrade gracefully.
    AtCapacity,
    /// An internal error (account creation, seeding, token mint).
    Internal(String),
}

struct GuestMeta {
    created_at_ms: i64,
}

/// Owns the lifecycle of every guest session: mint, TTL sweep, global reset, and
/// startup purge of orphans.
pub struct GuestManager {
    cfg:      GuestConfig,
    // Whether a Restriction profile is active — the fail-closed precondition for
    // minting. Captured at construction (config is startup-immutable).
    restricted_active: bool,
    auth:     Arc<LocalAuthService>,
    memory:   Arc<MemorySystem>,
    wiki:     Arc<WikiRegistry>,
    history:  Arc<HistoryStore>,
    data_dir: PathBuf,
    // Server identity surfaced in the mint response so a native app / browser
    // knows which instance it just connected to.
    server_name:     String,
    server_base_url: Option<String>,
    active:   Mutex<HashMap<String, GuestMeta>>,
}

impl GuestManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg:               GuestConfig,
        restricted_active: bool,
        auth:              Arc<LocalAuthService>,
        memory:            Arc<MemorySystem>,
        wiki:              Arc<WikiRegistry>,
        history:           Arc<HistoryStore>,
        data_dir:          PathBuf,
        server_name:       String,
        server_base_url:   Option<String>,
    ) -> Self {
        Self { cfg, restricted_active, auth, memory, wiki, history, data_dir,
               server_name, server_base_url, active: Mutex::new(HashMap::new()) }
    }

    /// Server identity for the mint response: (display name, canonical base URL).
    pub fn server_info(&self) -> (&str, Option<&str>) {
        (&self.server_name, self.server_base_url.as_deref())
    }

    /// THE fail-closed gate: guest minting is possible only when a restriction
    /// profile is active AND guest sessions are explicitly enabled. Both must
    /// hold — a misconfiguration (guest on, no profile) refuses.
    pub fn is_enabled(&self) -> bool {
        self.restricted_active && self.cfg.enabled
    }

    /// Number of not-yet-expired guests (expired ones are reaped by the sweeper).
    fn live_count(&self, now_ms: i64) -> usize {
        let ttl_ms = self.cfg.session_ttl_secs.max(1) as i64 * 1000;
        self.active.lock().unwrap().values()
            .filter(|m| now_ms - m.created_at_ms < ttl_ms)
            .count()
    }

    /// Mint a new guest session: create the account, seed its wiki, and issue a
    /// short-TTL token. Fail-closed at the top.
    pub async fn mint(&self) -> Result<GuestSession, GuestMintError> {
        if !self.is_enabled() {
            return Err(GuestMintError::Disabled);
        }
        let now_ms = Utc::now().timestamp_millis();
        if self.cfg.max_active > 0 && self.live_count(now_ms) >= self.cfg.max_active as usize {
            return Err(GuestMintError::AtCapacity);
        }

        let user = self.auth.create_guest_user()
            .map_err(|e| GuestMintError::Internal(format!("create guest: {e}")))?;

        // Seed the guest's wiki from the baseline BEFORE first use, so the pages
        // are present when the wiki is opened. Best-effort: a seeding failure
        // leaves an empty wiki rather than failing the whole mint.
        if let Some(dir) = self.cfg.seed_wiki_dir.as_deref() {
            let baseline = self.resolve_seed_dir(dir);
            if baseline.is_dir() {
                if let Err(e) = self.wiki.seed_user_from_dir(&user.id, &baseline) {
                    warn!("guest {}: wiki seed from {:?} failed: {e}", user.id, baseline);
                }
            } else {
                warn!("guest {}: seed_wiki_dir {:?} is not a directory — starting empty",
                    user.id, baseline);
            }
        }

        let ttl = self.cfg.session_ttl_secs.max(1);
        let token = self.auth.issue_access_token_for(&user, ttl as i64)
            .map_err(|e| {
                // Roll back the half-created guest so we don't leak an account.
                let _ = self.auth.delete_user(&user.id);
                GuestMintError::Internal(format!("issue token: {e}"))
            })?;

        self.active.lock().unwrap().insert(user.id.clone(), GuestMeta { created_at_ms: now_ms });
        info!("guest minted: {} (ttl {}s)", user.id, ttl);

        Ok(GuestSession {
            user,
            access_token: token,
            expires_at_ms: now_ms + ttl as i64 * 1000,
            session_ttl_secs: ttl,
        })
    }

    /// Resolve a configured seed dir: absolute as-is, else relative to the data
    /// dir.
    fn resolve_seed_dir(&self, dir: &str) -> PathBuf {
        let p = PathBuf::from(dir);
        if p.is_absolute() { p } else { self.data_dir.join(p) }
    }

    /// Tear a guest down completely: kill its tokens, delete the account, and
    /// wipe every store it touched (memory, wiki, chat history). Idempotent.
    pub async fn teardown(&self, user_id: &str) {
        self.active.lock().unwrap().remove(user_id);
        // Kill live tokens first (bumps token_version), then remove the account.
        if let Err(e) = self.auth.revoke_all_sessions(user_id) {
            warn!("guest teardown {user_id}: revoke sessions: {e}");
        }
        if let Err(e) = self.memory.purge_user(user_id).await {
            warn!("guest teardown {user_id}: memory purge: {e}");
        }
        if let Err(e) = self.wiki.purge_user(user_id) {
            warn!("guest teardown {user_id}: wiki purge: {e}");
        }
        if let Err(e) = self.history.purge_user(user_id) {
            warn!("guest teardown {user_id}: history purge: {e}");
        }
        if let Err(e) = self.auth.delete_user(user_id) {
            warn!("guest teardown {user_id}: delete account: {e}");
        }
    }

    /// Tear down every guest whose TTL has elapsed.
    pub async fn sweep_expired(&self) {
        let now_ms = Utc::now().timestamp_millis();
        let ttl_ms = self.cfg.session_ttl_secs.max(1) as i64 * 1000;
        let expired: Vec<String> = {
            let g = self.active.lock().unwrap();
            g.iter()
                .filter(|(_, m)| now_ms - m.created_at_ms >= ttl_ms)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in expired {
            self.teardown(&id).await;
        }
    }

    /// Tear down ALL active guests regardless of age (global-reset backstop).
    pub async fn reset_all(&self) {
        let all: Vec<String> = self.active.lock().unwrap().keys().cloned().collect();
        if !all.is_empty() {
            info!("guest global reset: tearing down {} session(s)", all.len());
        }
        for id in all {
            self.teardown(&id).await;
        }
    }

    /// On startup, purge EVERY guest account left in the DB — guests are
    /// ephemeral and never survive a restart, so any that exist are orphans from
    /// a previous run. Also clears their data. Runs regardless of whether guest
    /// mode is currently enabled (so disabling it still cleans up old guests).
    pub async fn startup_purge(&self) {
        let guests = match self.auth.list_guest_users() {
            Ok(g) => g,
            Err(e) => { warn!("guest startup purge: list failed: {e}"); return; }
        };
        if guests.is_empty() { return; }
        info!("guest startup purge: removing {} orphaned guest account(s)", guests.len());
        for u in guests {
            self.teardown(&u.id).await;
        }
    }

    /// Spawn the background maintenance loop: TTL sweeps and the optional global
    /// reset. Ticks at a fraction of the TTL (bounded to [10s, 300s]).
    pub fn spawn_maintenance(self: Arc<Self>) {
        let tick = (self.cfg.session_ttl_secs / 4).clamp(10, 300);
        let reset_every = self.cfg.global_reset_secs;
        tokio::spawn(async move {
            let mut elapsed_since_reset: u64 = 0;
            let mut interval = tokio::time::interval(Duration::from_secs(tick));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await; // consume the immediate first tick
            loop {
                interval.tick().await;
                self.sweep_expired().await;
                if reset_every > 0 {
                    elapsed_since_reset += tick;
                    if elapsed_since_reset >= reset_every {
                        self.reset_all().await;
                        elapsed_since_reset = 0;
                    }
                }
            }
        });
    }
}

// Process-global guest manager, installed once at startup, read by the mint
// endpoint. `None` when the instance has no auth/history (guest sessions can't
// exist). The manager's own `is_enabled()` is the fail-closed authority.
static GLOBAL: OnceLock<Arc<GuestManager>> = OnceLock::new();

/// Install the process-wide guest manager. Called once at startup.
pub fn install_global(mgr: Arc<GuestManager>) {
    let _ = GLOBAL.set(mgr);
}

/// The process-wide guest manager, or `None` when guest sessions are unavailable.
pub fn global() -> Option<Arc<GuestManager>> {
    GLOBAL.get().cloned()
}
