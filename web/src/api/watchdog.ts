// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

/** Mirrors WatchdogIncident in src/automations/store.rs. */
export interface WatchdogIncident {
  id:                     string
  user_id:                string
  fingerprint:            string
  severity:               string
  source:                 string
  module:                 string
  message:                string
  payload_json:           string
  created_at:             number
  /** "none" | "queued" | "completed" | "failed". */
  analysis_status:        string
  analysis_started_at?:   number | null
  analysis_completed_at?: number | null
  conversation_id?:       string | null
  analysis_response?:     string | null
}

/** Mirrors AnalyzeResp in src/server/handlers/watchdog.rs. */
export interface AnalyzeIncidentResp {
  incident_id:     string
  conversation_id: string
  message:         string
}

export const watchdogApi = {
  get:  (id: string) =>
    api.get<WatchdogIncident>(`/api/watchdog/incidents/${encodeURIComponent(id)}`).then(r => r.data),
  list: (params: { limit?: number; user_id?: string } = {}) =>
    api.get<WatchdogIncident[]>('/api/watchdog/incidents', { params }).then(r => r.data),
  analyze: (id: string) =>
    api.post<AnalyzeIncidentResp>(`/api/watchdog/incidents/${encodeURIComponent(id)}/analyze`).then(r => r.data),
}
