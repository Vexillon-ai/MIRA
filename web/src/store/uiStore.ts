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

  /// Unix ms when the admin clicked "Skip for now" on the setup walkthrough.
  /// Unlike dismiss, this keeps the slim reminder banner but collapses it and
  /// releases the user-onboarding gate (they chose to move past setup). Per-
  /// browser.
  setupChecklistSkippedAt: number | null
  setSetupChecklistSkippedAt: (v: number | null) => void

  /// Which user id the per-user dismissal flags above belong to. These flags
  /// live in browser localStorage, which survives an uninstall + data-dir wipe
  /// — so without scoping, a reinstall (which mints a fresh admin) would
  /// inherit the old "skipped"/"dismissed" state and the setup wizard would
  /// never re-appear. `syncOwner` resets the per-user flags whenever the
  /// current user id differs from the one they were set under.
  ownerUserId: string | null
  syncOwner: (userId: string) => void
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

      setupChecklistSkippedAt: null,
      setSetupChecklistSkippedAt: (v) => set({ setupChecklistSkippedAt: v }),

      ownerUserId: null,
      syncOwner: (userId) => set((s) => {
        if (s.ownerUserId === userId) return s
        // New (or first-seen) user — e.g. after a reinstall: drop the stale
        // per-user dismissal flags so first-run prompts behave like a fresh
        // browser. (A pre-scoping flag has ownerUserId=null, so it resets once
        // on upgrade; the user re-skips and it's re-pinned to their id.)
        return {
          ownerUserId:               userId,
          onboardingDismissedAt:     null,
          setupChecklistDismissedAt: null,
          setupChecklistSkippedAt:   null,
        }
      }),
    }),
    { name: 'mira-ui' }
  )
)
