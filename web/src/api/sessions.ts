// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export interface SessionInfo {
  session_id: string
  user_id: string
  channel: string
  created_at: number   // Unix seconds
  last_active: number  // Unix seconds
  message_count: number
}

export const sessionsApi = {
  list: () =>
    api.get<SessionInfo[]>('/api/sessions').then((r) => r.data),

  evict: (id: string) =>
    api.delete(`/api/sessions/${id}`),
}
