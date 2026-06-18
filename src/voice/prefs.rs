// SPDX-License-Identifier: AGPL-3.0-or-later

// src/voice/prefs.rs
//! Per-channel voice preferences — response policy and voice id, with a
//! layered resolver that merges user overrides over server defaults.
//!
//! The shape is intentionally a `HashMap<String, ChannelVoicePrefs>` rather
//! than columns/fields per known channel so plugin channels work out of the
//! box. Missing keys (or null fields) inherit the next layer down.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// When the agent should reply with synthesized voice on a given channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePolicy {
    /// Always reply with voice (when the channel and routed backend can).
    Always,
    /// Reply with voice only when the inbound user message was itself voice.
    /// Channels without a notion of "inbound voice" treat this as `Never`.
    OnVoiceInput,
    /// Never reply with voice.
    Never,
}

impl ResponsePolicy {
    /// Built-in fallback when both user and server defaults are unset.
    /// `Never` is the safest default — voice replies are opt-in.
    pub const fn default_value() -> Self { ResponsePolicy::Never }

    pub fn as_str(self) -> &'static str {
        match self {
            ResponsePolicy::Always       => "always",
            ResponsePolicy::OnVoiceInput => "on_voice_input",
            ResponsePolicy::Never        => "never",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelVoicePrefs {
    /// `None` means "inherit from the next layer down."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_policy: Option<ResponsePolicy>,
    /// Backend voice id override. `None` or empty = inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_id: Option<String>,
}

/// Map of channel id → prefs. Missing channel keys inherit fully.
pub type VoicePrefsMap = HashMap<String, ChannelVoicePrefs>;

/// Resolved per-call voice config for one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVoice {
    pub policy:   ResponsePolicy,
    pub voice_id: Option<String>,
}

/// Layered resolution: user → server default → built-in (Never / no voice).
pub fn resolve_voice(
    channel:         &str,
    user_prefs:      Option<&VoicePrefsMap>,
    server_defaults: &VoicePrefsMap,
) -> ResolvedVoice {
    let user = user_prefs.and_then(|m| m.get(channel));
    let srv  = server_defaults.get(channel);

    let policy = user.and_then(|u| u.response_policy)
        .or_else(|| srv.and_then(|s| s.response_policy))
        .unwrap_or_else(ResponsePolicy::default_value);

    let voice_id = user.and_then(|u| u.voice_id.clone()).filter(|s| !s.is_empty())
        .or_else(|| srv.and_then(|s| s.voice_id.clone()).filter(|s| !s.is_empty()));

    ResolvedVoice { policy, voice_id }
}

/// Parse the JSON blob stored on `users.voice_prefs`. Empty/null/malformed
/// returns an empty map — voice prefs are best-effort, never load-bearing.
pub fn parse_user_prefs(json: Option<&str>) -> VoicePrefsMap {
    let raw = match json.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None    => return VoicePrefsMap::default(),
    };
    serde_json::from_str::<VoicePrefsMap>(raw).unwrap_or_default()
}

/// Sanitise a user-supplied prefs map before persisting:
///   * drop entries whose policy is None AND voice_id is None/empty (full
///     inherit — no point storing them);
///   * trim voice ids and turn empty strings into `None`.
pub fn normalise(mut map: VoicePrefsMap) -> VoicePrefsMap {
    map.retain(|_, v| {
        if let Some(ref s) = v.voice_id {
            if s.trim().is_empty() {
                v.voice_id = None;
            } else {
                v.voice_id = Some(s.trim().to_string());
            }
        }
        v.response_policy.is_some() || v.voice_id.is_some()
    });
    map
}

/// Serialize a normalised map for storage. Empty map → `None` so the column
/// stays NULL (cleanest representation of "all inherit").
pub fn to_storage_json(map: &VoicePrefsMap) -> Option<String> {
    if map.is_empty() { return None; }
    serde_json::to_string(map).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srv_defaults() -> VoicePrefsMap {
        let mut m = VoicePrefsMap::new();
        m.insert("telegram".into(), ChannelVoicePrefs {
            response_policy: Some(ResponsePolicy::OnVoiceInput),
            voice_id:        Some("alloy".into()),
        });
        m.insert("web".into(), ChannelVoicePrefs {
            response_policy: Some(ResponsePolicy::Never),
            voice_id:        None,
        });
        m
    }

    #[test]
    fn empty_user_prefs_inherits_server_defaults() {
        let server = srv_defaults();
        let r = resolve_voice("telegram", None, &server);
        assert_eq!(r.policy, ResponsePolicy::OnVoiceInput);
        assert_eq!(r.voice_id.as_deref(), Some("alloy"));
    }

    #[test]
    fn user_pref_overrides_server_default() {
        let server = srv_defaults();
        let mut user = VoicePrefsMap::new();
        user.insert("telegram".into(), ChannelVoicePrefs {
            response_policy: Some(ResponsePolicy::Always),
            voice_id:        Some("nova".into()),
        });
        let r = resolve_voice("telegram", Some(&user), &server);
        assert_eq!(r.policy, ResponsePolicy::Always);
        assert_eq!(r.voice_id.as_deref(), Some("nova"));
    }

    #[test]
    fn user_partial_inherits_voice_id() {
        let server = srv_defaults();
        let mut user = VoicePrefsMap::new();
        user.insert("telegram".into(), ChannelVoicePrefs {
            response_policy: Some(ResponsePolicy::Always),
            voice_id:        None, // inherit
        });
        let r = resolve_voice("telegram", Some(&user), &server);
        assert_eq!(r.policy, ResponsePolicy::Always);
        assert_eq!(r.voice_id.as_deref(), Some("alloy"));
    }

    #[test]
    fn unknown_channel_falls_back_to_never() {
        let server = srv_defaults();
        let r = resolve_voice("discord", None, &server);
        assert_eq!(r.policy, ResponsePolicy::Never);
        assert!(r.voice_id.is_none());
    }

    #[test]
    fn parse_handles_null_and_garbage() {
        assert!(parse_user_prefs(None).is_empty());
        assert!(parse_user_prefs(Some("")).is_empty());
        assert!(parse_user_prefs(Some("not json")).is_empty());
        let m = parse_user_prefs(Some(r#"{"web":{"response_policy":"always"}}"#));
        assert_eq!(m["web"].response_policy, Some(ResponsePolicy::Always));
    }

    #[test]
    fn normalise_drops_empty_entries() {
        let mut m = VoicePrefsMap::new();
        m.insert("web".into(), ChannelVoicePrefs::default());
        m.insert("tui".into(), ChannelVoicePrefs {
            response_policy: None,
            voice_id:        Some("   ".into()),
        });
        m.insert("signal".into(), ChannelVoicePrefs {
            response_policy: Some(ResponsePolicy::Always),
            voice_id:        None,
        });
        let n = normalise(m);
        assert!(!n.contains_key("web"));
        assert!(!n.contains_key("tui"));
        assert!(n.contains_key("signal"));
    }

    #[test]
    fn to_storage_json_round_trips() {
        let mut m = VoicePrefsMap::new();
        m.insert("telegram".into(), ChannelVoicePrefs {
            response_policy: Some(ResponsePolicy::OnVoiceInput),
            voice_id:        Some("alloy".into()),
        });
        let s = to_storage_json(&m).unwrap();
        let parsed = parse_user_prefs(Some(&s));
        assert_eq!(parsed, m);
    }

    #[test]
    fn empty_map_serialises_to_none() {
        let m = VoicePrefsMap::new();
        assert!(to_storage_json(&m).is_none());
    }
}
