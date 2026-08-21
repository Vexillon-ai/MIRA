// SPDX-License-Identifier: AGPL-3.0-or-later

// "My Preferences" — the single home for a user's own settings, reachable by
// EVERY authenticated user (not just admins). Consolidates the per-user surfaces
// that used to be scattered: mobile pairing + browser push (previously trapped in
// the admin-only Settings page), the web-interface theme, and per-channel voice
// reply preferences (moved here from the Profile dialog).
//
// Global/instance configuration stays in the admin-only Settings page; this page
// only ever touches the caller's own per-user state.

import { SlidersHorizontal, Bell, Palette, Volume2 } from 'lucide-react'
import NotificationSettings from '@/components/NotificationSettings'
import CompanionCheckinTest from '@/components/CompanionCheckinTest'
import BriefingTestButton from '@/components/BriefingTestButton'
import ThemePicker from '@/components/ThemePicker'
import VoiceReplyPrefs from '@/components/VoiceReplyPrefs'
import { useUiStore } from '@/store/uiStore'
import styles from './PreferencesPage.module.css'

export default function PreferencesPage() {
  const { sidebarCollapsed, setSidebarCollapsed } = useUiStore()
  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <SlidersHorizontal size={20} />
        <div>
          <h1>My Preferences</h1>
          <p className={styles.subtitle}>Your own settings — these apply to you, not the whole instance.</p>
        </div>
      </div>

      <div className={styles.body}>
        <section className={styles.card}>
          <div className={styles.cardHead}><Bell size={15} /> Notifications &amp; devices</div>
          <div className={styles.cardBody}>
            <p className={styles.cardDesc}>
              Pair your phone (scan the QR in the MIRA app) and turn on browser push so MIRA can reach you.
            </p>
            <NotificationSettings />
            <p className={styles.cardDesc} style={{ marginTop: 18 }}>
              Test that proactive messages actually reach you:
            </p>
            <CompanionCheckinTest />
            <BriefingTestButton />
          </div>
        </section>

        <section className={styles.card}>
          <div className={styles.cardHead}><Palette size={15} /> Appearance</div>
          <div className={styles.cardBody}>
            <p className={styles.cardDesc}>Web-interface preferences for this browser.</p>
            <ThemePicker />
            <label style={{ display: 'flex', alignItems: 'center', gap: 8, marginTop: 16, fontSize: 13 }}>
              <input
                type="checkbox"
                checked={sidebarCollapsed}
                onChange={(e) => setSidebarCollapsed(e.target.checked)}
              />
              Start with the sidebar collapsed
            </label>
          </div>
        </section>

        <section className={styles.card}>
          <div className={styles.cardHead}><Volume2 size={15} /> Voice replies</div>
          <div className={styles.cardBody}>
            <VoiceReplyPrefs />
          </div>
        </section>
      </div>
    </div>
  )
}
