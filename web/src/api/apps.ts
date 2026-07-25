// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

// The apps framework (Guardian Phase 2). An "app" is an installed package with an
// `app` component: it ships its own UI, exposes tools MIRA can call, and emits
// declared events onto the shared bus. Apps are listed via the packages API and
// filtered client-side (an app = a package whose manifest has an `app` component).

export const appsApi = {
  /**
   * Fetch an app's UI entry HTML (admin-authenticated via the api client's Bearer
   * token). Rendered into a sandboxed `<iframe srcdoc>` so it runs at an opaque
   * origin — it can't read MIRA's token and reaches MIRA only via postMessage.
   */
  async getUi(id: string): Promise<string> {
    const { data } = await api.get<string>(
      `/api/admin/apps/${encodeURIComponent(id)}/ui/`,
      { responseType: 'text' },
    )
    return data
  },

  /** Emit one of the app's declared events onto the shared bus. */
  async emit(id: string, event: string, payload: unknown): Promise<{ emitted: boolean; event: string; domain: string; severity: string }> {
    const { data } = await api.post(
      `/api/admin/apps/${encodeURIComponent(id)}/emit`,
      { event, payload },
    )
    return data
  },

  /**
   * The app's config schema + current NON-secret values + which secret keys are
   * set (secret values are never returned). Empty schema = nothing to configure.
   */
  async getConfig(id: string): Promise<AppConfig> {
    const { data } = await api.get<AppConfig>(
      `/api/admin/apps/${encodeURIComponent(id)}/config`,
    )
    return data
  },

  /**
   * Set app config. Keys must be declared in the schema; declared-`secret` keys
   * are routed to the encrypted vault, the rest to the package's config.
   */
  async putConfig(id: string, config: Record<string, unknown>): Promise<{ ok: boolean; id: string }> {
    const { data } = await api.put(
      `/api/admin/apps/${encodeURIComponent(id)}/config`,
      { config },
    )
    return data
  },
}

export interface AppConfigField {
  key: string
  label?: string
  help?: string
  type?: string
  secret?: boolean
  required?: boolean
}

export interface AppConfig {
  id: string
  config_schema: AppConfigField[]
  values: Record<string, unknown>
  secrets_set: string[]
}
