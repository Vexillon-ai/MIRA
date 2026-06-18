// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/DailyBriefingSettings.tsx
//
// Q1.6 — Daily Briefing panel for the Settings → Notifications tab.
// Three things:
//   * Toggle (on/off)
//   * Hour picker (0-23 local) — only meaningful when on
//   * "Send now" button — fires a briefing on demand for testing,
//     bypassing the once-per-day + hour-match gates

import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Sunrise, Send, Loader2 } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import btn from './actionButton.module.css'

interface BriefingConfig {
  enabled:          boolean
  hour:             number
  last_briefing_at: number | null
  last_checkin_at:  number | null
  companion_active: boolean
}

// Compact "3h ago" / "2 days ago" relative time for delivery health.
function relTime(ms: number): string {
  const diff = Date.now() - ms
  const m = Math.round(diff / 60000)
  if (m < 1)  return 'just now'
  if (m < 60) return `${m} min ago`
  const h = Math.round(m / 60)
  if (h < 24) return `${h}h ago`
  const d = Math.round(h / 24)
  return `${d} day${d === 1 ? '' : 's'} ago`
}

export default function DailyBriefingSettings() {
  const qc = useQueryClient()
  const [pendingHour, setPendingHour] = useState<number | null>(null)

  const cfg = useQuery<BriefingConfig>({
    queryKey: ['daily-briefing'],
    queryFn:  () => api.get<BriefingConfig>('/api/me/briefing').then((r) => r.data),
    staleTime: 30_000,
  })

  const patchMut = useMutation<BriefingConfig, unknown, Partial<Pick<BriefingConfig, 'enabled' | 'hour'>>>({
    mutationFn: (body) => api.patch<BriefingConfig>('/api/me/briefing', body).then((r) => r.data),
    onSuccess:  () => {
      qc.invalidateQueries({ queryKey: ['daily-briefing'] })
      setPendingHour(null)
      toast.success('Briefing settings updated.')
    },
    onError: (e: unknown) => {
      const m = (e as { response?: { data?: { error?: string } } })?.response?.data?.error
              ?? (e as Error).message
      toast.error(`Update failed: ${m}`)
    },
  })

  const sendNowMut = useMutation<{ data: { channel: string; chars: number } }, unknown, void>({
    mutationFn: () => api.post('/api/me/briefing/send-now'),
    onSuccess:  (r) => {
      qc.invalidateQueries({ queryKey: ['daily-briefing'] })
      toast.success(`Briefing sent on ${r.data?.channel ?? '?'} (${r.data?.chars ?? '?'} chars).`)
    },
    onError: (e: unknown) => {
      const m = (e as { response?: { data?: { error?: string } } })?.response?.data?.error
              ?? (e as Error).message
      toast.error(`Send failed: ${m}`)
    },
  })

  if (cfg.isLoading) {
    return <div style={{ padding: 12, fontSize: 13, color: 'var(--text-muted)' }}>Loading…</div>
  }
  const c = cfg.data
  if (!c) return null

  // Disambiguate the two "off" states for the user — companion off is
  // a different problem from briefing off.
  if (!c.companion_active && !c.enabled) {
    return (
      <div style={{ padding: 12, fontSize: 13, color: 'var(--text-muted)' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 6 }}>
          <Sunrise size={16} />
          <strong>Daily Briefing</strong>
        </div>
        Companion mode is required for daily briefings. Ask MIRA to enable companion mode
        ("turn on companion mode" / "enable companion") and pick a safety contact, then
        come back here.
      </div>
    )
  }

  const liveHour = pendingHour ?? c.hour

  return (
    <div style={{ padding: 12, display: 'flex', flexDirection: 'column', gap: 14 }}>
      <div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <Sunrise size={16} style={{ color: c.enabled ? 'var(--accent)' : 'var(--text-muted)' }} />
          <strong style={{ fontSize: 13 }}>
            Daily Briefing {c.enabled ? '— on' : '— off'}
          </strong>
        </div>
        <p style={{ margin: '6px 0 10px', fontSize: 12, color: 'var(--text-muted)' }}>
          Each morning at the time you pick, MIRA pulls together today's calendar,
          tomorrow's preview, recent wiki updates, and yesterday's automation runs,
          then writes you a short summary in your voice. Delivered through the same
          channel as your companion check-ins (Signal / Telegram / web push).
        </p>

        <div style={{ display: 'flex', gap: 12, alignItems: 'center', flexWrap: 'wrap' }}>
          <label style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 13 }}>
            <input
              type="checkbox"
              checked={c.enabled}
              disabled={patchMut.isPending}
              onChange={(e) => patchMut.mutate({ enabled: e.target.checked })}
            />
            Enabled
          </label>

          <label style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 13 }}>
            Send at
            <select
              value={liveHour}
              disabled={!c.enabled || patchMut.isPending}
              onChange={(e) => setPendingHour(Number(e.target.value))}
              style={{ padding: '2px 6px' }}
            >
              {Array.from({ length: 24 }, (_, i) => (
                <option key={i} value={i}>{i.toString().padStart(2, '0')}:00</option>
              ))}
            </select>
            local
            {pendingHour !== null && pendingHour !== c.hour && (
              <button
                className={btn.btn}
                style={{ marginLeft: 6 }}
                onClick={() => patchMut.mutate({ hour: pendingHour })}
                disabled={patchMut.isPending}
              >
                Save
              </button>
            )}
          </label>
        </div>

        {/* Proactive-delivery health: last ACTUAL deliveries (stamped only
            on success), so a missed morning is visible at a glance. */}
        <div style={{ margin: '10px 0 0', fontSize: 11, color: 'var(--text-muted)',
                      display: 'flex', flexDirection: 'column', gap: 3 }}>
          <span>
            Last briefing delivered:{' '}
            {c.last_briefing_at
              ? <span title={new Date(c.last_briefing_at).toLocaleString()}>{relTime(c.last_briefing_at)}</span>
              : <em>never</em>}
          </span>
          <span>
            Last check-in delivered:{' '}
            {c.last_checkin_at
              ? <span title={new Date(c.last_checkin_at).toLocaleString()}>{relTime(c.last_checkin_at)}</span>
              : <em>never</em>}
          </span>
          {c.enabled && c.companion_active &&
            (!c.last_briefing_at || Date.now() - c.last_briefing_at > 25 * 3600_000) && (
            <span style={{ color: 'var(--warning, #b8860b)' }}>
              ⚠ No briefing in over a day. This usually means the machine running MIRA was
              asleep at briefing time — proactive delivery only works while MIRA is up.
            </span>
          )}
        </div>
      </div>

      <div>
        <button
          onClick={() => sendNowMut.mutate()}
          className={btn.btn}
          disabled={sendNowMut.isPending || !c.companion_active}
          title={!c.companion_active
            ? 'Companion mode must be enabled (and set up with a safety contact) first.'
            : 'Fire a briefing now, bypassing the daily schedule. Useful for testing.'}
        >
          {sendNowMut.isPending
            ? <Loader2 size={13} style={{ animation: 'mira-spin 1s linear infinite' }} />
            : <Send size={13} />}
          {sendNowMut.isPending ? 'Sending…' : 'Send a briefing now'}
        </button>
      </div>
    </div>
  )
}
