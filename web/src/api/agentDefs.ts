// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

/** A saved, reusable agent profile (Phase B — named agents). */
export interface AgentDefinition {
  id:            string
  name:          string
  description:   string
  system_prompt: string
  allowed_tools: string[]
  model_alias:   string | null
  budget_usd:    number | null
  enabled:       boolean
  created_at:    number
  updated_at:    number
}

/** Create/update payload (the server assigns id + timestamps). */
export interface AgentDefinitionInput {
  name:          string
  description:   string
  system_prompt: string
  allowed_tools: string[]
  model_alias:   string | null
  budget_usd:    number | null
  enabled:       boolean
}

export const agentDefsApi = {
  list: () =>
    api.get<AgentDefinition[]>('/api/agents/definitions').then(r => r.data),

  create: (input: AgentDefinitionInput) =>
    api.post<AgentDefinition>('/api/agents/definitions', input).then(r => r.data),

  update: (id: string, input: AgentDefinitionInput) =>
    api.put<AgentDefinition>(`/api/agents/definitions/${encodeURIComponent(id)}`, input).then(r => r.data),

  remove: (id: string) =>
    api.delete(`/api/agents/definitions/${encodeURIComponent(id)}`).then(r => r.data),
}
