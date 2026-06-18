// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

/**
 * Capability RBAC profile. Each allow-list axis is `null`/absent when the
 * profile doesn't restrict it (default-allow). Mirrors the Rust
 * `CapabilityProfile` (serde skips `None` fields).
 */
export interface CapabilityProfile {
  providers?:           string[] | null
  models?:              string[] | null
  tools?:               string[] | null
  channels?:            string[] | null
  max_task_budget_usd?: number | null
  session_budget_usd?:  number | null
}

export const capabilitiesApi = {
  getGroup: (id: string) =>
    api.get<CapabilityProfile>(`/api/groups/${encodeURIComponent(id)}/capabilities`).then(r => r.data),
  setGroup: (id: string, profile: CapabilityProfile) =>
    api.put<CapabilityProfile>(`/api/groups/${encodeURIComponent(id)}/capabilities`, profile).then(r => r.data),

  getUser: (id: string) =>
    api.get<CapabilityProfile>(`/api/users/${encodeURIComponent(id)}/capabilities`).then(r => r.data),
  setUser: (id: string, profile: CapabilityProfile) =>
    api.put<CapabilityProfile>(`/api/users/${encodeURIComponent(id)}/capabilities`, profile).then(r => r.data),

  /** The caller's own effective (merged) profile — used to filter pickers. */
  mine: () =>
    api.get<CapabilityProfile>('/api/me/capabilities').then(r => r.data),
}

/** True if the effective profile permits selecting (provider, model). */
export function capsAllowModel(
  caps: CapabilityProfile | undefined,
  provider: string,
  model: string,
): boolean {
  if (!caps) return true
  const okProvider = !caps.providers || caps.providers.includes(provider)
  const okModel    = !caps.models    || caps.models.includes(model)
  return okProvider && okModel
}

/** True if the effective profile permits using a channel (e.g. 'signal'). */
export function capsAllowChannel(
  caps: CapabilityProfile | undefined,
  channel: string,
): boolean {
  if (!caps || !caps.channels) return true
  return caps.channels.includes(channel)
}
