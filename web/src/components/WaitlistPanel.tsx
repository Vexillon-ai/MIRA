// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/WaitlistPanel.tsx
//
// Q1.7 — admin-only landing-page waitlist viewer. Lives in Settings →
// Server & Security. Shows total signups, most recent 200, and exposes
// CSV export + per-row delete.

import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Download, Trash2, Users } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import btn from './actionButton.module.css'

interface WaitlistEntry {
  id:         string
  email:      string
  created_at: string
  user_agent: string | null
  source:     string | null
}

interface WaitlistResponse {
  count:   number
  entries: WaitlistEntry[]
}

export default function WaitlistPanel() {
  const qc = useQueryClient()
  const [downloading, setDownloading] = useState(false)

  const q = useQuery<WaitlistResponse>({
    queryKey: ['waitlist'],
    queryFn:  () => api.get<WaitlistResponse>('/api/admin/waitlist').then((r) => r.data),
    staleTime: 30_000,
  })

  const deleteMut = useMutation({
    mutationFn: (id: string) => api.delete(`/api/admin/waitlist/${id}`),
    onSuccess:  () => {
      qc.invalidateQueries({ queryKey: ['waitlist'] })
      toast.success('Removed.')
    },
    onError: (e: unknown) => toast.error(`Delete failed: ${(e as Error).message}`),
  })

  const exportCsv = async () => {
    setDownloading(true)
    try {
      const resp = await api.get('/api/admin/waitlist/export', { responseType: 'blob' })
      const cd = resp.headers['content-disposition'] as string | undefined
      const m = cd?.match(/filename="?([^";]+)"?/)
      const filename = m?.[1] ?? `mira-waitlist-${Date.now()}.csv`
      const url = URL.createObjectURL(resp.data as Blob)
      const a = document.createElement('a')
      a.href = url; a.download = filename
      document.body.appendChild(a); a.click(); document.body.removeChild(a)
      setTimeout(() => URL.revokeObjectURL(url), 0)
    } catch (e) {
      toast.error(`Export failed: ${(e as Error).message}`)
    } finally {
      setDownloading(false)
    }
  }

  if (q.isLoading) {
    return <div style={{ padding: 12, fontSize: 13, color: 'var(--text-muted)' }}>Loading…</div>
  }
  // 503 / 5xx — store not opened. Surface a hint instead of just spinning.
  if (q.isError) {
    return (
      <div style={{ padding: 12, fontSize: 12, color: 'var(--text-muted)' }}>
        Waitlist store not available. The handler returns 503 if
        <code>~/.mira/data/waitlist.db</code> failed to open at boot —
        check server logs for the reason.
      </div>
    )
  }

  const data = q.data!
  return (
    <div style={{ padding: 12, display: 'flex', flexDirection: 'column', gap: 12 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        <Users size={16} />
        <strong style={{ fontSize: 13 }}>
          {data.count} signup{data.count === 1 ? '' : 's'}
        </strong>
        <span style={{ flex: 1 }} />
        <button className={btn.btn} onClick={exportCsv} disabled={downloading || data.count === 0}>
          <Download size={12} />
          {downloading ? 'Exporting…' : 'Export CSV'}
        </button>
      </div>

      <p style={{ margin: 0, fontSize: 12, color: 'var(--text-muted)' }}>
        Landing page at <code>/landing</code> (or wherever you deploy
        <code>web/landing/</code>) posts here. Newest first; up to 200 shown.
      </p>

      {data.entries.length === 0 ? (
        <div style={{ fontSize: 13, color: 'var(--text-muted)', padding: '12px 0' }}>
          No signups yet. Share the landing page URL to get the first.
        </div>
      ) : (
        <ul style={{ listStyle: 'none', padding: 0, margin: 0 }}>
          {data.entries.map((e) => (
            <li key={e.id} style={{
              display: 'flex', alignItems: 'center', gap: 8,
              padding: '6px 4px', borderTop: '1px solid var(--border-subtle)',
              fontSize: 12,
            }}>
              <span style={{ flex: 1, fontFamily: 'monospace', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                {e.email}
              </span>
              {e.source && (
                <span style={{ fontSize: 11, color: 'var(--text-muted)' }}>
                  {e.source}
                </span>
              )}
              <span style={{ fontSize: 11, color: 'var(--text-muted)' }}>
                {new Date(e.created_at).toLocaleString()}
              </span>
              <button
                onClick={() => {
                  if (confirm(`Remove ${e.email} from the waitlist?`)) deleteMut.mutate(e.id)
                }}
                disabled={deleteMut.isPending}
                title="Remove"
                style={{ padding: '2px 6px' }}
              >
                <Trash2 size={11} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}
