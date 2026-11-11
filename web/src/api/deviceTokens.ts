// SPDX-License-Identifier: AGPL-3.0-or-later

// per-device token management API.

import { api } from './client'

export interface DeviceTokenSummary {
  token_id:     string
  name:         string
  scopes:       string[]
  created_at:   number        // unix millis
  expires_at:   number | null
  last_used_at: number | null
}

export interface CreatedDeviceToken {
  token_id: string
  /** The raw `mdt_…` token — shown exactly once, never retrievable again. */
  token:    string
  name:     string
  scopes:   string[]
}

export interface CreateDeviceTokenBody {
  name:      string
  scopes?:   string[]
  ttl_secs?: number | null
}

export const deviceTokensApi = {
  list: (userId: string) =>
    api.get<DeviceTokenSummary[]>(`/api/users/${userId}/device-tokens`).then((r) => r.data),

  create: (userId: string, body: CreateDeviceTokenBody) =>
    api.post<CreatedDeviceToken>(`/api/users/${userId}/device-tokens`, body).then((r) => r.data),

  revoke: (userId: string, tokenId: string) =>
    api.delete(`/api/users/${userId}/device-tokens/${tokenId}`),
}
