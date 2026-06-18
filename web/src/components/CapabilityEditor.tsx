// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { X } from 'lucide-react'
import { capabilitiesApi, type CapabilityProfile } from '@/api/capabilities'
import styles from './CapabilityEditor.module.css'

interface Props {
  scope: 'group' | 'user'
  id:    string
  name:  string
  onClose: () => void
}

const AXES: { key: 'providers' | 'models' | 'tools' | 'channels'; label: string; hint: string }[] = [
  { key: 'providers', label: 'Providers', hint: 'e.g. openai, anthropic, lmstudio' },
  { key: 'models',    label: 'Models',    hint: 'e.g. gpt-4o-mini, claude-haiku-4-5' },
  { key: 'tools',     label: 'Tools',     hint: 'e.g. web_search, calendar, code_run' },
  { key: 'channels',  label: 'Channels',  hint: 'e.g. signal, telegram, email, discord' },
]

/**
 * Admin editor for one capability profile (a group's or a user's). Each
 * allow-list axis has a "Restrict" toggle: off = unrestricted (the field is
 * omitted/null), on = a comma/newline-separated list. Saving an all-empty
 * profile clears it server-side. Capabilities are **additive grants** across a
 * user's groups + their direct profile; budget caps take the tightest value.
 */
export default function CapabilityEditor({ scope, id, name, onClose }: Props) {
  const qc = useQueryClient()
  const queryKey = ['capabilities', scope, id]

  const { data, isLoading } = useQuery<CapabilityProfile>({
    queryKey,
    queryFn: () => (scope === 'group' ? capabilitiesApi.getGroup(id) : capabilitiesApi.getUser(id)),
  })

  return (
    <div className={styles.overlay} onClick={onClose}>
      <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
        <div className={styles.head}>
          <h3>Capabilities — <span className={styles.scope}>{scope}</span> “{name}”</h3>
          <button className={styles.iconBtn} onClick={onClose}><X size={16} /></button>
        </div>
        {isLoading
          ? <p className={styles.loading}>Loading…</p>
          : <EditorForm scope={scope} id={id} initial={data ?? {}} onSaved={() => {
              qc.invalidateQueries({ queryKey })
              onClose()
            }} />}
      </div>
    </div>
  )
}

function listToText(v?: string[] | null): string {
  return (v ?? []).join(', ')
}
function textToList(t: string): string[] {
  return t.split(/[,\n]/).map(s => s.trim()).filter(Boolean)
}

function EditorForm({
  scope, id, initial, onSaved,
}: { scope: 'group' | 'user'; id: string; initial: CapabilityProfile; onSaved: () => void }) {
  // Per-axis "restricted?" toggle + text; a restricted axis with empty text is
  // a real "deny all" restriction (matches the backend).
  const [restrict, setRestrict] = useState<Record<string, boolean>>({
    providers: initial.providers != null,
    models:    initial.models != null,
    tools:     initial.tools != null,
    channels:  initial.channels != null,
  })
  const [text, setText] = useState<Record<string, string>>({
    providers: listToText(initial.providers),
    models:    listToText(initial.models),
    tools:     listToText(initial.tools),
    channels:  listToText(initial.channels),
  })
  const [taskBudget, setTaskBudget]       = useState(initial.max_task_budget_usd?.toString() ?? '')
  const [sessionBudget, setSessionBudget] = useState(initial.session_budget_usd?.toString() ?? '')
  const [error, setError] = useState('')

  const saveMut = useMutation({
    mutationFn: (p: CapabilityProfile) =>
      scope === 'group' ? capabilitiesApi.setGroup(id, p) : capabilitiesApi.setUser(id, p),
    onSuccess: onSaved,
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: string } })?.response?.data ?? 'Save failed'
      setError(typeof msg === 'string' ? msg : 'Save failed')
    },
  })

  function build(): CapabilityProfile {
    const p: CapabilityProfile = {}
    for (const { key } of AXES) {
      if (restrict[key]) p[key] = textToList(text[key])
    }
    const tb = parseFloat(taskBudget)
    if (!Number.isNaN(tb)) p.max_task_budget_usd = tb
    const sb = parseFloat(sessionBudget)
    if (!Number.isNaN(sb)) p.session_budget_usd = sb
    return p
  }

  const unrestricted =
    !Object.values(restrict).some(Boolean) && !taskBudget.trim() && !sessionBudget.trim()

  return (
    <div className={styles.body}>
      <p className={styles.note}>
        Off = unrestricted. Grants are <strong>additive</strong> across a user’s groups; budget
        caps take the <strong>tightest</strong> value. Admins bypass all restrictions.
      </p>

      {AXES.map(({ key, label, hint }) => (
        <div key={key} className={styles.axis}>
          <label className={styles.axisHead}>
            <input
              type="checkbox"
              checked={restrict[key]}
              onChange={(e) => setRestrict((r) => ({ ...r, [key]: e.target.checked }))}
            />
            <span>Restrict {label}</span>
          </label>
          {restrict[key] && (
            <textarea
              className={styles.input}
              rows={2}
              value={text[key]}
              placeholder={hint}
              onChange={(e) => setText((t) => ({ ...t, [key]: e.target.value }))}
            />
          )}
        </div>
      ))}

      <div className={styles.budgets}>
        <label>
          Max task budget (USD)
          <input
            type="number" step="0.01" min="0"
            className={styles.numInput}
            value={taskBudget}
            placeholder="no cap"
            onChange={(e) => setTaskBudget(e.target.value)}
          />
        </label>
        <label>
          Session budget (USD)
          <input
            type="number" step="0.01" min="0"
            className={styles.numInput}
            value={sessionBudget}
            placeholder="no cap"
            onChange={(e) => setSessionBudget(e.target.value)}
          />
        </label>
      </div>

      {error && <p className={styles.error}>{error}</p>}

      <div className={styles.actions}>
        <span className={styles.status}>
          {unrestricted ? 'No restrictions (profile will be cleared)' : 'Restrictions active'}
        </span>
        <button
          className={styles.btn}
          disabled={saveMut.isPending}
          onClick={() => { setError(''); saveMut.mutate(build()) }}
        >
          {saveMut.isPending ? 'Saving…' : 'Save'}
        </button>
      </div>
    </div>
  )
}
