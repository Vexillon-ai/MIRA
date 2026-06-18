// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from 'zustand'
import { persist } from 'zustand/middleware'

interface UiState {
  sidebarCollapsed: boolean
  setSidebarCollapsed: (v: boolean) => void
  toggleSidebar: () => void

  /// Unix ms when the user clicked "Maybe later" on the onboarding welcome
  /// modal. Checked against a 7-day cooldown before re-showing.
  onboardingDismissedAt: number | null
  setOnboardingDismissedAt: (v: number | null) => void

  /// Unix ms when the admin dismissed the "Finish setup" checklist banner.
  /// Once set it stays hidden (it also auto-hides when all steps are done).
  /// Per-browser; the step states themselves are derived from the server.
  setupChecklistDismissedAt: number | null
  setSetupChecklistDismissedAt: (v: number | null) => void
}

export const useUiStore = create<UiState>()(
  persist(
    (set) => ({
      sidebarCollapsed: false,
      setSidebarCollapsed: (v) => set({ sidebarCollapsed: v }),
      toggleSidebar: () => set((s) => ({ sidebarCollapsed: !s.sidebarCollapsed })),

      onboardingDismissedAt: null,
      setOnboardingDismissedAt: (v) => set({ onboardingDismissedAt: v }),

      setupChecklistDismissedAt: null,
      setSetupChecklistDismissedAt: (v) => set({ setupChecklistDismissedAt: v }),
    }),
    { name: 'mira-ui' }
  )
)
