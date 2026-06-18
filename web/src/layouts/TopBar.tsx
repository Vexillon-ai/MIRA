// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useRef, useState } from 'react'
import { Bell, ChevronDown, Monitor, MessageSquare, Radio, Menu } from 'lucide-react'
import { useQuery } from '@tanstack/react-query'
import { useNavigate } from 'react-router-dom'
import { useNotificationStore } from '@/store/notificationStore'
import { useChatStore } from '@/store/chatStore'
import { useAuthStore } from '@/store/authStore'
import { conversationsApi } from '@/api/conversations'
import { formatDistanceToNow } from 'date-fns'
import ProfileDialog from '@/components/ProfileDialog'
import Avatar from '@/components/Avatar'
import type { Conversation } from '@/api/types'
import styles from './TopBar.module.css'

const CHANNEL_ICONS: Record<string, React.ReactNode> = {
  web:      <Monitor size={13} />,
  signal:   <Radio size={13} />,
  telegram: <MessageSquare size={13} />,
  tui:      <Monitor size={13} />,
  cli:      <Monitor size={13} />,
}

const CHANNEL_LABELS: Record<string, string> = {
  web:      'Web',
  signal:   'Signal',
  telegram: 'Telegram',
  tui:      'TUI',
  cli:      'CLI',
}

export default function TopBar({ onMenuClick }: { onMenuClick?: () => void }) {
  const navigate      = useNavigate()
  const { unreadCount, notifications, markAllRead } = useNotificationStore()
  const activeConvId  = useChatStore((s) => s.activeConversationId)
  const user          = useAuthStore((s) => s.user)
  const [showChannels, setShowChannels] = useState(false)
  const [showNotifs,   setShowNotifs]   = useState(false)
  const [showProfile,  setShowProfile]  = useState(false)
  const channelRef = useRef<HTMLDivElement>(null)
  const notifRef   = useRef<HTMLDivElement>(null)

  const avatarLabel   = user?.display_name ?? user?.username ?? 'Profile'

  // Close the channel / notifications popovers on any mousedown outside
  // their anchor, and on Escape. Without this, clicking elsewhere on the
  // page leaves the dropdown stuck open until the user clicks the bell
  // again — surprising UX for popover menus.
  useEffect(() => {
    if (!showChannels && !showNotifs) return
    const onMouseDown = (e: MouseEvent) => {
      const target = e.target as Node | null
      if (showChannels && channelRef.current && !channelRef.current.contains(target)) {
        setShowChannels(false)
      }
      if (showNotifs && notifRef.current && !notifRef.current.contains(target)) {
        setShowNotifs(false)
      }
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setShowChannels(false)
        setShowNotifs(false)
      }
    }
    document.addEventListener('mousedown', onMouseDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onMouseDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [showChannels, showNotifs])

  const { data: conversations = [] } = useQuery({
    queryKey: ['conversations'],
    queryFn:  conversationsApi.list,
    staleTime: 15_000,
  })

  // Group conversations by channel for the dropdown
  const byChannel = conversations.reduce<Record<string, Conversation[]>>((acc, conv) => {
    const ch = conv.channel || 'web'
    if (!acc[ch]) acc[ch] = []
    acc[ch].push(conv)
    return acc
  }, {})

  const channels = Object.keys(byChannel).sort()

  const activeConv = activeConvId
    ? conversations.find(c => c.id === activeConvId)
    : null

  // Drive the dropdown button's icon/label from the active conversation's
  // channel so the TopBar reflects whether the user is viewing a Web, TUI,
  // Signal, or Telegram session. Falls back to "Web" when no conversation
  // is selected (matches /chat without an id).
  const activeChannel = activeConv?.channel ?? 'web'
  const activeIcon  = CHANNEL_ICONS[activeChannel]  ?? <Monitor size={14} />
  const activeLabel = CHANNEL_LABELS[activeChannel] ?? activeChannel

  return (
    <div className={styles.topbar}>
      {/* Mobile hamburger */}
      {onMenuClick && (
        <button className={styles.menuBtn} onClick={onMenuClick} aria-label="Toggle sidebar">
          <Menu size={18} />
        </button>
      )}
      {/* Active conversation title */}
      <div className={styles.title}>
        {activeConv ? activeConv.title || 'Untitled' : 'MIRA'}
      </div>

      <div className={styles.actions}>
        {/* Channel / session switcher */}
        <div className={styles.dropdown} ref={channelRef}>
          <button
            className={styles.channelBtn}
            onClick={() => setShowChannels(v => !v)}
          >
            {activeIcon}
            {activeLabel}
            <ChevronDown size={12} />
          </button>

          {showChannels && (
            <div className={styles.dropdownMenu}>
              {channels.map(ch => (
                <div key={ch} className={styles.channelGroup}>
                  <div className={styles.channelHeader}>
                    {CHANNEL_ICONS[ch] ?? <MessageSquare size={13} />}
                    {CHANNEL_LABELS[ch] ?? ch}
                    <span className={styles.channelCount}>{byChannel[ch].length}</span>
                  </div>
                  {byChannel[ch].slice(0, 5).map(conv => (
                    <button
                      key={conv.id}
                      className={`${styles.channelItem} ${activeConvId === String(conv.id) ? styles.channelItemActive : ''}`}
                      onClick={() => {
                        navigate(`/chat/${conv.id}`)
                        setShowChannels(false)
                      }}
                    >
                      <span className={styles.channelItemTitle}>
                        {conv.title || 'Untitled'}
                      </span>
                      {conv.last_message_at && (
                        <span className={styles.channelItemTime}>
                          {formatDistanceToNow(new Date(conv.last_message_at * 1000), { addSuffix: true })}
                        </span>
                      )}
                    </button>
                  ))}
                </div>
              ))}
              {channels.length === 0 && (
                <p className={styles.empty}>No sessions yet</p>
              )}
            </div>
          )}
        </div>

        {/* Notification bell */}
        <div className={styles.dropdown} ref={notifRef}>
          <button
            className={`${styles.notifBtn} ${unreadCount > 0 ? styles.notifBtnActive : ''}`}
            onClick={() => { setShowNotifs(v => !v); if (!showNotifs) markAllRead() }}
          >
            <Bell size={16} />
            {unreadCount > 0 && (
              <span className={styles.badge}>{unreadCount > 9 ? '9+' : unreadCount}</span>
            )}
          </button>

          {showNotifs && (
            <div className={`${styles.dropdownMenu} ${styles.notifMenu}`}>
              <div className={styles.notifHeader}>
                <span>Notifications</span>
              </div>
              {notifications.length === 0 && (
                <p className={styles.empty}>No notifications</p>
              )}
              {notifications.slice(0, 10).map(n => (
                <button
                  key={n.id}
                  className={`${styles.notifItem} ${!n.read ? styles.notifItemUnread : ''}`}
                  onClick={() => {
                    // Route based on what the notification actually
                    // points at:
                    //   * conversationId set → open that chat
                    //   * otherwise → most likely a system / watchdog
                    //     event with no chat target; route to /watchdog
                    //     instead of dumping the user at an empty
                    //     /chat screen (the previous behaviour).
                    if (n.conversationId) {
                      navigate(`/chat/${n.conversationId}`)
                    } else {
                      navigate('/watchdog')
                    }
                    setShowNotifs(false)
                  }}
                >
                  <span className={styles.notifIcon}>
                    {n.channel === 'signal' ? '📱' : n.channel === 'telegram' ? '✈️' : '💬'}
                  </span>
                  <div className={styles.notifContent}>
                    <span className={styles.notifChannel}>{n.channel ?? 'web'}</span>
                    {n.message && <span className={styles.notifMessage}>{n.message.slice(0, 80)}</span>}
                    <span className={styles.notifTime}>
                      {formatDistanceToNow(new Date(n.timestamp), { addSuffix: true })}
                    </span>
                  </div>
                </button>
              ))}
            </div>
          )}
        </div>

        {/* User avatar — opens profile dialog */}
        {user && (
          <button
            className={styles.avatarBtn}
            onClick={() => setShowProfile(true)}
            title={`${avatarLabel} — open profile`}
            aria-label="Open profile"
          >
            <Avatar user={user} size={28} />
            <span className={styles.avatarName}>{avatarLabel}</span>
          </button>
        )}
      </div>

      <ProfileDialog open={showProfile} onClose={() => setShowProfile(false)} />
    </div>
  )
}
