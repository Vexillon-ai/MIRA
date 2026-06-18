// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from 'zustand'
import { persist } from 'zustand/middleware'

/**
 * Per-browser voice preferences. Auto-play is opt-in (defaults off) per the
 *  design doc — users explicitly toggle it on. Persists to localStorage
 * so the choice survives reloads. A future stage will sync these to the
 * `user_profile.voice_*` columns once we wire that path.
 */
interface VoiceState {
  enabled: boolean
  /** Backend voice id override, or null to use the server default. */
  voiceId: string | null
  /** Speech rate multiplier (0.5–2.0). 1.0 = normal. */
  speed:   number
  setEnabled: (v: boolean) => void
  setVoiceId: (v: string | null) => void
  setSpeed:   (v: number) => void
  toggle:     () => void
}

export const useVoiceStore = create<VoiceState>()(
  persist(
    (set) => ({
      enabled: false,
      voiceId: null,
      speed:   1.0,
      setEnabled: (enabled) => set({ enabled }),
      setVoiceId: (voiceId) => set({ voiceId }),
      setSpeed:   (speed)   => set({ speed }),
      toggle:               () => set((s) => ({ enabled: !s.enabled })),
    }),
    { name: 'mira-voice' },
  ),
)
