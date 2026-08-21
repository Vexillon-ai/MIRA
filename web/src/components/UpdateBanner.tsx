// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/UpdateBanner.tsx
//
// admin-only "new MIRA version available" banner. Polls
// /api/admin/update-check on a long interval; renders a slim bar above
// the page when the upstream Releases API reports a newer tag.
//
// Disabled-by-default server-side (config.server.update_check.enabled),
// admin-only client-side. Non-admins get no extra request; the
// endpoint returns 403 for them anyway.

import { useMutation, useQuery } from '@tanstack/react-query'
import { Sparkles, X as XIcon, Download, RotateCw } from 'lucide-react'
import { useState } from 'react'
import { api } from '@/api/client'
import { driveUpgrade, phaseLabel, type UpgradeStatus } from '@/api/upgradeReload'
import { useAuthStore } from '@/store/authStore'
import styles from './UpdateBanner.module.css'

interface UpdateCheckResponse {
  enabled:         boolean
  current:         string
  latest?:         string
  latest_name?:    string | null
  newer_available: boolean
  release_url?:    string | null
}

// Key used to remember which version the user has dismissed. Re-shows
// the banner when a *newer* version lands (we compare strings, not
// semver — good enough for "did I already dismiss this exact release").
const DISMISSED_KEY = 'mira.updateBanner.dismissed'

export default function UpdateBanner() {
  const user = useAuthStore((s) => s.user)
  const isAdmin = user?.role === 'admin'
  const [dismissed, setDismissed] = useState<string | null>(() => {
    try { return localStorage.getItem(DISMISSED_KEY) } catch { return null }
  })

  const { data } = useQuery<UpdateCheckResponse>({
    queryKey: ['update-check'],
    queryFn:  () => api.get('/api/admin/update-check').then((r) => r.data),
    enabled:  isAdmin,
    // 6 hours. The endpoint is admin-only and the data churns slowly;
    // polling more often is wasteful and hammers the upstream API.
    staleTime: 6 * 60 * 60 * 1000,
    refetchInterval: 6 * 60 * 60 * 1000,
    // Silent failure — the endpoint is best-effort. A misconfigured
    // source_url or unreachable Releases API shouldn't bleed into the UI.
    retry: false,
  })

  // One-click upgrade: POST kicks off download→verify→swap→restart server-side
  // (202), then we DRIVE it — polling the server's real phase/progress and only
  // reloading once it's back on the new version.
  const [progress, setProgress] = useState<UpgradeStatus | null>(null)
  const [driving, setDriving]   = useState(false)
  const [upgradeError, setUpgradeError] = useState<string | null>(null)

  const startDrive = async () => {
    setDriving(true)
    setUpgradeError(null)
    const res = await driveUpgrade(data?.current ?? '', setProgress)
    if (!res.ok) {
      setUpgradeError(res.error
        ? res.error.charAt(0).toUpperCase() + res.error.slice(1) + '.'
        : 'This is taking longer than expected — it may still be downloading on a slow connection.')
    }
    setDriving(false)
  }

  const upgradeMut = useMutation({
    mutationFn: () => api.post('/api/admin/upgrade').then((r) => r.data),
    onSuccess:  () => { void startDrive() },
    onError: (e) => {
      // 409 = an upgrade is already running (e.g. a re-click). Attach to the
      // in-flight job instead of erroring — the server refuses to start a second.
      const status = (e as { response?: { status?: number } })?.response?.status
      if (status === 409) void startDrive()
    },
  })

  // While upgrading, show the real phase + download progress. The page reloads
  // itself once the new build answers; a manual fallback stays available.
  if (driving) {
    const pct = progress && progress.bytes_total > 0
      ? Math.min(100, Math.round((progress.bytes_done / progress.bytes_total) * 100))
      : null
    return (
      <div className={styles.banner} role="status">
        <RotateCw size={14} />
        <span className={styles.text}>
          {upgradeError
            ? <>Upgrade status: {upgradeError} You can also run <code>mira upgrade --binary</code> from a terminal.</>
            : <>Upgrading MIRA{progress?.target_version ? <> to <strong>{progress.target_version}</strong></> : ''}
                {' — '}{phaseLabel(progress?.phase)}{pct !== null ? ` (${pct}%)` : ''}.{' '}
                This can take a few minutes on a slow connection — no need to re-click.</>}
        </span>
        <button className={styles.link} onClick={() => window.location.reload()}>
          Reload now
        </button>
      </div>
    )
  }

  if (!isAdmin || !data?.enabled || !data.newer_available) return null
  if (data.latest && data.latest === dismissed) return null

  return (
    <div className={styles.banner} role="status">
      <Sparkles size={14} />
      <span className={styles.text}>
        MIRA <strong>{data.latest}</strong> is available — you're on {data.current}.
        {data.latest_name && <em className={styles.name}> ({data.latest_name})</em>}
        {upgradeMut.isError && (
          <em className={styles.name}> — upgrade failed to start; try the CLI: mira upgrade --binary</em>
        )}
      </span>
      <button
        className={styles.link}
        disabled={upgradeMut.isPending}
        onClick={() => {
          if (window.confirm(`Upgrade MIRA to ${data.latest}? It will download, verify the signature, swap the binary, and restart.`)) {
            upgradeMut.mutate()
          }
        }}
        title="Download, verify, and install the new version, then restart"
      >
        <Download size={12} /> {upgradeMut.isPending ? 'Starting…' : 'Upgrade now'}
      </button>
      {data.release_url && (
        <a
          className={styles.link}
          href={data.release_url}
          target="_blank"
          rel="noreferrer"
        >
          View release →
        </a>
      )}
      <button
        className={styles.close}
        onClick={() => {
          if (data.latest) {
            try { localStorage.setItem(DISMISSED_KEY, data.latest) } catch { /* OK */ }
            setDismissed(data.latest)
          }
        }}
        title="Dismiss until a newer release"
      >
        <XIcon size={12} />
      </button>
    </div>
  )
}
