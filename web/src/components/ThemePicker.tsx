// SPDX-License-Identifier: AGPL-3.0-or-later

// Per-user web-interface theme picker. The theme is a per-browser preference
// (persisted in localStorage via themeStore) — no server round-trip — so it's
// safe on the per-user "My Preferences" page.

import { Check } from 'lucide-react'
import { THEMES, useThemeStore } from '@/store/themeStore'
import styles from './ThemePicker.module.css'

export default function ThemePicker() {
  const { theme, setTheme } = useThemeStore()
  return (
    <div className={styles.themeGrid}>
      {THEMES.map((t) => (
        <button
          key={t.value}
          className={`${styles.themeCard} ${theme === t.value ? styles.themeCardActive : ''}`}
          onClick={() => setTheme(t.value)}
          aria-pressed={theme === t.value}
        >
          <div className={styles.themePreview} style={{ background: t.bg }}>
            <div className={styles.themePreviewAccent} style={{ background: t.accent }} />
            <div className={styles.themePreviewBar} style={{ background: t.accent + '30' }} />
            <div className={styles.themePreviewBar} style={{ background: t.accent + '18' }} />
          </div>
          <div className={styles.themeCardLabel}>
            {t.label}
            {theme === t.value && <span className={styles.themeCardCheck}><Check size={11} /></span>}
          </div>
        </button>
      ))}
    </div>
  )
}
