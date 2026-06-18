// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export interface ProviderHealth {
  name: string
  healthy: boolean
  latency_ms: number | null
  model: string
  url: string | null
}

export interface ModelInfo {
  id: string
  provider: string
}

export interface StatusInfo {
  version: string
  uptime_secs: number
  active_sessions: number
  memory_count: number
  conversation_count: number
  message_count: number
  provider_name: string
  /** True when a supervisor (systemd, Docker, launchd) will relaunch MIRA
   *  after a clean exit. False when running under `cargo run` or a bare
   *  manual launch — the Restart button degrades to "Stop" in that case. */
  supervised: boolean
  /** Which supervisor was detected, when known. */
  supervisor: 'systemd' | 'docker' | 'launchd' | null
}

// ── OpenRouter catalog ───────────────────────────────────────────────────────
// Mirrors src/server/handlers/providers.rs::CatalogResponse + CatalogEntry.
// Pricing values are USD per token (e.g. 5e-6 = $5 per million tokens).

export interface OpenRouterPricing {
  prompt:     number
  completion: number
  image:      number
  request:    number
}

export interface OpenRouterModel {
  id:             string
  name:           string
  context_length: number
  modality:       string
  pricing:        OpenRouterPricing
}

export interface OpenRouterCatalog {
  fetched_at: number  // unix seconds
  count:      number
  models:     OpenRouterModel[]
}

export const providersApi = {
  health: () =>
    api.get<{ providers: ProviderHealth[] }>('/api/providers/health').then(r => r.data.providers),
  models: () =>
    api.get<ModelInfo[]>('/api/providers/models').then(r => r.data),
  status: () =>
    api.get<StatusInfo>('/api/status').then(r => r.data),
  openRouterCatalog: (refresh = false) =>
    api.get<OpenRouterCatalog>('/api/providers/openrouter/models', {
      params: refresh ? { refresh: true } : {},
    }).then(r => r.data),
}

// ── Pricing helpers ──────────────────────────────────────────────────────────
// Mirror logic in src/providers/openrouter/pricing.rs so the web client can
// price a turn locally without a round-trip.

/** Returns total USD for a turn, or null if model unknown / fully free. */
export function costForTurn(
  catalog: OpenRouterCatalog | undefined,
  modelId: string,
  promptTokens: number,
  completionTokens: number,
): number | null {
  if (!catalog) return null
  const m = catalog.models.find(x => x.id === modelId)
  if (!m) return null
  const p = m.pricing
  if (p.prompt === 0 && p.completion === 0 && p.request === 0) return null
  return p.prompt * promptTokens + p.completion * completionTokens + p.request
}

/** Magnitude-aware USD formatting: 4 decimals above $0.01, 6 below. */
export function formatUsd(cost: number): string {
  if (cost >= 0.01) return `$${cost.toFixed(4)}`
  return `$${cost.toFixed(6)}`
}
