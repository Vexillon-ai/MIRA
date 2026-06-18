// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export type ConditionOp = 'contains' | 'not_contains' | 'equals' | 'not_empty' | 'empty'

export interface StepCondition {
  step:  string
  op:    ConditionOp
  value: string
}

export interface WorkflowStep {
  id:                string
  agent:             string | null
  skill:             string | null
  brief:             string
  depends_on:        string[]
  budget_usd:        number | null
  continue_on_error: boolean
  when:              StepCondition | null
  requires_approval: boolean
}

export interface WorkflowDefinition {
  id:          string
  name:        string
  description: string
  steps:       WorkflowStep[]
  enabled:     boolean
  created_at:  number
  updated_at:  number
}

/** Create/update payload (server assigns id + timestamps). */
export interface WorkflowInput {
  name:        string
  description: string
  steps:       WorkflowStep[]
  enabled:     boolean
}

export type RunStatus = 'pending' | 'running' | 'completed' | 'failed' | 'skipped' | 'paused'

export interface StepRun {
  step_id: string
  target:  string
  status:  RunStatus
  task_id: string | null
  output:  string | null
  error:   string | null
}

export interface WorkflowRun {
  id:            string
  workflow_id:   string
  workflow_name: string
  status:        RunStatus
  input:         string
  steps:         StepRun[]
  error:         string | null
  user_id:       string | null
  created_at:    number
  updated_at:    number
}

export const workflowsApi = {
  list: () =>
    api.get<WorkflowDefinition[]>('/api/workflows').then(r => r.data),

  create: (input: WorkflowInput) =>
    api.post<WorkflowDefinition>('/api/workflows', input).then(r => r.data),

  update: (id: string, input: WorkflowInput) =>
    api.put<WorkflowDefinition>(`/api/workflows/${encodeURIComponent(id)}`, input).then(r => r.data),

  remove: (id: string) =>
    api.delete(`/api/workflows/${encodeURIComponent(id)}`).then(r => r.data),

  run: (id: string, input: string) =>
    api.post<{ run_id: string }>(`/api/workflows/${encodeURIComponent(id)}/run`, { input }).then(r => r.data),

  listRuns: (limit = 50) =>
    api.get<WorkflowRun[]>(`/api/workflows/runs?limit=${limit}`).then(r => r.data),

  getRun: (id: string) =>
    api.get<WorkflowRun>(`/api/workflows/runs/${encodeURIComponent(id)}`).then(r => r.data),

  approve: (runId: string, stepId: string, decision: 'approve' | 'reject') =>
    api.post<WorkflowRun>(`/api/workflows/runs/${encodeURIComponent(runId)}/approve`,
      { step_id: stepId, decision }).then(r => r.data),
}
