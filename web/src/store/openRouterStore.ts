// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from 'zustand'
import { persist } from 'zustand/middleware'

/// User's pinned OpenRouter model IDs. These get merged into the chat
/// model dropdown so the user can pick from the curated catalog without
/// touching mira.toml. Persisted to localStorage so picks survive reload.
interface OpenRouterState {
  pinnedModelIds: string[]
  togglePinned:   (id: string) => void
  isPinned:       (id: string) => boolean
}

export const useOpenRouterStore = create<OpenRouterState>()(
  persist(
    (set, get) => ({
      pinnedModelIds: [],
      togglePinned: (id) => set((s) => ({
        pinnedModelIds: s.pinnedModelIds.includes(id)
          ? s.pinnedModelIds.filter(x => x !== id)
          : [...s.pinnedModelIds, id],
      })),
      isPinned: (id) => get().pinnedModelIds.includes(id),
    }),
    { name: 'mira-openrouter' },
  ),
)
