// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/UpdatesCard.tsx
//
// Admin-only "Updates" status + actions, rendered in Settings. Shows the
// current vs latest version + last-checked time, a "Check now" button, and —
// where the host supports it — "Upgrade now" and "Roll back". On hosts that
// can't self-upgrade (Docker, unsupervised) it shows the server's platform
// guidance instead of a button. All actions are admin-only server-side.

import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Download, RotateCcw, RefreshCw } from 'lucide-react'
import { api } from '@/api/client'
import styles from './UpdatesCard.module.css'

interface UpdateInfo {
  enabled:           boolean
  current:           string
  latest?:           string
  newer_available:   boolean
  release_url?:      string | null
  last_checked?:     string
  host_kind?:        string
  can_self_upgrade?: boolean
  upgrade_guidance?: string | null
}
interface RollbackInfo {
  current:   string
  snapshots: { version: string; has_config: boolean }[]
}

function ago(iso?: string): string {
  if (!iso) return 'never'
  const t = new Date(iso).getTime()
  if (Number.isNaN(t)) return 'never'
  const s = Math.max(0, Math.floor((Date.now() - t) / 1000))
  if (s < 60)    return 'just now'
  if (s < 3600)  return `${Math.floor(s / 60)}m ago`
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`
  return `${Math.floor(s / 86400)}d ago`
}

export default function UpdatesCard() {
  const qc = useQueryClient()
  const [msg, setMsg] = useState<string | null>(null)

  const info = useQuery<UpdateInfo>({
    queryKey: ['update-check'],
    queryFn:  () => api.get('/api/admin/update-check').then((r) => r.data),
    refetchOnWindowFocus: false,
  })
  const rb = useQuery<RollbackInfo>({
    queryKey: ['rollback-list'],
    queryFn:  () => api.get('/api/admin/rollback').then((r) => r.data),
    refetchOnWindowFocus: false,
  })

  const checkNow = useMutation({
    mutationFn: () => api.get('/api/admin/update-check?force=true').then((r) => r.data),
    onSuccess:  (d) => { qc.setQueryData(['update-check'], d); setMsg(null) },
  })
  const upgrade = useMutation({
    mutationFn: () => api.post('/api/admin/upgrade').then((r) => r.data),
    onSuccess:  (d) => setMsg(d?.message ?? 'Upgrade started — MIRA will restart shortly.'),
    onError:    () => setMsg('Upgrade failed to start — try `mira upgrade` from a terminal.'),
  })
  const rollback = useMutation({
    mutationFn: (version: string) => api.post('/api/admin/rollback', { version }).then((r) => r.data),
    onSuccess:  (d) => setMsg(d?.message ?? 'Rollback started — MIRA will restart shortly.'),
    onError:    (e: any) => setMsg(e?.response?.data?.error ?? 'Rollback failed to start.'),
  })

  const d = info.data
  const snaps = rb.data?.snapshots ?? []
  // Only offer a rollback to a version OTHER than the one we're running.
  const rollbackTarget = snaps.find((s) => s.version !== d?.current)

  return (
    <div className={styles.card}>
      <div className={styles.status}>
        {info.isLoading ? 'Checking…'
          : d?.enabled === false ? <>Automatic checks are off. Current version <b>v{d?.current}</b>.</>
          : d?.newer_available ? <><b>Update available: v{d.latest}</b> — you're on v{d.current}. <span className={styles.dim}>· checked {ago(d.last_checked)}</span></>
          : <>You're up to date — <b>v{d?.current}</b>. <span className={styles.dim}>· checked {ago(d?.last_checked)}</span></>}
      </div>

      <div className={styles.actions}>
        <button className={styles.btn} onClick={() => checkNow.mutate()} disabled={checkNow.isPending}>
          <RefreshCw size={13} /> {checkNow.isPending ? 'Checking…' : 'Check now'}
        </button>

        {d?.newer_available && d?.can_self_upgrade && (
          <button
            className={`${styles.btn} ${styles.primary}`}
            disabled={upgrade.isPending}
            onClick={() => {
              if (confirm(`Upgrade to v${d.latest}? MIRA will download, verify, swap and restart.`)) upgrade.mutate()
            }}
          >
            <Download size={13} /> {upgrade.isPending ? 'Starting…' : `Upgrade now to v${d.latest}`}
          </button>
        )}

        {rollbackTarget && d?.can_self_upgrade && (
          <button
            className={styles.btn}
            disabled={rollback.isPending}
            onClick={() => {
              if (confirm(`Roll back to v${rollbackTarget.version}? MIRA restores the previous binary + config and restarts.`))
                rollback.mutate(rollbackTarget.version)
            }}
          >
            <RotateCcw size={13} /> {rollback.isPending ? 'Starting…' : `Roll back to v${rollbackTarget.version}`}
          </button>
        )}

        {d?.release_url && (
          <a className={styles.link} href={d.release_url} target="_blank" rel="noreferrer">Release notes ↗</a>
        )}
      </div>

      {/* Platform guidance when we can't self-upgrade (Docker / unsupervised). */}
      {d?.newer_available && !d?.can_self_upgrade && d?.upgrade_guidance && (
        <div className={styles.guidance}>{d.upgrade_guidance}</div>
      )}

      {msg && <div className={styles.msg}>{msg}</div>}
    </div>
  )
}
