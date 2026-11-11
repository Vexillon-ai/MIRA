// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export type MemoryScope = 'user' | 'group' | 'system'

export interface MemoryItem {
  id: number
  content: string
  category: string
  tags: string[]
  created_at: number  // ms
  relevance_score: number
  scope: MemoryScope
  scope_id: string | null
  created_by: string | null
  supersedes: number | null
  superseded_by: number | null
  /** Persisted baseline strength (0..1). */
  strength: number
  /** Decay-adjusted strength as of this response (0..1). */
  effective_strength: number
  access_count: number
  /** Epoch ms of last reinforcement. */
  last_reinforced: number
  stability: 'permanent' | 'stable' | 'episodic' | 'ephemeral' | string
  // ── Provenance (review surface) ──
  /** How the memory was produced — 'user_explicit' | 'auto_extracted' | 'imported'. */
  source_kind: 'user_explicit' | 'auto_extracted' | 'imported' | null
  /** Free-form detail attached to the source (e.g. importer name). */
  source_detail: string | null
  /** Channel that produced the triggering turn (e.g. 'web', 'tg', 'signal'). */
  source_channel: string | null
  /** Conversation id that produced this memory — deep-link back to transcript. */
  source_conversation_id: string | null
  /** Message id that produced this memory, when applicable. */
  source_message_id: string | null
}

export interface CreateMemoryRequest {
  content:  string
  category?: string
  tags?:    string[]
  scope?:   MemoryScope
  /** Required when scope === 'group'; ignored for other scopes. */
  scope_id?: string
}

export interface SupersedeMemoryRequest {
  content:   string
  category?: string
  tags?:     string[]
}

export type MemorySort = 'strength' | 'recent'

export interface ListMemoryQuery {
  q?:        string
  category?: string
  scope?:    MemoryScope | 'all'
  sort?:     MemorySort
  limit?:    number
  offset?:   number
  /** Filter by how the memory was produced. */
  source?:   'user_explicit' | 'auto_extracted' | 'imported' | 'all'
  /** Filter to memories carrying this exact tag (e.g. 'rollup'). */
  tag?:      string
  /** Server-side kind split: 'saved' (non-rollup) | 'summary' (daily rollups) | 'all'. */
  kind?:     'saved' | 'summary' | 'all'
}

export const memoryApi = {
  list: (params?: ListMemoryQuery) =>
    api.get<MemoryItem[]>('/api/memory', { params }).then((r) => r.data),

  /** Like `list`, but also returns the full server-side match count from the
   *  `X-Total-Count` header, so a tab can show an accurate total even when the
   *  page is capped. Falls back to the page length if the header is absent. */
  listWithTotal: (params?: ListMemoryQuery) =>
    api.get<MemoryItem[]>('/api/memory', { params }).then((r) => {
      const header = r.headers['x-total-count']
      const total = header != null ? Number(header) : r.data.length
      return { items: r.data, total: Number.isFinite(total) ? total : r.data.length }
    }),

  get: (id: number) =>
    api.get<MemoryItem>(`/api/memory/${id}`).then((r) => r.data),

  create: (body: CreateMemoryRequest) =>
    api.post<MemoryItem>('/api/memory', body).then((r) => r.data),

  supersede: (id: number, body: SupersedeMemoryRequest) =>
    api.post<MemoryItem>(`/api/memory/${id}/supersede`, body).then((r) => r.data),

  delete: (id: number) =>
    api.delete(`/api/memory/${id}`),

  /** Undo a soft-delete of your own memory (the "Undo" toast). */
  restore: (id: number) =>
    api.post(`/api/memory/${id}/restore`),

  /** The supersede chain through a memory (oldest → newest, incl. superseded). */
  history: (id: number) =>
    api.get<MemoryItem[]>(`/api/memory/${id}/history`).then((r) => r.data),

  /** Delete ALL of the caller's own (user-scoped) memories. By default this is
   *  a soft delete (hidden from recall, retained briefly in the audit log and
   *  purged after the retention window). Pass `hard` to erase them immediately
   *  with no retention. */
  deleteMine: (hard = false) =>
    api.delete<{ deleted: number }>(`/api/memory/mine${hard ? '?hard=true' : ''}`).then((r) => r.data),

  /** Export everything visible as a Blob (JSON default, or 'md'), for download. */
  exportBlob: (format: 'json' | 'md' = 'json') =>
    api.get(`/api/memory/export?format=${format}`, { responseType: 'blob' }).then((r) => r.data as Blob),

  /** Per-source status (admin): load state, locality, capabilities + live health. */
  providersStatus: () =>
    api.get<{ providers: ProviderStatus[] }>('/api/admin/memory/providers/status').then((r) => r.data.providers),

  /** Trigger the sleep-like consolidator on-demand for every user (admin
   *  only). Runs Phases C → A → D regardless of the per-phase config
   *  flags (the nightly job respects them; this manual button is for
   *  testing what they would do). Returns per-phase counts. */
  runConsolidatorNow: () =>
    api.post<ConsolidatorRunResult>('/api/admin/consolidator/run-now')
       .then((r) => r.data),

  /** The caller's OWN memory catalog from a configured external source (any
   *  memory-SPI HTTP service) — WHAT IS HELD, not a read of it. `available:false`
   *  when no catalog source is configured. Owner-scoped server-side. */
  catalog: (params?: CatalogQuery) =>
    api.get<CatalogResponse>('/api/memory/catalog', { params }).then((r) => r.data),
}

export interface CatalogQuery {
  /** Fetch one subject's held facts (with tier labels). */
  subject?:  string
  /** List only subjects whose name/alias/title starts with this. */
  prefix?:   string
  /** List only subjects whose name/alias/title contains this (not with prefix). */
  contains?: string
  page?:     number
  per_page?: number
}

/** SPI trust level: how much the source stands behind a fact. */
export type TrustLevel = 'verified' | 'reported' | 'unverified'

/** A fact as the catalog's per-subject detail returns it. An `unverified` item
 *  is a source passage, shown as an attributed quote. */
export interface CatalogFact {
  relation?:    string | null
  content?:     string | null
  trust?:       TrustLevel | string | null
  trust_label?: string | null
}

export interface CatalogSubject {
  name:     string
  aliases?: string[]
  label?:   string | null
}

/** The provider's catalog payload (generic memory SPI shape). */
export interface CatalogPayload {
  scope?:  Record<string, unknown>
  counts?: {
    subjects?: number
    facts?:    number
    by_trust?: Partial<Record<TrustLevel, number>> & Record<string, number>
    [k: string]: unknown
  }
  page?: {
    page?: number; per_page?: number
    n_matching_subjects?: number; n_pages_subjects?: number
    [k: string]: unknown
  }
  subjects?: CatalogSubject[]
  subject_detail?: {
    name?: string; state?: string; reason?: string | null
    n_facts?: number; truncated?: unknown
    facts?: CatalogFact[]
  }
}

export interface ProviderStatus {
  id:         string
  /** Present on build-failure rows (providers that never loaded). */
  transport?: string
  locality:   string            // loopback | lan | remote | n/a
  read:       boolean
  write:      boolean
  forget:     boolean
  catalog:    boolean
  citations:  boolean
  /** 'error' = configured but failed to build (refused/missing key/TLS/MCP). */
  health:     'ready' | 'warming' | 'unavailable' | 'error' | string
  reason?:    string | null
}

export interface CatalogResponse {
  available: boolean
  state?:    string
  catalog?:  CatalogPayload
  reason?:   string
  error?:    string
  /** Id of the provider serving the catalog (for the UI header). */
  source?:   string
}

export interface ConsolidatorRunResult {
  users_processed: number
  contradictions_groups: number
  contradictions_edges_closed: number
  entities_merged: number
  entity_edges_repointed: number
  importance_edges_scored: number
  entity_dedup_ratio: number
  importance_half_life_days: number
}
