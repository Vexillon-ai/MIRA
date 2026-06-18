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
}

export const memoryApi = {
  list: (params?: ListMemoryQuery) =>
    api.get<MemoryItem[]>('/api/memory', { params }).then((r) => r.data),

  get: (id: number) =>
    api.get<MemoryItem>(`/api/memory/${id}`).then((r) => r.data),

  create: (body: CreateMemoryRequest) =>
    api.post<MemoryItem>('/api/memory', body).then((r) => r.data),

  supersede: (id: number, body: SupersedeMemoryRequest) =>
    api.post<MemoryItem>(`/api/memory/${id}/supersede`, body).then((r) => r.data),

  delete: (id: number) =>
    api.delete(`/api/memory/${id}`),

  /** Trigger the sleep-like consolidator on-demand for every user (admin
   *  only). Runs Phases C → A → D regardless of the per-phase config
   *  flags (the nightly job respects them; this manual button is for
   *  testing what they would do). Returns per-phase counts. */
  runConsolidatorNow: () =>
    api.post<ConsolidatorRunResult>('/api/admin/consolidator/run-now')
       .then((r) => r.data),
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
