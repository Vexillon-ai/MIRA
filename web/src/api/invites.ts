// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'
import type { User } from './types'

export interface Invite {
  id:         string
  created_by: string
  role:       string
  email_hint: string | null
  max_uses:   number
  used_count: number
  expires_at: number | null
  revoked:    boolean
  created_at: number
}

export interface CreateInviteRequest {
  role?:             string
  email_hint?:       string
  max_uses?:         number
  expires_in_hours?: number
}

export interface CreateInviteResponse {
  id:    string
  token: string
  url:   string
  role:  string
}

export const invitesApi = {
  list:   () => api.get<Invite[]>('/api/invites').then((r) => r.data),
  create: (body: CreateInviteRequest) =>
    api.post<CreateInviteResponse>('/api/invites', body).then((r) => r.data),
  revoke: (id: string) => api.delete(`/api/invites/${encodeURIComponent(id)}`),

  // Pending-approval queue
  pending: () => api.get<User[]>('/api/admin/users/pending').then((r) => r.data),
  approve: (userId: string) =>
    api.post(`/api/users/${encodeURIComponent(userId)}/approve`).then((r) => r.data),
}
