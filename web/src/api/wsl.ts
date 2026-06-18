// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

/** A configured URL unreachable at its IP but reachable via windows-host. */
export interface MisroutedUrl {
  path:      string
  current:   string
  suggested: string
}

export interface HostUrlCheck {
  is_wsl:   boolean
  findings: MisroutedUrl[]
}

export const wslApi = {
  /** Scan the live config for Windows-host URLs unreachable from WSL. */
  check: () =>
    api.get<HostUrlCheck>('/api/wsl/host-url-check').then(r => r.data),

  /** Rewrite the misrouted URLs to windows-host (server re-scans; safe path). */
  fix: () =>
    api.post<{ changed: MisroutedUrl[]; note: string }>('/api/wsl/fix-host-urls').then(r => r.data),
}
