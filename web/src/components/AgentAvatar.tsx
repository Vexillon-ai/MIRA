// SPDX-License-Identifier: AGPL-3.0-or-later

import { useQuery } from '@tanstack/react-query'
import { api } from '@/api/client'
import miraLogo from '@/assets/mira-logo.svg'
import { PRESETS_BY_KEY } from './Avatar'
import styles from './Avatar.module.css'

export interface AgentAppearance {
  avatar:            string | null
  avatar_updated_at: number | null
}

export function useAgentAppearance() {
  return useQuery({
    queryKey: ['agent-appearance'],
    queryFn:  async () => {
      const r = await api.get<AgentAppearance>('/api/agent/appearance')
      return r.data
    },
    staleTime: 60_000,
  })
}

interface Props {
  size?:      number
  className?: string
}

/**
 * Render the assistant avatar, resolving in order:
 *  1. `upload:<ext>` — `/avatars/agent.{ext}` with cache-bust.
 *  2. `preset:<key>` — lucide icon on preset gradient.
 *  3. Fallback — MIRA logo.
 */
export default function AgentAvatar({ size = 34, className }: Props) {
  const { data } = useAgentAppearance()
  const value = data?.avatar ?? ''
  const v     = data?.avatar_updated_at ?? 0

  if (value.startsWith('upload:')) {
    const ext = value.slice('upload:'.length)
    return (
      <img
        src={`/avatars/agent.${ext}?v=${v}`}
        alt=""
        className={`${styles.image} ${className ?? ''}`}
        style={{ width: size, height: size }}
      />
    )
  }

  if (value.startsWith('preset:')) {
    const preset = PRESETS_BY_KEY[value.slice('preset:'.length)]
    if (preset) {
      const { Icon, bg } = preset
      return (
        <span
          className={`${styles.preset} ${className ?? ''}`}
          style={{ width: size, height: size, background: bg }}
          aria-hidden="true"
        >
          <Icon size={Math.round(size * 0.55)} color="white" />
        </span>
      )
    }
  }

  return (
    <img
      src={miraLogo}
      alt=""
      className={className}
      style={{ width: size, height: size, filter: 'drop-shadow(0 0 4px rgba(79, 142, 247, 0.35))' }}
    />
  )
}
