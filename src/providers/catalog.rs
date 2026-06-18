// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/catalog.rs

//! Cross-provider model catalog.
//!
//! Each provider client implements a `catalog()` method that returns a
//! [`ModelCatalog`] — a uniform list of (`id`, `display_name`,
//! `context_window`, `input_price_per_1m`, `output_price_per_1m`)
//! entries. The web Settings UI calls
//! `GET /api/providers/{slug}/catalog` to populate the per-provider
//! model dropdown so users don't have to type model names by hand.
//!
//! Pricing and context-window data comes from the provider's API
//! when available (Anthropic, Gemini) and a hand-curated overlay
//! otherwise (OpenAI, DeepSeek, Moonshot, Groq, xAI, etc. only return
//! ids from `/v1/models`). Each overlay carries a "data current at"
//! date in its docstring; the agreed maintenance posture is "refresh
//! when a model changes, accept some drift in between."
//!
//! Catalogs are cached on disk under
//! `<data_dir>/cache/<slug>-catalog.json` with a TTL (default 24h).
//! Fetch failures fall back to the stale cache when one exists, so a
//! flaky upstream never empties the dropdown.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// One row in a provider's model catalog.
///
/// `id` is the wire identifier used in `default_model` config (e.g.
/// `"claude-sonnet-4-5"`, `"gpt-4o-mini"`, `"deepseek-reasoner"`).
/// `display_name` is the friendly label the UI shows when present;
/// when missing, the UI falls back to `id`.
///
/// The optional metric fields (`context_window`,
/// `input_price_per_1m`, `output_price_per_1m`) come from a mix of
/// provider-supplied data + hand-curated overlays. `None` means
/// "unknown" rather than "zero" — the UI hides those columns when
/// every entry in a catalog has them unset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Total context window in tokens (input + output combined for
    /// most providers; Anthropic / Gemini publish separate input and
    /// output limits — we report the input limit here since it's the
    /// constraint users actually care about for context sizing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// USD per 1M input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price_per_1m: Option<f64>,
    /// USD per 1M output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price_per_1m: Option<f64>,
    /// Optional one-line tag visible in the dropdown — usually
    /// modality hints ("reasoning", "vision", "audio") or
    /// release-status flags ("beta", "deprecated").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl ModelEntry {
    /// Convenience constructor for the bare `id`-only case the
    /// catch-all path produces.
    pub fn id_only(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            context_window: None,
            input_price_per_1m: None,
            output_price_per_1m: None,
            notes: None,
        }
    }
}

/// A complete provider catalog returned by `catalog()`. The HTTP
/// handler serialises this verbatim; the SPA picker consumes the
/// same shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCatalog {
    /// Slug matching the provider config block (e.g. `"openai"`,
    /// `"anthropic"`, `"deepseek"`).
    pub provider: String,
    pub entries: Vec<ModelEntry>,
    /// Unix epoch seconds when this catalog was assembled. Used by
    /// the freshness check and shown in the UI ("fetched 2 hours ago").
    pub fetched_at: u64,
    /// `"live"` when fetched from the upstream API this call,
    /// `"cache"` when served from disk, `"static"` when constructed
    /// entirely from the hand-curated overlay (no API roundtrip).
    pub source: String,
}

impl ModelCatalog {
    pub fn new(provider: impl Into<String>, entries: Vec<ModelEntry>, source: &str) -> Self {
        Self {
            provider:   provider.into(),
            entries,
            fetched_at: now_secs(),
            source:     source.to_string(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Disk cache
// ─────────────────────────────────────────────────────────────────────────────

fn cache_path(data_dir: &Path, slug: &str) -> PathBuf {
    // <data_dir>/cache/<slug>-catalog.json. Each provider gets its
    // own file so a stale fetch for one doesn't expire another.
    data_dir.join("cache").join(format!("{slug}-catalog.json"))
}

/// Return the cached catalog if it's still within `max_age_hours` of
/// being written. `0` means "always re-fetch" — short-circuit by
/// returning None unconditionally so callers don't need a special
/// case. Returns `None` (rather than an error) on read/parse
/// failures — the upstream fetch path is the recovery surface.
pub fn load_if_fresh(data_dir: &Path, slug: &str, max_age_hours: u64) -> Option<ModelCatalog> {
    if max_age_hours == 0 { return None; }
    let path = cache_path(data_dir, slug);
    let raw  = std::fs::read_to_string(&path).ok()?;
    let cat: ModelCatalog = serde_json::from_str(&raw).ok()?;
    let age_secs = now_secs().saturating_sub(cat.fetched_at);
    if age_secs > max_age_hours.saturating_mul(3600) {
        return None;
    }
    Some(cat)
}

/// Same as `load_if_fresh` but ignores age — used as a stale-fallback
/// when an upstream fetch fails so the UI doesn't empty out.
pub fn load_any(data_dir: &Path, slug: &str) -> Option<ModelCatalog> {
    let path = cache_path(data_dir, slug);
    let raw  = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist a catalog to disk. Creates the `cache/` subdirectory on
/// first write. Best-effort: callers warn-and-continue on failure
/// rather than failing the request — a working catalog without a
/// cache is still better than no catalog at all.
pub fn save_catalog(data_dir: &Path, cat: &ModelCatalog) -> std::io::Result<()> {
    let path = cache_path(data_dir, &cat.provider);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(cat)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, body)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Pricing overlay helper
// ─────────────────────────────────────────────────────────────────────────────

/// One row in a hand-curated pricing overlay. Matched against model
/// ids by longest-prefix-wins so that, e.g., `"gpt-4o-mini"` overrides
/// `"gpt-4o"` for an id starting with `"gpt-4o-mini"`.
///
/// Provider implementations declare an `&'static [PricingRow]` table
/// and pass it to [`apply_overlay`] when building their catalog.
#[derive(Debug, Clone, Copy)]
pub struct PricingRow {
    /// Prefix to match against `ModelEntry.id`. Empty matches all.
    pub id_prefix: &'static str,
    /// Total context window in tokens.
    pub context_window: u32,
    /// USD per 1M input tokens.
    pub input_price_per_1m: f64,
    /// USD per 1M output tokens.
    pub output_price_per_1m: f64,
    /// Optional display label shown alongside the id; falls back to
    /// the model id when empty.
    pub display_name: &'static str,
    /// Optional notes string ("reasoning", "vision", etc.); empty
    /// renders as no tag in the UI.
    pub notes: &'static str,
}

/// Apply the longest-prefix-wins overlay to each entry. Fields that
/// the entry already has set (from the upstream API) are not
/// overwritten — overlay is a fallback, not a force.
pub fn apply_overlay(entries: &mut [ModelEntry], overlay: &[PricingRow]) {
    for e in entries.iter_mut() {
        // Find the longest matching prefix. O(n*m) but n is tiny.
        let best = overlay
            .iter()
            .filter(|row| e.id.starts_with(row.id_prefix))
            .max_by_key(|row| row.id_prefix.len());
        let Some(row) = best else { continue; };
        if e.context_window.is_none()      { e.context_window = Some(row.context_window); }
        if e.input_price_per_1m.is_none()  { e.input_price_per_1m = Some(row.input_price_per_1m); }
        if e.output_price_per_1m.is_none() { e.output_price_per_1m = Some(row.output_price_per_1m); }
        if e.display_name.is_none() && !row.display_name.is_empty() {
            e.display_name = Some(row.display_name.to_string());
        }
        if e.notes.is_none() && !row.notes.is_empty() {
            e.notes = Some(row.notes.to_string());
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample() -> ModelCatalog {
        ModelCatalog::new("test", vec![
            ModelEntry {
                id:                  "model-a".into(),
                display_name:        Some("Model A".into()),
                context_window:      Some(128_000),
                input_price_per_1m:  Some(0.50),
                output_price_per_1m: Some(1.50),
                notes:               Some("reasoning".into()),
            },
            ModelEntry::id_only("model-b"),
        ], "live")
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let cat = sample();
        save_catalog(dir.path(), &cat).unwrap();
        let back = load_any(dir.path(), "test").expect("expected to load");
        assert_eq!(back, cat);
    }

    #[test]
    fn load_if_fresh_returns_none_when_too_old() {
        let dir = tempdir().unwrap();
        let mut cat = sample();
        // Pretend it was fetched 48 hours ago.
        cat.fetched_at = now_secs().saturating_sub(48 * 3600);
        save_catalog(dir.path(), &cat).unwrap();
        assert!(load_if_fresh(dir.path(), "test", 24).is_none(),
            "48h-old cache should be stale at 24h TTL");
        // But the stale-fallback accessor still returns it.
        assert!(load_any(dir.path(), "test").is_some());
    }

    #[test]
    fn load_if_fresh_returns_some_within_ttl() {
        let dir = tempdir().unwrap();
        let cat = sample(); // fetched_at = now
        save_catalog(dir.path(), &cat).unwrap();
        let back = load_if_fresh(dir.path(), "test", 24).expect("expected fresh");
        assert_eq!(back.entries.len(), 2);
    }

    #[test]
    fn load_if_fresh_zero_ttl_always_refetches() {
        let dir = tempdir().unwrap();
        let cat = sample();
        save_catalog(dir.path(), &cat).unwrap();
        assert!(load_if_fresh(dir.path(), "test", 0).is_none(),
            "ttl=0 must always return None to force refetch");
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let dir = tempdir().unwrap();
        assert!(load_if_fresh(dir.path(), "nope", 24).is_none());
        assert!(load_any(dir.path(), "nope").is_none());
    }

    #[test]
    fn overlay_fills_missing_fields_only() {
        let mut entries = vec![
            ModelEntry::id_only("gpt-4o-mini-2024-07-18"),
            ModelEntry::id_only("gpt-4o-2024-08-06"),
            ModelEntry {
                id:                 "gpt-4o-already-priced".into(),
                input_price_per_1m: Some(99.0),
                ..ModelEntry::id_only("x")
            },
        ];
        const OVERLAY: &[PricingRow] = &[
            PricingRow {
                id_prefix: "gpt-4o-mini", context_window: 128_000,
                input_price_per_1m: 0.15, output_price_per_1m: 0.60,
                display_name: "GPT-4o mini", notes: "",
            },
            PricingRow {
                id_prefix: "gpt-4o", context_window: 128_000,
                input_price_per_1m: 2.50, output_price_per_1m: 10.00,
                display_name: "GPT-4o", notes: "",
            },
        ];
        apply_overlay(&mut entries, OVERLAY);
        // Longest-prefix wins: gpt-4o-mini-* matched the mini row.
        assert_eq!(entries[0].input_price_per_1m, Some(0.15));
        assert_eq!(entries[0].display_name.as_deref(), Some("GPT-4o mini"));
        // gpt-4o-2024-08-06 matched the gpt-4o row (no longer prefix
        // wins).
        assert_eq!(entries[1].input_price_per_1m, Some(2.50));
        // Pre-existing fields are NOT overwritten.
        assert_eq!(entries[2].input_price_per_1m, Some(99.0));
    }

    #[test]
    fn overlay_skips_entries_without_a_match() {
        let mut entries = vec![ModelEntry::id_only("totally-unknown")];
        const OVERLAY: &[PricingRow] = &[PricingRow {
            id_prefix: "gpt-4o", context_window: 128_000,
            input_price_per_1m: 2.50, output_price_per_1m: 10.00,
            display_name: "GPT-4o", notes: "",
        }];
        apply_overlay(&mut entries, OVERLAY);
        assert!(entries[0].context_window.is_none());
        assert!(entries[0].input_price_per_1m.is_none());
    }
}
