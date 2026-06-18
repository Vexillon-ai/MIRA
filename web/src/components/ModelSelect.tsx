// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState, useMemo } from 'react'
import { useProviderCatalog } from '@/hooks/useProviderCatalog'
import styles from './ModelSelect.module.css'

interface ModelSelectProps {
  /** Provider slug — matches `providers.<slug>` in mira_config.json. */
  provider:    string
  value:       string
  onChange:    (next: string) => void
  /** Placeholder for the free-text fallback. */
  placeholder?: string
  /** Whether to fetch the provider's catalog. Defaults to true; pass the
   *  provider's enabled state so a disabled provider isn't contacted just
   *  because its Settings section rendered. */
  catalogEnabled?: boolean
}

/**
 * Per-provider model picker. Renders a `<select>` populated from
 * `/api/providers/{slug}/catalog` so users don't have to type model
 * ids by hand. Falls back to a free-text `<input>` when:
 *
 * - The catalog hasn't been fetched yet (still loading).
 * - The catalog fetch failed (provider not configured, upstream
 *   blew up, etc.).
 * - The catalog is empty (rare; provider returned no models).
 * - The user clicks "Type custom id" to override.
 *
 * Styling mirrors the SettingsPage form inputs (lighter background
 * than the section card, monospace, matching border + focus colour)
 * so the picker reads as part of the same row as the URL / timeout
 * fields above it.
 */
export default function ModelSelect({
  provider, value, onChange, placeholder, catalogEnabled = true,
}: ModelSelectProps) {
  const { data, isLoading, error } = useProviderCatalog(provider, catalogEnabled)
  const [customMode, setCustomMode] = useState(false)

  // Build the option list. Always include the current value (even if
  // not in the catalog) so a typo doesn't silently disappear.
  const options = useMemo(() => {
    const list = data?.entries ?? []
    const hasCurrent = list.some((e) => e.id === value)
    return hasCurrent || !value
      ? list
      : [{ id: value, display_name: undefined, notes: 'not in catalog' }, ...list]
  }, [data, value])

  const useTextFallback =
    customMode || isLoading || Boolean(error) || options.length === 0

  if (useTextFallback) {
    return (
      <div className={styles.row}>
        <input
          type="text"
          className={styles.input}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={placeholder}
        />
        {data && data.entries.length > 0 && customMode && (
          <button
            type="button"
            className={styles.toggle}
            onClick={() => setCustomMode(false)}
            title="Switch back to the catalog dropdown"
          >
            Pick from catalog
          </button>
        )}
      </div>
    )
  }

  return (
    <div className={styles.row}>
      <select
        className={styles.select}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      >
        {!value && <option value="">(select a model)</option>}
        {options.map((e) => (
          <option key={e.id} value={e.id}>{formatOption(e)}</option>
        ))}
      </select>
      <button
        type="button"
        className={styles.toggle}
        onClick={() => setCustomMode(true)}
        title="Switch to free-text entry — useful for models the catalog hasn't listed yet"
      >
        Type custom id
      </button>
    </div>
  )
}

interface OptionLike {
  id: string
  display_name?: string
  context_window?: number
  input_price_per_1m?: number
  output_price_per_1m?: number
  notes?: string
}

function formatOption(e: OptionLike): string {
  const parts: string[] = [e.id]
  if (e.display_name) parts.push(`— ${e.display_name}`)
  const meta: string[] = []
  if (e.context_window) meta.push(`${formatTokens(e.context_window)} ctx`)
  if (e.input_price_per_1m != null && e.output_price_per_1m != null) {
    meta.push(`$${e.input_price_per_1m.toFixed(2)}/$${e.output_price_per_1m.toFixed(2)} per 1M`)
  }
  if (e.notes) meta.push(e.notes)
  if (meta.length > 0) parts.push(`(${meta.join(' · ')})`)
  return parts.join(' ')
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`
  if (n >= 1_000)     return `${Math.round(n / 1_000)}K`
  return String(n)
}
