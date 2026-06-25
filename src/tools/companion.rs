// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/companion.rs
//! Model-callable companion-mode tools.
//!
//! Six tools the model invokes on behalf of the calling user:
//!
//! - `companion_status` — what's the current state?
//! - `companion_enable` — turn the mode on (requires safety contact).
//! - `companion_disable` — turn it off.
//! - `companion_pause` — pause for N hours.
//! - `companion_resume` — undo the pause.
//! - `companion_configure` — partial update (quiet hours, channels,
//! safety contact).
//!
//! All six tools act on the caller's `_user_id` injected by the chat
//! handler. Admin-on-behalf-of-another-user actions go via the HTTP
//! admin endpoints (future slice), not these tools.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::companion::{CompanionSettings, CompanionSystem, CompanionUpdate};
use crate::companion::settings::{FrequencyMode, MessageMix, ToneAxes};
use crate::MiraError;

// ── Shared helpers ───────────────────────────────────────────────────────────

// Pull the trusted `_user_id` out of injected tool args. Cross-user
// access is impossible by construction — there's no `target_user_id`
// parameter accepted by any of these tools.
fn require_user_id(args: &ToolArgs, tool: &str) -> Result<String, ToolResult> {
    args.get("_user_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .ok_or_else(|| ToolResult::failure(format!(
            "{tool} called without _user_id (chat handler must inject)"
        )))
}

// Map an internal `CompanionError` to a tool failure with a
// model-friendly message. The Rust error chain is fine for logs but
// the model gets a cleaned-up string.
fn map_err(tool: &str, e: crate::companion::CompanionError) -> ToolResult {
    use crate::companion::CompanionError::*;
    match e {
        NotEnabled(uid) => ToolResult::failure(format!(
            "{tool}: companion mode is not enabled for user '{uid}' — \
             call `companion_enable` first"
        )),
        SetupIncomplete(missing) => ToolResult::failure(format!(
            "{tool}: setup is incomplete (missing: {missing}). Use \
             `companion_configure` to fill in the missing fields."
        )),
        UnknownSafetyContact(uid) => ToolResult::failure(format!(
            "{tool}: '{uid}' is not a known MIRA user — pick someone \
             with an account before enabling companion mode"
        )),
        SelfSafetyContact => ToolResult::failure(format!(
            "{tool}: a user cannot be their own safety contact — pick \
             someone else who can be notified if things look serious"
        )),
        Invalid(msg) => ToolResult::failure(format!("{tool}: {msg}")),
        e => ToolResult::failure(format!("{tool}: {e}")),
    }
}

// Shape the settings struct into a JSON payload the model can read.
fn settings_to_json(s: &CompanionSettings) -> Value {
    json!({
        "enabled": s.enabled,
        "paused": s.paused_until
            .map(|d| d > chrono::Utc::now())
            .unwrap_or(false),
        "paused_until_ms": s.paused_until.map(|d| d.timestamp_millis()),
        "quiet_hours": s.quiet_hours,
        "preferred_channels": s.preferred_channels,
        "safety_contact_user_id": s.safety_contact_user_id,
        // Per-user cadence overrides; null fields inherit the instance default.
        "cadence": {
            "max_per_day": s.cadence.max_per_day,
            "min_gap_minutes": s.cadence.min_gap_minutes,
            "max_unanswered_checkins": s.cadence.max_unanswered_checkins,
        },
        "setup_completed": s.setup_completed_at.is_some(),
        "setup_completed_at_ms": s.setup_completed_at.map(|d| d.timestamp_millis()),
        "created_at_ms": s.created_at.timestamp_millis(),
        "updated_at_ms": s.updated_at.timestamp_millis(),
    })
}

// ── companion_status ─────────────────────────────────────────────────────────

pub struct CompanionStatusTool {
    system: Arc<CompanionSystem>,
}

impl CompanionStatusTool {
    pub fn new(system: Arc<CompanionSystem>) -> Self { Self { system } }
}

#[async_trait]
impl Tool for CompanionStatusTool {
    fn name(&self) -> &str { "companion_status" }

    fn description(&self) -> &str {
        "Check whether companion mode is enabled for the caller and \
         report its current state — enabled / paused / setup-complete, \
         the configured safety contact, quiet hours, and preferred \
         channels. Returns `enabled: false` for users who never \
         enabled the mode (no error)."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        let payload = match self.system.get(&user_id) {
            Ok(Some(s))  => settings_to_json(&s),
            Ok(None)     => json!({ "enabled": false, "setup_completed": false }),
            Err(e)       => return Ok(map_err(self.name(), e)),
        };
        Ok(ToolResult::success(payload.to_string()))
    }
}

// ── companion_enable ─────────────────────────────────────────────────────────

pub struct CompanionEnableTool {
    system: Arc<CompanionSystem>,
}

impl CompanionEnableTool {
    pub fn new(system: Arc<CompanionSystem>) -> Self { Self { system } }
}

#[async_trait]
impl Tool for CompanionEnableTool {
    fn name(&self) -> &str { "companion_enable" }

    fn description(&self) -> &str {
        "Turn on companion mode for the caller. The mode biases me \
         toward warm conversational behaviour and seeds persona pages \
         into the user's wiki (style, routines, likes, family, \
         safety). A safety contact is REQUIRED for non-admin users — \
         the user id of another MIRA user who should be notified if \
         the safety floor triggers. Admin users may omit the safety \
         contact (e.g. for testing or self-managed setups); if omitted, \
         the safety floor still audit-logs distress events but won't \
         deliver outbound notices. Re-running this on an already-enabled \
         user refreshes the safety contact and preserves existing settings. \
         Check-in timing is derived automatically: quiet_hours falls out \
         of the user's onboarding contact-hour window when present, and \
         channel preference falls back to the user's last-used channel. \
         Do NOT ask the user for times or channels after a successful \
         enable — the returned settings JSON shows what was inferred, and \
         the user can override via `companion_configure` if needed."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "safety_contact_user_id": {
                    "type": "string",
                    "description":
                        "User id of another MIRA user who should be notified \
                         if the safety floor triggers (e.g. a family \
                         member). Must be an existing user; cannot be the \
                         caller themselves. REQUIRED for non-admin callers; \
                         optional for admins."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        let safety_raw = args.get("safety_contact_user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let safety: Option<&str> = if safety_raw.is_empty() { None } else { Some(&safety_raw) };

        // was strict: contact required for everyone. Relaxed
        // post-0.125 so admins can enable without configuring a
        // contact (testing / self-managed setups). The auth service
        // tells us the caller's role; tests / channel-only builds
        // without auth wired behave as the strict case (refuse
        // missing contact) — safer default.
        if safety.is_none() {
            let is_admin = self.system.auth()
                .and_then(|a| a.get_user(&user_id).ok().flatten())
                .map(|u| u.role == crate::auth::Role::Admin)
                .unwrap_or(false);
            if !is_admin {
                return Ok(ToolResult::failure(
                    "companion_enable: `safety_contact_user_id` is required \
                     for non-admin users. Ask an admin to enable companion \
                     mode for you, or provide a contact."
                ));
            }
        }
        match self.system.enable(&user_id, safety) {
            Ok(s)  => Ok(ToolResult::success(settings_to_json(&s).to_string())),
            Err(e) => Ok(map_err(self.name(), e)),
        }
    }
}

// ── companion_disable ────────────────────────────────────────────────────────

pub struct CompanionDisableTool {
    system: Arc<CompanionSystem>,
}

impl CompanionDisableTool {
    pub fn new(system: Arc<CompanionSystem>) -> Self { Self { system } }
}

#[async_trait]
impl Tool for CompanionDisableTool {
    fn name(&self) -> &str { "companion_disable" }

    fn description(&self) -> &str {
        "Turn off companion mode for the caller. Settings (safety \
         contact, quiet hours, channel preferences) are kept on disk \
         so re-enabling later restores them. Use when the user wants \
         to stop the check-ins and conversational bias entirely."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        match self.system.disable(&user_id) {
            Ok(()) => Ok(ToolResult::success(json!({ "disabled": true }).to_string())),
            Err(e) => Ok(map_err(self.name(), e)),
        }
    }
}

// ── companion_pause ──────────────────────────────────────────────────────────

pub struct CompanionPauseTool {
    system: Arc<CompanionSystem>,
}

impl CompanionPauseTool {
    pub fn new(system: Arc<CompanionSystem>) -> Self { Self { system } }
}

#[async_trait]
impl Tool for CompanionPauseTool {
    fn name(&self) -> &str { "companion_pause" }

    fn description(&self) -> &str {
        "Pause companion mode for a number of hours. The check-ins and \
         conversational bias stop until the pause expires or the user \
         calls `companion_resume`. Pass `hours = 0` for an indefinite \
         pause — the user must explicitly resume in that case. Use \
         when the user says \"give me a break\" or \"don't ping me \
         for a few days\"."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["hours"],
            "properties": {
                "hours": {
                    "type": "number",
                    "description":
                        "Hours from now until the pause expires. 0 means \
                         indefinite (user must explicitly resume)."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        let hours = args.get("hours").and_then(|v| v.as_f64()).unwrap_or(-1.0);
        if hours < 0.0 {
            return Ok(ToolResult::failure(
                "companion_pause: `hours` must be ≥ 0 (use 0 for indefinite)"
            ));
        }
        match self.system.pause(&user_id, hours) {
            Ok(s)  => Ok(ToolResult::success(settings_to_json(&s).to_string())),
            Err(e) => Ok(map_err(self.name(), e)),
        }
    }
}

// ── companion_resume ─────────────────────────────────────────────────────────

pub struct CompanionResumeTool {
    system: Arc<CompanionSystem>,
}

impl CompanionResumeTool {
    pub fn new(system: Arc<CompanionSystem>) -> Self { Self { system } }
}

#[async_trait]
impl Tool for CompanionResumeTool {
    fn name(&self) -> &str { "companion_resume" }

    fn description(&self) -> &str {
        "Clear the pause and resume companion mode immediately. \
         No-op if not paused."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        match self.system.resume(&user_id) {
            Ok(s)  => Ok(ToolResult::success(settings_to_json(&s).to_string())),
            Err(e) => Ok(map_err(self.name(), e)),
        }
    }
}

// ── companion_configure ──────────────────────────────────────────────────────

pub struct CompanionConfigureTool {
    system: Arc<CompanionSystem>,
}

impl CompanionConfigureTool {
    pub fn new(system: Arc<CompanionSystem>) -> Self { Self { system } }
}

#[async_trait]
impl Tool for CompanionConfigureTool {
    fn name(&self) -> &str { "companion_configure" }

    fn description(&self) -> &str {
        "Update one or more companion-mode (Presence) settings. Only the fields \
         you pass change; everything else is preserved. Use whenever the user \
         tunes how I reach out or how I sound, e.g.:\n\
         • \"message me less\" / \"more often\" → lower/raise the rhythm band \
           (min_per_day + max_per_day).\n\
         • \"only check in in the mornings\" → frequency_mode='scheduled' + \
           scheduled_times like [\"08:00\"].\n\
         • \"be funnier\" → raise tone.playfulness; \"keep it short\" → lower \
           tone.verbosity; \"be warmer\" → raise tone.warmth (each 0..=100).\n\
         • \"stop the jokes\" → message_mix.joke=false; \"don't tell me what \
           you've been up to\" → share_agent_activity=false.\n\
         • Plus the classics: quiet_hours, preferred_channels, safety_contact, \
           and the cadence guards (max_per_day, min_gap_minutes, \
           max_unanswered_checkins)."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "quiet_hours": {
                    "type": "array",
                    "items": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 2,
                        "items": { "type": "string" }
                    },
                    "description":
                        "Pairs of [start, end] in 'HH:MM' 24-hour format, \
                         e.g. [[\"22:00\",\"06:30\"]]. The companion will \
                         not initiate check-ins inside these windows."
                },
                "preferred_channels": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["signal","telegram","web","tui"] },
                    "description":
                        "Ordered list, most-preferred first. The scheduler \
                         uses the first reachable channel."
                },
                "safety_contact_user_id": {
                    "type": "string",
                    "description":
                        "User id of another MIRA user who should be \
                         notified if the safety floor triggers."
                },
                "max_per_day": {
                    "type": "integer",
                    "minimum": 0,
                    "description":
                        "Per-user override: max proactive check-ins per local \
                         day. Overrides the instance default."
                },
                "min_gap_minutes": {
                    "type": "integer",
                    "minimum": 0,
                    "description":
                        "Per-user override: minimum minutes between check-ins."
                },
                "max_unanswered_checkins": {
                    "type": "integer",
                    "minimum": 0,
                    "description":
                        "Per-user override: pause check-ins after this many go \
                         unanswered in a row (resets when the user replies; \
                         0 = no cap)."
                },
                "frequency_mode": {
                    "type": "string",
                    "enum": ["fuzzy", "scheduled"],
                    "description":
                        "'fuzzy' = a daily band placed at varied times (the \
                         default friend-not-alarm-clock rhythm); 'scheduled' = \
                         fire at the specific local times in `scheduled_times`."
                },
                "min_per_day": {
                    "type": "integer",
                    "minimum": 0,
                    "description":
                        "Fuzzy-mode lower bound on proactive messages per local \
                         day (the upper bound is `max_per_day`). Lower this for \
                         'message me less', raise it for 'more often'."
                },
                "scheduled_times": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description":
                        "Local 'HH:MM' times for scheduled mode, e.g. \
                         [\"08:00\",\"18:30\"]. Used only when \
                         frequency_mode='scheduled'."
                },
                "tone": {
                    "type": "object",
                    "properties": {
                        "warmth":      { "type": "integer", "minimum": 0, "maximum": 100 },
                        "playfulness": { "type": "integer", "minimum": 0, "maximum": 100 },
                        "verbosity":   { "type": "integer", "minimum": 0, "maximum": 100 }
                    },
                    "description":
                        "Personality sliders, each 0..=100 (50 = neutral). Raise \
                         playfulness for 'be funnier', lower verbosity for 'keep \
                         it short', raise warmth for 'be warmer'."
                },
                "message_mix": {
                    "type": "object",
                    "properties": {
                        "check_in":      { "type": "boolean" },
                        "joke":          { "type": "boolean" },
                        "status_update": { "type": "boolean" },
                        "follow_up":     { "type": "boolean" },
                        "share":         { "type": "boolean" },
                        "encouragement": { "type": "boolean" }
                    },
                    "description":
                        "Which kinds of proactive message I may send. Set \
                         joke=false for 'stop the jokes', etc."
                },
                "share_agent_activity": {
                    "type": "boolean",
                    "description":
                        "Whether to include 'here's what I've been up to' \
                         updates drawn from my autonomous agents / automations."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };

        let quiet_hours: Option<Vec<(String, String)>> = args.get("quiet_hours")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|p| {
                let p = p.as_array()?;
                let start = p.get(0)?.as_str()?.to_string();
                let end   = p.get(1)?.as_str()?.to_string();
                Some((start, end))
            }).collect());

        let preferred_channels: Option<Vec<String>> = args.get("preferred_channels")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|c| c.as_str().map(String::from)).collect());

        let safety_contact_user_id: Option<String> = args.get("safety_contact_user_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string());

        let max_per_day: Option<u32> = args.get("max_per_day")
            .and_then(|v| v.as_u64()).map(|n| n as u32);
        let min_gap_minutes: Option<i64> = args.get("min_gap_minutes")
            .and_then(|v| v.as_i64());
        let max_unanswered_checkins: Option<u32> = args.get("max_unanswered_checkins")
            .and_then(|v| v.as_u64()).map(|n| n as u32);

        // ── Presence (rhythm + personality) fields ──
        let frequency_mode: Option<FrequencyMode> = match args.get("frequency_mode")
            .and_then(|v| v.as_str())
        {
            Some("fuzzy")     => Some(FrequencyMode::Fuzzy),
            Some("scheduled") => Some(FrequencyMode::Scheduled),
            Some(other)       => return Ok(ToolResult::failure(format!(
                "companion_configure: frequency_mode must be 'fuzzy' or 'scheduled', got '{other}'"
            ))),
            None              => None,
        };
        let min_per_day: Option<u32> = args.get("min_per_day")
            .and_then(|v| v.as_u64()).map(|n| n as u32);
        let share_agent_activity: Option<bool> = args.get("share_agent_activity")
            .and_then(|v| v.as_bool());

        // scheduled_times — validate every entry is "HH:MM".
        let scheduled_times: Option<Vec<String>> = match args.get("scheduled_times") {
            None => None,
            Some(v) => {
                let Some(arr) = v.as_array() else {
                    return Ok(ToolResult::failure(
                        "companion_configure: scheduled_times must be an array of 'HH:MM' strings"
                    ));
                };
                let mut out = Vec::with_capacity(arr.len());
                for t in arr {
                    let Some(s) = t.as_str() else {
                        return Ok(ToolResult::failure(
                            "companion_configure: scheduled_times entries must be strings"
                        ));
                    };
                    if !valid_hhmm(s) {
                        return Ok(ToolResult::failure(format!(
                            "companion_configure: scheduled time '{s}' must be 'HH:MM' (24h)"
                        )));
                    }
                    out.push(s.to_string());
                }
                Some(out)
            }
        };

        // tone — object {warmth, playfulness, verbosity}, each 0..=100.
        let tone: Option<ToneAxes> = match args.get("tone") {
            None => None,
            Some(v) => {
                let Some(obj) = v.as_object() else {
                    return Ok(ToolResult::failure(
                        "companion_configure: tone must be an object {warmth, playfulness, verbosity}"
                    ));
                };
                let axis = |k: &str| obj.get(k).and_then(|x| x.as_u64());
                let warmth      = axis("warmth").unwrap_or(50);
                let playfulness = axis("playfulness").unwrap_or(50);
                let verbosity   = axis("verbosity").unwrap_or(50);
                if warmth > 100 || playfulness > 100 || verbosity > 100 {
                    return Ok(ToolResult::failure(
                        "companion_configure: tone axes must each be 0..=100"
                    ));
                }
                Some(ToneAxes {
                    warmth: warmth as u8,
                    playfulness: playfulness as u8,
                    verbosity: verbosity as u8,
                })
            }
        };

        // message_mix — object of the six booleans. Missing keys keep the
        // current value (read-modify-write below).
        let message_mix_obj = args.get("message_mix")
            .and_then(|v| v.as_object())
            .cloned();
        let message_mix_present = args.get("message_mix").is_some();
        if message_mix_present && message_mix_obj.is_none() {
            return Ok(ToolResult::failure(
                "companion_configure: message_mix must be an object of booleans"
            ));
        }

        let presence_touched = frequency_mode.is_some() || min_per_day.is_some()
            || scheduled_times.is_some() || tone.is_some()
            || message_mix_obj.is_some() || share_agent_activity.is_some();

        // Refuse a no-op call so the model gets useful feedback.
        if quiet_hours.is_none() && preferred_channels.is_none()
            && safety_contact_user_id.is_none() && max_per_day.is_none()
            && min_gap_minutes.is_none() && max_unanswered_checkins.is_none()
            && !presence_touched
        {
            return Ok(ToolResult::failure(
                "companion_configure: pass at least one of quiet_hours, \
                 preferred_channels, safety_contact_user_id, max_per_day, \
                 min_gap_minutes, max_unanswered_checkins, frequency_mode, \
                 min_per_day, scheduled_times, tone, message_mix, \
                 share_agent_activity"
            ));
        }

        // Band sanity: an explicit min must not exceed an explicit max.
        if let (Some(mn), Some(mx)) = (min_per_day, max_per_day) {
            if mn > mx {
                return Ok(ToolResult::failure(
                    "companion_configure: min_per_day cannot exceed max_per_day"
                ));
            }
        }

        let legacy_touched = quiet_hours.is_some() || preferred_channels.is_some()
            || safety_contact_user_id.is_some() || max_per_day.is_some()
            || min_gap_minutes.is_some() || max_unanswered_checkins.is_some();

        // Apply the legacy (quiet_hours / channels / safety / cadence) fields
        // via the facade — it validates the safety contact and refuses on a
        // never-enabled user, preserving the prior behaviour.
        if legacy_touched {
            let update = CompanionUpdate {
                quiet_hours,
                preferred_channels,
                safety_contact_user_id,
                max_per_day,
                min_gap_minutes,
                max_unanswered_checkins,
            };
            if let Err(e) = self.system.configure(&user_id, update) {
                return Ok(map_err(self.name(), e));
            }
        }

        // Apply the Presence fields with a read-modify-write on the store.
        // Creates a fresh DISABLED row if the user has none yet (so the model
        // can pre-tune before enabling), mirroring the HTTP PUT path.
        if presence_touched {
            let store = self.system.store();
            let now = chrono::Utc::now();
            let mut s = match store.get(&user_id) {
                Ok(Some(s)) => s,
                Ok(None) => CompanionSettings {
                    user_id: user_id.clone(),
                    enabled: false,
                    paused_until: None,
                    quiet_hours: vec![],
                    preferred_channels: vec![],
                    safety_contact_user_id: None,
                    setup_completed_at: None,
                    last_checkin_at: None,
                    consecutive_missed_checkins: 0,
                    daily_briefing_enabled: false,
                    daily_briefing_hour: 7,
                    last_briefing_at: None,
                    cadence: Default::default(),
                    presence: Default::default(),
                    created_at: now,
                    updated_at: now,
                },
                Err(e) => return Ok(ToolResult::failure(format!(
                    "companion_configure: load failed: {e}"
                ))),
            };
            if let Some(v) = frequency_mode       { s.presence.frequency_mode = v; }
            if let Some(v) = min_per_day          { s.presence.min_per_day = v; }
            if let Some(v) = scheduled_times      { s.presence.scheduled_times = v; }
            if let Some(v) = tone                 { s.presence.tone = v; }
            if let Some(v) = share_agent_activity { s.presence.share_agent_activity = v; }
            if let Some(obj) = message_mix_obj {
                let b = |k: &str, cur: bool| obj.get(k).and_then(|x| x.as_bool()).unwrap_or(cur);
                let mix = MessageMix {
                    check_in:      b("check_in",      s.presence.message_mix.check_in),
                    joke:          b("joke",          s.presence.message_mix.joke),
                    status_update: b("status_update", s.presence.message_mix.status_update),
                    follow_up:     b("follow_up",     s.presence.message_mix.follow_up),
                    share:         b("share",         s.presence.message_mix.share),
                    encouragement: b("encouragement", s.presence.message_mix.encouragement),
                };
                s.presence.message_mix = mix;
            }
            s.updated_at = now;
            if let Err(e) = store.upsert(&s) {
                return Ok(ToolResult::failure(format!(
                    "companion_configure: save failed: {e}"
                )));
            }
        }

        // Return the freshest settings snapshot.
        match self.system.get(&user_id) {
            Ok(Some(s)) => Ok(ToolResult::success(settings_to_json(&s).to_string())),
            Ok(None)    => Ok(ToolResult::success(json!({
                "enabled": false, "setup_completed": false
            }).to_string())),
            Err(e)      => Ok(map_err(self.name(), e)),
        }
    }
}

/// Minimal "HH:MM" (24h) validator for scheduled-mode times.
fn valid_hhmm(s: &str) -> bool {
    let (h, m) = match s.split_once(':') { Some(p) => p, None => return false };
    matches!((h.parse::<u8>(), m.parse::<u8>()), (Ok(h), Ok(m)) if h <= 23 && m <= 59)
        && h.len() == 2 && m.len() == 2
}

// ── companion_briefing_set ───────────────────────────────────────────────────
//
// Q1.6 follow-up — the Daily Briefing feature has a per-user
// enabled/hour toggle that previously was only reachable via the HTTP
// settings panel. Without an agent tool, when the user said
// "turn on daily briefing" the model fell back to building cron-based
// automations (badly: it tripped over a cron-parser quirk + duped
// the call four times). This tool exposes the same toggle so a
// chat-driven setup just works.

pub struct CompanionBriefingSetTool {
    system: Arc<CompanionSystem>,
}

impl CompanionBriefingSetTool {
    pub fn new(system: Arc<CompanionSystem>) -> Self { Self { system } }
}

#[async_trait]
impl Tool for CompanionBriefingSetTool {
    fn name(&self) -> &str { "companion_briefing_set" }

    fn description(&self) -> &str {
        "Turn the Daily Briefing on/off and choose the local hour it \
         fires at. Each morning at that hour MIRA pulls together today's \
         calendar, tomorrow's preview, recent wiki updates, and \
         yesterday's automation runs, writes a warm summary in your \
         voice, and delivers it via your companion channel \
         (Signal / Telegram / web). Use this whenever the user asks \
         about morning briefings, daily check-ins, or 'wake me up \
         with a summary' — do NOT build a custom cron-based automation \
         for this; the briefing has its own scheduler that handles \
         tz / dedup / channel routing correctly. Requires companion \
         mode to be enabled first."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "enabled": {
                    "type": "boolean",
                    "description": "True to turn the briefing on; false to turn it off."
                },
                "hour": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 23,
                    "description": "Local hour (0-23) the briefing fires at. Defaults to 7 (07:00 local) on first enable."
                }
            },
            "anyOf": [
                { "required": ["enabled"] },
                { "required": ["hour"] }
            ]
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s)  => s,
            Err(r) => return Ok(r),
        };

        let enabled = args.get("enabled").and_then(|v| v.as_bool());
        let hour    = args.get("hour").and_then(|v| v.as_i64());

        if enabled.is_none() && hour.is_none() {
            return Ok(ToolResult::failure(
                "companion_briefing_set: pass at least one of `enabled` or `hour`",
            ));
        }
        if let Some(h) = hour {
            if !(0..=23).contains(&h) {
                return Ok(ToolResult::failure(
                    "companion_briefing_set: `hour` must be 0..=23",
                ));
            }
        }

        let store = self.system.store();
        let mut s = match store.get(&user_id) {
            Ok(Some(s)) => s,
            Ok(None) => return Ok(ToolResult::failure(
                "companion_briefing_set: companion mode is not enabled for this user. \
                 Call companion_enable first."
            )),
            Err(e) => return Ok(ToolResult::failure(format!(
                "companion_briefing_set: get_settings failed: {e}"
            ))),
        };
        if let Some(en) = enabled { s.daily_briefing_enabled = en; }
        if let Some(h)  = hour    { s.daily_briefing_hour = h as u8; }
        s.updated_at = chrono::Utc::now();

        if let Err(e) = store.upsert(&s) {
            return Ok(ToolResult::failure(format!(
                "companion_briefing_set: upsert failed: {e}"
            )));
        }
        Ok(ToolResult::success(
            json!({
                "enabled":              s.daily_briefing_enabled,
                "hour":                 s.daily_briefing_hour,
                "last_briefing_at":     s.last_briefing_at.map(|d| d.timestamp_millis()),
                "preferred_channels":   s.preferred_channels,
            }).to_string(),
        ))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::CompanionSystem;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn fresh_system() -> (tempfile::TempDir, Arc<CompanionSystem>) {
        let dir = tempdir().unwrap();
        let sys = Arc::new(CompanionSystem::open(dir.path()).unwrap());
        (dir, sys)
    }

    #[tokio::test]
    async fn status_returns_disabled_for_unknown_user() {
        let (_dir, sys) = fresh_system();
        let tool = CompanionStatusTool::new(sys);
        let r = tool.execute(json!({"_user_id": "ghost"})).await.unwrap();
        assert!(r.success);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["setup_completed"], false);
    }

    #[tokio::test]
    async fn status_requires_user_id() {
        let (_dir, sys) = fresh_system();
        let tool = CompanionStatusTool::new(sys);
        let r = tool.execute(json!({})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("_user_id"));
    }

    #[tokio::test]
    async fn enable_with_safety_contact_succeeds() {
        let (_dir, sys) = fresh_system();
        let tool = CompanionEnableTool::new(sys);
        let r = tool.execute(json!({
            "_user_id": "alice",
            "safety_contact_user_id": "david",
        })).await.unwrap();
        assert!(r.success, "{:?}", r.error);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["safety_contact_user_id"], "david");
        assert_eq!(v["setup_completed"], true);
    }

    #[tokio::test]
    async fn enable_refuses_missing_safety_contact_for_non_admin() {
        // No auth wired → the role check falls through to the
        // "treat as non-admin" branch, which still requires a
        // contact. This is the safe default for tests / minimal
        // builds — covered admins explicitly enabling the no-contact
        // path live in the auth-wired integration test below.
        let (_dir, sys) = fresh_system();
        let tool = CompanionEnableTool::new(sys);
        let r = tool.execute(json!({"_user_id": "alice"})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("safety_contact_user_id"));
    }

    fn wire_auth_with_user(role: crate::auth::Role)
        -> (tempfile::TempDir, std::sync::Arc<crate::auth::LocalAuthService>, String)
    {
        use std::sync::Arc;
        use crate::auth::{LocalAuthService, NewUser};
        let dir = tempfile::tempdir().unwrap();
        let auth = Arc::new(LocalAuthService::new(
            &dir.path().join("auth.db"),
            "test-secret".into(),
            7,
        ).unwrap());
        let username = match role {
            crate::auth::Role::Admin => "admin",
            crate::auth::Role::User  => "user1",
        };
        let user = auth.create_user(NewUser {
            username: username.into(),
            display_name: None,
            email: None,
            password: "test-password-1234".into(),
            role,
        }).unwrap();
        (dir, auth, user.id)
    }

    #[tokio::test]
    async fn enable_without_contact_allowed_for_admin() {
        use std::sync::Arc;
        let (dir, auth, admin_id) = wire_auth_with_user(crate::auth::Role::Admin);
        let sys = Arc::new(
            crate::companion::CompanionSystem::open(dir.path()).unwrap()
                .with_auth(auth)
        );
        let tool = CompanionEnableTool::new(sys);
        let r = tool.execute(json!({"_user_id": admin_id})).await.unwrap();
        assert!(r.success, "admin should be allowed to enable without a contact; got: {:?}", r.error);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["enabled"], true);
        assert!(v["safety_contact_user_id"].is_null());
    }

    #[tokio::test]
    async fn enable_without_contact_refused_for_non_admin_with_auth_wired() {
        // Symmetric to the admin test: a real auth row with role=User
        // → still refused. Ensures we don't accidentally fall through
        // to "admin treatment" when the role lookup says otherwise.
        use std::sync::Arc;
        let (dir, auth, user_id) = wire_auth_with_user(crate::auth::Role::User);
        let sys = Arc::new(
            crate::companion::CompanionSystem::open(dir.path()).unwrap()
                .with_auth(auth)
        );
        let tool = CompanionEnableTool::new(sys);
        let r = tool.execute(json!({"_user_id": user_id})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("non-admin"));
    }

    #[tokio::test]
    async fn enable_refuses_self_as_safety_contact() {
        let (_dir, sys) = fresh_system();
        let tool = CompanionEnableTool::new(sys);
        let r = tool.execute(json!({
            "_user_id": "alice", "safety_contact_user_id": "alice",
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("own safety contact"));
    }

    #[tokio::test]
    async fn full_lifecycle_via_tools() {
        let (_dir, sys) = fresh_system();
        let enable = CompanionEnableTool::new(Arc::clone(&sys));
        let status = CompanionStatusTool::new(Arc::clone(&sys));
        let pause  = CompanionPauseTool::new(Arc::clone(&sys));
        let resume = CompanionResumeTool::new(Arc::clone(&sys));
        let configure = CompanionConfigureTool::new(Arc::clone(&sys));
        let disable = CompanionDisableTool::new(Arc::clone(&sys));

        // Enable
        let r = enable.execute(json!({"_user_id":"u","safety_contact_user_id":"c"})).await.unwrap();
        assert!(r.success);

        // Pause for 2h
        let r = pause.execute(json!({"_user_id":"u","hours":2.0})).await.unwrap();
        assert!(r.success);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["paused"], true);

        // Resume
        let r = resume.execute(json!({"_user_id":"u"})).await.unwrap();
        assert!(r.success);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["paused"], false);

        // Configure: quiet hours + preferred channels
        let r = configure.execute(json!({
            "_user_id": "u",
            "quiet_hours": [["22:00","06:30"]],
            "preferred_channels": ["signal","web"],
        })).await.unwrap();
        assert!(r.success);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["quiet_hours"][0][0], "22:00");
        assert_eq!(v["preferred_channels"][0], "signal");

        // Status
        let r = status.execute(json!({"_user_id":"u"})).await.unwrap();
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["enabled"], true);

        // Disable
        let r = disable.execute(json!({"_user_id":"u"})).await.unwrap();
        assert!(r.success);
        let r = status.execute(json!({"_user_id":"u"})).await.unwrap();
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["enabled"], false);
    }

    #[tokio::test]
    async fn configure_refuses_empty_call() {
        let (_dir, sys) = fresh_system();
        let enable = CompanionEnableTool::new(Arc::clone(&sys));
        let configure = CompanionConfigureTool::new(Arc::clone(&sys));
        enable.execute(json!({"_user_id":"u","safety_contact_user_id":"c"})).await.unwrap();
        let r = configure.execute(json!({"_user_id":"u"})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("at least one"));
    }

    #[tokio::test]
    async fn pause_rejects_negative_hours() {
        let (_dir, sys) = fresh_system();
        let enable = CompanionEnableTool::new(Arc::clone(&sys));
        let pause = CompanionPauseTool::new(Arc::clone(&sys));
        enable.execute(json!({"_user_id":"u","safety_contact_user_id":"c"})).await.unwrap();
        let r = pause.execute(json!({"_user_id":"u","hours":-1.0})).await.unwrap();
        assert!(!r.success);
    }

    #[tokio::test]
    async fn configure_applies_presence_fields() {
        let (_dir, sys) = fresh_system();
        let configure = CompanionConfigureTool::new(Arc::clone(&sys));

        // No prior row — presence-only configure creates a disabled row and
        // applies the tuning ("be funnier" / "stop the jokes" / "mornings").
        let r = configure.execute(json!({
            "_user_id": "u",
            "frequency_mode": "scheduled",
            "min_per_day": 2,
            "scheduled_times": ["08:00", "18:30"],
            "tone": { "warmth": 70, "playfulness": 90, "verbosity": 20 },
            "message_mix": { "joke": true, "status_update": false },
            "share_agent_activity": false,
        })).await.unwrap();
        assert!(r.success, "{:?}", r.error);

        let s = sys.get("u").unwrap().expect("row created");
        assert!(!s.enabled, "presence-only configure must not enable companion");
        assert_eq!(s.presence.frequency_mode, FrequencyMode::Scheduled);
        assert_eq!(s.presence.min_per_day, 2);
        assert_eq!(s.presence.scheduled_times, vec!["08:00".to_string(), "18:30".to_string()]);
        assert_eq!(s.presence.tone.playfulness, 90);
        assert_eq!(s.presence.tone.verbosity, 20);
        assert!(s.presence.message_mix.joke);
        assert!(!s.presence.message_mix.status_update);
        // Untouched mix keys keep their defaults.
        assert!(s.presence.message_mix.check_in);
        assert!(!s.presence.share_agent_activity);
    }

    #[tokio::test]
    async fn configure_rejects_bad_scheduled_time_and_tone() {
        let (_dir, sys) = fresh_system();
        let configure = CompanionConfigureTool::new(Arc::clone(&sys));

        let r = configure.execute(json!({
            "_user_id": "u", "scheduled_times": ["8:00"],  // not HH:MM
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("HH:MM"));

        let r = configure.execute(json!({
            "_user_id": "u", "tone": { "warmth": 150 },
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("0..=100"));

        let r = configure.execute(json!({
            "_user_id": "u", "frequency_mode": "weekly",
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("fuzzy"));
    }
}
