// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/overlays.rs

//! Hand-curated pricing + context overlays per provider.
//!
//! Every public LLM-provider API surfaces model ids via
//! `GET /v1/models` (or equivalent), but only Anthropic and Gemini
//! publish context windows in that response, and none of them
//! publish pricing. The overlays below fill the gap — matched by
//! longest-prefix-wins on the model id (see
//! `providers::catalog::apply_overlay`).
//!
//! Maintenance: refresh when a provider announces a new model or
//! changes pricing. Each table's docstring carries the "data current
//! at" date so reviewers can spot stale entries. Out-of-date prices
//! don't break anything — they just show wrong numbers in the
//! Settings dropdown until the next maintenance pass.
//!
//! Source: each provider's official pricing page as of the date in
//! the table's docstring.

use crate::providers::catalog::PricingRow;

// ─────────────────────────────────────────────────────────────────────────────
// OpenAI
// ─────────────────────────────────────────────────────────────────────────────

/// Data current at 2026-05. Source: platform.openai.com/docs/pricing.
/// Ordering doesn't matter — longest-prefix-wins is enforced by the
/// caller. Listing the more specific prefix first is just a
/// readability convenience.
pub const OPENAI: &[PricingRow] = &[
    // o-series reasoning models
    PricingRow { id_prefix: "o4-mini",       context_window: 200_000,
        input_price_per_1m: 1.10,  output_price_per_1m: 4.40,
        display_name: "o4-mini",       notes: "reasoning" },
    PricingRow { id_prefix: "o3-mini",       context_window: 200_000,
        input_price_per_1m: 1.10,  output_price_per_1m: 4.40,
        display_name: "o3-mini",       notes: "reasoning" },
    PricingRow { id_prefix: "o3",            context_window: 200_000,
        input_price_per_1m: 2.00,  output_price_per_1m: 8.00,
        display_name: "o3",            notes: "reasoning" },
    PricingRow { id_prefix: "o1-mini",       context_window: 128_000,
        input_price_per_1m: 1.10,  output_price_per_1m: 4.40,
        display_name: "o1-mini",       notes: "reasoning" },
    PricingRow { id_prefix: "o1",            context_window: 200_000,
        input_price_per_1m: 15.00, output_price_per_1m: 60.00,
        display_name: "o1",            notes: "reasoning" },

    // GPT-4o family
    PricingRow { id_prefix: "gpt-4o-mini",   context_window: 128_000,
        input_price_per_1m: 0.15,  output_price_per_1m: 0.60,
        display_name: "GPT-4o mini",   notes: "" },
    PricingRow { id_prefix: "gpt-4o",        context_window: 128_000,
        input_price_per_1m: 2.50,  output_price_per_1m: 10.00,
        display_name: "GPT-4o",        notes: "vision" },
    PricingRow { id_prefix: "gpt-4-turbo",   context_window: 128_000,
        input_price_per_1m: 10.00, output_price_per_1m: 30.00,
        display_name: "GPT-4 Turbo",   notes: "" },
    PricingRow { id_prefix: "gpt-3.5-turbo", context_window: 16_385,
        input_price_per_1m: 0.50,  output_price_per_1m: 1.50,
        display_name: "GPT-3.5 Turbo", notes: "legacy" },
];

// ─────────────────────────────────────────────────────────────────────────────
// Anthropic
// ─────────────────────────────────────────────────────────────────────────────

/// Data current at 2026-05. Source: anthropic.com/pricing#api.
/// Anthropic's `/v1/models` returns `display_name` + `created_at`
/// directly, but no pricing or context window — the overlay fills
/// both. Names match the public model ids; date-stamped variants
/// (`claude-sonnet-4-5-20250929`) match the same family pricing
/// via the prefix.
pub const ANTHROPIC: &[PricingRow] = &[
    // 4.x family (current)
    PricingRow { id_prefix: "claude-opus-4-1",   context_window: 200_000,
        input_price_per_1m: 15.00, output_price_per_1m: 75.00,
        display_name: "Claude Opus 4.1",   notes: "top reasoning" },
    PricingRow { id_prefix: "claude-opus-4",     context_window: 200_000,
        input_price_per_1m: 15.00, output_price_per_1m: 75.00,
        display_name: "Claude Opus 4",     notes: "top reasoning" },
    PricingRow { id_prefix: "claude-sonnet-4-5", context_window: 200_000,
        input_price_per_1m: 3.00,  output_price_per_1m: 15.00,
        display_name: "Claude Sonnet 4.5", notes: "balanced" },
    PricingRow { id_prefix: "claude-sonnet-4",   context_window: 200_000,
        input_price_per_1m: 3.00,  output_price_per_1m: 15.00,
        display_name: "Claude Sonnet 4",   notes: "balanced" },
    PricingRow { id_prefix: "claude-haiku-4-5",  context_window: 200_000,
        input_price_per_1m: 1.00,  output_price_per_1m: 5.00,
        display_name: "Claude Haiku 4.5",  notes: "fast / cheap" },
    PricingRow { id_prefix: "claude-haiku-4",    context_window: 200_000,
        input_price_per_1m: 1.00,  output_price_per_1m: 5.00,
        display_name: "Claude Haiku 4",    notes: "fast / cheap" },

    // 3.7 / 3.5 legacy
    PricingRow { id_prefix: "claude-3-7-sonnet", context_window: 200_000,
        input_price_per_1m: 3.00,  output_price_per_1m: 15.00,
        display_name: "Claude 3.7 Sonnet", notes: "extended thinking" },
    PricingRow { id_prefix: "claude-3-5-sonnet", context_window: 200_000,
        input_price_per_1m: 3.00,  output_price_per_1m: 15.00,
        display_name: "Claude 3.5 Sonnet", notes: "legacy" },
    PricingRow { id_prefix: "claude-3-5-haiku",  context_window: 200_000,
        input_price_per_1m: 0.80,  output_price_per_1m: 4.00,
        display_name: "Claude 3.5 Haiku",  notes: "legacy" },
    PricingRow { id_prefix: "claude-3-opus",     context_window: 200_000,
        input_price_per_1m: 15.00, output_price_per_1m: 75.00,
        display_name: "Claude 3 Opus",     notes: "legacy" },
    PricingRow { id_prefix: "claude-3-haiku",    context_window: 200_000,
        input_price_per_1m: 0.25,  output_price_per_1m: 1.25,
        display_name: "Claude 3 Haiku",    notes: "legacy" },
];

// ─────────────────────────────────────────────────────────────────────────────
// Google Gemini
// ─────────────────────────────────────────────────────────────────────────────

/// Data current at 2026-05. Source: ai.google.dev/pricing. Gemini's
/// `/v1beta/models` returns `inputTokenLimit` directly so the
/// overlay focuses on pricing + display labels. The Gemini-2.5
/// family has tiered pricing (longer prompts cost more); we list
/// the standard <200K input tier here — the UI shows that and notes
/// "tiered" so users know prices increase past the threshold.
pub const GEMINI: &[PricingRow] = &[
    PricingRow { id_prefix: "models/gemini-2.5-pro",        context_window: 1_048_576,
        input_price_per_1m: 1.25,  output_price_per_1m: 10.00,
        display_name: "Gemini 2.5 Pro",        notes: "tiered ≤200K" },
    PricingRow { id_prefix: "models/gemini-2.5-flash-lite", context_window: 1_048_576,
        input_price_per_1m: 0.10,  output_price_per_1m: 0.40,
        display_name: "Gemini 2.5 Flash-Lite", notes: "cheapest" },
    PricingRow { id_prefix: "models/gemini-2.5-flash",      context_window: 1_048_576,
        input_price_per_1m: 0.30,  output_price_per_1m: 2.50,
        display_name: "Gemini 2.5 Flash",      notes: "fast" },
    PricingRow { id_prefix: "models/gemini-2.0-flash",      context_window: 1_048_576,
        input_price_per_1m: 0.10,  output_price_per_1m: 0.40,
        display_name: "Gemini 2.0 Flash",      notes: "legacy" },
    PricingRow { id_prefix: "models/gemini-1.5-pro",        context_window: 2_097_152,
        input_price_per_1m: 1.25,  output_price_per_1m: 5.00,
        display_name: "Gemini 1.5 Pro",        notes: "legacy" },
    PricingRow { id_prefix: "models/gemini-1.5-flash",      context_window: 1_048_576,
        input_price_per_1m: 0.075, output_price_per_1m: 0.30,
        display_name: "Gemini 1.5 Flash",      notes: "legacy" },
];

// ─────────────────────────────────────────────────────────────────────────────
// DeepSeek
// ─────────────────────────────────────────────────────────────────────────────

/// Data current at 2026-05. Source: api-docs.deepseek.com/quick_start/pricing.
/// DeepSeek runs aggressive off-peak discounting; we list the
/// peak-hours numbers since the UI doesn't yet have a time-of-day
/// pricing display.
pub const DEEPSEEK: &[PricingRow] = &[
    PricingRow { id_prefix: "deepseek-reasoner", context_window: 64_000,
        input_price_per_1m: 0.55, output_price_per_1m: 2.19,
        display_name: "DeepSeek R1 (reasoner)", notes: "reasoning · cache hit $0.14/M" },
    PricingRow { id_prefix: "deepseek-chat",     context_window: 64_000,
        input_price_per_1m: 0.27, output_price_per_1m: 1.10,
        display_name: "DeepSeek V3 (chat)",     notes: "cache hit $0.07/M" },
];

// ─────────────────────────────────────────────────────────────────────────────
// Moonshot (Kimi)
// ─────────────────────────────────────────────────────────────────────────────

/// Data current at 2026-05. Source: platform.moonshot.ai/docs/pricing.
/// The K2 family has a single tier; older kimi-thinking-preview is
/// a different price point.
pub const MOONSHOT: &[PricingRow] = &[
    PricingRow { id_prefix: "kimi-thinking-preview", context_window: 200_000,
        input_price_per_1m: 0.50, output_price_per_1m: 2.50,
        display_name: "Kimi Thinking",  notes: "reasoning · preview" },
    PricingRow { id_prefix: "kimi-k2",               context_window: 200_000,
        input_price_per_1m: 0.30, output_price_per_1m: 1.20,
        display_name: "Kimi K2",        notes: "balanced" },
    PricingRow { id_prefix: "moonshot-v1-128k",      context_window: 128_000,
        input_price_per_1m: 1.66, output_price_per_1m: 1.66,
        display_name: "Moonshot v1 128K", notes: "legacy" },
    PricingRow { id_prefix: "moonshot-v1-32k",       context_window: 32_000,
        input_price_per_1m: 0.34, output_price_per_1m: 0.34,
        display_name: "Moonshot v1 32K",  notes: "legacy" },
    PricingRow { id_prefix: "moonshot-v1-8k",        context_window: 8_000,
        input_price_per_1m: 0.17, output_price_per_1m: 0.17,
        display_name: "Moonshot v1 8K",   notes: "legacy" },
];

// ─────────────────────────────────────────────────────────────────────────────
// Groq
// ─────────────────────────────────────────────────────────────────────────────

/// Data current at 2026-05. Source: console.groq.com/pricing. Groq's
/// pricing varies a lot by hosted model — and they rotate the list
/// regularly. Only the headline open-weight models are listed; the
/// rest fall through to id-only entries.
pub const GROQ: &[PricingRow] = &[
    PricingRow { id_prefix: "llama-3.3-70b-versatile",
        context_window: 131_072,
        input_price_per_1m: 0.59, output_price_per_1m: 0.79,
        display_name: "Llama 3.3 70B Versatile", notes: "" },
    PricingRow { id_prefix: "llama-3.1-8b-instant",
        context_window: 131_072,
        input_price_per_1m: 0.05, output_price_per_1m: 0.08,
        display_name: "Llama 3.1 8B Instant",    notes: "" },
    PricingRow { id_prefix: "deepseek-r1-distill-llama-70b",
        context_window: 131_072,
        input_price_per_1m: 0.75, output_price_per_1m: 0.99,
        display_name: "DeepSeek R1 distill Llama-70B", notes: "reasoning" },
    PricingRow { id_prefix: "mixtral-8x7b",
        context_window: 32_768,
        input_price_per_1m: 0.24, output_price_per_1m: 0.24,
        display_name: "Mixtral 8x7B", notes: "legacy" },
    PricingRow { id_prefix: "gemma2-9b-it",
        context_window: 8_192,
        input_price_per_1m: 0.20, output_price_per_1m: 0.20,
        display_name: "Gemma 2 9B Instruct", notes: "" },
];

// ─────────────────────────────────────────────────────────────────────────────
// xAI (Grok)
// ─────────────────────────────────────────────────────────────────────────────

/// Data current at 2026-05. Source: docs.x.ai/api/models.
pub const XAI: &[PricingRow] = &[
    PricingRow { id_prefix: "grok-4",          context_window: 256_000,
        input_price_per_1m: 3.00,  output_price_per_1m: 15.00,
        display_name: "Grok-4",      notes: "top reasoning" },
    PricingRow { id_prefix: "grok-3-mini",     context_window: 131_072,
        input_price_per_1m: 0.30,  output_price_per_1m: 0.50,
        display_name: "Grok-3 mini", notes: "reasoning · cheap" },
    PricingRow { id_prefix: "grok-3-fast",     context_window: 131_072,
        input_price_per_1m: 5.00,  output_price_per_1m: 25.00,
        display_name: "Grok-3 Fast", notes: "" },
    PricingRow { id_prefix: "grok-3",          context_window: 131_072,
        input_price_per_1m: 3.00,  output_price_per_1m: 15.00,
        display_name: "Grok-3",      notes: "" },
    PricingRow { id_prefix: "grok-2-vision",   context_window: 32_768,
        input_price_per_1m: 2.00,  output_price_per_1m: 10.00,
        display_name: "Grok-2 Vision", notes: "vision" },
    PricingRow { id_prefix: "grok-2",          context_window: 131_072,
        input_price_per_1m: 2.00,  output_price_per_1m: 10.00,
        display_name: "Grok-2",      notes: "legacy" },
];

/// Pick the right overlay table for a provider slug. Returns an
/// empty slice for slugs we don't curate (the catch-all
/// `openai_compat` block, LM Studio, etc.).
pub fn for_provider(slug: &str) -> &'static [PricingRow] {
    match slug {
        "openai"   => OPENAI,
        "anthropic"=> ANTHROPIC,
        "gemini"   => GEMINI,
        "deepseek" => DEEPSEEK,
        "moonshot" => MOONSHOT,
        "groq"     => GROQ,
        "xai"      => XAI,
        _          => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::catalog::{apply_overlay, ModelEntry};

    #[test]
    fn for_provider_returns_known_table_for_openai() {
        assert!(!for_provider("openai").is_empty());
    }

    #[test]
    fn for_provider_returns_empty_for_unknown_slug() {
        assert!(for_provider("nope").is_empty());
    }

    #[test]
    fn longest_prefix_match_picks_4o_mini_over_4o() {
        let mut entries = vec![ModelEntry::id_only("gpt-4o-mini-2024-07-18")];
        apply_overlay(&mut entries, OPENAI);
        // Should match "gpt-4o-mini" not the shorter "gpt-4o".
        assert_eq!(entries[0].input_price_per_1m, Some(0.15));
        assert_eq!(entries[0].display_name.as_deref(), Some("GPT-4o mini"));
    }

    #[test]
    fn anthropic_dated_id_matches_family() {
        let mut entries = vec![ModelEntry::id_only("claude-sonnet-4-5-20250929")];
        apply_overlay(&mut entries, ANTHROPIC);
        assert_eq!(entries[0].display_name.as_deref(), Some("Claude Sonnet 4.5"));
        assert_eq!(entries[0].input_price_per_1m, Some(3.00));
    }

    #[test]
    fn gemini_path_prefix_matches() {
        let mut entries = vec![ModelEntry::id_only("models/gemini-2.5-flash-002")];
        apply_overlay(&mut entries, GEMINI);
        // "gemini-2.5-flash" matches; "gemini-2.5-flash-lite" doesn't
        // because the id doesn't contain "-lite".
        assert_eq!(entries[0].display_name.as_deref(), Some("Gemini 2.5 Flash"));
    }
}
