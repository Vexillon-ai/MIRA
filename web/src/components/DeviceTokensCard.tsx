// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/DeviceTokensCard.tsx
//
// manage long-lived per-device tokens (Settings →
// Notifications, beside "Pair a mobile device"). A headless device (e.g. a
// voice client) uses an `mdt_…` bearer token instead of a human login. Create
// (name + scopes + optional expiry), list (name / created / last used), and
// revoke. The raw token is shown exactly once at creation — never retrievable
// again — so it's rendered in a copyable box with a clear warning.

import { useMemo, useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { KeyRound, Trash2, Copy, Check } from 'lucide-react'
import toast from 'react-hot-toast'
import { authApi } from '@/api/auth'
import { deviceTokensApi, type CreatedDeviceToken } from '@/api/deviceTokens'
import btn from './actionButton.module.css'

const ALL_SCOPES = ['chat', 'chat.cancel'] as const

function ago(ms: number | null): string {
  if (!ms) return 'never'
  const d = Date.now() - ms
  if (d < 60_000) return 'just now'
  if (d < 3_600_000) return `${Math.floor(d / 60_000)}m ago`
  if (d < 86_400_000) return `${Math.floor(d / 3_600_000)}h ago`
  return `${Math.floor(d / 86_400_000)}d ago`
}

export default function DeviceTokensCard() {
  const qc = useQueryClient()
  const meQuery = useQuery({ queryKey: ['me'], queryFn: authApi.me })
  const userId = meQuery.data?.id

  const tokensQuery = useQuery({
    queryKey: ['device-tokens', userId],
    queryFn: () => deviceTokensApi.list(userId!),
    enabled: !!userId,
  })

  const [name, setName] = useState('')
  const [scopes, setScopes] = useState<string[]>([...ALL_SCOPES])
  const [expiryDays, setExpiryDays] = useState<string>('')  // '' = never
  const [fresh, setFresh] = useState<CreatedDeviceToken | null>(null)
  const [copied, setCopied] = useState(false)

  const createMut = useMutation({
    mutationFn: () => deviceTokensApi.create(userId!, {
      name: name.trim(),
      scopes,
      ttl_secs: expiryDays ? Math.round(parseFloat(expiryDays) * 86_400) : null,
    }),
    onSuccess: (tok) => {
      setFresh(tok)
      setName('')
      setCopied(false)
      qc.invalidateQueries({ queryKey: ['device-tokens', userId] })
    },
    onError: (e) => toast.error(`Couldn't create token: ${(e as Error).message}`),
  })

  const revokeMut = useMutation({
    mutationFn: (tokenId: string) => deviceTokensApi.revoke(userId!, tokenId),
    onSuccess: () => {
      toast.success('Device token revoked.')
      qc.invalidateQueries({ queryKey: ['device-tokens', userId] })
    },
    onError: (e) => toast.error(`Couldn't revoke: ${(e as Error).message}`),
  })

  const toggleScope = (s: string) =>
    setScopes((prev) => prev.includes(s) ? prev.filter((x) => x !== s) : [...prev, s])

  const copyFresh = async () => {
    if (!fresh) return
    try {
      await navigator.clipboard.writeText(fresh.token)
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch {
      toast.error('Copy failed — select and copy the token manually.')
    }
  }

  const canCreate = useMemo(
    () => !!userId && name.trim().length > 0 && scopes.length > 0 && !createMut.isPending,
    [userId, name, scopes, createMut.isPending],
  )

  const tokens = tokensQuery.data ?? []

  return (
    <div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <KeyRound size={16} style={{ color: 'var(--accent)' }} />
        <strong style={{ fontSize: 13 }}>Device tokens</strong>
      </div>
      <p style={{ margin: '4px 0 8px', fontSize: 12, color: 'var(--text-muted)' }}>
        Long-lived tokens for a headless device (like a voice assistant) to sign in without a
        password. A token is shown once here, stored only as a hash, and can be revoked any time.
      </p>

      {/* Create form */}
      <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 8, marginBottom: 8 }}>
        <input
          type="text"
          placeholder="Device name (e.g. Kitchen speaker)"
          value={name}
          onChange={(e) => setName(e.target.value)}
          style={{ flex: '1 1 180px', minWidth: 140, fontSize: 13, padding: '6px 8px' }}
        />
        <input
          type="number"
          min={0}
          placeholder="Expiry (days, blank = never)"
          value={expiryDays}
          onChange={(e) => setExpiryDays(e.target.value)}
          style={{ width: 150, fontSize: 13, padding: '6px 8px' }}
        />
      </div>
      <div style={{ display: 'flex', gap: 12, marginBottom: 8, fontSize: 12 }}>
        {ALL_SCOPES.map((s) => (
          <label key={s} style={{ display: 'flex', alignItems: 'center', gap: 4, cursor: 'pointer' }}>
            <input type="checkbox" checked={scopes.includes(s)} onChange={() => toggleScope(s)} />
            {s}
          </label>
        ))}
      </div>
      <button className={btn.btn} onClick={() => createMut.mutate()} disabled={!canCreate}>
        <KeyRound size={13} />Create token
      </button>

      {/* Freshly-created token — shown once */}
      {fresh && (
        <div style={{
          marginTop: 10, padding: 10, borderRadius: 8,
          border: '1px solid var(--accent)', background: 'var(--bg-elevated, rgba(127,127,127,0.08))',
        }}>
          <div style={{ fontSize: 12, color: 'var(--text-muted)', marginBottom: 6 }}>
            Copy this token now — it won't be shown again:
          </div>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <code style={{
              flex: 1, fontSize: 12, wordBreak: 'break-all',
              background: 'rgba(127,127,127,0.12)', padding: '6px 8px', borderRadius: 6,
            }}>{fresh.token}</code>
            <button className={btn.btn} onClick={copyFresh}>
              {copied ? <Check size={13} /> : <Copy size={13} />}{copied ? 'Copied' : 'Copy'}
            </button>
          </div>
          <button
            className={btn.btn}
            style={{ marginTop: 8 }}
            onClick={() => setFresh(null)}
          >Done</button>
        </div>
      )}

      {/* Existing tokens */}
      <div style={{ marginTop: 12 }}>
        {tokensQuery.isLoading && <div style={{ fontSize: 12 }}>Loading…</div>}
        {!tokensQuery.isLoading && tokens.length === 0 && (
          <div style={{ fontSize: 12, color: 'var(--text-muted)' }}>No device tokens yet.</div>
        )}
        {tokens.map((t) => (
          <div key={t.token_id} style={{
            display: 'flex', alignItems: 'center', gap: 8, padding: '6px 0',
            borderTop: '1px solid rgba(127,127,127,0.15)', fontSize: 12,
          }}>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ fontWeight: 600 }}>{t.name}</div>
              <div style={{ color: 'var(--text-muted)' }}>
                {t.scopes.join(', ')} · created {new Date(t.created_at).toLocaleDateString()} · used {ago(t.last_used_at)}
                {t.expires_at ? ` · expires ${new Date(t.expires_at).toLocaleDateString()}` : ''}
              </div>
            </div>
            <button
              className={btn.btn}
              onClick={() => revokeMut.mutate(t.token_id)}
              disabled={revokeMut.isPending}
              title="Revoke"
            ><Trash2 size={13} />Revoke</button>
          </div>
        ))}
      </div>
    </div>
  )
}
