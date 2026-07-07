// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/api/upgradeReload.ts
//
// After an in-app upgrade or rollback is triggered, the server swaps its binary
// and restarts (via the supervisor). This polls the status endpoint until it's
// back on a DIFFERENT version, then reloads the page itself — so the admin never
// has to click a "Reload" button, and the reload picks up the new build's assets.

import { providersApi } from '@/api/providers'

/**
 * Poll `status` until the server has restarted onto a version other than
 * `currentVersion`, then `window.location.reload()`. Errors (server mid-restart)
 * are ignored. Resolves `true` once it reloads; `false` on timeout so the caller
 * can fall back to a manual-reload hint.
 *
 * The default timeout is generous: a Windows SCM-recovery relaunch can take
 * ~15–30s, and a slow graceful drain adds to that.
 */
export async function waitForNewVersionThenReload(
  currentVersion: string,
  timeoutMs = 150_000,
): Promise<boolean> {
  const start = Date.now()
  while (Date.now() - start < timeoutMs) {
    await new Promise((r) => setTimeout(r, 2500))
    try {
      const s = await providersApi.status()
      if (s.version && s.version !== currentVersion) {
        window.location.reload()
        return true
      }
    } catch {
      // Server is restarting (connection refused / 5xx) — keep polling.
    }
  }
  return false
}
