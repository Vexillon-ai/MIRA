// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from 'zustand'
import { setAccessToken } from '@/api/client'
import { authApi } from '@/api/auth'
import { useChatStore } from '@/store/chatStore'
import { queryClient } from '@/api/queryClient'
import type { User } from '@/api/types'

// Wipe all per-user client state on a user switch. React Query keys aren't
// user-scoped and the chat store is in-memory, so without this the previous
// account's conversations / open chat / cached data can surface to the next
// user in the same browser (the "new user saw the previous user's last chat"
// isolation bug). Synchronous so no stale frame renders before the new user.
function clearPerUserState() {
  useChatStore.getState().reset()
  queryClient.clear()
}

interface AuthState {
  user: User | null
  isLoading: boolean
  isAuthenticated: boolean

  login: (username: string, password: string) => Promise<void>
  logout: () => Promise<void>
  refresh: () => Promise<boolean>
  setUser: (user: User | null) => void
}

export const useAuthStore = create<AuthState>((set) => ({
  user: null,
  isLoading: true,
  isAuthenticated: false,

  setUser: (user) => set({ user, isAuthenticated: !!user, isLoading: false }),

  login: async (username, password) => {
    const data = await authApi.login({ username, password })
    // Drop any prior account's cached/in-memory state BEFORE we flip to the new
    // user, so the chat view never renders the previous user's conversation.
    clearPerUserState()
    setAccessToken(data.access_token)
    set({ user: data.user, isAuthenticated: true, isLoading: false })
  },

  logout: async () => {
    try { await authApi.logout() } catch { /* ignore */ }
    setAccessToken(null)
    clearPerUserState()
    set({ user: null, isAuthenticated: false, isLoading: false })
  },

  refresh: async () => {
    try {
      const data = await authApi.refresh()
      setAccessToken(data.access_token)
      const user = await authApi.me()
      set({ user, isAuthenticated: true, isLoading: false })
      return true
    } catch {
      setAccessToken(null)
      set({ user: null, isAuthenticated: false, isLoading: false })
      return false
    }
  },
}))
