// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Shared state for the freshly-installed admin's "Finish setup" walkthrough —
// the steps `mira setup` defers to the web UI (Voice, a Channel, check-ins).
// Both surfaces read from here so the two first-run flows can't race:
//   - SetupChecklistBanner renders the walkthrough.
//   - OnboardingWelcomeModal waits for `complete` before introducing itself,
//     so user onboarding never pops up in front of the install walkthrough.
// Both call this hook with the same query key, so React Query dedupes to a
// single fetch.

import { useQuery } from '@tanstack/react-query'
import { api } from '@/api/client'
import { useAuthStore } from '@/store/authStore'
import { useUiStore } from '@/store/uiStore'

export interface SetupStatus {
  voice: boolean
  channel: boolean
  companion: boolean
}

export interface SetupStep {
  key: keyof SetupStatus
  label: string
  desc: string
  to: string
}

export const SETUP_STEPS: SetupStep[] = [
  { key: 'voice',     label: 'Set up voice',      desc: 'Spoken replies & check-ins (text-to-speech)',     to: '/settings' },
  { key: 'channel',   label: 'Connect a channel', desc: 'Reach MIRA on Telegram, Signal, Discord, email…', to: '/channel-accounts' },
  { key: 'companion', label: 'Enable check-ins',  desc: 'Proactive companion check-ins & a daily briefing', to: '/settings' },
]

export interface SetupChecklistState {
  /** The setup walkthrough applies to this user (an admin who hasn't dismissed it). */
  active: boolean
  /** Live per-step done-state; undefined until the first fetch resolves. */
  status: SetupStatus | undefined
  doneCount: number
  /** Every step is done. */
  allDone: boolean
  /** The admin clicked "Skip for now" — slim banner stays, onboarding released. */
  skipped: boolean
  /**
   * The setup wizard is the active first-run surface (an admin who hasn't
   * skipped it). The wizard shows while this is true; the slim banner shows
   * once it's false (skipped/dismissed). Onboarding's `complete` gate already
   * subsumes this — it's `allDone || !wizardActive`.
   */
  wizardActive: boolean
  /**
   * The walkthrough is finished for gating purposes: it never applied
   * (non-admin, or the admin dismissed the banner), every step is done, or the
   * admin skipped. While an admin still has outstanding steps and hasn't
   * skipped — including during the initial load before status arrives — this
   * stays false, so dependent flows (user onboarding) hold back instead of
   * flashing in front of the walkthrough.
   */
  complete: boolean
}

export function useSetupChecklist(): SetupChecklistState {
  const role = useAuthStore((s) => s.user?.role)
  const isAdmin = role === 'admin'
  const dismissedAt = useUiStore((s) => s.setupChecklistDismissedAt)
  const skippedAt = useUiStore((s) => s.setupChecklistSkippedAt)
  const skipped = skippedAt !== null

  const active = isAdmin && !dismissedAt

  const { data: status } = useQuery<SetupStatus>({
    queryKey: ['setup-checklist'],
    enabled: active,
    staleTime: 60_000,
    retry: false,
    queryFn: async () => {
      const [voice, channels, briefing] = await Promise.all([
        api.get('/api/tts/status').then((r) => r.data as { enabled?: boolean }).catch(() => null),
        api.get('/api/channel-accounts').then((r) => r.data as Array<{ enabled?: boolean }>).catch(() => []),
        api.get('/api/me/briefing').then((r) => r.data as { enabled?: boolean; companion_active?: boolean }).catch(() => null),
      ])
      return {
        voice: !!voice?.enabled,
        channel: Array.isArray(channels) && channels.some((c) => c.enabled),
        companion: !!(briefing?.companion_active || briefing?.enabled),
      }
    },
  })

  const doneCount = status ? SETUP_STEPS.filter((s) => status[s.key]).length : 0
  const allDone = doneCount === SETUP_STEPS.length
  // Not active → nothing to wait on. Skipped → the admin chose to move on.
  // Otherwise complete only once status has loaded AND every step is done.
  const complete = !active || skipped || (!!status && allDone)
  const wizardActive = active && !skipped

  return { active, status, doneCount, allDone, skipped, complete, wizardActive }
}
