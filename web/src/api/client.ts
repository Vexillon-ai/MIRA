// SPDX-License-Identifier: AGPL-3.0-or-later

import axios, { type AxiosError, type InternalAxiosRequestConfig } from 'axios'
import type { RefreshResponse } from './types'

const BASE = import.meta.env.VITE_API_BASE ?? ''

export const api = axios.create({
  baseURL: BASE,
  withCredentials: true, // send refresh-token cookie
})

// ── Token storage (in-memory only — never localStorage) ─────────────────────

let accessToken: string | null = null

export function setAccessToken(token: string | null) {
  accessToken = token
}

export function getAccessToken(): string | null {
  return accessToken
}

// ── Request interceptor: attach Bearer token ──────────────────────────────────

api.interceptors.request.use((config: InternalAxiosRequestConfig) => {
  if (accessToken) {
    config.headers.Authorization = `Bearer ${accessToken}`
  }
  return config
})

// ── Response interceptor: auto-refresh on 401 ────────────────────────────────

let isRefreshing = false
let refreshQueue: Array<(token: string) => void> = []

api.interceptors.response.use(
  (res) => res,
  async (error: AxiosError) => {
    const original = error.config as InternalAxiosRequestConfig & { _retry?: boolean }

    if (
      error.response?.status === 401 &&
      !original._retry &&
      !original.url?.includes('/api/auth/')
    ) {
      if (isRefreshing) {
        return new Promise((resolve) => {
          refreshQueue.push((token) => {
            original.headers.Authorization = `Bearer ${token}`
            resolve(api(original))
          })
        })
      }

      original._retry = true
      isRefreshing = true

      try {
        const { data } = await api.post<RefreshResponse>('/api/auth/refresh')
        setAccessToken(data.access_token)
        refreshQueue.forEach((cb) => cb(data.access_token))
        refreshQueue = []
        original.headers.Authorization = `Bearer ${data.access_token}`
        return api(original)
      } catch {
        setAccessToken(null)
        refreshQueue = []
        window.dispatchEvent(new Event('mira:auth:logout'))
        return Promise.reject(error)
      } finally {
        isRefreshing = false
      }
    }

    return Promise.reject(error)
  },
)
