// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from 'zustand'

export interface AppNotification {
  id: string
  kind: 'inbound_message' | 'conversation_updated'
  channel?: string
  conversationId?: string
  message?: string
  timestamp: number
  read: boolean
}

interface NotificationState {
  notifications: AppNotification[]
  unreadCount: number
  add: (n: Omit<AppNotification, 'id' | 'timestamp' | 'read'>) => void
  markRead: (id: string) => void
  markAllRead: () => void
  clear: () => void
}

export const useNotificationStore = create<NotificationState>((set) => ({
  notifications: [],
  unreadCount: 0,

  add: (n) => {
    const notif: AppNotification = {
      ...n,
      id: `${Date.now()}-${Math.random()}`,
      timestamp: Date.now(),
      read: false,
    }
    set((s) => ({
      notifications: [notif, ...s.notifications].slice(0, 50),
      unreadCount: s.unreadCount + 1,
    }))
  },

  markRead: (id) => set((s) => ({
    notifications: s.notifications.map(n => n.id === id ? { ...n, read: true } : n),
    unreadCount: Math.max(0, s.unreadCount - 1),
  })),

  markAllRead: () => set((s) => ({
    notifications: s.notifications.map(n => ({ ...n, read: true })),
    unreadCount: 0,
  })),

  clear: () => set({ notifications: [], unreadCount: 0 }),
}))
