// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/api/agents.ts
//
// Multi-agent registry view (slice B7). Mirrors
// src/server/handlers/agents.rs exactly.

import { api } from './client'

export type AgentStatus =
  | 'pending' | 'running' | 'paused'
  | 'completed' | 'failed' | 'interrupted'

export interface LlmChoiceDto {
  alias:    string
  provider: string
  model:    string | null
}

export interface AgentDto {
  id:             string
  parent:         string | null
  skill_id:       string | null
  status:         AgentStatus
  depth:          number
  created_at_ms:  number
  current_step:   string | null
  /** Last self-reported progress fraction 0.0–1.0 (Phase A3), or null. */
  percent_done:   number | null
  result_summary: string | null
  failure_reason: string | null
  /** Structured fault (Phase A1): `{ code, … }` — a precise cause when failed
   *  (budget_exceeded / timeout / policy_denied / …). null on success. */
  fault:          { code: string; [k: string]: unknown } | null
  spent_usd:      number
  /** null when the agent has unlimited budget (root only). */
  max_usd:        number | null
  child_ids:      string[]
  /** Which LLM the agent is using (slice B8). null when no choice was made. */
  llm_choice:     LlmChoiceDto | null
}

export interface FleetAggregate {
  total:           number
  running:         number
  paused:          number
  completed:       number
  failed:          number
  interrupted:     number
  total_spent_usd: number
}

export interface AgentsResponse {
  agents:                AgentDto[]
  max_recursion_depth:   number
  default_session_usd:   number
  /** Fleet-wide rollup (Phase A3). */
  aggregate:             FleetAggregate
}

export type InterruptReason = 'user' | 'timeout' | 'budget' | 'policy'

// ── Audit log (slice B9 + D1 + D4) ───────────────────────────────────

/** Snake-case event kinds the audit store records. Mirrors
 *  AuditEvent::kind() in src/agent/audit.rs. */
export type AuditKind =
  | 'spawn_requested'
  | 'spawn_approved'
  | 'spawn_denied'
  | 'status_change'
  | 'agent_budget_exceeded'
  | 'session_budget_exceeded'
  | 'interrupted'
  | 'policy_decision'

export const ALL_AUDIT_KINDS: readonly AuditKind[] = [
  'spawn_requested', 'spawn_approved', 'spawn_denied', 'status_change',
  'agent_budget_exceeded', 'session_budget_exceeded',
  'interrupted', 'policy_decision',
]

export interface AuditRow {
  id:        number
  ts_ms:     number
  agent_id:  string
  kind:      AuditKind
  /** Variant-specific payload — same shape as AuditEvent in Rust. */
  event:     Record<string, unknown> & { kind: AuditKind }
  prev_hmac: string
  hmac:      string
}

export interface AuditResponse {
  rows:        AuditRow[]
  /** False iff verify_chain detected tampering or a deletion. */
  chain_ok:    boolean
  chain_break: string | null
}

export interface AuditQuery {
  agent_id?: string
  /** Comma-separated kinds (handler splits + filters unknowns). */
  kinds?:    AuditKind[]
  since_ms?: number
  until_ms?: number
  limit?:    number
}

export const agentsApi = {
  async list(): Promise<AgentsResponse> {
    const { data } = await api.get<AgentsResponse>('/api/agents')
    return data
  },

  /** Stop one agent, optionally propagating to its descendants. */
  async interrupt(
    agentId: string,
    opts: { reason?: InterruptReason; propagate?: boolean } = {},
  ): Promise<{ signalled: number }> {
    const { data } = await api.post<{ signalled: number }>(
      `/api/agents/${encodeURIComponent(agentId)}/interrupt`,
      { reason: opts.reason ?? 'user', propagate: opts.propagate ?? true },
    )
    return data
  },

  async pause(agentId: string): Promise<void> {
    await api.post(`/api/agents/${encodeURIComponent(agentId)}/pause`)
  },

  async resume(agentId: string): Promise<void> {
    await api.post(`/api/agents/${encodeURIComponent(agentId)}/resume`)
  },

  /** Fetch audit-log rows (slice D4). All filters optional. */
  async audit(q: AuditQuery = {}): Promise<AuditResponse> {
    const params: Record<string, string> = {}
    if (q.agent_id) params.agent_id = q.agent_id
    if (q.kinds && q.kinds.length > 0) params.kinds = q.kinds.join(',')
    if (q.since_ms != null) params.since_ms = String(q.since_ms)
    if (q.until_ms != null) params.until_ms = String(q.until_ms)
    if (q.limit    != null) params.limit    = String(q.limit)
    const { data } = await api.get<AuditResponse>('/api/agents/audit', { params })
    return data
  },

  // ── 0.113.0 — agent detail (activity + stdout) ──────────────────

  async activity(agentId: string): Promise<AgentActivity> {
    const { data } = await api.get<AgentActivity>(
      `/api/agents/${encodeURIComponent(agentId)}/activity`,
    )
    return data
  },

  async stdout(
    agentId: string,
    opts: { tail?: number; offset?: number; which?: 'stdout' | 'stderr' } = {},
  ): Promise<AgentStdoutChunk> {
    const params: Record<string, string> = {}
    if (opts.tail   != null) params.tail   = String(opts.tail)
    if (opts.offset != null) params.offset = String(opts.offset)
    if (opts.which)          params.which  = opts.which
    const { data } = await api.get<AgentStdoutChunk>(
      `/api/agents/${encodeURIComponent(agentId)}/stdout`, { params },
    )
    return data
  },
}

// 0.113.0 — agent-detail types
export interface AuditEntry {
  ts_ms:  number
  kind:   string
  detail: unknown
}

export interface ProgressEntry {
  ts_ms:         number
  summary:       string
  percent_done?: number | null
  llm_spend_usd: number
}

export interface AgentActivity {
  agent:    AgentDto | null
  audit:    AuditEntry[]
  progress: ProgressEntry[]
}

export interface AgentStdoutChunk {
  content:   string
  size:      number
  offset:    number
  running:   boolean
  truncated: boolean
}
