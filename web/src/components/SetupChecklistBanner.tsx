// SPDX-License-Identifier: AGPL-3.0-or-later
//
// "Finish setup" — a slim, expandable banner that guides a freshly-installed
// admin through the steps the `mira setup` terminal wizard deferred to the web
// UI: Voice, a Channel, and proactive check-ins. Each step's done-state is
// derived from existing endpoints; the banner auto-hides once every step is
// done, or when the admin dismisses it (remembered per browser).

import { useState } from 'react'
import { Link } from 'react-router-dom'
import { useQuery } from '@tanstack/react-query'
import { Sparkles, Check, ChevronDown, ChevronUp, X as XIcon, ArrowRight } from 'lucide-react'
import { api } from '@/api/client'
import { useAuthStore } from '@/store/authStore'
import { useUiStore } from '@/store/uiStore'
import styles from './SetupChecklistBanner.module.css'

interface SetupStatus {
  voice: boolean
  channel: boolean
  companion: boolean
}

interface Step {
  key: keyof SetupStatus
  label: string
  desc: string
  to: string
}

const STEPS: Step[] = [
  { key: 'voice',     label: 'Set up voice',      desc: 'Spoken replies & check-ins (text-to-speech)',          to: '/settings' },
  { key: 'channel',   label: 'Connect a channel', desc: 'Reach MIRA on Telegram, Signal, Discord, email…',      to: '/channel-accounts' },
  { key: 'companion', label: 'Enable check-ins',  desc: 'Proactive companion check-ins & a daily briefing',     to: '/settings' },
]

export default function SetupChecklistBanner() {
  const user = useAuthStore((s) => s.user)
  const isAdmin = user?.role === 'admin'
  const dismissedAt = useUiStore((s) => s.setupChecklistDismissedAt)
  const setDismissedAt = useUiStore((s) => s.setSetupChecklistDismissedAt)
  const [expanded, setExpanded] = useState(false)

  const show = isAdmin && !dismissedAt

  const { data } = useQuery<SetupStatus>({
    queryKey: ['setup-checklist'],
    enabled: show,
    staleTime: 60_000,
    retry: false,
    queryFn: async () => {
      const [voice, channels, briefing] = await Promise.all([
        api.get('/api/tts/status').then((r) => r.data as { enabled?: boolean }).catch(() => null),
        api.get('/api/channel-accounts').then((r) => r.data as Array<{ enabled?: boolean }>).catch(() => []),
        api.get('/api/me/briefing').then((r) => r.data as { enabled?: boolean; companion_active?: boolean }).catch(() => null),
      ])
      return {
        voice: !!voice?.enabled,
        channel: Array.isArray(channels) && channels.some((c) => c.enabled),
        companion: !!(briefing?.companion_active || briefing?.enabled),
      }
    },
  })

  if (!show || !data) return null

  const doneCount = STEPS.filter((s) => data[s.key]).length
  if (doneCount === STEPS.length) return null // all done → nothing to nag about

  return (
    <div className={styles.wrap} role="status">
      <div className={styles.header}>
        <button
          className={styles.bar}
          onClick={() => setExpanded((v) => !v)}
          aria-expanded={expanded}
        >
          <Sparkles size={14} className={styles.spark} />
          <span className={styles.title}>Finish setting up MIRA</span>
          <span className={styles.count}>{doneCount}/{STEPS.length} done</span>
          {expanded ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
        </button>
        <button
          className={styles.close}
          title="Dismiss"
          aria-label="Dismiss setup checklist"
          onClick={() => setDismissedAt(Date.now())}
        >
          <XIcon size={13} />
        </button>
      </div>

      {expanded && (
        <ul className={styles.list}>
          {STEPS.map((s) => {
            const done = data[s.key]
            return (
              <li key={s.key} className={`${styles.item} ${done ? styles.itemDone : ''}`}>
                <span className={`${styles.dot} ${done ? styles.dotDone : ''}`}>
                  {done && <Check size={12} />}
                </span>
                <span className={styles.itemText}>
                  <span className={styles.itemLabel}>{s.label}</span>
                  <span className={styles.itemDesc}>{s.desc}</span>
                </span>
                {!done && (
                  <Link className={styles.action} to={s.to} onClick={() => setExpanded(false)}>
                    Set up <ArrowRight size={12} />
                  </Link>
                )}
              </li>
            )
          })}
        </ul>
      )}
    </div>
  )
}
