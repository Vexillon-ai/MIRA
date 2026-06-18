// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { Workflow, Plus, Trash2, Play, ChevronRight } from 'lucide-react'
import {
  workflowsApi, type WorkflowDefinition, type WorkflowInput, type WorkflowStep,
  type WorkflowRun, type RunStatus, type ConditionOp,
} from '@/api/workflows'
import { agentDefsApi } from '@/api/agentDefs'
import styles from './WorkflowsPage.module.css'

const OPS: ConditionOp[] = ['contains', 'not_contains', 'equals', 'not_empty', 'empty']

function blankStep(n: number): WorkflowStep {
  return { id: `step-${n}`, agent: '', skill: null, brief: '', depends_on: [], budget_usd: null, continue_on_error: false, when: null, requires_approval: false }
}

export default function WorkflowsPage() {
  const qc = useQueryClient()
  const [tab, setTab] = useState<'workflows' | 'runs'>('workflows')
  const [editing, setEditing] = useState<{ id: string | null; input: WorkflowInput } | null>(null)
  const [running, setRunning] = useState<WorkflowDefinition | null>(null)

  const listQ = useQuery({ queryKey: ['workflows'], queryFn: workflowsApi.list })
  const agentsQ = useQuery({ queryKey: ['agent-defs'], queryFn: agentDefsApi.list })

  const invalidate = () => qc.invalidateQueries({ queryKey: ['workflows'] })
  const saveMut = useMutation({
    mutationFn: (e: { id: string | null; input: WorkflowInput }) =>
      e.id ? workflowsApi.update(e.id, e.input) : workflowsApi.create(e.input),
    onSuccess: () => { toast.success('Saved'); setEditing(null); invalidate() },
    onError: (e: any) => toast.error(e?.response?.data?.error ?? 'Save failed'),
  })
  const delMut = useMutation({
    mutationFn: (id: string) => workflowsApi.remove(id),
    onSuccess: () => { toast.success('Deleted'); invalidate() },
    onError: (e: any) => toast.error(e?.response?.data?.error ?? 'Delete failed'),
  })

  const defs = listQ.data ?? []
  const agentHandles = (agentsQ.data ?? []).filter(a => a.enabled).map(a => a.name)

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1><Workflow size={18} style={{ verticalAlign: 'text-bottom', marginRight: 8 }} />Workflows</h1>
        <p>Chain named agents and skills into a DAG. Each step targets an agent or skill, can interpolate the run input (<code>{'{{input}}'}</code>) and any upstream step's output (<code>{'{{steps.<id>.output}}'}</code>), and declares its dependencies. Independent steps run in parallel; outputs feed forward.</p>
        <div className={styles.tabs}>
          {/* eslint-disable no-restricted-syntax -- tab buttons are styled by the `.tabs > button` descendant rule in WorkflowsPage.module.css */}
          <button data-active={tab === 'workflows'} onClick={() => setTab('workflows')}>Workflows</button>
          <button data-active={tab === 'runs'} onClick={() => setTab('runs')}>Run history</button>
          {/* eslint-enable no-restricted-syntax */}
          <span className={styles.spacer} />
          {tab === 'workflows' && (
            <button className={styles.primaryBtn}
                    onClick={() => setEditing({ id: null, input: { name: '', description: '', steps: [blankStep(1)], enabled: true } })}>
              <Plus size={14} /> New workflow
            </button>
          )}
        </div>
      </header>

      {tab === 'workflows' ? (
        <div className={styles.body}>
          {listQ.isLoading && <p className={styles.muted}>Loading…</p>}
          {!listQ.isLoading && defs.length === 0 && (
            <p className={styles.muted}>No workflows yet. Create one — e.g. a <code>research</code> step feeding a <code>write-up</code> step.</p>
          )}
          <div className={styles.list}>
            {defs.map(d => (
              <div key={d.id} className={styles.card} data-disabled={!d.enabled}>
                <div className={styles.cardHead}>
                  <div>
                    <h3 className={styles.name}><code>{d.name}</code>{!d.enabled && <span className={styles.tag}>disabled</span>}</h3>
                    {d.description && <p className={styles.desc}>{d.description}</p>}
                  </div>
                  <div className={styles.cardActions}>
                    <button className={styles.iconBtn} disabled={!d.enabled} title="Run" onClick={() => setRunning(d)}><Play size={14} /> Run</button>
                    <button className={styles.iconBtn} title="Edit" onClick={() => setEditing({ id: d.id, input: toInput(d) })}>Edit</button>
                    <button className={styles.iconBtn} title="Delete" onClick={() => { if (confirm(`Delete workflow ${d.name}?`)) delMut.mutate(d.id) }}><Trash2 size={14} /></button>
                  </div>
                </div>
                <div className={styles.flow}>
                  {d.steps.map(s => (
                    <span key={s.id} className={styles.chip} title={s.brief}>
                      {s.id}<em>{s.agent ? `@${s.agent}` : s.skill}</em>
                      {s.depends_on.length > 0 && <span className={styles.dep}>← {s.depends_on.join(', ')}</span>}
                    </span>
                  ))}
                </div>
              </div>
            ))}
          </div>
        </div>
      ) : (
        <RunsTab />
      )}

      {editing && (
        <Editor
          initial={editing}
          agentHandles={agentHandles}
          busy={saveMut.isPending}
          onCancel={() => setEditing(null)}
          onSave={(input) => saveMut.mutate({ id: editing.id, input })}
        />
      )}
      {running && (
        <RunModal
          workflow={running}
          onClose={() => setRunning(null)}
          onStarted={() => { setRunning(null); setTab('runs') }}
        />
      )}
    </div>
  )
}

function toInput(d: WorkflowDefinition): WorkflowInput {
  return { name: d.name, description: d.description, steps: d.steps, enabled: d.enabled }
}

// ── Run history ───────────────────────────────────────────────────────────────

function RunsTab() {
  const qc = useQueryClient()
  const [expanded, setExpanded] = useState<string | null>(null)
  const runsQ = useQuery({
    queryKey: ['workflow-runs'],
    queryFn: () => workflowsApi.listRuns(50),
    refetchInterval: (q) => {
      const data = q.state.data as WorkflowRun[] | undefined
      return data?.some(r => r.status === 'running' || r.status === 'pending') ? 2000 : false
    },
  })
  const approveMut = useMutation({
    mutationFn: (a: { runId: string; stepId: string; decision: 'approve' | 'reject' }) =>
      workflowsApi.approve(a.runId, a.stepId, a.decision),
    onSuccess: (_d, a) => { toast.success(a.decision === 'approve' ? 'Approved — resuming' : 'Rejected'); qc.invalidateQueries({ queryKey: ['workflow-runs'] }) },
    onError: (e: any) => toast.error(e?.response?.data?.error ?? 'Action failed'),
  })
  const runs = runsQ.data ?? []

  return (
    <div className={styles.body}>
      {runsQ.isLoading && <p className={styles.muted}>Loading…</p>}
      {!runsQ.isLoading && runs.length === 0 && <p className={styles.muted}>No runs yet. Run a workflow to see it here.</p>}
      <div className={styles.list}>
        {runs.map(r => {
          const done = r.steps.filter(s => s.status === 'completed').length
          return (
            <div key={r.id} className={styles.runCard}>
              <button className={styles.runHead} onClick={() => setExpanded(expanded === r.id ? null : r.id)}>
                <ChevronRight size={14} className={styles.chev} data-open={expanded === r.id} />
                <StatusChip status={r.status} />
                <code className={styles.runName}>{r.workflow_name}</code>
                <span className={styles.runMeta}>{done}/{r.steps.length} steps · {fmtTime(r.created_at)}</span>
                {r.input && <span className={styles.runInput} title={r.input}>“{r.input}”</span>}
              </button>
              {expanded === r.id && (
                <div className={styles.runSteps}>
                  {r.error && <div className={styles.runError}>{r.error}</div>}
                  {r.steps.map(s => (
                    <div key={s.step_id} className={styles.stepRow}>
                      <StatusChip status={s.status} small />
                      <code className={styles.stepName}>{s.step_id}</code>
                      <span className={styles.stepTarget}>{s.target}</span>
                      {s.status === 'paused' && (
                        <span className={styles.approveRow}>
                          <button className={styles.approveBtn} disabled={approveMut.isPending}
                                  onClick={() => approveMut.mutate({ runId: r.id, stepId: s.step_id, decision: 'approve' })}>Approve</button>
                          <button className={styles.rejectBtn} disabled={approveMut.isPending}
                                  onClick={() => approveMut.mutate({ runId: r.id, stepId: s.step_id, decision: 'reject' })}>Reject</button>
                        </span>
                      )}
                      {(s.output || s.error) && (
                        <pre className={styles.stepOut}>{s.output ?? s.error}</pre>
                      )}
                    </div>
                  ))}
                </div>
              )}
            </div>
          )
        })}
      </div>
    </div>
  )
}

function StatusChip({ status, small }: { status: RunStatus; small?: boolean }) {
  return <span className={styles.statusChip} data-status={status} data-small={small ? 'true' : undefined}>{status}</span>
}

function fmtTime(secs: number) {
  return new Date(secs * 1000).toLocaleString()
}

// ── Run modal ─────────────────────────────────────────────────────────────────

function RunModal({ workflow, onClose, onStarted }: {
  workflow: WorkflowDefinition; onClose: () => void; onStarted: () => void
}) {
  const [input, setInput] = useState('')
  const mut = useMutation({
    mutationFn: () => workflowsApi.run(workflow.id, input),
    onSuccess: (r) => { toast.success(`Started — run ${r.run_id.slice(0, 8)}`); onStarted() },
    onError: (e: any) => toast.error(e?.response?.data?.error ?? 'Run failed to start'),
  })
  return (
    <div className={styles.modalBackdrop} onClick={onClose}>
      <div className={styles.modal} onClick={e => e.stopPropagation()}>
        <h3>Run <code>{workflow.name}</code></h3>
        <label className={styles.field}>
          <span>Input <em>(interpolated into steps that use <code>{'{{input}}'}</code>)</em></span>
          <textarea rows={4} value={input} onChange={e => setInput(e.target.value)} placeholder="e.g. the topic, the question, the target…" />
        </label>
        <div className={styles.modalActions}>
          <button className={styles.ghostBtn} onClick={onClose} disabled={mut.isPending}>Cancel</button>
          <button className={styles.primaryBtn} onClick={() => mut.mutate()} disabled={mut.isPending}>
            <Play size={14} /> {mut.isPending ? 'Starting…' : 'Run'}
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Editor ────────────────────────────────────────────────────────────────────

function Editor({ initial, agentHandles, busy, onCancel, onSave }: {
  initial: { id: string | null; input: WorkflowInput }
  agentHandles: string[]
  busy: boolean
  onCancel: () => void
  onSave: (input: WorkflowInput) => void
}) {
  const [v, setV] = useState<WorkflowInput>(initial.input)
  const set = (patch: Partial<WorkflowInput>) => setV({ ...v, ...patch })
  const setStep = (i: number, patch: Partial<WorkflowStep>) =>
    set({ steps: v.steps.map((s, j) => j === i ? { ...s, ...patch } : s) })

  const idsBefore = (i: number) => v.steps.slice(0, i).map(s => s.id).filter(Boolean)

  return (
    <div className={styles.modalBackdrop} onClick={onCancel}>
      <div className={styles.modalWide} onClick={e => e.stopPropagation()}>
        <h3>{initial.id ? 'Edit workflow' : 'New workflow'}</h3>
        <div className={styles.row}>
          <label className={styles.field}>
            <span>Name <em>(lowercase, dashes)</em></span>
            <input value={v.name} disabled={!!initial.id} onChange={e => set({ name: e.target.value })} placeholder="weekly-brief" />
          </label>
          <label className={styles.checkRow} style={{ alignSelf: 'flex-end', paddingBottom: 8 }}>
            <input type="checkbox" checked={v.enabled} onChange={e => set({ enabled: e.target.checked })} /><span>Enabled</span>
          </label>
        </div>
        <label className={styles.field}>
          <span>Description</span>
          <input value={v.description} onChange={e => set({ description: e.target.value })} placeholder="What this workflow does." />
        </label>

        <div className={styles.stepsHead}>
          <span>Steps</span>
          <button className={styles.smallBtn} onClick={() => set({ steps: [...v.steps, blankStep(v.steps.length + 1)] })}>
            <Plus size={12} /> Add step
          </button>
        </div>

        <div className={styles.steps}>
          {v.steps.map((s, i) => (
            <div key={i} className={styles.stepEditor}>
              <div className={styles.stepGrid}>
                <label className={styles.field}>
                  <span>Step id</span>
                  <input value={s.id} onChange={e => setStep(i, { id: e.target.value })} placeholder="research" />
                </label>
                <label className={styles.field}>
                  <span>Target</span>
                  <div className={styles.targetRow}>
                    <select value={s.agent != null && s.agent !== '' ? 'agent' : (s.skill ? 'skill' : 'agent')}
                            onChange={e => e.target.value === 'agent'
                              ? setStep(i, { agent: '', skill: null })
                              : setStep(i, { agent: null, skill: '' })}>
                      <option value="agent">Agent</option>
                      <option value="skill">Skill</option>
                    </select>
                    {s.skill != null ? (
                      <input value={s.skill} onChange={e => setStep(i, { skill: e.target.value })} placeholder="com.mira.research" />
                    ) : (
                      <>
                        <input list={`agents-${i}`} value={s.agent ?? ''} onChange={e => setStep(i, { agent: e.target.value })} placeholder="researcher" />
                        <datalist id={`agents-${i}`}>{agentHandles.map(h => <option key={h} value={h} />)}</datalist>
                      </>
                    )}
                  </div>
                </label>
                <label className={styles.field}>
                  <span>Budget USD <em>(optional)</em></span>
                  <input type="number" step="0.5" value={s.budget_usd ?? ''}
                         onChange={e => setStep(i, { budget_usd: e.target.value === '' ? null : Number(e.target.value) })} placeholder="default" />
                </label>
                <button className={styles.removeStep} title="Remove step"
                        onClick={() => set({ steps: v.steps.filter((_, j) => j !== i) })}><Trash2 size={13} /></button>
              </div>

              <label className={styles.field}>
                <span>Brief <em>(supports <code>{'{{input}}'}</code> and <code>{'{{steps.<id>.output}}'}</code>)</em></span>
                <textarea rows={2} value={s.brief} onChange={e => setStep(i, { brief: e.target.value })}
                          placeholder="Research {{input}} and list the key findings." />
              </label>

              {idsBefore(i).length > 0 && (
                <div className={styles.field}>
                  <span>Depends on</span>
                  <div className={styles.depPicker}>
                    {idsBefore(i).map(dep => (
                      <label key={dep} className={styles.depChip}>
                        <input type="checkbox" checked={s.depends_on.includes(dep)}
                               onChange={e => {
                                 const next = e.target.checked
                                   ? [...s.depends_on, dep]
                                   : s.depends_on.filter(d => d !== dep)
                                 // dropping a dep also clears a guard that referenced it
                                 const when = s.when && next.includes(s.when.step) ? s.when : null
                                 setStep(i, { depends_on: next, when })
                               }} />
                        {dep}
                      </label>
                    ))}
                  </div>
                </div>
              )}

              <div className={styles.stepOpts}>
                <label className={styles.checkRow}>
                  <input type="checkbox" checked={s.continue_on_error}
                         onChange={e => setStep(i, { continue_on_error: e.target.checked })} />
                  <span>Continue on error <em>(failure here doesn't fail the run)</em></span>
                </label>
                <label className={styles.checkRow}>
                  <input type="checkbox" checked={s.requires_approval}
                         onChange={e => setStep(i, { requires_approval: e.target.checked })} />
                  <span>Requires approval <em>(pause for a human before this step)</em></span>
                </label>
                <label className={styles.checkRow}>
                  <input type="checkbox" checked={!!s.when} disabled={s.depends_on.length === 0}
                         onChange={e => setStep(i, { when: e.target.checked
                           ? { step: s.depends_on[0] ?? '', op: 'contains', value: '' } : null })} />
                  <span>Guard <em>(run only if a dependency's output matches)</em></span>
                </label>
              </div>

              {s.when && (
                <div className={styles.guardRow}>
                  <span>only if</span>
                  <select value={s.when.step} onChange={e => setStep(i, { when: { ...s.when!, step: e.target.value } })}>
                    {s.depends_on.map(d => <option key={d} value={d}>{d}</option>)}
                  </select>
                  <select value={s.when.op} onChange={e => setStep(i, { when: { ...s.when!, op: e.target.value as ConditionOp } })}>
                    {OPS.map(op => <option key={op} value={op}>{op.replace('_', ' ')}</option>)}
                  </select>
                  {s.when.op !== 'not_empty' && s.when.op !== 'empty' && (
                    <input value={s.when.value} onChange={e => setStep(i, { when: { ...s.when!, value: e.target.value } })} placeholder="value" />
                  )}
                </div>
              )}
            </div>
          ))}
        </div>

        <div className={styles.modalActions}>
          <button className={styles.ghostBtn} onClick={onCancel} disabled={busy}>Cancel</button>
          <button className={styles.primaryBtn} disabled={busy || !v.name.trim() || v.steps.length === 0}
                  onClick={() => onSave(normalize(v))}>
            {busy ? 'Saving…' : 'Save'}
          </button>
        </div>
      </div>
    </div>
  )
}

/** Coerce empty target strings to null so the server's single-target check is
 *  clean (an empty `agent` string + null `skill` would otherwise read as a
 *  dual/no target). */
function normalize(v: WorkflowInput): WorkflowInput {
  return {
    ...v,
    steps: v.steps.map(s => ({
      ...s,
      agent: s.agent && s.agent.trim() ? s.agent.trim() : null,
      skill: s.skill && s.skill.trim() ? s.skill.trim() : null,
    })),
  }
}
