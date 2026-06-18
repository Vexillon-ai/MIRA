// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'
import type { LoginRequest, LoginResponse, User } from './types'

export const authApi = {
  login: (data: LoginRequest) =>
    api.post<LoginResponse>('/api/auth/login', data).then((r) => r.data),

  logout: () =>
    api.post('/api/auth/logout').then((r) => r.data),

  refresh: () =>
    api.post<{ access_token: string; token_type: string; expires_in: number }>('/api/auth/refresh').then((r) => r.data),

  me: () =>
    api.get<User>('/api/auth/me').then((r) => r.data),

  /** Enabled OIDC providers — drives the SSO login buttons (empty = off). */
  oidcProviders: () =>
    api.get<OidcProviderButton[]>('/api/auth/oidc/providers').then((r) => r.data),

  // ── Self-service onboarding ──────────────────────────────────────────────
  signupConfig: () =>
    api.get<SignupConfig>('/api/auth/signup/config').then((r) => r.data),
  inviteInfo: (token: string) =>
    api.get<InviteInfo>(`/api/auth/invite?token=${encodeURIComponent(token)}`).then((r) => r.data),
  signup: (data: SignupRequest) =>
    api.post<SignupResponse>('/api/auth/signup', data).then((r) => r.data),
}

export interface OidcProviderButton {
  id:           string
  display_name: string
}

export interface SignupConfig {
  open_signup:      boolean
  require_approval: boolean
}

export interface InviteInfo {
  valid:      boolean
  role:       string | null
  email_hint: string | null
}

export interface SignupRequest {
  username:      string
  password:      string
  email?:        string
  display_name?: string
  invite_token?: string
}

export interface SignupResponse {
  status:        'active' | 'pending'
  access_token?: string
  user?:         User
}
