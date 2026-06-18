// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/api/policy.ts
//
// Admin-policy-rules HTTP client (D3 backend).
// Mirrors src/server/handlers/policy.rs and src/policy/admin.rs.

import { api } from './client'

/** Snake-case event kinds the engine evaluates rules against.
 *  Mirrors PolicyEvent::kind() in src/policy/event.rs. */
export type PolicyEventKind =
  | 'spawn_worker'
  | 'tool_invocation'
  | 'llm_call'
  | 'network_egress'
  | 'filesystem_access'
  | 'secret_read'

export const ALL_EVENT_KINDS: readonly PolicyEventKind[] = [
  'spawn_worker', 'tool_invocation', 'llm_call',
  'network_egress', 'filesystem_access', 'secret_read',
]

/** One predicate. Discriminated union — matches the `#[serde(tag="type")]`
 *  on Predicate in src/policy/admin.rs. The `value` field shape varies
 *  by predicate type; PathBuf serialises as a JSON string. */
export type Predicate =
  | { type: 'skill_id_equals';            value: string }
  | { type: 'tool_name_equals';           value: string }
  | { type: 'provider_equals';            value: string }
  | { type: 'model_equals';               value: string }
  | { type: 'secret_name_equals';         value: string }
  | { type: 'host_equals';                value: string }
  | { type: 'host_has_suffix';            value: string }
  | { type: 'path_under';                 value: string }
  | { type: 'fs_mode_equals';             value: string }
  | { type: 'running_cost_exceeds_usd';   value: number }
  | { type: 'session_cost_exceeds_usd';   value: number }
  | { type: 'depth_exceeds';              value: number }

export const ALL_PREDICATE_TYPES: readonly Predicate['type'][] = [
  'skill_id_equals', 'tool_name_equals', 'provider_equals', 'model_equals',
  'secret_name_equals', 'host_equals', 'host_has_suffix', 'path_under',
  'fs_mode_equals', 'running_cost_exceeds_usd', 'session_cost_exceeds_usd',
  'depth_exceeds',
]

/** Whether the predicate's `value` is numeric. Drives input rendering. */
export function predicateValueIsNumeric(t: Predicate['type']): boolean {
  return t === 'running_cost_exceeds_usd'
      || t === 'session_cost_exceeds_usd'
      || t === 'depth_exceeds'
}

/** Default empty value for a freshly-added predicate row. */
export function defaultPredicateValue(t: Predicate['type']): string | number {
  return predicateValueIsNumeric(t) ? 0 : ''
}

export interface AdminRule {
  id:             string
  name:           string
  enabled:        boolean
  event_kind:     PolicyEventKind
  predicates:     Predicate[]
  reason:         string
  created_at_ms:  number
  updated_at_ms:  number
}

/** Body shape for POST/PUT. Server fills in created_at_ms/updated_at_ms. */
export interface AdminRuleInput {
  id:         string
  name:       string
  enabled:    boolean
  event_kind: PolicyEventKind
  predicates: Predicate[]
  reason:     string
}

interface ListResponse { rules: AdminRule[] }
interface RuleResponse { rule:  AdminRule  }
interface DeleteResponse { deleted: boolean }

export const policyApi = {
  async list(): Promise<AdminRule[]> {
    const { data } = await api.get<ListResponse>('/api/policy/rules')
    return data.rules
  },

  async get(id: string): Promise<AdminRule> {
    const { data } = await api.get<RuleResponse>(`/api/policy/rules/${encodeURIComponent(id)}`)
    return data.rule
  },

  /** Idempotent upsert by id. Use for create AND for updates that
   *  replace a rule wholesale; PUT is for in-place edits where you
   *  want the server to 404 on a missing id (safer for the edit
   *  flow — accidental id typos don't silently create new rows). */
  async create(input: AdminRuleInput): Promise<AdminRule> {
    const { data } = await api.post<RuleResponse>('/api/policy/rules', input)
    return data.rule
  },

  async update(id: string, input: AdminRuleInput): Promise<AdminRule> {
    const { data } = await api.put<RuleResponse>(
      `/api/policy/rules/${encodeURIComponent(id)}`, input,
    )
    return data.rule
  },

  async delete(id: string): Promise<boolean> {
    const { data } = await api.delete<DeleteResponse>(
      `/api/policy/rules/${encodeURIComponent(id)}`,
    )
    return data.deleted
  },
}
