// SPDX-License-Identifier: AGPL-3.0-or-later

// src/api/catalog.ts

import { api } from './client'

/**
 * One row in a provider's model catalog. Mirrors the Rust
 * `ModelEntry` in `src/providers/catalog.rs`. All metric fields are
 * optional — `undefined` means "unknown" rather than zero.
 */
export interface ModelEntry {
  id:                    string
  display_name?:         string
  /** Total context window in tokens. */
  context_window?:       number
  /** USD per 1M input tokens. */
  input_price_per_1m?:   number
  /** USD per 1M output tokens. */
  output_price_per_1m?:  number
  /** Optional short tag — "reasoning", "vision", "legacy", etc. */
  notes?:                string
}

export interface ModelCatalog {
  provider:   string
  entries:    ModelEntry[]
  /** Unix epoch seconds when this catalog was assembled. */
  fetched_at: number
  /** "live" (fresh fetch), "cache" (served from disk within TTL),
   *  "stale-cache" (upstream failed; serving cached), or "static". */
  source:     string
}

export const catalogApi = {
  /**
   * Fetch a provider's model catalog. The server caches the response
   * on disk for 24h; pass `refresh = true` to force a re-fetch.
   *
   * Returns 404 when the provider slug is unknown, 400 when the
   * provider isn't configured (missing api_key / base_url), or
   * 502 when the upstream call fails and there's no cached fallback.
   */
  async fetch(slug: string, refresh = false): Promise<ModelCatalog> {
    const { data } = await api.get<ModelCatalog>(
      `/api/providers/${encodeURIComponent(slug)}/catalog`,
      { params: refresh ? { refresh: true } : undefined },
    )
    return data
  },
}
