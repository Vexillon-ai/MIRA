// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export interface EnableCompanionRequest {
  /** Another MIRA user to notify if the safety floor triggers. Optional for
   *  admins; required for non-admins. */
  safety_contact_user_id?: string | null
  /** Per-user cap on proactive check-ins per local day. */
  max_per_day?: number
  /** Turn the daily briefing on as part of enabling. */
  briefing_enabled?: boolean
  /** Local hour (0..=23) the briefing fires at. */
  briefing_hour?: number
}

export interface CompanionEnableResult {
  companion_active: boolean
  enabled: boolean
  hour: number
  safety_contact_user_id: string | null
  max_per_day: number | null
}

export const companionApi = {
  /** POST /api/me/companion/enable — self-serve enable used by the setup wizard. */
  enable: (body: EnableCompanionRequest) =>
    api.post<CompanionEnableResult>('/api/me/companion/enable', body).then((r) => r.data),
}

// ── Presence settings (the companion personality / rhythm tuning surface) ─────

/** Personality sliders, each 0..100. */
export interface PresenceTone {
  warmth: number
  playfulness: number
  verbosity: number
}

/** Care-net role: who the monitored person is. Tunes escalation framing + tone. */
export type CareRole = 'standard' | 'child' | 'elder'

/** Which kinds of proactive messages MIRA is allowed to send. */
export interface PresenceMessageMix {
  check_in: boolean
  joke: boolean
  status_update: boolean
  follow_up: boolean
  share: boolean
  encouragement: boolean
}

/** Full shape returned by GET /api/me/companion. */
export interface PresenceSettings {
  enabled: boolean
  active: boolean
  setup_completed: boolean
  paused_until_ms: number | null
  safety_contact_user_id: string | null
  /** Inclusive [start, end] "HH:MM" pairs MIRA must stay quiet during. */
  quiet_hours: [string, string][]
  preferred_channels: string[]
  daily_briefing_enabled: boolean
  daily_briefing_hour: number
  last_checkin_at_ms: number | null
  min_per_day: number
  max_per_day: number | null
  min_gap_minutes: number | null
  max_unanswered_checkins: number | null
  frequency_mode: 'fuzzy' | 'scheduled'
  /** "HH:MM" times used when frequency_mode === 'scheduled'. */
  scheduled_times: string[]
  tone: PresenceTone
  message_mix: PresenceMessageMix
  share_agent_activity: boolean
  /** Care-net (Pass 2): who the person is. */
  care_role: CareRole
  /** Whether the care arrangement has been disclosed to + acknowledged. */
  care_consent: boolean
}

/** Fields PUT /api/me/companion accepts (a partial update). Note this does NOT
 *  include `enabled` — turning the companion on goes through the setup wizard
 *  (or chat) because of the safety-contact gate. */
export type PresenceUpdate = Partial<
  Pick<
    PresenceSettings,
    | 'frequency_mode'
    | 'min_per_day'
    | 'max_per_day'
    | 'min_gap_minutes'
    | 'scheduled_times'
    | 'tone'
    | 'message_mix'
    | 'share_agent_activity'
    | 'quiet_hours'
    | 'preferred_channels'
    | 'daily_briefing_enabled'
    | 'daily_briefing_hour'
    | 'safety_contact_user_id'
    | 'care_role'
    | 'care_consent'
  >
>

/** GET /api/me/companion — current presence settings. */
export function getPresence(): Promise<PresenceSettings> {
  return api.get<PresenceSettings>('/api/me/companion').then((r) => r.data)
}

/** PUT /api/me/companion — partial update; returns the full updated settings. */
export function updatePresence(body: PresenceUpdate): Promise<PresenceSettings> {
  return api.put<PresenceSettings>('/api/me/companion', body).then((r) => r.data)
}
