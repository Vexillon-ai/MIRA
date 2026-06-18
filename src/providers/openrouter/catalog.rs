// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/openrouter/catalog.rs

//! OpenRouter model catalog: fetch, cache, and serve the list of models the
//! user's API key can route to, along with per-token pricing.
//!
//! ## Cache
//!
//! The full upstream response is normalised into [`Catalog`] and persisted at
//! `<data_dir>/cache/openrouter-models.json`. Subsequent calls within
//! `catalog_refresh_hours` of `fetched_at` return the cached copy without a
//! network round-trip.
//!
//! `catalog(force=true)` bypasses the freshness check and always re-fetches,
//! overwriting the cache on success. A failed re-fetch leaves the existing
//! cache untouched so we never lose a working catalog because of a flaky
//! upstream.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::MiraError;

/// Per-token pricing in USD. OpenRouter returns these as decimal strings
/// ("0.000003"); we parse to `f64` once on ingest so consumers don't have to.
///
/// `request` and `image` are flat per-call surcharges (USD/request,
/// USD/image) — not all models have them.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Pricing {
    #[serde(default)] pub prompt:     f64,
    #[serde(default)] pub completion: f64,
    #[serde(default)] pub image:      f64,
    #[serde(default)] pub request:    f64,
}

/// One model in the OpenRouter catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id:             String,
    pub name:           String,
    #[serde(default)] pub context_length: u64,
    #[serde(default)] pub modality:       String,
    pub pricing:        Pricing,
}

/// On-disk cache envelope. `fetched_at` is unix seconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub fetched_at: u64,
    pub models:     Vec<CatalogEntry>,
}

impl Catalog {
    /// Returns true when the cache is older than `max_age_hours`.
    pub fn is_stale(&self, max_age_hours: u64) -> bool {
        if max_age_hours == 0 { return true; }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        now.saturating_sub(self.fetched_at) > max_age_hours * 3600
    }

    /// Look up a model by its OpenRouter id (e.g. `"openai/gpt-4o"`).
    pub fn find(&self, model_id: &str) -> Option<&CatalogEntry> {
        self.models.iter().find(|m| m.id == model_id)
    }

    /// Path the catalog is cached to under `data_dir`.
    pub fn cache_path(data_dir: &Path) -> PathBuf {
        data_dir.join("cache").join("openrouter-models.json")
    }

    /// Load from disk. Returns `Ok(None)` when the cache file doesn't exist —
    /// callers treat that as "needs a fetch", not as an error. Parse failures
    /// are surfaced (the cache is corrupt; force a re-fetch).
    pub fn load(data_dir: &Path) -> Result<Option<Self>, MiraError> {
        let path = Self::cache_path(data_dir);
        match std::fs::read(&path) {
            Ok(bytes)  => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e)     => Err(MiraError::IoError(e)),
        }
    }

    /// Atomically persist the catalog: write to a sibling tmp file then rename.
    /// Creates `<data_dir>/cache/` on demand.
    pub fn save(&self, data_dir: &Path) -> Result<(), MiraError> {
        let path = Self::cache_path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

// ─── Upstream wire shape ────────────────────────────────────────────────
// Subset of the OpenRouter `/api/v1/models` payload we actually consume.
// Pricing values arrive as strings — parse via `parse_price` on ingest.

#[derive(Debug, Deserialize)]
pub(crate) struct UpstreamResponse {
    pub data: Vec<UpstreamModel>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpstreamModel {
    pub id:                String,
    #[serde(default)] pub name:           String,
    #[serde(default)] pub context_length: Option<u64>,
    #[serde(default)] pub architecture:   Option<UpstreamArch>,
    #[serde(default)] pub pricing:        Option<UpstreamPricing>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpstreamArch {
    #[serde(default)] pub modality: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct UpstreamPricing {
    #[serde(default)] pub prompt:     Option<String>,
    #[serde(default)] pub completion: Option<String>,
    #[serde(default)] pub image:      Option<String>,
    #[serde(default)] pub request:    Option<String>,
}

fn parse_price(s: Option<String>) -> f64 {
    s.as_deref().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)
}

impl From<UpstreamModel> for CatalogEntry {
    fn from(m: UpstreamModel) -> Self {
        let pricing = m.pricing.unwrap_or_default();
        let modality = m.architecture
            .and_then(|a| a.modality)
            .unwrap_or_default();
        let name = if m.name.is_empty() { m.id.clone() } else { m.name };
        Self {
            id:             m.id,
            name,
            context_length: m.context_length.unwrap_or(0),
            modality,
            pricing: Pricing {
                prompt:     parse_price(pricing.prompt),
                completion: parse_price(pricing.completion),
                image:      parse_price(pricing.image),
                request:    parse_price(pricing.request),
            },
        }
    }
}

/// Build a [`Catalog`] from a freshly-decoded upstream response, stamping
/// `fetched_at` to "now".
pub(crate) fn catalog_from_upstream(resp: UpstreamResponse) -> Catalog {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut models: Vec<CatalogEntry> =
        resp.data.into_iter().map(CatalogEntry::from).collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Catalog { fetched_at: now, models }
}

/// Try to read a fresh-enough cached catalog. Used by callers that want a
/// "best effort, never fail" view (e.g. TUI startup before the network is
/// up). Returns `None` if the cache is missing, stale, or corrupt.
pub fn load_if_fresh(data_dir: &Path, max_age_hours: u64) -> Option<Catalog> {
    match Catalog::load(data_dir) {
        Ok(Some(c)) if !c.is_stale(max_age_hours) => Some(c),
        Ok(_)  => None,
        Err(e) => { warn!("openrouter catalog cache unreadable: {e}"); None }
    }
}

/// Convenience used by tests and by [`OpenRouterProvider::catalog`].
pub(crate) fn parse_upstream_json(json: &str) -> Result<Catalog, MiraError> {
    let resp: UpstreamResponse = serde_json::from_str(json)?;
    debug!("openrouter catalog: parsed {} models", resp.data.len());
    Ok(catalog_from_upstream(resp))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "data": [
        {
          "id": "openai/gpt-4o",
          "name": "OpenAI: GPT-4o",
          "context_length": 128000,
          "architecture": { "modality": "text+image->text" },
          "pricing": {
            "prompt": "0.000005",
            "completion": "0.000015",
            "image": "0.007225",
            "request": "0"
          }
        },
        {
          "id": "meta-llama/llama-3.2-3b-instruct",
          "name": "Meta: Llama 3.2 3B Instruct",
          "context_length": 131072,
          "architecture": { "modality": "text->text" },
          "pricing": { "prompt": "0.00000003", "completion": "0.00000005" }
        },
        {
          "id": "vendor/no-pricing",
          "name": "Vendor: No Pricing"
        }
      ]
    }"#;

    #[test]
    fn parses_fixture_and_normalises() {
        let cat = parse_upstream_json(FIXTURE).unwrap();
        assert_eq!(cat.models.len(), 3);
        // sorted by id
        assert_eq!(cat.models[0].id, "meta-llama/llama-3.2-3b-instruct");
        assert_eq!(cat.models[1].id, "openai/gpt-4o");
        assert_eq!(cat.models[2].id, "vendor/no-pricing");

        let gpt = cat.find("openai/gpt-4o").unwrap();
        assert_eq!(gpt.context_length, 128000);
        assert_eq!(gpt.modality, "text+image->text");
        assert!((gpt.pricing.prompt     - 0.000005 ).abs() < 1e-12);
        assert!((gpt.pricing.completion - 0.000015 ).abs() < 1e-12);
        assert!((gpt.pricing.image      - 0.007225 ).abs() < 1e-9);

        let none = cat.find("vendor/no-pricing").unwrap();
        assert_eq!(none.pricing.prompt,     0.0);
        assert_eq!(none.pricing.completion, 0.0);
    }

    #[test]
    fn stale_check_respects_zero_and_age() {
        let mut cat = parse_upstream_json(FIXTURE).unwrap();
        assert!(cat.is_stale(0), "0 hours always stale");
        cat.fetched_at = 0;
        assert!(cat.is_stale(1), "epoch is older than 1h ago");
        cat.fetched_at = SystemTime::now().duration_since(UNIX_EPOCH)
            .unwrap().as_secs();
        assert!(!cat.is_stale(24), "just-fetched is fresh");
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cat = parse_upstream_json(FIXTURE).unwrap();
        cat.save(dir.path()).expect("save");
        let loaded = Catalog::load(dir.path()).expect("load").expect("present");
        assert_eq!(loaded.models.len(), cat.models.len());
        assert_eq!(loaded.models[1].id, "openai/gpt-4o");
    }

    #[test]
    fn load_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Catalog::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn load_if_fresh_skips_stale() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = parse_upstream_json(FIXTURE).unwrap();
        cat.fetched_at = 0;
        cat.save(dir.path()).unwrap();
        assert!(load_if_fresh(dir.path(), 24).is_none(),
            "epoch-stamped cache must read as stale");
    }
}
