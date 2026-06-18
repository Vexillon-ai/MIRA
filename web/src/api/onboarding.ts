// SPDX-License-Identifier: AGPL-3.0-or-later

import axios from 'axios'
import { api } from './client'
import type { OnboardingState, StartOnboardingResponse } from './types'

interface PostCompleteChatResponse {
  conversation_id: string
}

export const onboardingApi = {
  state: () =>
    api.get<OnboardingState>('/api/onboarding/state').then((r) => r.data),

  start: () =>
    api.post<StartOnboardingResponse>('/api/onboarding/start').then((r) => r.data),

  restartGroup: (group_id: string) =>
    api.post<void>('/api/onboarding/restart-group', { group_id }).then((r) => r.data),

  reset: () =>
    api.post<void>('/api/onboarding/reset', {}).then((r) => r.data),

  /// User-invoked finalization backstop. Stamps `onboarded_at` when the
  /// required-group activity guard passes. Used by the "Finish" button on
  /// the progress strip when the LLM reached the end conversationally
  /// without calling the `complete_onboarding` tool.
  finalize: () =>
    api.post<void>('/api/onboarding/finalize', {}).then((r) => r.data),

  /// Create a fresh chat after onboarding finishes. The server seeds a
  /// personalized opener using the profile details just captured.
  postCompleteChat: () =>
    api.post<PostCompleteChatResponse>('/api/onboarding/post-complete-chat').then((r) => r.data),
}

/// Surface a user-facing message for an onboarding-endpoint failure. The
/// backend returns plain-text bodies for 4xx (e.g. "unknown group_id: foo"),
/// so prefer that. Falls through to the caller's default for 5xx / network.
export function onboardingErrorMessage(err: unknown, fallback: string): string {
  if (axios.isAxiosError(err) && err.response) {
    const status = err.response.status
    const data   = err.response.data
    if (status >= 400 && status < 500) {
      if (typeof data === 'string' && data.trim()) return data.trim()
      if (data && typeof data === 'object' && 'message' in data && typeof (data as { message: unknown }).message === 'string') {
        return (data as { message: string }).message
      }
    }
  }
  return fallback
}
