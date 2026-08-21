// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/BriefingTestButton.tsx
//
// "Send a test briefing now" — fires a daily-briefing on demand (bypassing the
// once-per-day + hour gates) so you can confirm it reaches your companion
// channel. Per-user (POST /api/me/briefing/send-now, 202 async); the full
// enable/hour config lives on the Presence page, this is just the tester.

import { useMutation } from '@tanstack/react-query'
import { Sunrise, Send, Loader2 } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import btn from './actionButton.module.css'

export default function BriefingTestButton() {
  const sendNow = useMutation<{ detail?: string }, unknown, void>({
    mutationFn: () => api.post('/api/me/briefing/send-now').then((r) => r.data),
    onSuccess:  (d) => toast.success(
      d?.detail ?? 'Briefing is being generated — it will arrive on your companion channel shortly.'),
    onError: (e: unknown) => {
      const m = (e as { response?: { data?: { error?: string } } })?.response?.data?.error
              ?? (e as Error).message
      toast.error(`Briefing trigger failed: ${m}`)
    },
  })

  return (
    <div style={{ padding: 12, display: 'flex', flexDirection: 'column', gap: 10 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <Sunrise size={16} style={{ color: 'var(--accent)' }} />
        <strong style={{ fontSize: 13 }}>Daily briefing</strong>
      </div>
      <p style={{ margin: 0, fontSize: 12, color: 'var(--text-muted)' }}>
        Send a briefing to yourself right now, bypassing the schedule. Turn the
        daily briefing on (and pick its hour) under <strong>Presence</strong>.
      </p>
      <div>
        <button
          className={btn.btn}
          onClick={() => sendNow.mutate()}
          disabled={sendNow.isPending}
          title="Generate + deliver a briefing now to confirm it reaches you."
        >
          {sendNow.isPending
            ? <Loader2 size={13} style={{ animation: 'mira-spin 1s linear infinite' }} />
            : <Send size={13} />}
          {sendNow.isPending ? 'Sending…' : 'Send a test briefing'}
        </button>
      </div>
    </div>
  )
}
