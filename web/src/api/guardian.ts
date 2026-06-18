// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

/** Mirrors GuardianActionKind in src/agent/guardian_actions.rs. */
export type GuardianActionKind =
  | 'rerun_audit' | 'restart_bridge' | 'requeue_automation' | 'trim_logs'

/** Mirrors GuardianActionStatus. */
export type GuardianActionStatus = 'pending' | 'declined' | 'executed' | 'failed'

/** Mirrors GuardianAction. */
export interface GuardianAction {
  id:         string
  kind:       GuardianActionKind
  target:     string | null
  reason:     string
  status:     GuardianActionStatus
  created_at: number
  decided_at: number | null
  result:     string | null
}

interface DecisionResult {
  id:      string
  status:  string
  result?: string
  error?:  string
}

/** Mirrors the GET /api/guardian/provision/status payload. */
export interface ProvisionStatus {
  guardian_mode:      string
  local_model_ok:     boolean
  model_check:        string
  guardian_alias_set: boolean
  ollama: {
    url:               string
    reachable:         boolean
    version:           string | null
    recommended_model: string
    model_present:     boolean
  }
  next_step: string
}

/** Mirrors WatchStatus in src/agent/guardian.rs (proactive watch-loop telemetry). */
export interface GuardianWatch {
  interval_secs:        number
  last_run_at:          number | null
  last_alert_at:        number | null
  last_alert_summary:   string | null
  last_alert_detectors: number
  alerts_total:         number
}

/** Mirrors the GET /api/guardian/status payload. */
export interface GuardianStatus {
  mode:                string   // "Off" | "Monitor" | "Active"
  local_model_ok:      boolean
  model_check:         string
  guardian_alias_set:  boolean
  watch_interval_secs: number
  isolation_dry_run:   boolean
  watch:               GuardianWatch
  recent_actions:      GuardianAction[]
}

export const guardianApi = {
  /** Always-on status: mode, local-model verdict, watch-loop liveness, recent actions. */
  status: () =>
    api.get<GuardianStatus>('/api/guardian/status').then(r => r.data),

  /** Pending action proposals awaiting operator decision. */
  pending: () =>
    api.get<GuardianAction[]>('/api/guardian/actions', { params: { status: 'pending' } })
      .then(r => r.data),

  /** What the Guardian needs for a local model (P2b). */
  provisionStatus: () =>
    api.get<ProvisionStatus>('/api/guardian/provision/status').then(r => r.data),

  /** Pull + bind a local Ollama model for the Guardian (background). */
  provision: () =>
    api.post<{ status: string; model: string; note?: string; error?: string }>(
      '/api/guardian/provision').then(r => r.data),

  /** Approve a proposal → server executes the bounded action. */
  approve: (id: string) =>
    api.post<DecisionResult>(`/api/guardian/actions/${id}/approve`).then(r => r.data),

  /** Decline a proposal → never executes. */
  decline: (id: string, note?: string) =>
    api.post<DecisionResult>(`/api/guardian/actions/${id}/decline`, note ? { note } : {})
      .then(r => r.data),
}
