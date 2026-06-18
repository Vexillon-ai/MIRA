// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useRef } from 'react'
import toast from 'react-hot-toast'
import { useQueryClient } from '@tanstack/react-query'
import { useAuthStore } from '@/store/authStore'
import { useNotificationStore } from '@/store/notificationStore'
import { getAccessToken } from '@/api/client'

interface RawNotification {
  kind: 'inbound_message' | 'conversation_updated' | 'system_degraded' | 'guardian_alert'
  channel?: string
  conversation_id?: string
  user_id?: string
  message?: string
}

export function useNotifications() {
  const isAuthenticated = useAuthStore((s) => s.isAuthenticated)
  const add             = useNotificationStore((s) => s.add)
  const qc              = useQueryClient()
  const esRef           = useRef<EventSource | null>(null)

  useEffect(() => {
    if (!isAuthenticated) return

    // EventSource doesn't support custom headers, so we pass the token as a
    // query param. The server reads it as a fallback when no Bearer header present.
    const token = getAccessToken()
    const url   = `/api/notifications/stream${token ? `?token=${token}` : ''}`
    const es    = new EventSource(url)
    esRef.current = es

    es.addEventListener('notification', (e: MessageEvent) => {
      try {
        const raw: RawNotification = JSON.parse(e.data)

        if (raw.kind === 'inbound_message') {
          const channel = raw.channel ?? 'unknown'
          const preview = raw.message ? `: "${raw.message.slice(0, 60)}"` : ''
          toast(`New ${channel} message${preview}`, {
            icon: channel === 'signal' ? '📱' : channel === 'telegram' ? '✈️' : '💬',
            duration: 5000,
          })
          add({
            kind: raw.kind,
            channel: raw.channel,
            conversationId: raw.conversation_id,
            message: raw.message,
          })
          if (raw.conversation_id) {
            qc.invalidateQueries({ queryKey: ['messages', raw.conversation_id] })
          }
        } else if (raw.kind === 'system_degraded') {
          // A subsystem (LLM provider, TTS, STT, embeddings, reasoning) fell
          // back to a degraded path. Surface it prominently + invalidate the
          // health view so its banner refreshes.
          toast(raw.message ?? 'A subsystem is degraded', {
            icon: '⚠️',
            duration: 9000,
            style: { borderLeft: '3px solid #d9a03c' },
          })
          qc.invalidateQueries({ queryKey: ['health-degradations'] })
        } else if (raw.kind === 'guardian_alert') {
          // MIRA-Guardian's proactive watch loop flagged a health issue.
          toast(raw.message ?? 'MIRA-Guardian flagged a health issue', {
            icon: '🛡️',
            duration: 12000,
            style: { borderLeft: '3px solid #c0506a' },
          })
          qc.invalidateQueries({ queryKey: ['health-degradations'] })
        } else if (raw.kind === 'conversation_updated') {
          // Refetch the messages query so any open ChatPage on this
          // conversation picks up the new turn live (covers
          // server-initiated writes the page didn't drive itself —
          // watchdog analyze, channel inbound, automation replies).
          if (raw.conversation_id) {
            qc.invalidateQueries({ queryKey: ['messages', raw.conversation_id] })
          }
          if (raw.channel !== 'web') {
            // Only show notification dots for non-web channel updates
            // (web user driving useChat already sees them inline).
            add({
              kind: raw.kind,
              channel: raw.channel,
              conversationId: raw.conversation_id,
            })
          }
        }
      } catch { /* ignore */ }
    })

    es.onerror = () => {
      // EventSource auto-reconnects — just log.
    }

    return () => {
      es.close()
      esRef.current = null
    }
  }, [isAuthenticated, add])
}
