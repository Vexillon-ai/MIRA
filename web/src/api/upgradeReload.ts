// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/api/upgradeReload.ts
//
// Drives the in-app upgrade UX. The old version polled ONLY for a version change
// with a fixed 150 s budget — but a real upgrade is dominated by download +
// verify + swap of a ~51 MB asset (often >3 min on this host), so it timed out a
// minute before the swap landed and the banner looked frozen forever even though
// the upgrade succeeded every time.
//
// Now we poll the server's own upgrade-status job (real phases + bytes), and only
// fall back to the version-poll for the restart tail — where the server is down
// and can't report anything. Failure is declared when the SERVER says so (or a
// long no-progress stall), never on a guessed total elapsed time.

import { api } from '@/api/client'
import { providersApi } from '@/api/providers'

export interface UpgradeStatus {
  in_progress:     boolean
  phase:           string   // idle|resolving|downloading|verifying|extracting|snapshotting|swapping|restarting|done|failed
  target_version?: string
  bytes_done:      number
  bytes_total:     number
  error?:          string
  started_at_ms:   number
  updated_at_ms:   number
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

/** A friendly label for each server-reported upgrade phase. */
export function phaseLabel(phase?: string): string {
  switch (phase) {
    case 'resolving':    return 'checking the release'
    case 'downloading':  return 'downloading'
    case 'verifying':    return 'verifying signature'
    case 'extracting':   return 'extracting'
    case 'snapshotting': return 'saving a rollback snapshot'
    case 'swapping':     return 'installing'
    case 'restarting':   return 'restarting'
    case 'done':         return 'finishing up'
    default:             return 'starting'
  }
}

/**
 * Poll `status` until the server has restarted onto a version other than
 * `currentVersion`, then `window.location.reload()`. Errors (server mid-restart)
 * are ignored. Resolves `true` once it reloads; `false` on timeout.
 *
 * Only used for the RESTART tail — the swap has already happened by the time we
 * call it — so the budget is generous.
 */
export async function waitForNewVersionThenReload(
  currentVersion: string,
  timeoutMs = 300_000,
): Promise<boolean> {
  const start = Date.now()
  while (Date.now() - start < timeoutMs) {
    await sleep(2500)
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

/**
 * Drive an in-app upgrade to completion, reporting real progress via `onProgress`.
 *
 * Phase 1 — poll `/api/admin/upgrade/status` for the server's phase + bytes until
 * it reaches `restarting`/`done`, reports `failed`, or drops (the swap-then-
 * restart makes it unreachable). Phase 2 — wait for the new version and reload.
 *
 * Resolves `{ ok, error? }`. `ok:false` means the server reported a failure or the
 * upgrade stalled (no progress) — the caller shows the error instead of stalling.
 */
export async function driveUpgrade(
  currentVersion: string,
  onProgress: (s: UpgradeStatus | null) => void,
): Promise<{ ok: boolean; error?: string }> {
  const HARD_CAP_MS   = 20 * 60_000  // absolute backstop (very slow link)
  const STALL_MS      = 5 * 60_000   // no-progress → declare stalled
  const start         = Date.now()
  let   lastChangeAt  = Date.now()
  let   lastUpdatedMs = 0
  let   sawServer     = false

  while (Date.now() - start < HARD_CAP_MS) {
    await sleep(2000)
    let s: UpgradeStatus | null = null
    try {
      s = (await api.get('/api/admin/upgrade/status')).data as UpgradeStatus
      sawServer = true
    } catch {
      // Unreachable. If we already saw it working, this is the restart → tail.
      if (sawServer) break
      continue
    }
    onProgress(s)

    if (s.phase === 'failed') return { ok: false, error: s.error || 'upgrade failed' }
    if (s.phase === 'restarting' || s.phase === 'done') break

    // No-progress backstop — base failure on last-observed progress, not on
    // total elapsed time (a slow download is not a failure).
    if (s.updated_at_ms !== lastUpdatedMs) { lastUpdatedMs = s.updated_at_ms; lastChangeAt = Date.now() }
    if (Date.now() - lastChangeAt > STALL_MS) {
      return { ok: false, error: 'upgrade stalled — no progress for several minutes' }
    }
  }

  // Restart tail: the binary is swapped; wait for the new build to answer.
  const reloaded = await waitForNewVersionThenReload(currentVersion)
  return { ok: reloaded }
}
