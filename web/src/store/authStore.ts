// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from 'zustand'
import { setAccessToken } from '@/api/client'
import { authApi } from '@/api/auth'
import type { User } from '@/api/types'

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
    setAccessToken(data.access_token)
    set({ user: data.user, isAuthenticated: true, isLoading: false })
  },

  logout: async () => {
    try { await authApi.logout() } catch { /* ignore */ }
    setAccessToken(null)
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
