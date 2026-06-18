// SPDX-License-Identifier: AGPL-3.0-or-later

import {
  Rocket, Star, Heart, Cat, Dog, Bird, Flower, Sparkles,
  Zap, Music, Sun, Moon,
} from 'lucide-react'
import type { User } from '@/api/types'
import styles from './Avatar.module.css'

export interface PresetDef {
  key:   string
  label: string
  Icon:  typeof Rocket
  bg:    string
}

export const AVATAR_PRESETS: PresetDef[] = [
  { key: 'rocket',   label: 'Rocket',   Icon: Rocket,   bg: 'linear-gradient(135deg, #4f8ef7, #6b5cff)' },
  { key: 'star',     label: 'Star',     Icon: Star,     bg: 'linear-gradient(135deg, #f59f00, #f08c00)' },
  { key: 'heart',    label: 'Heart',    Icon: Heart,    bg: 'linear-gradient(135deg, #f06595, #e64980)' },
  { key: 'cat',      label: 'Cat',      Icon: Cat,      bg: 'linear-gradient(135deg, #9775fa, #7048e8)' },
  { key: 'dog',      label: 'Dog',      Icon: Dog,      bg: 'linear-gradient(135deg, #ffa94d, #fa5252)' },
  { key: 'bird',     label: 'Bird',     Icon: Bird,     bg: 'linear-gradient(135deg, #22b8cf, #1098ad)' },
  { key: 'flower',   label: 'Flower',   Icon: Flower,   bg: 'linear-gradient(135deg, #ff8787, #ff6b6b)' },
  { key: 'sparkles', label: 'Sparkles', Icon: Sparkles, bg: 'linear-gradient(135deg, #d0bfff, #845ef7)' },
  { key: 'zap',      label: 'Zap',      Icon: Zap,      bg: 'linear-gradient(135deg, #fcc419, #fd7e14)' },
  { key: 'music',    label: 'Music',    Icon: Music,    bg: 'linear-gradient(135deg, #63e6be, #12b886)' },
  { key: 'sun',      label: 'Sun',      Icon: Sun,      bg: 'linear-gradient(135deg, #ffd43b, #fab005)' },
  { key: 'moon',     label: 'Moon',     Icon: Moon,     bg: 'linear-gradient(135deg, #5c7cfa, #4263eb)' },
]

export const PRESETS_BY_KEY = Object.fromEntries(
  AVATAR_PRESETS.map((p) => [p.key, p]),
) as Record<string, PresetDef>

interface Props {
  user:  Pick<User, 'id' | 'username' | 'display_name' | 'avatar' | 'updated_at'>
  size?: number
}

/**
 * Render a user avatar. Resolves in order:
 *  1. `upload:<ext>` — hits `/avatars/{id}.{ext}` with `?v=updated_at` cache-bust.
 *  2. `preset:<key>` — renders a lucide icon on its preset gradient.
 *  3. Fallback — first letter of display_name or username on the accent color.
 */
export default function Avatar({ user, size = 32 }: Props) {
  const value = user.avatar ?? ''

  if (value.startsWith('upload:')) {
    const ext = value.slice('upload:'.length)
    const src = `/avatars/${user.id}.${ext}?v=${user.updated_at}`
    return (
      <img
        src={src}
        alt=""
        className={styles.image}
        style={{ width: size, height: size }}
      />
    )
  }

  if (value.startsWith('preset:')) {
    const key = value.slice('preset:'.length)
    const preset = PRESETS_BY_KEY[key]
    if (preset) {
      const { Icon, bg } = preset
      return (
        <span
          className={styles.preset}
          style={{ width: size, height: size, background: bg }}
          aria-hidden="true"
        >
          <Icon size={Math.round(size * 0.55)} color="white" />
        </span>
      )
    }
  }

  const initial = (user.display_name ?? user.username ?? '?')
    .slice(0, 1)
    .toUpperCase()
  return (
    <span
      className={styles.initial}
      style={{ width: size, height: size, fontSize: Math.round(size * 0.44) }}
      aria-hidden="true"
    >
      {initial}
    </span>
  )
}
