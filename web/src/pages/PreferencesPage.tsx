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
import ThemePicker from '@/components/ThemePicker'
import VoiceReplyPrefs from '@/components/VoiceReplyPrefs'
import styles from './PreferencesPage.module.css'

export default function PreferencesPage() {
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
          </div>
        </section>

        <section className={styles.card}>
          <div className={styles.cardHead}><Palette size={15} /> Appearance</div>
          <div className={styles.cardBody}>
            <p className={styles.cardDesc}>Pick a theme for the web interface. Saved in this browser.</p>
            <ThemePicker />
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
