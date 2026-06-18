// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { Bot, Plus, Trash2, Power, PowerOff } from 'lucide-react'
import { agentDefsApi, type AgentDefinition, type AgentDefinitionInput } from '@/api/agentDefs'
import styles from './AgentDefinitionsPage.module.css'

const EMPTY: AgentDefinitionInput = {
  name: '', description: '', system_prompt: '',
  allowed_tools: [], model_alias: null, budget_usd: null, enabled: true,
}

export default function AgentDefinitionsPage() {
  const qc = useQueryClient()
  const listQ = useQuery({ queryKey: ['agent-defs'], queryFn: agentDefsApi.list })
  const [editing, setEditing] = useState<{ id: string | null; input: AgentDefinitionInput } | null>(null)

  const invalidate = () => qc.invalidateQueries({ queryKey: ['agent-defs'] })
  const saveMut = useMutation({
    mutationFn: (e: { id: string | null; input: AgentDefinitionInput }) =>
      e.id ? agentDefsApi.update(e.id, e.input) : agentDefsApi.create(e.input),
    onSuccess: () => { toast.success('Saved'); setEditing(null); invalidate() },
    onError: (e: any) => toast.error(e?.response?.data?.error ?? e?.message ?? 'Save failed'),
  })
  const delMut = useMutation({
    mutationFn: (id: string) => agentDefsApi.remove(id),
    onSuccess: () => { toast.success('Deleted'); invalidate() },
    onError: (e: any) => toast.error(e?.response?.data?.error ?? 'Delete failed'),
  })
  const toggleMut = useMutation({
    mutationFn: (d: AgentDefinition) => agentDefsApi.update(d.id, { ...defToInput(d), enabled: !d.enabled }),
    onSuccess: () => invalidate(),
    onError: (e: any) => toast.error(e?.response?.data?.error ?? 'Toggle failed'),
  })

  const defs = listQ.data ?? []

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1><Bot size={18} style={{ verticalAlign: 'text-bottom', marginRight: 8 }} />Named Agents</h1>
        <p>Saved agent profiles — a persona, tool set, model, and budget you configure once. Ask MIRA in chat to use one (“have the researcher dig into …”) and it runs as a background task; MIRA can also delegate to one on its own initiative. Each runs with its own tools, model, and budget.</p>
        <button className={styles.primaryBtn} onClick={() => setEditing({ id: null, input: { ...EMPTY } })}>
          <Plus size={14} /> New agent
        </button>
      </header>

      <div className={styles.body}>
        {listQ.isLoading && <p className={styles.muted}>Loading…</p>}
        {!listQ.isLoading && defs.length === 0 && (
          <p className={styles.muted}>No named agents yet. Create one — e.g. a “researcher” with web tools and a research model.</p>
        )}

        <div className={styles.list}>
          {defs.map(d => (
            <div key={d.id} className={styles.card} data-disabled={!d.enabled}>
              <div className={styles.cardHead}>
                <div>
                  <h3 className={styles.name}><code>@{d.name}</code>{!d.enabled && <span className={styles.disabledTag}>disabled</span>}</h3>
                  {d.description && <p className={styles.desc}>{d.description}</p>}
                </div>
                <div className={styles.cardActions}>
                  <button className={styles.iconBtn} title={d.enabled ? 'Disable' : 'Enable'}
                          disabled={toggleMut.isPending} onClick={() => toggleMut.mutate(d)}>
                    {d.enabled ? <PowerOff size={14} /> : <Power size={14} />}
                  </button>
                  <button className={styles.iconBtn} title="Edit" onClick={() => setEditing({ id: d.id, input: defToInput(d) })}>Edit</button>
                  <button className={styles.iconBtn} title="Delete"
                          onClick={() => { if (confirm(`Delete agent @${d.name}?`)) delMut.mutate(d.id) }}>
                    <Trash2 size={14} />
                  </button>
                </div>
              </div>
              <div className={styles.meta}>
                {d.model_alias && <span>model: <code>{d.model_alias}</code></span>}
                {d.budget_usd != null && <span>budget: <code>${d.budget_usd.toFixed(2)}</code></span>}
                <span>tools: <code>{d.allowed_tools.length ? d.allowed_tools.join(', ') : 'default set'}</code></span>
              </div>
            </div>
          ))}
        </div>
      </div>

      {editing && (
        <Editor
          initial={editing}
          busy={saveMut.isPending}
          onCancel={() => setEditing(null)}
          onSave={(input) => saveMut.mutate({ id: editing.id, input })}
        />
      )}
    </div>
  )
}

function defToInput(d: AgentDefinition): AgentDefinitionInput {
  return {
    name: d.name, description: d.description, system_prompt: d.system_prompt,
    allowed_tools: d.allowed_tools, model_alias: d.model_alias, budget_usd: d.budget_usd, enabled: d.enabled,
  }
}

function Editor({ initial, busy, onCancel, onSave }: {
  initial: { id: string | null; input: AgentDefinitionInput }
  busy: boolean
  onCancel: () => void
  onSave: (input: AgentDefinitionInput) => void
}) {
  const [v, setV] = useState<AgentDefinitionInput>(initial.input)
  const [toolsText, setToolsText] = useState(initial.input.allowed_tools.join(', '))

  const submit = () => {
    const allowed_tools = toolsText.split(',').map(s => s.trim()).filter(Boolean)
    onSave({ ...v, allowed_tools })
  }

  return (
    <div className={styles.modalBackdrop} onClick={onCancel}>
      <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
        <h3>{initial.id ? 'Edit agent' : 'New agent'}</h3>
        <label className={styles.field}>
          <span>Name <em>(lowercase, dashes — the @handle)</em></span>
          <input value={v.name} disabled={!!initial.id}
                 onChange={(e) => setV({ ...v, name: e.target.value })} placeholder="researcher" />
        </label>
        <label className={styles.field}>
          <span>Description</span>
          <input value={v.description} onChange={(e) => setV({ ...v, description: e.target.value })} placeholder="Digs into a topic and writes a sourced brief." />
        </label>
        <label className={styles.field}>
          <span>Persona / system prompt</span>
          <textarea rows={5} value={v.system_prompt} onChange={(e) => setV({ ...v, system_prompt: e.target.value })}
                    placeholder="You are a meticulous research assistant. Always cite sources…" />
        </label>
        <div className={styles.row}>
          <label className={styles.field}>
            <span>Model alias <em>(optional)</em></span>
            <input value={v.model_alias ?? ''} onChange={(e) => setV({ ...v, model_alias: e.target.value || null })} placeholder="research" />
          </label>
          <label className={styles.field}>
            <span>Budget USD <em>(optional)</em></span>
            <input type="number" step="0.5" value={v.budget_usd ?? ''}
                   onChange={(e) => setV({ ...v, budget_usd: e.target.value === '' ? null : Number(e.target.value) })} placeholder="3.0" />
          </label>
        </div>
        <label className={styles.field}>
          <span>Allowed tools <em>(comma-separated; empty = default set)</em></span>
          <input value={toolsText} onChange={(e) => setToolsText(e.target.value)} placeholder="web_search, web_fetch, image_generate" />
        </label>
        <label className={styles.checkRow}>
          <input type="checkbox" checked={v.enabled} onChange={(e) => setV({ ...v, enabled: e.target.checked })} />
          <span>Enabled</span>
        </label>
        <div className={styles.modalActions}>
          <button className={styles.ghostBtn} onClick={onCancel} disabled={busy}>Cancel</button>
          <button className={styles.primaryBtn} onClick={submit} disabled={busy || !v.name.trim()}>
            {busy ? 'Saving…' : 'Save'}
          </button>
        </div>
      </div>
    </div>
  )
}
