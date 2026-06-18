// SPDX-License-Identifier: AGPL-3.0-or-later

// src/companion/mod.rs
//! Companion mode — 
//!
//! A user-explicit, per-account mode that biases MIRA toward warm
//! conversational behaviour, seeds a persona into the user's wiki,
//! and (in later slices) schedules proactive check-ins with a
//! safety floor.
//!
//! See `design-docs/companion/design-proposal.md` for the full design.
//!
//! # What ships in 
//! - SQL settings store (this DB carries the operational flags; the
//! wiki carries the persona content).
//! - 5 persona templates auto-seeded into the user's wiki on
//! `enable`.
//! - 6 model-callable tools (status / enable / disable / pause /
//! resume / configure).
//! - A facade ([`CompanionSystem`]) that other layers — the chat
//! handler, the future check-in scheduler, the future engagement
//! assessor — call into.
//!
//! # What does NOT ship in 
//! - Proactive check-ins.
//! - Chit-chat detection / engagement assessment.
//! - Safety floor enforcement — only the documentation
//! page is seeded; the runtime behaviour is a stub.
//! - Group-based notification bridge.

pub mod briefing;
pub mod chitchat;
pub mod dispatcher;
pub mod engagement;
pub mod engagement_log;
pub mod groups;
pub mod persona;
pub mod policy;
pub mod routing;
pub mod safety;
pub mod safety_log;
pub mod scheduler;
pub mod settings;

pub use settings::{CompanionSettings, CompanionStore};

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use tracing::{debug, info, warn};

use crate::auth::LocalAuthService;
use crate::wiki::{PageFrontmatter, Provenance, WikiOp, WikiPath, WikiRegistry};

#[derive(Debug, thiserror::Error)]
pub enum CompanionError {
    #[error("companion: io: {0}")]
    Io(#[from] std::io::Error),
    #[error("companion: sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("companion: json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("companion: wiki: {0}")]
    Wiki(#[from] crate::wiki::WikiError),
    #[error("companion: not enabled for user '{0}'")]
    NotEnabled(String),
    #[error("companion: setup incomplete — missing {0}")]
    SetupIncomplete(String),
    #[error("companion: invalid safety contact '{0}' (no such user)")]
    UnknownSafetyContact(String),
    #[error("companion: cannot use self as safety contact")]
    SelfSafetyContact,
    #[error("companion: invalid argument: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, CompanionError>;

// Public facade. One instance per MIRA server, held by the gateway
// and shared into tool implementations via `Arc<CompanionSystem>`.
// // The facade reaches into:
// - the local auth service to validate safety contacts
// - the wiki registry to seed persona pages into the user's wiki
// - the `CompanionStore` for settings persistence
pub struct CompanionSystem {
    // Held as `Arc` so the scheduler and dispatcher can
    // share the same store handle without re-opening the SQLite
    // connection. Internal users go through the facade methods on
    // `Self`; external callers that need the raw store (the
    // scheduler + dispatcher) call `store_arc()`.
    store: Arc<CompanionStore>,
    // Per-turn engagement labels. The post-hook writes
    // here; the scheduler reads from it for cadence adjustment.
    // Opens against the same companion.db file as `store`.
    engagement: Arc<crate::companion::engagement_log::EngagementLog>,
    // Append-only safety audit log. Both the distress
    // post-hook and the missed-check-in escalator write here.
    safety_log: Arc<crate::companion::safety_log::SafetyLog>,
    // companion-aware group settings. Used by the
    // safety floor's group-bridge path.
    groups: Arc<crate::companion::groups::CompanionGroupStore>,
    auth: Option<Arc<LocalAuthService>>,
    wiki: Option<Arc<WikiRegistry>>,
    // history store for delivering safety notices into
    // the contact's web thread. Wired by the gateway after
    // construction.
    history: Option<Arc<crate::history::HistoryStore>>,
    // notification bus so the contact's open web tab
    // wakes up when a safety notice is delivered. Held in a
    // `OnceLock` because the bus is constructed later in the
    // gateway flow than the CompanionSystem itself — see
    // `set_notifications`.
    notifications: std::sync::OnceLock<Arc<crate::notifications::NotificationBus>>,
}

impl CompanionSystem {
    // Open the settings store at `<data_dir>/companion.db`. Auth and
    // wiki are wired in via `with_auth` / `with_wiki` after
    // construction so the facade is usable in tests without those
    // dependencies (with a graceful no-op for the parts that need
    // them).
    pub fn open(data_dir: &Path) -> Result<Self> {
        let db_path = data_dir.join("companion.db");
        let store = Arc::new(CompanionStore::open(&db_path)?);
        let engagement = Arc::new(
            crate::companion::engagement_log::EngagementLog::open(&db_path)?,
        );
        let safety_log = Arc::new(
            crate::companion::safety_log::SafetyLog::open(&db_path)?,
        );
        let groups = Arc::new(
            crate::companion::groups::CompanionGroupStore::open(&db_path)?,
        );
        Ok(Self {
            store, engagement, safety_log, groups,
            auth: None, wiki: None,
            history: None,
            notifications: std::sync::OnceLock::new(),
        })
    }

    pub fn with_auth(mut self, auth: Arc<LocalAuthService>) -> Self {
        self.auth = Some(auth);
        self
    }

    pub fn with_wiki(mut self, wiki: Arc<WikiRegistry>) -> Self {
        self.wiki = Some(wiki);
        self
    }

    // Wire the history store so the safety floor can deliver
    // notices into the contact's "Safety alerts" thread.
    pub fn with_history(mut self, history: Arc<crate::history::HistoryStore>) -> Self {
        self.history = Some(history);
        self
    }

    // Wire the notification bus after construction (gateway uses
    // this — the bus is built later than `CompanionSystem`).
    // Returns `Err(())` if already set; safe to ignore on retry.
    pub fn set_notifications(
        &self,
        bus: Arc<crate::notifications::NotificationBus>,
    ) -> std::result::Result<(), ()> {
        self.notifications.set(bus).map_err(|_| ())
    }

    pub fn store(&self) -> &CompanionStore { &self.store }

    // Clone the shared store handle. Used by the gateway to wire
    // the scheduler + dispatcher.
    pub fn store_arc(&self) -> Arc<CompanionStore> { Arc::clone(&self.store) }

    // Clone the engagement log handle. Used by the gateway to wire
    // the scheduler's cadence adjustment + AgentCore's post-hook.
    pub fn engagement_arc(&self) -> Arc<crate::companion::engagement_log::EngagementLog> {
        Arc::clone(&self.engagement)
    }

    // Clone the safety log handle. Used by the gateway to build the
    // SafetyFloor for both the AgentCore post-hook and the scheduler.
    pub fn safety_log_arc(&self) -> Arc<crate::companion::safety_log::SafetyLog> {
        Arc::clone(&self.safety_log)
    }

    // Clone the group store handle.
    pub fn groups_arc(&self) -> Arc<crate::companion::groups::CompanionGroupStore> {
        Arc::clone(&self.groups)
    }

    // Borrow the auth service handle if installed. Read-only — used
    // by the safety floor builder.
    pub fn auth(&self) -> Option<Arc<LocalAuthService>> {
        self.auth.clone()
    }

    // Clone the history store handle if wired.
    pub fn history_arc(&self) -> Option<Arc<crate::history::HistoryStore>> {
        self.history.clone()
    }

    // Clone the notification bus handle if wired.
    pub fn notifications_arc(&self) -> Option<Arc<crate::notifications::NotificationBus>> {
        self.notifications.get().cloned()
    }

    // Fetch settings for `user_id` (or `Ok(None)` for never-enabled).
    pub fn get(&self, user_id: &str) -> Result<Option<CompanionSettings>> {
        Ok(self.store.get(user_id)?)
    }

    // Convenience accessor — is the user currently in the
    // enabled-and-not-paused state? Returns false for never-enabled,
    // disabled, setup-incomplete, and paused states. Used by future
    // slices' hot paths.
    pub fn is_active(&self, user_id: &str) -> bool {
        let now = Utc::now();
        match self.store.get(user_id) {
            Ok(Some(s)) => s.is_active(now),
            _ => false,
        }
    }

    // Enable companion mode for `user_id`. Seeds persona templates
    // into the user's wiki and (when all minimum bootstrap fields are
    // present) stamps `setup_completed_at`.
    //     // `safety_contact_user_id` is `Option<&str>` because admin
    // callers may enable companion mode without a configured
    // contact — the role check lives at the caller layer (the
    // `companion_enable` tool / HTTP endpoint), not here. When the
    // contact is `None`, the safety floor's `NoContact` path takes
    // over: distress events still produce audit rows, just no
    // outbound delivery.
    //     // Idempotent — calling `enable` on an already-enabled user is a
    // no-op for the wiki templates (they're guarded by an existence
    // check). Passing `None` for the safety contact while a prior
    // row had one set CLEARS the contact; passing `Some(...)`
    // refreshes it.
    pub fn enable(
        &self,
        user_id: &str,
        safety_contact_user_id: Option<&str>,
    ) -> Result<CompanionSettings> {
        if let Some(contact) = safety_contact_user_id {
            if user_id == contact {
                return Err(CompanionError::SelfSafetyContact);
            }
            // Validate the safety contact resolves to a real user. We
            // accept missing auth (tests) but enforce existence when
            // auth is wired.
            if let Some(auth) = &self.auth {
                let exists = auth.get_user(contact)
                    .map_err(|e| CompanionError::Invalid(format!("auth lookup: {e}")))?
                    .is_some();
                if !exists {
                    return Err(CompanionError::UnknownSafetyContact(contact.to_string()));
                }
            }
        }

        let now = Utc::now();
        let prior = self.store.get(user_id)?;
        let created_at = prior.as_ref().map(|p| p.created_at).unwrap_or(now);
        let setup_done = prior.as_ref().and_then(|p| p.setup_completed_at)
            .or(Some(now));

        // Preserve any prior quiet_hours; if the user had none on file,
        // try to derive them from their onboarding contact window so we
        // don't have to ask the user again for something they already
        // told us. Falls through to `[]` (policy default 22:00–07:00)
        // when onboarding never captured the field.
        let quiet_hours = match prior.as_ref().map(|p| p.quiet_hours.clone()) {
            Some(qh) if !qh.is_empty() => qh,
            _ => derive_quiet_hours_from_onboarding(self.auth.as_deref(), user_id)
                .unwrap_or_default(),
        };
        if !quiet_hours.is_empty() && prior.as_ref().map(|p| p.quiet_hours.is_empty()).unwrap_or(true) {
            info!(
                "companion: inferred quiet_hours {quiet_hours:?} for '{user_id}' \
                 from onboarding contact window"
            );
        }

        let settings = CompanionSettings {
            user_id: user_id.to_string(),
            enabled: true,
            paused_until: None,
            quiet_hours,
            preferred_channels: prior.as_ref()
                .map(|p| p.preferred_channels.clone()).unwrap_or_default(),
            safety_contact_user_id: safety_contact_user_id.map(String::from),
            setup_completed_at: setup_done,
            last_checkin_at: prior.as_ref().and_then(|p| p.last_checkin_at),
            consecutive_missed_checkins: prior.as_ref()
                .map(|p| p.consecutive_missed_checkins).unwrap_or(0),
            // Q1.6 — briefing settings preserved across re-enable so
            // a disable/re-enable cycle doesn't silently drop a
            // configured briefing schedule.
            daily_briefing_enabled: prior.as_ref()
                .map(|p| p.daily_briefing_enabled).unwrap_or(false),
            daily_briefing_hour: prior.as_ref()
                .map(|p| p.daily_briefing_hour).unwrap_or(7),
            last_briefing_at: prior.as_ref().and_then(|p| p.last_briefing_at),
            // Cadence overrides survive a disable/re-enable cycle.
            cadence: prior.as_ref().map(|p| p.cadence.clone()).unwrap_or_default(),
            created_at,
            updated_at: now,
        };
        self.store.upsert(&settings)?;

        // Seed persona templates into the wiki. Tolerate wiki errors —
        // companion mode can still function (settings are stored) even
        // if the wiki isn't reachable. The user can re-run `enable`
        // later to retry seeding.
        if let Some(wiki_reg) = &self.wiki {
            let agent_name = resolve_agent_name(self.auth.as_deref(), user_id);
            let user_name  = resolve_user_name(self.auth.as_deref(), user_id);
            if let Err(e) = seed_persona(wiki_reg, user_id, &agent_name, &user_name) {
                warn!("companion enable: persona seed failed for {user_id} (non-fatal): {e}");
            }
        }

        match safety_contact_user_id {
            Some(c) => info!("companion: enabled for user '{user_id}', safety contact '{c}'"),
            None    => info!("companion: enabled for user '{user_id}', no safety contact (admin)"),
        }
        Ok(settings)
    }

    // Disable. The row stays (so re-enable preserves settings); only
    // `enabled` flips. Future slices' hot paths see `is_active = false`.
    pub fn disable(&self, user_id: &str) -> Result<()> {
        let now = Utc::now();
        let mut settings = match self.store.get(user_id)? {
            Some(s) => s,
            None => return Err(CompanionError::NotEnabled(user_id.to_string())),
        };
        settings.enabled = false;
        settings.updated_at = now;
        self.store.upsert(&settings)?;
        info!("companion: disabled for user '{user_id}'");
        Ok(())
    }

    // Pause for `hours` hours from now. `0` or negative is treated as
    // an indefinite pause (paused_until = far future) — callers
    // should explicitly `resume` to unpause in that case.
    pub fn pause(&self, user_id: &str, hours: f64) -> Result<CompanionSettings> {
        let now = Utc::now();
        let mut settings = match self.store.get(user_id)? {
            Some(s) => s,
            None => return Err(CompanionError::NotEnabled(user_id.to_string())),
        };
        let until = if hours <= 0.0 {
            // Indefinite — pick a clearly-very-future timestamp so
            // `is_active` returns false until explicit resume.
            now + chrono::Duration::days(365 * 10)
        } else {
            now + chrono::Duration::milliseconds((hours * 3_600_000.0) as i64)
        };
        settings.paused_until = Some(until);
        settings.updated_at = now;
        self.store.upsert(&settings)?;
        debug!("companion: paused '{user_id}' until {until}");
        Ok(settings)
    }

    pub fn resume(&self, user_id: &str) -> Result<CompanionSettings> {
        let now = Utc::now();
        let mut settings = match self.store.get(user_id)? {
            Some(s) => s,
            None => return Err(CompanionError::NotEnabled(user_id.to_string())),
        };
        settings.paused_until = None;
        settings.updated_at = now;
        self.store.upsert(&settings)?;
        debug!("companion: resumed '{user_id}'");
        Ok(settings)
    }

    // Partial update — any `Some` field is applied; `None` is left
    // untouched. Used by the `companion_configure` tool.
    pub fn configure(
        &self,
        user_id: &str,
        update: CompanionUpdate,
    ) -> Result<CompanionSettings> {
        let now = Utc::now();
        let mut settings = match self.store.get(user_id)? {
            Some(s) => s,
            None => return Err(CompanionError::NotEnabled(user_id.to_string())),
        };
        if let Some(qh) = update.quiet_hours {
            settings.quiet_hours = qh;
        }
        if let Some(pc) = update.preferred_channels {
            settings.preferred_channels = pc;
        }
        if let Some(safety) = update.safety_contact_user_id {
            if safety == user_id {
                return Err(CompanionError::SelfSafetyContact);
            }
            if let Some(auth) = &self.auth {
                let exists = auth.get_user(&safety)
                    .map_err(|e| CompanionError::Invalid(format!("auth lookup: {e}")))?
                    .is_some();
                if !exists {
                    return Err(CompanionError::UnknownSafetyContact(safety));
                }
            }
            settings.safety_contact_user_id = Some(safety);
        }
        if let Some(v) = update.max_unanswered_checkins {
            settings.cadence.max_unanswered_checkins = Some(v);
        }
        if let Some(v) = update.max_per_day {
            settings.cadence.max_per_day = Some(v);
        }
        if let Some(v) = update.min_gap_minutes {
            settings.cadence.min_gap_minutes = Some(v);
        }
        settings.updated_at = now;
        self.store.upsert(&settings)?;
        Ok(settings)
    }
}

// Partial-update struct for `configure`.
#[derive(Debug, Default, Clone)]
pub struct CompanionUpdate {
    pub quiet_hours: Option<Vec<(String, String)>>,
    pub preferred_channels: Option<Vec<String>>,
    pub safety_contact_user_id: Option<String>,
    // Per-user cadence overrides. `Some(v)` sets the override; `None` leaves
    // the current value (which itself may be `None` = inherit the global
    // default). Applied individually so the model can tune one knob at a time.
    pub max_unanswered_checkins: Option<u32>,
    pub max_per_day: Option<u32>,
    pub min_gap_minutes: Option<i64>,
}

// ── Internals ────────────────────────────────────────────────────────────────

// Seed persona templates into the user's wiki. Each template is
// auto-applied (the user-explicit enable gesture authorises it; the
// review queue is for *extraction*, not seeding). Existing files are
// preserved so re-running `enable` on a user who's already configured
// doesn't overwrite their edits.
fn seed_persona(
    wiki_reg: &WikiRegistry,
    user_id: &str,
    agent_name: &str,
    user_name: &str,
) -> Result<()> {
    let wiki = wiki_reg.for_user(user_id)?;
    for (rel, body) in persona::templates() {
        let path = WikiPath::parse(rel)
            .map_err(|e| CompanionError::Invalid(format!("template path '{rel}': {e}")))?;
        // Skip if the user already has the file — preserve their edits.
        if let Ok(Some(_)) = wiki.store().try_read_page(&path) {
            debug!("companion seed: '{rel}' already exists for {user_id}, skipping");
            continue;
        }
        let rendered = persona::render(body, agent_name, user_name);
        // The rendered template already includes its own frontmatter
        // (we author the markdown that way), so we parse-then-write
        // to land it through the audit pipeline cleanly.
        let (fm, doc_body) = crate::wiki::frontmatter::parse(&rendered)
            .unwrap_or_else(|_| (PageFrontmatter::default(), rendered.clone()));
        wiki.submit_and_apply(
            WikiOp::WritePage { path, frontmatter: fm, body: doc_body },
            Provenance::user_ui(user_id),
        )?;
    }
    Ok(())
}

fn resolve_agent_name(auth: Option<&LocalAuthService>, user_id: &str) -> String {
    auth.and_then(|a| a.get_profile(user_id).ok().flatten())
        .and_then(|p| p.agent_name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "MIRA".to_string())
}

fn resolve_user_name(auth: Option<&LocalAuthService>, user_id: &str) -> String {
    auth.and_then(|a| a.get_user(user_id).ok().flatten())
        .map(|u| u.display_name
            .filter(|s| !s.is_empty())
            .unwrap_or(u.username))
        .unwrap_or_else(|| "you".to_string())
}

// Derive a quiet_hours window from the user's onboarding contact-hour
// answer (`user_profile.contact_hours_start/end`, minutes from midnight
// in the user's local timezone). If the user told onboarding they're
// contactable 09:00–22:00, we set quiet_hours to the complement
// window — a single wrap-around pair `("22:00", "09:00")`. The policy
// (`policy::evaluate`) reads quiet_hours in the user's tz, so the same
// minutes-of-day semantics are preserved end-to-end.
fn derive_quiet_hours_from_onboarding(
    auth: Option<&LocalAuthService>,
    user_id: &str,
) -> Option<Vec<(String, String)>> {
    let profile = auth?.get_profile(user_id).ok().flatten()?;
    let pair = complement_contact_window(
        profile.contact_hours_start?,
        profile.contact_hours_end?,
    )?;
    Some(vec![pair])
}

// Pure inversion of a `[start, end)` contact window (minutes from
// midnight) into a single quiet-hours pair `(quiet_start, quiet_end)`
// in `HH:MM` form. Returns `None` when:
// - either bound is out of `[0, 1440]`
// - the bounds are equal or `end <= start` (overnight contactable —
// we don't invert into a sliver of daytime the user didn't ask for)
// - the user is contactable all day (no quiet window worth recording)
fn complement_contact_window(start: i64, end: i64) -> Option<(String, String)> {
    if !(0..24 * 60).contains(&start) || !(0..=24 * 60).contains(&end) {
        return None;
    }
    if start == 0 && end >= 24 * 60 {
        return None; // contactable all day
    }
    if end <= start {
        return None; // overnight contactable, skip
    }
    let fmt = |m: i64| {
        let m = m.rem_euclid(24 * 60);
        format!("{:02}:{:02}", m / 60, m % 60)
    };
    Some((fmt(end), fmt(start)))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh_system() -> (tempfile::TempDir, CompanionSystem) {
        let dir = tempdir().unwrap();
        let sys = CompanionSystem::open(dir.path()).unwrap();
        (dir, sys)
    }

    #[test]
    fn enable_creates_settings_with_setup_stamped() {
        let (_dir, sys) = fresh_system();
        let s = sys.enable("alice", Some("david")).unwrap();
        assert!(s.enabled);
        assert_eq!(s.safety_contact_user_id.as_deref(), Some("david"));
        assert!(s.setup_completed_at.is_some(),
            "minimum bootstrap (safety contact) satisfied → setup stamped");
    }

    #[test]
    fn enable_refuses_self_as_safety_contact() {
        let (_dir, sys) = fresh_system();
        let err = sys.enable("alice", Some("alice")).unwrap_err();
        assert!(matches!(err, CompanionError::SelfSafetyContact));
    }

    #[test]
    fn enable_is_idempotent_and_refreshes_safety_contact() {
        let (_dir, sys) = fresh_system();
        let first = sys.enable("alice", Some("david")).unwrap();
        let second = sys.enable("alice", Some("sarah")).unwrap();
        // created_at is preserved across re-enable
        assert_eq!(first.created_at.timestamp_millis(), second.created_at.timestamp_millis());
        // safety contact got refreshed
        assert_eq!(second.safety_contact_user_id.as_deref(), Some("sarah"));
    }

    #[test]
    fn enable_with_no_safety_contact_succeeds() {
        // Admin path — the system itself accepts None; the
        // non-admin guard lives at the tool layer.
        let (_dir, sys) = fresh_system();
        let s = sys.enable("admin-tarek", None).unwrap();
        assert!(s.enabled);
        assert!(s.safety_contact_user_id.is_none());
        assert!(s.setup_completed_at.is_some(),
            "no-contact enable still completes setup (admin path)");
    }

    #[test]
    fn enable_can_clear_contact_on_re_enable() {
        let (_dir, sys) = fresh_system();
        let first = sys.enable("admin", Some("david")).unwrap();
        assert_eq!(first.safety_contact_user_id.as_deref(), Some("david"));
        let second = sys.enable("admin", None).unwrap();
        assert!(second.safety_contact_user_id.is_none(),
            "re-enable with None clears the prior contact");
    }

    #[test]
    fn complement_contact_window_inverts_standard_window() {
        // contactable 09:00–22:00 → quiet 22:00–09:00 (wrap pair)
        assert_eq!(
            complement_contact_window(9 * 60, 22 * 60),
            Some(("22:00".into(), "09:00".into())),
        );
    }

    #[test]
    fn complement_contact_window_skips_all_day() {
        assert!(complement_contact_window(0, 24 * 60).is_none(),
            "user contactable 24h → no quiet window worth recording");
    }

    #[test]
    fn complement_contact_window_skips_overnight() {
        // Unusual: user says they're contactable 22:00–06:00. We don't
        // try to invert this into a tiny daytime quiet pair.
        assert!(complement_contact_window(22 * 60, 6 * 60).is_none());
    }

    #[test]
    fn complement_contact_window_skips_equal_bounds() {
        assert!(complement_contact_window(9 * 60, 9 * 60).is_none());
    }

    #[test]
    fn complement_contact_window_rejects_out_of_range() {
        assert!(complement_contact_window(-1, 22 * 60).is_none());
        assert!(complement_contact_window(9 * 60, 1500).is_none());
    }

    #[test]
    fn enable_inherits_quiet_hours_from_onboarding_contact_window() {
        use std::sync::Arc;
        use crate::auth::{LocalAuthService, NewUser, Role};
        let dir = tempdir().unwrap();
        let auth = Arc::new(LocalAuthService::new(
            &dir.path().join("auth.db"),
            "test-secret".into(),
            7,
        ).unwrap());
        let user = auth.create_user(NewUser {
            username: "alice".into(),
            display_name: None,
            email: None,
            password: "test-password-1234".into(),
            role: Role::Admin,
        }).unwrap();
        // Simulate onboarding having captured a 09:00–22:00 contact window.
        auth.upsert_profile_field(&user.id, "contact_hours_start", 9 * 60_i64).unwrap();
        auth.upsert_profile_field(&user.id, "contact_hours_end",   22 * 60_i64).unwrap();

        let sys = CompanionSystem::open(dir.path()).unwrap().with_auth(auth);
        let s = sys.enable(&user.id, None).unwrap();
        assert_eq!(
            s.quiet_hours,
            vec![("22:00".to_string(), "09:00".to_string())],
            "enable should infer quiet_hours from the onboarding contact window",
        );
    }

    #[test]
    fn enable_does_not_overwrite_existing_quiet_hours() {
        use std::sync::Arc;
        use crate::auth::{LocalAuthService, NewUser, Role};
        let dir = tempdir().unwrap();
        let auth = Arc::new(LocalAuthService::new(
            &dir.path().join("auth.db"),
            "test-secret".into(),
            7,
        ).unwrap());
        let user = auth.create_user(NewUser {
            username: "alice".into(),
            display_name: None,
            email: None,
            password: "test-password-1234".into(),
            role: Role::Admin,
        }).unwrap();
        auth.upsert_profile_field(&user.id, "contact_hours_start", 9 * 60_i64).unwrap();
        auth.upsert_profile_field(&user.id, "contact_hours_end",   22 * 60_i64).unwrap();
        let sys = CompanionSystem::open(dir.path()).unwrap().with_auth(auth);

        // First enable infers; user then manually configures something
        // different; re-enable preserves their override.
        sys.enable(&user.id, None).unwrap();
        let custom = vec![("23:00".to_string(), "06:00".to_string())];
        sys.configure(&user.id, CompanionUpdate {
            quiet_hours: Some(custom.clone()),
            ..CompanionUpdate::default()
        }).unwrap();
        let s2 = sys.enable(&user.id, None).unwrap();
        assert_eq!(s2.quiet_hours, custom,
            "re-enable must not stomp the user's manual override");
    }

    #[test]
    fn disable_flips_enabled_but_keeps_row() {
        let (_dir, sys) = fresh_system();
        sys.enable("alice", Some("david")).unwrap();
        sys.disable("alice").unwrap();
        let s = sys.get("alice").unwrap().unwrap();
        assert!(!s.enabled);
        assert_eq!(s.safety_contact_user_id.as_deref(), Some("david"),
            "settings preserved across disable for re-enable continuity");
    }

    #[test]
    fn disable_unknown_user_errors() {
        let (_dir, sys) = fresh_system();
        let err = sys.disable("nobody").unwrap_err();
        assert!(matches!(err, CompanionError::NotEnabled(_)));
    }

    #[test]
    fn pause_resume_round_trip() {
        let (_dir, sys) = fresh_system();
        sys.enable("alice", Some("david")).unwrap();
        assert!(sys.is_active("alice"));
        sys.pause("alice", 1.0).unwrap();
        assert!(!sys.is_active("alice"));
        sys.resume("alice").unwrap();
        assert!(sys.is_active("alice"));
    }

    #[test]
    fn pause_zero_hours_means_indefinite() {
        let (_dir, sys) = fresh_system();
        sys.enable("alice", Some("david")).unwrap();
        let s = sys.pause("alice", 0.0).unwrap();
        let until = s.paused_until.unwrap();
        let years = (until - Utc::now()).num_days() / 365;
        assert!(years >= 9, "indefinite pause should be far future, got {years} years");
    }

    #[test]
    fn configure_applies_partial_update() {
        let (_dir, sys) = fresh_system();
        sys.enable("alice", Some("david")).unwrap();
        let s = sys.configure("alice", CompanionUpdate {
            quiet_hours: Some(vec![("22:00".into(), "06:30".into())]),
            preferred_channels: Some(vec!["signal".into(), "web".into()]),
            ..Default::default()
        }).unwrap();
        assert_eq!(s.quiet_hours.len(), 1);
        assert_eq!(s.preferred_channels, vec!["signal".to_string(), "web".to_string()]);
        // Untouched: safety contact preserved
        assert_eq!(s.safety_contact_user_id.as_deref(), Some("david"));
    }

    #[test]
    fn configure_sets_per_user_cadence_overrides() {
        let (_dir, sys) = fresh_system();
        sys.enable("alice", Some("david")).unwrap();
        // Defaults: all None → inherit global.
        assert_eq!(sys.get("alice").unwrap().unwrap().cadence.max_per_day, None);

        // Set two of three; the third stays inherited.
        let s = sys.configure("alice", CompanionUpdate {
            max_per_day: Some(2),
            max_unanswered_checkins: Some(1),
            ..Default::default()
        }).unwrap();
        assert_eq!(s.cadence.max_per_day, Some(2));
        assert_eq!(s.cadence.max_unanswered_checkins, Some(1));
        assert_eq!(s.cadence.min_gap_minutes, None, "untouched knob stays inherited");

        // Partial update preserves prior overrides.
        let s2 = sys.configure("alice", CompanionUpdate {
            min_gap_minutes: Some(180),
            ..Default::default()
        }).unwrap();
        assert_eq!(s2.cadence.max_per_day, Some(2), "earlier override preserved");
        assert_eq!(s2.cadence.min_gap_minutes, Some(180));
    }

    #[test]
    fn configure_refuses_self_as_safety_contact() {
        let (_dir, sys) = fresh_system();
        sys.enable("alice", Some("david")).unwrap();
        let err = sys.configure("alice", CompanionUpdate {
            safety_contact_user_id: Some("alice".into()),
            ..Default::default()
        }).unwrap_err();
        assert!(matches!(err, CompanionError::SelfSafetyContact));
    }

    #[test]
    fn is_active_false_for_never_enabled() {
        let (_dir, sys) = fresh_system();
        assert!(!sys.is_active("ghost"));
    }
}
