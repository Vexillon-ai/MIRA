// SPDX-License-Identifier: AGPL-3.0-or-later

import { api, getAccessToken } from './client'

/** Mirrors HealthLevel in src/health/mod.rs. */
export type HealthLevel = 'green' | 'yellow' | 'red'

/** Mirrors ActionPolicy in src/health/mod.rs. */
export type ActionPolicy = 'disabled' | 'notify_only' | 'auto_cleanup'

/** Mirrors DetectorAnalytics in src/health/mod.rs (0.110.0 / slice 5c). */
export interface DetectorAnalytics {
  forecast_red_in_hours?: number | null
  anomaly_z?:             number | null
  correlated_detectors?:  string[]
}

/** Mirrors DetectorReport in src/health/mod.rs. */
export interface DetectorReport {
  name:                  string
  level:                 HealthLevel
  message:               string
  value?:                number | null
  payload:               unknown
  auto_action_eligible:  boolean
  analytics?:            DetectorAnalytics | null
}

/** 0.110.0 — slice 5a custom SQL detector. */
export interface CustomDetectorRow {
  name:        string
  description?: string | null
  target_db:   string
  sql:         string
  yellow_at?:  number | null
  red_at?:     number | null
  direction:   string
  enabled:     boolean
  created_at:  number
  updated_at:  number
  updated_by:  string
}

/** 0.110.0 — slice 5b webhook target (response shape with secret stripped). */
export interface WebhookListRow {
  id:           string
  url:          string
  has_secret:   boolean
  levels_csv?:  string | null
  enabled:      boolean
  description?: string | null
  created_at:   number
  updated_at:   number
  updated_by:   string
  last_fire_at?: number | null
  last_status?:  number | null
  last_error?:   string | null
}

/** 0.110.0 — slice 5a threshold override row. */
export interface ThresholdRow {
  detector_name: string
  yellow_at?:    number | null
  red_at?:       number | null
  direction:     string
  updated_at:    number
  updated_by:    string
}

/** Mirrors HealthSnapshot in src/health/mod.rs. */
export interface HealthSnapshot {
  taken_at:    number
  duration_ms: number
  reports:     DetectorReport[]
}

/** Mirrors SnapshotSummary in src/health/store.rs. */
export interface SnapshotSummary {
  taken_at:               number
  duration_ms:            number
  triggered_signal_count: number
  worst_level:            string
  incident_id?:           string | null
}

/** Mirrors DetectorConfigEntry in src/server/handlers/health_dashboard.rs. */
export interface DetectorConfigEntry {
  detector_name: string
  policy:        ActionPolicy
  note?:         string | null
  updated_at?:   number | null
  updated_by?:   string | null
  overridden:    boolean
  /** 0.109.0 — when in the future, the detector is currently snoozed. */
  snooze_until?: number | null
}

/** Mirrors IpBanRow in src/server/handlers/health_dashboard.rs. */
export interface IpBanRow {
  ip:           string
  banned_until: number
  reason?:      string | null
}

/** Mirrors WatchdogIncident — re-using the same shape as @/api/watchdog. */
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
  analysis_status:        string
  analysis_started_at?:   number | null
  analysis_completed_at?: number | null
  conversation_id?:       string | null
  analysis_response?:     string | null
}

export interface Degradation {
  subsystem:  string
  label:      string
  from:       string
  to:         string
  reason:     string
  persistent: boolean
  first_at:   number
  last_at:    number
  count:      number
}

export const healthApi = {
  snapshot: () =>
    api.get<HealthSnapshot>('/api/health/snapshot').then(r => r.data),

  /** Live subsystems currently on a degraded fallback path. */
  degradations: () =>
    api.get<Degradation[]>('/api/health/degradations').then(r => r.data),

  history: (hours = 24) =>
    api.get<SnapshotSummary[]>('/api/health/history', { params: { hours } }).then(r => r.data),

  incidents: (limit = 50) =>
    api.get<WatchdogIncident[]>('/api/health/incidents', { params: { limit } }).then(r => r.data),

  config: () =>
    api.get<DetectorConfigEntry[]>('/api/health/config').then(r => r.data),

  setPolicy: (detector_name: string, policy: ActionPolicy, note?: string, snooze_secs?: number) =>
    api.put<{ saved?: boolean; reset?: boolean; snooze_until?: number | null }>(
      '/api/health/config',
      { detector_name, policy, note, snooze_secs },
    ).then(r => r.data),

  runNow: () =>
    api.post<{ queued: boolean }>('/api/health/run-now').then(r => r.data),

  ipBans: () =>
    api.get<IpBanRow[]>('/api/health/ip-bans').then(r => r.data),

  liftIpBan: (ip: string) =>
    api.post<{ lifted: boolean }>(
      `/api/health/ip-bans/${encodeURIComponent(ip)}/lift`,
    ).then(r => r.data),

  // ── 0.110.0 — slice 5 surfaces ─────────────────────────────────

  listCustomDetectors: () =>
    api.get<CustomDetectorRow[]>('/api/health/custom-detectors').then(r => r.data),

  upsertCustomDetector: (body: Partial<CustomDetectorRow> & { name: string; target_db: string; sql: string }) =>
    api.put<{ saved: boolean }>('/api/health/custom-detectors', body).then(r => r.data),

  deleteCustomDetector: (name: string) =>
    api.delete<{ deleted: boolean }>(`/api/health/custom-detectors/${encodeURIComponent(name)}`).then(r => r.data),

  testCustomDetector: (target_db: string, sql: string) =>
    api.post<{ level: HealthLevel; value?: number; message: string; payload: unknown }>(
      '/api/health/custom-detectors/test',
      { target_db, sql },
    ).then(r => r.data),

  listWebhooks: () =>
    api.get<WebhookListRow[]>('/api/health/webhooks').then(r => r.data),

  upsertWebhook: (body: { id?: string; url: string; secret?: string; levels_csv?: string; enabled?: boolean; description?: string }) =>
    api.put<{ saved: boolean; id: string }>('/api/health/webhooks', body).then(r => r.data),

  deleteWebhook: (id: string) =>
    api.delete<{ deleted: boolean }>(`/api/health/webhooks/${encodeURIComponent(id)}`).then(r => r.data),

  listThresholds: () =>
    api.get<ThresholdRow[]>('/api/health/thresholds').then(r => r.data),

  upsertThreshold: (body: { detector_name: string; yellow_at?: number | null; red_at?: number | null; direction?: string }) =>
    api.put<{ saved?: boolean; cleared?: boolean }>('/api/health/thresholds', body).then(r => r.data),

  // ── 0.111.0 — task artifacts ───────────────────────────────────

  listArtifacts: () =>
    api.get<ArtifactListEntry[]>('/api/health/artifacts').then(r => r.data),

  deleteArtifact: (name: string) =>
    api.delete<{ deleted: boolean }>(`/api/health/artifacts/${encodeURIComponent(name)}`).then(r => r.data),

  migrateArtifacts: () =>
    api.post<{ moved: number; details: { from: string; to: string }[] }>(
      '/api/health/artifacts/migrate',
    ).then(r => r.data),

  // ── A4 — browse + open a task's files ──────────────────────────
  listTaskFiles: (taskId: string) =>
    api.get<{ task_id: string; files: TaskFileEntry[] }>(
      `/api/admin/tasks/${encodeURIComponent(taskId)}/files`,
    ).then(r => r.data.files),
}

/** One file inside a task's artifact dir. */
export interface TaskFileEntry {
  path:       string
  size_bytes: number
}

/** Direct URL to serve/preview/download a task file. The serve endpoint is
 *  admin-gated and `<img>`/`<a>` can't set headers, so the JWT rides as
 *  `?token=` (supported by the auth extractor's EventSource fallback). */
export function taskFileUrl(taskId: string, path: string, download = false): string {
  const token = getAccessToken()
  const qs = new URLSearchParams({ path })
  if (download) qs.set('download', '1')
  if (token) qs.set('token', token)
  return `/api/admin/tasks/${encodeURIComponent(taskId)}/file?${qs.toString()}`
}

/** Task-artifact entry returned by /api/health/artifacts. */
export interface ArtifactListEntry {
  name:           string
  skill:          string
  size_bytes:     number
  absolute_path:  string
  manifest: {
    task_id:       string
    skill_id:      string
    user_id?:      string | null
    channel?:      string | null
    brief_excerpt: string
    created_at:    number
    finished_at?:  number | null
    status:        string
    slug?:         string | null
  }
}
