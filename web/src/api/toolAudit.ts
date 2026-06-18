// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

// ── Types ───────────────────────────────────────────────────────────────────

export interface ToolAuditRow {
  id:                number
  actor:             string
  tool:              string
  args_digest:       string
  started_at:        number  // ms since epoch
  duration_ms:       number
  outcome:           'success' | 'failure' | 'error'
  truncated_output?: string | null
}

export interface ToolAuditListResponse {
  rows:  ToolAuditRow[]
  total: number
}

export interface ListToolAuditParams {
  limit?:   number
  offset?:  number
  actor?:   string
  tool?:    string
  outcome?: 'success' | 'failure' | 'error' | ''
}

// ── API ─────────────────────────────────────────────────────────────────────

export const toolAuditApi = {
  list(params: ListToolAuditParams = {}): Promise<ToolAuditListResponse> {
    return api
      .get<ToolAuditListResponse>('/api/admin/tool_audit', { params })
      .then((r) => r.data)
  },
}
