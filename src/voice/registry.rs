// SPDX-License-Identifier: AGPL-3.0-or-later

// src/voice/registry.rs
//! Channel registry — descriptors for every channel the server knows about.
//!
//! The registry is built with the four built-in channels (`web`, `tui`,
//! `telegram`, `signal`) and exposes `register()` so a plugin loader can
//! contribute new entries at runtime. UI grids (Profile dialog, admin
//! Settings) iterate over `list()` so adding a channel never requires a
//! frontend or schema change.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ChannelDescriptor {
    /// Stable channel id used as the key in voice prefs and routing maps.
    pub id:             String,
    /// Human label shown in settings UI.
    pub display_name:   String,
    /// Whether this channel can deliver synthesized audio. UI hides voice
    /// controls for channels that can't.
    pub supports_voice: bool,
}

#[derive(Clone)]
pub struct ChannelRegistry {
    inner: Arc<RwLock<BTreeMap<String, ChannelDescriptor>>>,
}

impl ChannelRegistry {
    /// Registry seeded with the four built-in channels in canonical order.
    pub fn builtin() -> Self {
        let mut map: BTreeMap<String, ChannelDescriptor> = BTreeMap::new();
        for d in [
            ChannelDescriptor {
                id: "web".into(),
                display_name: "Web chat".into(),
                supports_voice: true,
            },
            ChannelDescriptor {
                id: "tui".into(),
                display_name: "Terminal".into(),
                supports_voice: true,
            },
            ChannelDescriptor {
                id: "telegram".into(),
                display_name: "Telegram".into(),
                supports_voice: true,
            },
            ChannelDescriptor {
                id: "signal".into(),
                display_name: "Signal".into(),
                supports_voice: true,
            },
            ChannelDescriptor {
                id: "mobile".into(),
                display_name: "Mobile app".into(),
                // The native mobile app plays synthesized audio inline (it uses
                // the same /api/tts endpoints as web). → true.
                supports_voice: true,
            },
            // The newer channels. `supports_voice` is HONEST per channel —
            // the UI hides voice controls when it's false, so these don't
            // show a toggle that would silently do nothing. Only channels
            // whose transport actually delivers synthesized audio are true.
            ChannelDescriptor {
                id: "discord".into(),
                display_name: "Discord".into(),
                // Discord bots have no voice-message API (voice channels are
                // a separate RTP protocol); replies are text. → false.
                supports_voice: false,
            },
            ChannelDescriptor {
                id: "matrix".into(),
                display_name: "Matrix".into(),
                // Would need the media repo + (usually) E2EE, which we don't
                // do yet. Text-only today. → false.
                supports_voice: false,
            },
            ChannelDescriptor {
                id: "whatsapp".into(),
                display_name: "WhatsApp".into(),
                // Audio needs Meta's media upload + the 24h window; not wired.
                // Text-only today. → false.
                supports_voice: false,
            },
            ChannelDescriptor {
                id: "slack".into(),
                display_name: "Slack".into(),
                // No native voice-message type; would be a file upload, not a
                // voice note. Text-only today. → false.
                supports_voice: false,
            },
            ChannelDescriptor {
                id: "email".into(),
                display_name: "Email".into(),
                // Inherently text (could attach audio, but that's not a
                // "voice note"). → false.
                supports_voice: false,
            },
        ] {
            map.insert(d.id.clone(), d);
        }
        Self { inner: Arc::new(RwLock::new(map)) }
    }

    /// Insert or replace a descriptor. Plugin loaders call this during
    /// channel start-up before the HTTP router is built.
    pub fn register(&self, d: ChannelDescriptor) {
        if let Ok(mut g) = self.inner.write() {
            g.insert(d.id.clone(), d);
        }
    }

    pub fn list(&self) -> Vec<ChannelDescriptor> {
        match self.inner.read() {
            Ok(g)  => g.values().cloned().collect(),
            Err(p) => p.into_inner().values().cloned().collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<ChannelDescriptor> {
        self.inner.read().ok().and_then(|g| g.get(id).cloned())
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self { Self::builtin() }
}

impl std::fmt::Debug for ChannelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelRegistry")
            .field("channels", &self.list())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_lists_all_channels_in_btree_order() {
        let r = ChannelRegistry::builtin();
        let ids: Vec<String> = r.list().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec![
            "discord", "email", "matrix", "mobile", "signal",
            "slack", "telegram", "tui", "web", "whatsapp",
        ]); // BTreeMap order
    }

    #[test]
    fn voice_capability_is_honest_per_channel() {
        let r = ChannelRegistry::builtin();
        // Channels that actually deliver synthesized audio today.
        for id in ["web", "tui", "telegram", "signal", "mobile"] {
            assert!(r.get(id).unwrap().supports_voice, "{id} should support voice");
        }
        // Text-only channels — the UI must not show a no-op voice toggle.
        for id in ["discord", "matrix", "whatsapp", "slack", "email"] {
            assert!(!r.get(id).unwrap().supports_voice, "{id} should be text-only");
        }
    }

    #[test]
    fn register_adds_new_descriptor() {
        let r = ChannelRegistry::builtin();
        r.register(ChannelDescriptor {
            id: "discord".into(),
            display_name: "Discord".into(),
            supports_voice: true,
        });
        assert!(r.get("discord").is_some());
    }

    #[test]
    fn register_replaces_existing() {
        let r = ChannelRegistry::builtin();
        r.register(ChannelDescriptor {
            id: "web".into(),
            display_name: "Browser".into(),
            supports_voice: false,
        });
        let d = r.get("web").unwrap();
        assert_eq!(d.display_name, "Browser");
        assert!(!d.supports_voice);
    }
}
