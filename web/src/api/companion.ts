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
