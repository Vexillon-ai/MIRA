// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/BackupRestore.tsx
//
// Q1.5 + follow-on hardening — backup/restore panel for Settings → Server.
//
// Surfaces:
//   * Download backup (plain or AES-256-GCM/argon2id encrypted) — GET or POST
//     /api/admin/backup. Encryption is UI-only because passphrases would
//     leak through chat transcripts if exposed to the agent tool.
//   * Restore from backup (file picker → POST multipart; .tar.gz.enc files
//     prompt for a passphrase that the server uses to decrypt before the
//     same two-phase swap-on-restart restore).
//   * Schedule (read/write `backup.scheduled_*` via /api/config). Defaults
//     are conservative: disabled, daily, retain 7. The server runs the
//     scheduler from the gateway when `scheduled_enabled` is on at boot;
//     toggling this requires a service restart.
//   * Scheduled backups list with "Run now", "Restore", and "Download" per
//     row. Restoring or downloading an encrypted file prompts inline.

import { useEffect, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  AlertTriangle, Download, Loader2, Lock, PlayCircle, RefreshCcw, Upload,
} from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import btn from './actionButton.module.css'

const spinStyle: React.CSSProperties = {
  animation: 'mira-spin 1s linear infinite',
}
if (typeof document !== 'undefined' && !document.getElementById('mira-spin-style')) {
  const el = document.createElement('style')
  el.id = 'mira-spin-style'
  el.textContent = '@keyframes mira-spin { to { transform: rotate(360deg); } }'
  document.head.appendChild(el)
}

interface BackupEntry { name: string; bytes: number; modified_ms: number; encrypted: boolean }
interface ConfigResp {
  backup?: {
    scheduled_enabled?: boolean
    scheduled_interval_secs?: number
    scheduled_retention_count?: number
  }
}

const fmtBytes = (n: number): string => {
  if (n < 1024)             return `${n} B`
  if (n < 1024 * 1024)      return `${(n / 1024).toFixed(1)} KB`
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`
}
const fmtTime = (ms: number): string => new Date(ms).toLocaleString()

const triggerBlobDownload = (blob: Blob, filename: string) => {
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  document.body.appendChild(a)
  a.click()
  document.body.removeChild(a)
  setTimeout(() => URL.revokeObjectURL(url), 0)
}

export default function BackupRestore() {
  // ── Download state ────────────────────────────────────────────────────
  const [downloading, setDownloading] = useState(false)
  const [encrypt,     setEncrypt]     = useState(false)
  const [passphrase,  setPassphrase]  = useState('')

  // ── Upload state ──────────────────────────────────────────────────────
  const [uploading, setUploading] = useState(false)
  const fileRef = useRef<HTMLInputElement>(null)

  // ── Schedule state (mirrors backup.* in config) ───────────────────────
  const queryClient = useQueryClient()
  const configQ = useQuery<ConfigResp>({
    queryKey: ['mira-config'],
    queryFn:  async () => (await api.get<ConfigResp>('/api/config')).data,
  })

  const cfgBackup    = configQ.data?.backup ?? {}
  const cfgEnabled   = cfgBackup.scheduled_enabled ?? false
  const cfgIntervalS = cfgBackup.scheduled_interval_secs ?? 86_400
  const cfgRetention = cfgBackup.scheduled_retention_count ?? 7

  const [schedEnabled,    setSchedEnabled]    = useState(cfgEnabled)
  const [schedDays,       setSchedDays]       = useState(Math.max(1, Math.round(cfgIntervalS / 86_400)))
  const [schedRetention,  setSchedRetention]  = useState(cfgRetention)

  // Sync local form state when config arrives / changes upstream.
  useEffect(() => {
    setSchedEnabled(cfgEnabled)
    setSchedDays(Math.max(1, Math.round(cfgIntervalS / 86_400)))
    setSchedRetention(cfgRetention)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cfgEnabled, cfgIntervalS, cfgRetention])

  const saveScheduleMut = useMutation({
    mutationFn: async () => {
      // Read-modify-write — we PUT the full config block touched, not the
      // whole config; the server's put_config merges. We only own the
      // backup.* keys here.
      const current = (await api.get<Record<string, unknown>>('/api/config')).data
      const next = {
        ...current,
        backup: {
          ...(current.backup as object ?? {}),
          scheduled_enabled:         schedEnabled,
          scheduled_interval_secs:   Math.max(3600, schedDays * 86_400),
          scheduled_retention_count: Math.max(1, schedRetention),
        },
      }
      return api.put('/api/config', next)
    },
    onSuccess: () => {
      toast.success('Schedule saved. Restart MIRA to apply the toggle change.')
      void queryClient.invalidateQueries({ queryKey: ['mira-config'] })
    },
    onError: (e: unknown) => {
      const m = (e as { response?: { data?: { error?: string } } })?.response?.data?.error
              ?? (e as Error).message
      toast.error(`Save failed: ${m}`)
    },
  })

  // ── Scheduled backups list ────────────────────────────────────────────
  const listQ = useQuery<BackupEntry[]>({
    queryKey: ['admin-backups'],
    queryFn:  async () => (await api.get<BackupEntry[]>('/api/admin/backups')).data,
    refetchOnWindowFocus: false,
  })

  const runNowMut = useMutation({
    mutationFn: async () => api.post('/api/admin/backups/run-now'),
    onSuccess:  () => {
      toast.success('Backup created.')
      void queryClient.invalidateQueries({ queryKey: ['admin-backups'] })
    },
    onError:    (e: unknown) => toast.error(`Run failed: ${(e as Error).message}`),
  })

  const restoreScheduledMut = useMutation({
    mutationFn: async ({ name, passphrase }: { name: string; passphrase?: string }) =>
      api.post(`/api/admin/backups/${encodeURIComponent(name)}/restore`,
               passphrase ? { passphrase } : {}),
    onSuccess:  () => toast.success(
      'Restore staged. Server is restarting; data swaps on next boot. ' +
      'Previous data preserved at .pre_restore_backup/.',
      { duration: 9000 },
    ),
    onError:    (e: unknown) => {
      const m = (e as { response?: { data?: { error?: string } } })?.response?.data?.error
              ?? (e as Error).message
      toast.error(`Restore failed: ${m}`)
    },
  })

  // ── Download (plain or encrypted) ─────────────────────────────────────
  const downloadBackup = async () => {
    if (encrypt && passphrase.length < 8) {
      toast.error('Passphrase must be at least 8 characters.')
      return
    }
    setDownloading(true)
    try {
      const resp = encrypt
        ? await api.post('/api/admin/backup', { passphrase }, { responseType: 'blob' })
        : await api.get('/api/admin/backup',                    { responseType: 'blob' })
      const cd = resp.headers['content-disposition'] as string | undefined
      const match = cd?.match(/filename="?([^";]+)"?/)
      const filename = match?.[1] ?? `mira-backup-${Date.now()}.tar.gz${encrypt ? '.enc' : ''}`
      triggerBlobDownload(resp.data as Blob, filename)
      toast.success(`Backup saved (${filename}).`)
      if (encrypt) setPassphrase('') // don't leave it sitting in state
    } catch (e) {
      const m = (e as { response?: { data?: { error?: string } } })?.response?.data?.error
              ?? (e as Error).message
      toast.error(`Backup failed: ${m}`)
    } finally {
      setDownloading(false)
    }
  }

  // ── Upload-driven restore ─────────────────────────────────────────────
  const uploadMut = useMutation<unknown, unknown, { file: File; passphrase?: string }>({
    mutationFn: async ({ file, passphrase }) => {
      const form = new FormData()
      form.append('archive', file, file.name)
      if (passphrase) form.append('passphrase', passphrase)
      return api.post('/api/admin/restore', form, {
        headers: { 'Content-Type': 'multipart/form-data' },
      })
    },
    onSuccess: () => {
      toast.success(
        'Backup uploaded. Server is restarting; restore applies on next boot. ' +
        'Previous data is preserved at .pre_restore_backup/ if you need to roll back.',
        { duration: 9000 },
      )
    },
    onError: (e: unknown) => {
      const m = (e as { response?: { data?: { error?: string } } })?.response?.data?.error
              ?? (e as Error).message
      toast.error(`Restore failed: ${m}`)
    },
    onSettled: () => setUploading(false),
  })

  const onFilePicked = (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0]
    e.target.value = ''
    if (!file) return
    const okExt = ['.tar.gz', '.tgz', '.tar.gz.enc'].some(s => file.name.endsWith(s))
    if (!okExt) {
      toast.error(`${file.name}: not a .tar.gz or .tar.gz.enc — refusing.`)
      return
    }
    let pass: string | undefined
    if (file.name.endsWith('.enc')) {
      const p = window.prompt(`${file.name} is encrypted. Enter passphrase:`)
      if (p == null || !p) { toast.error('Restore cancelled.'); return }
      pass = p
    }
    const ok = confirm(
      `Restore from ${file.name}?\n\n` +
      'This will replace MIRA\'s data on the next restart. ' +
      'Your current data will be archived to .pre_restore_backup/ in case you need to roll back. ' +
      'The server will restart immediately after the upload completes.\n\n' +
      'Continue?',
    )
    if (!ok) return
    setUploading(true)
    uploadMut.mutate({ file, passphrase: pass })
  }

  // ── Render ────────────────────────────────────────────────────────────
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
      <p style={{ fontSize: 12, color: 'var(--text-muted)', margin: 0 }}>
        A backup includes your databases (history, memory, automations, channel
        accounts, etc.), the wiki, avatars, artifacts, installed skills + their
        secrets, the VAPID keypair, and the config file (including provider API
        keys). Models, TTS voices, and the sandbox rootfs are <em>not</em>
        included — they're re-downloadable on demand.
      </p>

      {/* ── Ad-hoc download + restore ─────────────────────────────────── */}
      <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
          <button
            onClick={downloadBackup}
            disabled={downloading}
            className={btn.btn}
          >
            {downloading
              ? <Loader2 size={13} style={spinStyle} />
              : <Download size={13} />}
            {downloading ? 'Building backup…' : 'Download backup'}
          </button>

          <label style={{ display: 'inline-flex', alignItems: 'center', gap: 4, fontSize: 12 }}>
            <input
              type="checkbox"
              checked={encrypt}
              onChange={e => setEncrypt(e.target.checked)}
            />
            <Lock size={12} /> Encrypt (AES-256-GCM)
          </label>

          {encrypt && (
            <input
              type="password"
              placeholder="Passphrase (≥ 8 chars)"
              value={passphrase}
              onChange={e => setPassphrase(e.target.value)}
              autoComplete="new-password"
              style={{ minWidth: 220 }}
            />
          )}

          <button
            onClick={() => fileRef.current?.click()}
            disabled={uploading}
            className={btn.btn}
          >
            {uploading
              ? <Loader2 size={13} style={spinStyle} />
              : <Upload size={13} />}
            {uploading ? 'Uploading…' : 'Restore from backup…'}
          </button>

          <input
            ref={fileRef}
            type="file"
            accept=".tar.gz,.tgz,.enc,application/gzip,application/octet-stream"
            style={{ display: 'none' }}
            onChange={onFilePicked}
          />
        </div>

        {encrypt && (
          <p style={{ fontSize: 11, color: 'var(--text-muted)', margin: 0 }}>
            The passphrase never leaves your browser before encryption. Without
            it, the file is unrecoverable — MIRA cannot reset it.
          </p>
        )}
      </div>

      <div
        style={{
          display: 'flex',
          alignItems: 'flex-start',
          gap: 6,
          fontSize: 11,
          color: 'var(--warning, #b58900)',
          padding: '6px 10px',
          background: 'rgba(255,193,7,0.05)',
          border: '1px solid rgba(255,193,7,0.2)',
          borderRadius: 4,
        }}
      >
        <AlertTriangle size={13} style={{ marginTop: 1, flexShrink: 0 }} />
        <span>
          Restore replaces all MIRA data and triggers a restart. Conversations
          on the wire mid-restore will be interrupted. The previous data is
          archived at <code>&lt;data_dir&gt;/.pre_restore_backup/</code> in
          case you need to roll back manually.
        </span>
      </div>

      {/* ── Scheduled backups ─────────────────────────────────────────── */}
      <fieldset style={{ border: '1px solid var(--border)', borderRadius: 4, padding: 10 }}>
        <legend style={{ fontSize: 12, fontWeight: 600, padding: '0 6px' }}>
          Scheduled backups
        </legend>

        <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', alignItems: 'center', marginBottom: 10 }}>
          <label style={{ display: 'inline-flex', alignItems: 'center', gap: 6, fontSize: 12 }}>
            <input
              type="checkbox"
              checked={schedEnabled}
              onChange={e => setSchedEnabled(e.target.checked)}
            />
            Enable
          </label>
          <label style={{ display: 'inline-flex', alignItems: 'center', gap: 4, fontSize: 12 }}>
            Every
            <input
              type="number"
              min={1}
              value={schedDays}
              onChange={e => setSchedDays(Math.max(1, Number(e.target.value) || 1))}
              style={{ width: 60 }}
            />
            day(s)
          </label>
          <label style={{ display: 'inline-flex', alignItems: 'center', gap: 4, fontSize: 12 }}>
            Keep
            <input
              type="number"
              min={1}
              value={schedRetention}
              onChange={e => setSchedRetention(Math.max(1, Number(e.target.value) || 1))}
              style={{ width: 60 }}
            />
            most-recent
          </label>
          <button
            onClick={() => saveScheduleMut.mutate()}
            disabled={saveScheduleMut.isPending}
            className={btn.btn}
          >
            {saveScheduleMut.isPending && <Loader2 size={12} style={spinStyle} />}
            Save schedule
          </button>
          <button
            onClick={() => runNowMut.mutate()}
            disabled={runNowMut.isPending}
            className={btn.btn}
          >
            {runNowMut.isPending
              ? <Loader2 size={12} style={spinStyle} />
              : <PlayCircle size={13} />}
            Run now
          </button>
          <button
            onClick={() => void listQ.refetch()}
            disabled={listQ.isFetching}
            className={btn.btn}
            title="Refresh list"
          >
            <RefreshCcw size={12} style={listQ.isFetching ? spinStyle : undefined} />
          </button>
        </div>

        <p style={{ fontSize: 11, color: 'var(--text-muted)', margin: '0 0 8px' }}>
          Scheduled backups are written to <code>&lt;data_dir&gt;/backups/</code>{' '}
          and are excluded from new backups (so they don't compound). Toggling
          "Enable" takes effect on the next restart.
        </p>

        {listQ.data && listQ.data.length === 0 ? (
          <p style={{ fontSize: 12, color: 'var(--text-muted)', margin: 0 }}>
            No scheduled backups yet.
          </p>
        ) : listQ.data ? (
          <table style={{ width: '100%', fontSize: 12, borderCollapse: 'collapse' }}>
            <thead>
              <tr style={{ textAlign: 'left', color: 'var(--text-muted)' }}>
                <th style={{ padding: '4px 6px' }}>Filename</th>
                <th style={{ padding: '4px 6px' }}>Size</th>
                <th style={{ padding: '4px 6px' }}>Created</th>
                <th style={{ padding: '4px 6px' }}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {listQ.data.map(b => (
                <tr key={b.name} style={{ borderTop: '1px solid var(--border)' }}>
                  <td style={{ padding: '4px 6px', fontFamily: 'monospace' }}>
                    {b.name}{' '}
                    {b.encrypted && (
                      <Lock size={10} style={{ verticalAlign: 'middle', opacity: 0.6 }} />
                    )}
                  </td>
                  <td style={{ padding: '4px 6px' }}>{fmtBytes(b.bytes)}</td>
                  <td style={{ padding: '4px 6px' }}>{fmtTime(b.modified_ms)}</td>
                  <td style={{ padding: '4px 6px' }}>
                    <button
                      onClick={() => {
                        let p: string | undefined
                        if (b.encrypted) {
                          const v = window.prompt(`${b.name} is encrypted. Passphrase:`)
                          if (v == null || !v) return
                          p = v
                        }
                        if (!confirm(
                          `Restore from ${b.name}? Server will restart and swap data on next boot.`,
                        )) return
                        restoreScheduledMut.mutate({ name: b.name, passphrase: p })
                      }}
                      disabled={restoreScheduledMut.isPending}
                      className={btn.btn} style={{ fontSize: 11, padding: '3px 8px' }}
                    >
                      Restore
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : listQ.isLoading ? (
          <Loader2 size={14} style={spinStyle} />
        ) : null}
      </fieldset>
    </div>
  )
}
