// SPDX-License-Identifier: AGPL-3.0-or-later

// src/health/degradation.rs
//! Tracks subsystems that have silently fallen back to a degraded path so the
//! condition can be **surfaced**, not just logged. Two consumers:
//!
//! 1. **A notification** (toast) fires the moment a fallback happens — so an
//!    operator sees "voice synthesis fell back to Piper" live.
//! 2. **A health detector** ([`crate::health::detectors`]) reads the live state
//!    so the System Health page shows a degraded indicator.
//!
//! Degradations come in two flavours:
//! - **persistent** — a startup/config condition that holds until restart or
//!   recovery (e.g. the embedding server was unreachable at boot, the reasoning
//!   provider failed to build). Always "active".
//! - **transient** — a per-request fallback (a TTS/STT call degraded). Counts +
//!   timestamps accumulate; treated as "active" only if seen within a recent
//!   window (the primary may work on the next call).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::notifications::{Notification, NotificationBus, NotificationKind};

/// Transient degradations seen within this many seconds count as "active".
pub const TRANSIENT_WINDOW_SECS: i64 = 3600; // 1 hour

/// One subsystem's current degradation state.
#[derive(Debug, Clone, Serialize)]
pub struct Degradation {
    /// Stable key, e.g. `"tts"`, `"stt"`, `"embeddings"`, `"reasoning"`.
    pub subsystem: String,
    /// Human label, e.g. `"Voice synthesis (TTS)"`.
    pub label: String,
    /// What was configured / requested and failed.
    pub from: String,
    /// What actually served instead.
    pub to: String,
    /// Short reason the primary failed.
    pub reason: String,
    /// Startup/config condition (sticky) vs per-request (windowed).
    pub persistent: bool,
    pub first_at: i64,
    pub last_at: i64,
    pub count: u64,
}

/// Records and surfaces subsystem fallbacks. Cheap to clone (`Arc` it).
pub struct DegradationTracker {
    state: Mutex<HashMap<String, Degradation>>,
    /// Attached once the bus exists (startup degradations are recorded before
    /// it's built; that's fine — no web client is connected at boot anyway, so
    /// the health indicator is what carries them).
    bus: Mutex<Option<Arc<NotificationBus>>>,
}

impl DegradationTracker {
    pub fn new() -> Self {
        Self { state: Mutex::new(HashMap::new()), bus: Mutex::new(None) }
    }

    /// Attach the notification bus so future `record`s also toast.
    pub fn attach_bus(&self, bus: Arc<NotificationBus>) {
        *self.bus.lock().unwrap() = Some(bus);
    }

    /// Trim a reason string to a short, single-line, presentable form.
    pub fn short(s: &str) -> String {
        let s = s.split(['\n', '{']).next().unwrap_or(s).trim();
        if s.chars().count() > 140 { format!("{}…", s.chars().take(139).collect::<String>()) } else { s.to_string() }
    }

    fn now() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    /// Record a fallback. Increments the per-subsystem counter, refreshes
    /// timestamps, and fires a notification (if a bus is attached).
    pub fn record(&self, subsystem: &str, label: &str, from: &str, to: &str, reason: &str, persistent: bool) {
        let now = Self::now();
        {
            let mut s = self.state.lock().unwrap();
            let entry = s.entry(subsystem.to_string()).or_insert_with(|| Degradation {
                subsystem: subsystem.to_string(),
                label: label.to_string(),
                from: from.to_string(),
                to: to.to_string(),
                reason: reason.to_string(),
                persistent,
                first_at: now,
                last_at: now,
                count: 0,
            });
            entry.label = label.to_string();
            entry.from = from.to_string();
            entry.to = to.to_string();
            entry.reason = reason.to_string();
            entry.persistent = persistent;
            entry.last_at = now;
            entry.count += 1;
        }
        tracing::warn!("subsystem degraded: {label} fell back {from} → {to} ({reason})");
        if let Some(bus) = self.bus.lock().unwrap().as_ref() {
            bus.send(Notification {
                kind: NotificationKind::SystemDegraded,
                conversation_id: None,
                channel: None,
                user_id: None,
                message: Some(format!("{label} is degraded — fell back from “{from}” to “{to}” ({reason}).")),
                category: None,
            });
        }
    }

    /// Clear a subsystem's degradation (it recovered).
    pub fn clear(&self, subsystem: &str) {
        self.state.lock().unwrap().remove(subsystem);
    }

    /// Currently-active degradations: all persistent ones + transient ones seen
    /// within [`TRANSIENT_WINDOW_SECS`]. Sorted by subsystem for stable output.
    pub fn active(&self) -> Vec<Degradation> {
        let now = Self::now();
        let s = self.state.lock().unwrap();
        let mut out: Vec<Degradation> = s
            .values()
            .filter(|d| d.persistent || (now - d.last_at) <= TRANSIENT_WINDOW_SECS)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.subsystem.cmp(&b.subsystem));
        out
    }
}

impl Default for DegradationTracker {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_stays_active_transient_windows() {
        let t = DegradationTracker::new();
        t.record("embeddings", "Embeddings", "lmstudio", "internal", "unreachable", true);
        t.record("tts", "Voice synthesis (TTS)", "openai", "piper", "timeout", false);
        let active = t.active();
        assert_eq!(active.len(), 2);
        // Force the transient one to look old, persistent stays.
        {
            let mut s = t.state.lock().unwrap();
            s.get_mut("tts").unwrap().last_at -= TRANSIENT_WINDOW_SECS + 10;
        }
        let active = t.active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].subsystem, "embeddings");
    }

    #[test]
    fn record_increments_and_clears() {
        let t = DegradationTracker::new();
        t.record("stt", "Speech recognition (STT)", "openai", "internal", "no key", false);
        t.record("stt", "Speech recognition (STT)", "openai", "internal", "no key", false);
        assert_eq!(t.active()[0].count, 2);
        t.clear("stt");
        assert!(t.active().is_empty());
    }
}
