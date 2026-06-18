// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from 'zustand'
import { persist } from 'zustand/middleware'

export type ThemeName =
  | 'mira-dark'
  | 'mira-light'
  | 'mira'
  | 'dracula'
  | 'gruvbox'
  | 'nord'
  | 'ocean'
  | 'forest'

export interface ThemeMeta {
  value: ThemeName
  label: string
  accent: string
  bg: string
  dark: boolean
}

export const THEMES: ThemeMeta[] = [
  { value: 'mira-dark',  label: 'MIRA Dark',   accent: '#7c5cfc', bg: '#0d0e14', dark: true  },
  { value: 'mira-light', label: 'MIRA Light',  accent: '#6c46f5', bg: '#f5f5fa', dark: false },
  { value: 'mira',       label: 'MIRA Brand',  accent: '#4f8ef7', bg: '#080c1c', dark: true  },
  { value: 'dracula',    label: 'Dracula',      accent: '#bd93f9', bg: '#282a36', dark: true  },
  { value: 'gruvbox',    label: 'Gruvbox',      accent: '#d65d0e', bg: '#282828', dark: true  },
  { value: 'nord',       label: 'Nord',         accent: '#88c0d0', bg: '#2e3440', dark: true  },
  { value: 'ocean',      label: 'Ocean',        accent: '#38bdf8', bg: '#0a1628', dark: true  },
  { value: 'forest',     label: 'Forest',       accent: '#4ade80', bg: '#0a1a0f', dark: true  },
]

interface ThemeState {
  theme: ThemeName
  setTheme: (t: ThemeName) => void
}

export const useThemeStore = create<ThemeState>()(
  persist(
    (set) => ({
      theme: 'mira-dark',
      setTheme: (theme) => {
        document.documentElement.setAttribute('data-theme', theme)
        set({ theme })
      },
    }),
    { name: 'mira-theme' }
  )
)

export function applyStoredTheme() {
  const stored = localStorage.getItem('mira-theme')
  if (stored) {
    try {
      const parsed = JSON.parse(stored) as { state?: { theme?: string } }
      const t = parsed?.state?.theme ?? 'mira-dark'
      document.documentElement.setAttribute('data-theme', t)
    } catch {
      document.documentElement.setAttribute('data-theme', 'mira-dark')
    }
  } else {
    document.documentElement.setAttribute('data-theme', 'mira-dark')
  }
}
