// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/openrouter/pricing.rs

//! USD-cost calculation for an OpenRouter generation.
//!
//! Prices on `Pricing` are USD-per-token (already parsed from the upstream
//! decimal strings on ingest). Cost = `prompt * prompt_tokens
//! + completion * completion_tokens + request` (the per-call surcharge, when
//! the model has one).
//!
//! Returns `None` when:
//! * the model id isn't in the supplied catalog, or
//! * we have no prompt/completion price *and* no per-request surcharge —
//!   meaning we genuinely don't know the cost (free or unpriced model). The
//!   UI uses `None` as the signal to hide the cost line rather than
//!   misleadingly print `$0.00`.

use crate::types::TokenUsage;
use super::catalog::Catalog;

/// Per-turn USD cost for `model_id` given `usage`. See module docs for `None`
/// semantics.
pub fn cost_for(catalog: &Catalog, model_id: &str, usage: &TokenUsage) -> Option<f64> {
    let entry = catalog.find(model_id)?;
    let p = &entry.pricing;
    if p.prompt == 0.0 && p.completion == 0.0 && p.request == 0.0 {
        return None;
    }
    let token_cost =
        p.prompt     * usage.prompt_tokens     as f64 +
        p.completion * usage.completion_tokens as f64;
    Some(token_cost + p.request)
}

/// Format a USD cost into a compact human string for the per-turn footer.
/// Picks decimals based on magnitude so sub-cent costs don't all read as
/// `$0.00`.
pub fn format_usd(cost: f64) -> String {
    if cost >= 1.0       { format!("${cost:.4}") }
    else if cost >= 0.01 { format!("${cost:.4}") }
    else                 { format!("${cost:.6}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::catalog::parse_upstream_json;

    const FIXTURE: &str = r#"{
      "data": [
        { "id": "openai/gpt-4o", "name": "GPT-4o",
          "pricing": { "prompt": "0.000005", "completion": "0.000015", "request": "0" } },
        { "id": "x/free", "name": "Free",
          "pricing": { "prompt": "0", "completion": "0" } },
        { "id": "x/per-request", "name": "Per request",
          "pricing": { "prompt": "0", "completion": "0", "request": "0.01" } }
      ]
    }"#;

    fn usage(p: u32, c: u32) -> TokenUsage {
        TokenUsage { prompt_tokens: p, completion_tokens: c, total_tokens: p + c, ..Default::default() }
    }

    #[test]
    fn cost_for_known_model_combines_prompt_and_completion() {
        let cat = parse_upstream_json(FIXTURE).unwrap();
        // 1000 prompt @ 5e-6 + 500 completion @ 15e-6 = 0.005 + 0.0075 = 0.0125
        let c = cost_for(&cat, "openai/gpt-4o", &usage(1000, 500)).unwrap();
        assert!((c - 0.0125).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn cost_for_free_returns_none_to_signal_no_cost_to_show() {
        let cat = parse_upstream_json(FIXTURE).unwrap();
        assert!(cost_for(&cat, "x/free", &usage(100, 100)).is_none());
    }

    #[test]
    fn cost_for_per_request_surcharge_is_counted_even_if_token_prices_zero() {
        let cat = parse_upstream_json(FIXTURE).unwrap();
        let c = cost_for(&cat, "x/per-request", &usage(0, 0)).unwrap();
        assert!((c - 0.01).abs() < 1e-9);
    }

    #[test]
    fn cost_for_unknown_model_returns_none() {
        let cat = parse_upstream_json(FIXTURE).unwrap();
        assert!(cost_for(&cat, "vendor/unknown", &usage(1, 1)).is_none());
    }

    #[test]
    fn format_usd_picks_decimals_by_magnitude() {
        assert_eq!(format_usd(1.2345),     "$1.2345");
        assert_eq!(format_usd(0.0125),     "$0.0125");
        assert_eq!(format_usd(0.000123),   "$0.000123");
    }
}
