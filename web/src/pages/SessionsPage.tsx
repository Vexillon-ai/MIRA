// SPDX-License-Identifier: AGPL-3.0-or-later

import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Wifi, WifiOff, Trash2, Monitor, Radio, MessageSquare } from 'lucide-react'
import { sessionsApi } from '@/api/sessions'
import { useAuthStore } from '@/store/authStore'
import { formatDistanceToNow } from 'date-fns'
import styles from './SessionsPage.module.css'

const CHANNEL_ICONS: Record<string, React.ReactNode> = {
  web:      <Monitor size={14} />,
  tui:      <Monitor size={14} />,
  cli:      <Monitor size={14} />,
  signal:   <Radio size={14} />,
  telegram: <MessageSquare size={14} />,
}

export default function SessionsPage() {
  const qc = useQueryClient()
  const { user } = useAuthStore()

  const { data: sessions = [], isLoading } = useQuery({
    queryKey: ['sessions'],
    queryFn:  sessionsApi.list,
    refetchInterval: 10_000,
  })

  const evictMut = useMutation({
    mutationFn: (id: string) => sessionsApi.evict(id),
    onSuccess:  () => qc.invalidateQueries({ queryKey: ['sessions'] }),
  })

  const now = Date.now() / 1000  // Unix seconds

  if (isLoading) return <div className={styles.loading}>Loading sessions…</div>

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1>Sessions</h1>
          <p>{sessions.length} active session{sessions.length !== 1 ? 's' : ''}</p>
        </div>
      </div>

      <div className={styles.list}>
        {sessions.length === 0 && (
          <div className={styles.emptyState}>
            <WifiOff size={40} />
            <p>No active sessions</p>
          </div>
        )}

        {sessions.map((s) => {
          const isRecent = now - s.last_active < 300  // active within 5 min
          return (
            <div key={s.session_id} className={styles.row}>
              <div className={`${styles.statusDot} ${isRecent ? styles.dotActive : ''}`} />

              <div className={styles.channel}>
                {CHANNEL_ICONS[s.channel] ?? <Wifi size={14} />}
              </div>

              <div className={styles.info}>
                <div className={styles.sessionId}>
                  {s.session_id.length > 32 ? s.session_id.slice(0, 32) + '…' : s.session_id}
                </div>
                <div className={styles.meta}>
                  {s.channel} · user {s.user_id.length > 20 ? s.user_id.slice(0, 20) + '…' : s.user_id}
                  {' · '}{s.message_count} messages
                </div>
              </div>

              <div className={styles.time}>
                {formatDistanceToNow(new Date(s.last_active * 1000), { addSuffix: true })}
              </div>

              {user?.role === 'admin' && (
                <button
                  className={`${styles.iconBtn} ${styles.danger}`}
                  onClick={() => {
                    if (confirm(`Evict session ${s.session_id}?`)) evictMut.mutate(s.session_id)
                  }}
                  title="Evict session"
                >
                  <Trash2 size={14} />
                </button>
              )}
            </div>
          )
        })}
      </div>
    </div>
  )
}
