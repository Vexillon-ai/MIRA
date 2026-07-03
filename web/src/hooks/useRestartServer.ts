// SPDX-License-Identifier: AGPL-3.0-or-later

import { useMutation, useQueryClient, type QueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { channelAccountsApi } from '@/api/channelAccounts'
import { providersApi } from '@/api/providers'

/**
 * Shared "Restart server" mutation.
 *
 * The server's graceful shutdown can hang on long-lived SSE streams
 * (notifications, logs) — the backend force-exits after a short grace
 * window, so the user should expect a brief gap before reconnection.
 *
 * The toast text branches on `supervised`: under a supervisor (systemd,
 * Docker, launchd) the process comes back on its own; otherwise the user
 * has to relaunch manually, and the toast says so.
 *
 * When supervised, kicks off a background poll loop that watches for the
 * server to drop offline and then come back, posting a "Server back
 * online" toast and invalidating React Query caches so the UI re-fetches.
 */
export function useRestartServer(opts?: {
  supervised?: boolean
  onSuccess?: () => void
}) {
  const supervised = opts?.supervised ?? false
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: async () => {
      await channelAccountsApi.restartServer()
      if (supervised) {
        // Toast the kickoff so the user sees feedback immediately,
        // then wait for the server to actually come back. Awaiting
        // the poll keeps `isPending=true` for the whole window — the
        // Restart button stays "Restarting…" with its spinner until
        // the server is reachable again, instead of flickering back
        // to "Restart server" the moment the 202 lands.
        toast.success('Restart scheduled — the server will come back shortly.')
        await pollUntilBackOnline(queryClient)
      } else {
        toast.success('Server stopped. Relaunch MIRA to continue.')
      }
    },
    onSuccess: () => { opts?.onSuccess?.() },
    onError: () => toast.error(supervised ? 'Restart failed' : 'Stop failed'),
  })
}

/**
 * Watch /api/status until we see the server go down (request fails) and
 * then come back up (request succeeds again). Toast on the transition.
 *
 * Bounded by a 60s wall-clock timeout so a permanently-broken supervisor
 * doesn't leave the loop running forever. 60s (not 30s) because a MIRA
 * booting with many features enabled — supervised Chatterbox, MCP servers,
 * channel pollers — can take a good while to rebind the port, and a
 * premature "didn't come back" error alarms the user when it's still coming
 * up. Uses a 1.5s poll interval — fast enough that the gap between "old
 * process exits" and "new process binds the port" usually shows up as at
 * least one failed poll, slow enough not to hammer the server during normal
 * operation.
 */
async function pollUntilBackOnline(qc: QueryClient): Promise<void> {
  const startedAt = Date.now()
  const TIMEOUT_MS = 60_000
  const POLL_MS    = 1_500
  let sawOffline   = false
  // Soft delay before the first probe — the API handler waits 250ms
  // before signalling shutdown, plus axum needs a moment to drain.
  await new Promise(r => setTimeout(r, 750))

  while (Date.now() - startedAt < TIMEOUT_MS) {
    try {
      await providersApi.status()
      if (sawOffline) {
        toast.success('Server back online.')
        // Refresh everything that depends on server state.
        qc.invalidateQueries()
        return
      }
    } catch {
      sawOffline = true
    }
    await new Promise(r => setTimeout(r, POLL_MS))
  }
  toast.error("Server didn't come back within 60s — check `mira status`.")
}
