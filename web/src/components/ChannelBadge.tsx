// SPDX-License-Identifier: AGPL-3.0-or-later

import styles from './ChannelBadge.module.css'

const LABELS: Record<string, string> = {
  telegram: 'TG',
  signal:   'SG',
  tui:      'TUI',
  web:      'WEB',
  mobile:   'MOB',
}

export default function ChannelBadge({ channel }: { channel: string }) {
  const label = LABELS[channel]
  if (!label) return null
  return <span className={styles.badge} data-channel={channel}>{label}</span>
}
