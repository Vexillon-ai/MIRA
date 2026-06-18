// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/CompanionCheckinTest.tsx
//
// Q2 #10 follow-up — "Send a check-in now" tester. The companion scheduler
// only fires check-ins when its policy says they're due AND the process is
// up at that moment, which makes proactive delivery hard to verify. This
// button fires one immediately (bypassing the policy gates) and shows the
// exact delivery outcome — delivered on which channel, skipped, or failed
// with the reason — so you can confirm proactive messages actually reach
// your phone.

import { useState } from 'react'
import { useMutation } from '@tanstack/react-query'
import { MessageCircle, Send, Loader2 } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import btn from './actionButton.module.css'

interface CheckinOutcome {
  ok:               boolean
  status:           'sent' | 'skipped' | 'failed'
  channel?:         string
  chars?:           number
  conversation_id?: string
  detail?:          string
}

export default function CompanionCheckinTest() {
  const [last, setLast] = useState<CheckinOutcome | null>(null)

  const sendNow = useMutation<CheckinOutcome, unknown, void>({
    mutationFn: () => api.post<CheckinOutcome>('/api/companion/checkin/test').then((r) => r.data),
    onSuccess:  (o) => {
      setLast(o)
      if (o.status === 'sent') {
        toast.success(`Check-in delivered on ${o.channel ?? '?'} (${o.chars ?? '?'} chars).`)
      } else if (o.status === 'skipped') {
        toast.error(`Check-in skipped: ${o.detail ?? 'no channel'}.`)
      } else {
        toast.error(`Check-in failed: ${o.detail ?? 'unknown'}.`)
      }
    },
    onError: (e: unknown) => {
      const m = (e as { response?: { data?: { error?: string } } })?.response?.data?.error
              ?? (e as Error).message
      toast.error(`Check-in trigger failed: ${m}`)
    },
  })

  return (
    <div style={{ padding: 12, display: 'flex', flexDirection: 'column', gap: 10 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <MessageCircle size={16} style={{ color: 'var(--accent)' }} />
        <strong style={{ fontSize: 13 }}>Companion check-in</strong>
      </div>
      <p style={{ margin: 0, fontSize: 12, color: 'var(--text-muted)' }}>
        Fire a check-in to yourself right now, bypassing the schedule's policy
        gates. It's delivered on your preferred companion channel (Signal /
        Telegram / web). Use this to confirm proactive messages actually reach
        you — the result below shows exactly what happened.
      </p>
      <div>
        <button
          className={btn.btn}
          onClick={() => sendNow.mutate()}
          disabled={sendNow.isPending}
          title="Fire a companion check-in now and report the delivery outcome."
        >
          {sendNow.isPending
            ? <Loader2 size={13} style={{ animation: 'mira-spin 1s linear infinite' }} />
            : <Send size={13} />}
          {sendNow.isPending ? 'Sending…' : 'Send a check-in now'}
        </button>
      </div>
      {last && (
        <p style={{
          margin: 0, fontSize: 11, fontFamily: 'var(--font-mono)',
          color: last.status === 'sent' ? 'var(--ok, #3a3)' : 'var(--text-muted)',
        }}>
          {last.status === 'sent'
            ? `✓ delivered on ${last.channel} (${last.chars} chars)`
            : `✗ ${last.status}: ${last.detail ?? ''}`}
        </p>
      )}
    </div>
  )
}
