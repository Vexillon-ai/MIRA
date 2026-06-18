// SPDX-License-Identifier: AGPL-3.0-or-later

import { useTtsVoices } from '@/hooks/useTtsVoices'
import styles from './VoiceIdPicker.module.css'

interface VoiceIdPickerProps {
  /** Current voice id; empty string = inherit. */
  value:        string
  onChange:     (next: string) => void
  /** Optional CSS class — usually the form `input` style. */
  className?:   string
  /** Label shown for the empty / inherit option. */
  inheritLabel?: string
  ariaLabel?:   string
  disabled?:    boolean
  /**
   * If provided, only show voices for this channel's routed backend.
   * Resolved via `/api/tts/status`'s `routing` map. Mutually exclusive
   * with `backend` — if both are set, `backend` wins.
   */
  channel?:     string
  /** Filter the list to a specific backend. Hides the optgroup label. */
  backend?:     string
}

/**
 * Dropdown of TTS voices. By default it shows every voice across every
 * configured backend, grouped by backend so users can see at a glance
 * which engine each voice belongs to. When `channel` or `backend` is
 * supplied the list is scoped to the routed backend for that channel,
 * which prevents picking a voice the routed engine doesn't recognize
 * (e.g. a Piper voice on a channel pinned to `openai_compat`).
 *
 * If the current value isn't in the loaded list (typo from a config edit,
 * or a backend the server can no longer reach) the picker still renders it
 * as a disabled option labelled "(unknown)" so the user can see and
 * change it.
 */
export default function VoiceIdPicker({
  value,
  onChange,
  className,
  inheritLabel = 'Inherit (default)',
  ariaLabel,
  disabled,
  channel,
  backend,
}: VoiceIdPickerProps) {
  const { data, isLoading } = useTtsVoices()
  const allGroups = data?.voices ?? {}
  const routing   = data?.routing ?? {}

  const resolvedBackend =
    backend ??
    (channel ? routing[channel] : undefined)

  const groups: Record<string, typeof allGroups[string]> = resolvedBackend
    ? (allGroups[resolvedBackend]
        ? { [resolvedBackend]: allGroups[resolvedBackend] }
        : {})
    : allGroups

  const knownIds = new Set(
    Object.values(groups).flat().map((v) => v.id),
  )
  const showUnknown = value !== '' && !knownIds.has(value)

  const cls = [styles.picker, className].filter(Boolean).join(' ')

  return (
    <select
      className={cls}
      aria-label={ariaLabel}
      value={value}
      disabled={disabled || isLoading}
      onChange={(e) => onChange(e.target.value)}
    >
      <option value="">{isLoading ? 'Loading voices…' : inheritLabel}</option>
      {showUnknown && (
        <option value={value} disabled>{value} (unknown for {resolvedBackend ?? 'any backend'})</option>
      )}
      {Object.entries(groups).map(([b, list]) => (
        list.length === 0 ? null : (
          // Hide the optgroup label when we're already filtered to one backend —
          // the field's own context (per-channel row) makes it redundant.
          resolvedBackend ? (
            list.map((v) => (
              <option key={`${b}:${v.id}`} value={v.id}>
                {v.name && v.name !== v.id ? `${v.id} — ${v.name}` : v.id}
              </option>
            ))
          ) : (
            <optgroup key={b} label={b}>
              {list.map((v) => (
                <option key={`${b}:${v.id}`} value={v.id}>
                  {v.name && v.name !== v.id ? `${v.id} — ${v.name}` : v.id}
                </option>
              ))}
            </optgroup>
          )
        )
      ))}
    </select>
  )
}
