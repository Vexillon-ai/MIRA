// SPDX-License-Identifier: AGPL-3.0-or-later

//! Restricted Mode — a fail-closed capability-restriction profile.
//!
//! This is a small, self-contained gate — deliberately **not** wired through the
//! agent-centric [`crate::policy::PolicyEngine`]. It is held as an
//! `Option<Arc<RestrictedPolicy>>` and consulted **unconditionally** at every
//! side-effect chokepoint (tool execution, the skill→builtin bypass, outbound
//! HTTP, channel sends, care escalations). On a normal instance the `Option` is
//! `None` and every check is a no-op, so the feature has zero behaviour cost when
//! off.
//!
//! Design guarantees:
//!   - **Fail-closed**: anything not explicitly allowed is denied.
//!   - **Deny by category**: a newly-added tool is denied by default.
//!   - **Runtime-immutable**: built once from config at startup; no path widens it.
//!   - **Safe even though public**: the checks are deterministic and assume a
//!     prompt-injection adversary who has read this code.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{NaiveDate, Utc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::{RestrictedCaps, RestrictedModeConfig};
use crate::tools::Tier;

/// Outcome of a capability check. `Deny` carries a user-facing reason that is
/// safe to surface to the model / chat (no internal detail beyond the profile
/// name and the capability that was blocked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestrictedDecision {
    Allow,
    Deny { reason: String },
}

impl RestrictedDecision {
    fn deny(reason: impl Into<String>) -> Self {
        Self::Deny { reason: reason.into() }
    }

    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    pub fn is_deny(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }

    /// The deny reason, or `None` when allowed. Handy at call sites that turn a
    /// deny into a `ToolResult::failure(..)`.
    pub fn deny_reason(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::Deny { reason } => Some(reason),
        }
    }
}

/// The one built-in profile shipped in Slice 1. Named so config can select it and
/// error messages can name it.
pub const HARDENED: &str = "hardened";

/// The safe showcase set the `hardened` profile permits. Every entry is a
/// `Tier::Pure` builtin that touches only MIRA-owned state (memory / wiki) or is
/// read-only / deterministic. Kept deliberately tight — easy to widen later, hard
/// to un-leak.
///
/// Note what is *absent*: `settings_set` (mutates config), `automations_*`
/// (schedule future sends), `companion_*` (reconfigure proactive delivery),
/// `calendar_*`, `backup_*` — all have real or deferred side effects even though
/// some report `Tier::Pure`.
const HARDENED_ALLOW_TOOLS: &[&str] = &[
    // Per-(session) memory.
    "recall_history",
    "memory_supersede",
    // Per-(session) wiki — read + write to the caller's own sandboxed wiki.
    "wiki_search",
    "wiki_read",
    "wiki_append_section",
    "wiki_write_page",
    "wiki_log_entry",
    // Read-only self-knowledge + deterministic helpers.
    "mira_help",
    "now",
    "date_math",
    "math_eval",
    // Reasoning over the caller's own conversation history.
    "summarize_conversation",
];

/// Which profile is active. An enum (not a free string) so the match at each
/// check is exhaustive and adding a profile forces every call site to consider it.
#[derive(Debug, Clone)]
enum Profile {
    Hardened,
}

/// A resolved, immutable capability-restriction profile. Construct with
/// [`RestrictedPolicy::from_config`]; hold behind an `Arc` and share it into every
/// enforcement point.
#[derive(Debug, Clone)]
pub struct RestrictedPolicy {
    profile: Profile,
}

impl RestrictedPolicy {
    /// Resolve the configured profile.
    ///
    /// - `profile` absent / empty / `null` ⇒ `Ok(None)` (Restricted Mode off).
    /// - `"hardened"` ⇒ `Ok(Some(_))`.
    /// - any other value ⇒ `Err(..)` — an **unknown profile is a fatal config
    ///   error**, not a silent fall-through to "unrestricted" (fail-closed). The
    ///   caller aborts startup.
    pub fn from_config(cfg: &RestrictedModeConfig) -> Result<Option<Self>, String> {
        let name = match cfg.profile.as_deref() {
            None => return Ok(None),
            Some(s) if s.trim().is_empty() => return Ok(None),
            Some(s) => s.trim(),
        };
        match name {
            HARDENED => Ok(Some(Self { profile: Profile::Hardened })),
            other => Err(format!(
                "restricted_mode.profile = '{other}' is not a known profile \
                 (expected '{HARDENED}' or unset). Refusing to start with an \
                 unrecognised restriction profile."
            )),
        }
    }

    /// The active profile's name (for logs, audit rows, and deny messages).
    pub fn profile_name(&self) -> &'static str {
        match self.profile {
            Profile::Hardened => HARDENED,
        }
    }

    /// Gate a tool invocation. Allowed **iff** the tool is on the profile's
    /// allow-list **and** its capability tier is `Pure`. The `Pure` backstop is
    /// absolute for `hardened`: even a mis-configured allow-list can never expose
    /// a `Network`/`Filesystem`/`Code`/`System` tool.
    pub fn check_tool(&self, name: &str, tier: Tier) -> RestrictedDecision {
        match self.profile {
            Profile::Hardened => {
                if tier != Tier::Pure {
                    return RestrictedDecision::deny(format!(
                        "'{name}' is disabled in restricted mode ({}): its capability \
                         tier '{tier}' is not permitted.",
                        self.profile_name()
                    ));
                }
                if HARDENED_ALLOW_TOOLS.contains(&name) {
                    RestrictedDecision::Allow
                } else {
                    RestrictedDecision::deny(format!(
                        "'{name}' is not available in restricted mode ({}).",
                        self.profile_name()
                    ))
                }
            }
        }
    }

    /// Gate a real outbound message to an external channel (email/SMS/Signal/
    /// Telegram/WhatsApp/Discord/Matrix/Slack/push). Always denied under
    /// `hardened` — the instance must not message real people.
    pub fn check_channel_send(&self, channel: &str) -> RestrictedDecision {
        match self.profile {
            Profile::Hardened => RestrictedDecision::deny(format!(
                "outbound {channel} messages are disabled in restricted mode ({}).",
                self.profile_name()
            )),
        }
    }

    /// Gate a care-network escalation to a real safety contact. Always denied
    /// under `hardened`. (The web-record arm that only writes a Safety-alerts
    /// thread is left untouched at the call site — it performs no external egress.)
    pub fn check_care_escalation(&self) -> RestrictedDecision {
        match self.profile {
            Profile::Hardened => RestrictedDecision::deny(format!(
                "care-network escalations to real contacts are disabled in \
                 restricted mode ({}).",
                self.profile_name()
            )),
        }
    }

    /// Gate arbitrary outbound HTTP (web fetch / search / url preview). Always
    /// denied under `hardened` — no SSRF surface, no data exfil path. Layered on
    /// top of the existing [`crate::tools::http_policy::HttpPolicy`] SSRF guard.
    pub fn check_network_egress(&self, host: &str) -> RestrictedDecision {
        match self.profile {
            Profile::Hardened => RestrictedDecision::deny(format!(
                "outbound network access (to '{host}') is disabled in restricted \
                 mode ({}).",
                self.profile_name()
            )),
        }
    }
}

// ── Process-global access for scattered send chokepoints ─────────────────────
//
// The tool registry and HTTP policy receive the `RestrictedPolicy` by explicit
// injection (clean builders, injectable tests). The channel-send fan-outs
// (`deliver_outbound`, `deliver_to_user`, …) are constructed by struct literals
// in many places and are impractical to thread through; because Restricted Mode
// is genuinely process-wide and **fixed at startup**, a set-once global is the
// correct, centralized surface for them (it also cannot be flipped at runtime —
// `OnceLock` is write-once, reinforcing the immutability guarantee).

static GLOBAL: OnceLock<Option<Arc<RestrictedPolicy>>> = OnceLock::new();

/// Install the process-wide Restricted Mode policy. Called exactly once, at
/// startup, from the gateway builder. Subsequent calls are ignored (write-once),
/// so nothing can widen or replace the profile after boot.
pub fn install_global(policy: Option<Arc<RestrictedPolicy>>) {
    let _ = GLOBAL.set(policy);
}

/// The process-wide Restricted Mode policy, or `None` when off / not yet
/// installed (e.g. in unit tests that never boot the gateway).
pub fn global() -> Option<Arc<RestrictedPolicy>> {
    GLOBAL.get().cloned().flatten()
}

/// Convenience for channel-send chokepoints: `Some(reason)` when the active
/// profile denies sending on `channel`, else `None`. Reads the process-global.
/// The `web` surface (the chat UI itself) is never a "real outbound channel" and
/// is always permitted — callers pass their real channel name, not `"web"`.
pub fn channel_send_denied(channel: &str) -> Option<String> {
    channel_send_denied_in(&global(), channel)
}

/// Injectable core of [`channel_send_denied`] — unit-tested directly without
/// touching the process-global.
fn channel_send_denied_in(
    policy: &Option<Arc<RestrictedPolicy>>,
    channel: &str,
) -> Option<String> {
    if channel.eq_ignore_ascii_case("web") {
        return None;
    }
    match policy.as_ref()?.check_channel_send(channel) {
        RestrictedDecision::Allow          => None,
        RestrictedDecision::Deny { reason } => Some(reason),
    }
}

// ── Resource / cost caps (Slice 2) ───────────────────────────────────────────
//
// A stateful runtime enforcing the per-user + global caps in `RestrictedCaps`.
// Held behind an `Arc`; built once at startup when a profile is active. Every
// cap is independently no-op when its config value is 0 (unlimited), so an
// operator opts into exactly the caps they want.

/// Verdict from a pre-turn cap check. `Deny` carries a short, user-facing message
/// safe to render directly in the chat UI (graceful degradation, never a stack
/// trace or internal detail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapVerdict {
    Ok,
    Deny { message: String },
}

impl CapVerdict {
    pub fn is_ok(&self) -> bool { matches!(self, Self::Ok) }
    pub fn denied_message(&self) -> Option<&str> {
        match self { Self::Ok => None, Self::Deny { message } => Some(message) }
    }
    fn deny(message: impl Into<String>) -> Self { Self::Deny { message: message.into() } }
}

/// A held concurrency slot for one in-flight turn. Dropping it frees the slot.
/// `Unlimited` when no concurrency cap is configured; `Acquired` holds a real
/// permit for the turn's lifetime.
pub enum TurnSlot {
    Unlimited,
    Acquired(#[allow(dead_code)] OwnedSemaphorePermit),
}

/// Rolling per-UTC-day token accumulator.
struct DailyTokens {
    day:    NaiveDate,
    tokens: u64,
}

/// Stateful Restricted Mode caps enforcer.
pub struct RestrictedCapsRuntime {
    caps:        RestrictedCaps,
    // Per-user sliding window of recent inbound-message timestamps.
    rate:        Mutex<HashMap<String, VecDeque<Instant>>>,
    // Global in-flight-turn permits. `None` when unlimited.
    concurrency: Option<Arc<Semaphore>>,
    // Global per-day token spend.
    daily:       Mutex<DailyTokens>,
}

impl RestrictedCapsRuntime {
    pub fn new(caps: RestrictedCaps) -> Self {
        let concurrency = (caps.max_concurrent_sessions > 0)
            .then(|| Arc::new(Semaphore::new(caps.max_concurrent_sessions as usize)));
        Self {
            caps,
            rate:        Mutex::new(HashMap::new()),
            concurrency,
            daily:       Mutex::new(DailyTokens { day: Utc::now().date_naive(), tokens: 0 }),
        }
    }

    /// Per-user inbound rate limit (messages per rolling 60s). Records the
    /// attempt when allowed, so callers check exactly once per inbound message.
    pub fn check_rate(&self, user_id: &str) -> CapVerdict {
        let limit = self.caps.messages_per_min;
        if limit == 0 { return CapVerdict::Ok; }
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let mut g = self.rate.lock().unwrap_or_else(|p| p.into_inner());
        let dq = g.entry(user_id.to_string()).or_default();
        while let Some(&front) = dq.front() {
            if now.duration_since(front) >= window { dq.pop_front(); } else { break; }
        }
        if dq.len() as u32 >= limit {
            return CapVerdict::deny(
                "You're sending messages a little too quickly — give me a moment and try again."
            );
        }
        dq.push_back(now);
        CapVerdict::Ok
    }

    /// Global per-UTC-day token ceiling. Checked before a turn runs; resets when
    /// the day rolls over.
    pub fn check_daily_ceiling(&self) -> CapVerdict {
        let ceiling = self.caps.daily_token_ceiling;
        if ceiling == 0 { return CapVerdict::Ok; }
        let today = Utc::now().date_naive();
        let mut g = self.daily.lock().unwrap_or_else(|p| p.into_inner());
        if g.day != today { g.day = today; g.tokens = 0; }
        if g.tokens >= ceiling {
            return CapVerdict::deny(
                "This service is unusually busy right now and has reached today's usage limit. \
                 Please try again later."
            );
        }
        CapVerdict::Ok
    }

    /// Add a completed turn's token usage to today's global tally. No-op when the
    /// daily ceiling is unlimited.
    pub fn record_tokens(&self, tokens: u64) {
        if self.caps.daily_token_ceiling == 0 || tokens == 0 { return; }
        let today = Utc::now().date_naive();
        let mut g = self.daily.lock().unwrap_or_else(|p| p.into_inner());
        if g.day != today { g.day = today; g.tokens = 0; }
        g.tokens = g.tokens.saturating_add(tokens);
    }

    /// Try to claim a global in-flight-turn slot. Returns `None` when the
    /// instance is at its concurrency cap (caller degrades gracefully); otherwise
    /// a `TurnSlot` the caller holds for the turn's duration.
    pub fn try_acquire_turn_slot(&self) -> Option<TurnSlot> {
        match &self.concurrency {
            None => Some(TurnSlot::Unlimited),
            Some(sem) => Arc::clone(sem).try_acquire_owned().ok().map(TurnSlot::Acquired),
        }
    }

    /// Clamp a per-turn response-token budget to the configured cap (0 = no clamp).
    pub fn clamp_response_tokens(&self, configured: u32) -> u32 {
        match self.caps.max_tokens_per_turn {
            0   => configured,
            cap => configured.min(cap),
        }
    }

    /// The hard per-turn response-token cap, or `None` when unset. Used to set the
    /// provider's `max_tokens` so output length is actually bounded (not just the
    /// context reservation).
    pub fn response_token_cap(&self) -> Option<u32> {
        (self.caps.max_tokens_per_turn > 0).then_some(self.caps.max_tokens_per_turn)
    }

    /// The effective context window for a turn given the config's base value.
    /// - cap 0 → the base is unchanged.
    /// - base 0 (unbudgeted / legacy fixed-window) but a cap is set → adopt the
    ///   cap as the window, so a restricted instance budgets history even when the
    ///   operator left `agent.context_length_tokens` unset.
    /// - otherwise → the smaller of the two.
    pub fn clamp_context_budget(&self, base: usize) -> usize {
        match self.caps.context_budget_tokens {
            0                    => base,
            cap if base == 0     => cap as usize,
            cap                  => base.min(cap as usize),
        }
    }

    /// Max conversation age in seconds, or `None` when unlimited.
    pub fn max_session_secs(&self) -> Option<u64> {
        (self.caps.max_session_secs > 0).then_some(self.caps.max_session_secs)
    }
}

// The caps runtime process-global, alongside the policy global. Same rationale:
// the turn path + chat handler reach it without threading through many builders.
static CAPS: OnceLock<Option<Arc<RestrictedCapsRuntime>>> = OnceLock::new();

/// Install the process-wide caps runtime. Called once at startup. Write-once.
pub fn install_caps_global(runtime: Option<Arc<RestrictedCapsRuntime>>) {
    let _ = CAPS.set(runtime);
}

/// The process-wide caps runtime, or `None` when Restricted Mode is off / caps
/// are not installed.
pub fn caps() -> Option<Arc<RestrictedCapsRuntime>> {
    CAPS.get().cloned().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hardened() -> RestrictedPolicy {
        let cfg = RestrictedModeConfig { profile: Some(HARDENED.into()), ..Default::default() };
        RestrictedPolicy::from_config(&cfg).unwrap().expect("hardened is Some")
    }

    #[test]
    fn absent_or_empty_profile_is_off() {
        for p in [None, Some(String::new()), Some("   ".into())] {
            let cfg = RestrictedModeConfig { profile: p, ..Default::default() };
            assert!(RestrictedPolicy::from_config(&cfg).unwrap().is_none());
        }
    }

    #[test]
    fn unknown_profile_is_a_fatal_error_not_silent_off() {
        // Fail-closed: an unrecognised profile must NOT resolve to "unrestricted".
        let cfg = RestrictedModeConfig { profile: Some("kiosk-lite".into()), ..Default::default() };
        assert!(RestrictedPolicy::from_config(&cfg).is_err());
    }

    #[test]
    fn profile_name_is_reported() {
        assert_eq!(hardened().profile_name(), "hardened");
    }

    #[test]
    fn hardened_allows_the_showcase_set() {
        let p = hardened();
        for name in HARDENED_ALLOW_TOOLS {
            assert!(
                p.check_tool(name, Tier::Pure).is_allow(),
                "expected '{name}' to be allowed"
            );
        }
    }

    #[test]
    fn hardened_denies_unknown_pure_tools() {
        let p = hardened();
        // Pure-tier but with deferred side effects — denied because unlisted.
        for name in ["settings_set", "automations_register_webhook", "companion_enable"] {
            let d = p.check_tool(name, Tier::Pure);
            assert!(d.is_deny(), "expected '{name}' to be denied");
        }
    }

    #[test]
    fn hardened_denies_non_pure_tiers_even_if_named() {
        let p = hardened();
        // The Pure backstop is absolute: naming a network/code/etc tool on the
        // allow-list would still not expose it. Prove it directly by feeding an
        // allow-listed *name* with a non-Pure tier.
        for tier in [Tier::Network, Tier::Filesystem, Tier::Code, Tier::System] {
            let d = p.check_tool("wiki_read", tier);
            assert!(d.is_deny(), "non-Pure tier {tier} must be denied");
        }
    }

    #[test]
    fn hardened_denies_all_side_effect_categories() {
        let p = hardened();
        assert!(p.check_channel_send("email").is_deny());
        assert!(p.check_channel_send("telegram").is_deny());
        assert!(p.check_care_escalation().is_deny());
        assert!(p.check_network_egress("example.com").is_deny());
    }

    #[test]
    fn channel_send_helper_allows_web_but_denies_real_channels_under_hardened() {
        let p = Some(Arc::new(hardened()));
        // The chat UI ("web") is never a real outbound channel — always allowed,
        // even under hardened, or the demo chat itself would break.
        assert!(channel_send_denied_in(&p, "web").is_none());
        assert!(channel_send_denied_in(&p, "WEB").is_none());
        // Real channels are denied.
        for ch in ["telegram", "email", "signal", "whatsapp", "external:foo"] {
            assert!(channel_send_denied_in(&p, ch).is_some(), "expected {ch} denied");
        }
    }

    #[test]
    fn channel_send_helper_is_noop_when_restricted_mode_off() {
        let off: Option<Arc<RestrictedPolicy>> = None;
        for ch in ["telegram", "email", "web"] {
            assert!(channel_send_denied_in(&off, ch).is_none());
        }
    }

    #[test]
    fn deny_reasons_name_the_profile_and_are_non_empty() {
        let p = hardened();
        let d = p.check_tool("shell_execute", Tier::Code);
        let reason = d.deny_reason().expect("deny carries a reason");
        assert!(reason.contains("restricted mode"));
        assert!(reason.contains("hardened"));
    }

    // ── caps runtime ────────────────────────────────────────────────────────

    #[test]
    fn caps_rate_limit_blocks_after_n_per_minute_and_is_per_user() {
        let rt = RestrictedCapsRuntime::new(RestrictedCaps { messages_per_min: 2, ..Default::default() });
        assert!(rt.check_rate("u1").is_ok());
        assert!(rt.check_rate("u1").is_ok());
        assert!(!rt.check_rate("u1").is_ok(), "3rd message in the window is blocked");
        assert!(rt.check_rate("u2").is_ok(), "a different user is unaffected");
    }

    #[test]
    fn caps_rate_limit_unlimited_when_zero() {
        let rt = RestrictedCapsRuntime::new(RestrictedCaps::default());
        for _ in 0..100 { assert!(rt.check_rate("u1").is_ok()); }
    }

    #[test]
    fn caps_daily_ceiling_denies_once_reached() {
        let rt = RestrictedCapsRuntime::new(RestrictedCaps { daily_token_ceiling: 100, ..Default::default() });
        assert!(rt.check_daily_ceiling().is_ok());
        rt.record_tokens(60);
        assert!(rt.check_daily_ceiling().is_ok());
        rt.record_tokens(60); // 120 >= 100
        assert!(!rt.check_daily_ceiling().is_ok());
        // record is a no-op when the ceiling is unlimited.
        let off = RestrictedCapsRuntime::new(RestrictedCaps::default());
        off.record_tokens(1_000_000);
        assert!(off.check_daily_ceiling().is_ok());
    }

    #[test]
    fn caps_clamps_take_the_min_and_zero_means_no_clamp() {
        let rt = RestrictedCapsRuntime::new(RestrictedCaps {
            max_tokens_per_turn: 256, context_budget_tokens: 1000, ..Default::default()
        });
        assert_eq!(rt.clamp_response_tokens(16384), 256);
        assert_eq!(rt.clamp_response_tokens(100), 100); // already under the cap
        assert_eq!(rt.clamp_context_budget(8000), 1000);
        let none = RestrictedCapsRuntime::new(RestrictedCaps::default());
        assert_eq!(none.clamp_response_tokens(16384), 16384);
        assert_eq!(none.clamp_context_budget(8000), 8000);
    }

    #[test]
    fn caps_context_budget_adopts_the_cap_when_base_is_unbudgeted() {
        // base 0 (legacy fixed-window, no agent.context_length_tokens) + a cap →
        // adopt the cap so a restricted instance still budgets history.
        let rt = RestrictedCapsRuntime::new(RestrictedCaps { context_budget_tokens: 4000, ..Default::default() });
        assert_eq!(rt.clamp_context_budget(0), 4000);
        // no cap + base 0 stays 0 (unchanged legacy behaviour).
        let none = RestrictedCapsRuntime::new(RestrictedCaps::default());
        assert_eq!(none.clamp_context_budget(0), 0);
    }

    #[test]
    fn caps_response_token_cap_reflects_config() {
        let rt = RestrictedCapsRuntime::new(RestrictedCaps { max_tokens_per_turn: 512, ..Default::default() });
        assert_eq!(rt.response_token_cap(), Some(512));
        let none = RestrictedCapsRuntime::new(RestrictedCaps::default());
        assert_eq!(none.response_token_cap(), None);
    }

    #[tokio::test]
    async fn caps_concurrency_slot_exhausts_then_frees_on_drop() {
        let rt = RestrictedCapsRuntime::new(RestrictedCaps { max_concurrent_sessions: 1, ..Default::default() });
        let slot = rt.try_acquire_turn_slot();
        assert!(slot.is_some(), "first slot acquired");
        assert!(rt.try_acquire_turn_slot().is_none(), "at capacity");
        drop(slot);
        assert!(rt.try_acquire_turn_slot().is_some(), "freed after drop");
    }

    #[test]
    fn caps_concurrency_unlimited_when_zero() {
        let rt = RestrictedCapsRuntime::new(RestrictedCaps::default());
        let mut held = Vec::new();
        for _ in 0..50 { held.push(rt.try_acquire_turn_slot().expect("unlimited")); }
    }
}
