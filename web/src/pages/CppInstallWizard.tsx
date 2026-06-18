// SPDX-License-Identifier: AGPL-3.0-or-later

import { useMemo, useState } from 'react'
import { useMutation } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { CheckCircle2, Circle, Loader2, AlertTriangle, SkipForward, Hourglass, Terminal, User, Bot } from 'lucide-react'
import {
  packagesApi,
  type ComponentSummary,
  type ConfigField,
  type PackageSummary,
  type TrustLevel,
  type UpdatePlan,
  type WizardState,
  type WizardStep,
} from '@/api/packages'
import styles from './PluginsPage.module.css'

/** Minimal `config.KEY == literal` / `!=` / bare-truthy evaluator — mirrors the
 *  backend so `visible_when` hides irrelevant fields as the admin types. */
function evalCond(expr: string | undefined, values: Record<string, any>): boolean {
  if (!expr) return true
  const e = expr.trim()
  for (const op of ['==', '!=']) {
    const idx = e.indexOf(op)
    if (idx > -1) {
      const lhs = e.slice(0, idx).trim().replace(/^config\./, '')
      const rhsRaw = e.slice(idx + op.length).trim()
      let rhs: any = rhsRaw
      if (rhsRaw === 'true') rhs = true
      else if (rhsRaw === 'false') rhs = false
      else if (/^-?\d+$/.test(rhsRaw)) rhs = Number(rhsRaw)
      else rhs = rhsRaw.replace(/^["']|["']$/g, '')
      const lv = values[lhs]
      const eq = lv === rhs
      return op === '==' ? eq : !eq
    }
  }
  const key = e.replace(/^config\./, '')
  return !!values[key] && values[key] !== 'false'
}

function FieldInput({ field, value, onChange }: { field: ConfigField; value: any; onChange: (v: any) => void }) {
  const common = { className: styles.fieldInput, id: `cf-${field.key}` }
  switch (field.type) {
    case 'bool':
      return <input type="checkbox" checked={!!value} onChange={(e) => onChange(e.target.checked)} />
    case 'enum':
      return (
        <select {...common} value={value ?? ''} onChange={(e) => onChange(e.target.value)}>
          <option value="" disabled>Choose…</option>
          {(field.enum ?? []).map((o) => <option key={o} value={o}>{o}</option>)}
        </select>
      )
    case 'int':
      return <input {...common} type="number" value={value ?? ''} onChange={(e) => onChange(e.target.value === '' ? '' : Number(e.target.value))} />
    case 'secret':
      return <input {...common} type="password" autoComplete="new-password" value={value ?? ''} onChange={(e) => onChange(e.target.value)} />
    case 'multiline':
      return <textarea {...common} rows={3} value={value ?? ''} onChange={(e) => onChange(e.target.value)} spellCheck={false} />
    default:
      return <input {...common} type="text" value={value ?? ''} onChange={(e) => onChange(e.target.value)} />
  }
}

/** The reviewable diff shown before applying an update. */
function UpdateDiff({ plan }: { plan: UpdatePlan }) {
  const cap = plan.capability
  const widen: string[] = [
    ...cap.added_egress.map((h) => `network → ${h}`),
    ...cap.added_secrets.map((s) => `secret: ${s}`),
    ...cap.added_filesystem.map((p) => `fs: ${p}`),
    ...cap.added_subprocess.map((c) => `subprocess: ${c}`),
    ...(cap.gained_subprocess ? ['can now run subprocesses'] : []),
    ...(cap.gained_listen_port ? [`listens on :${cap.gained_listen_port}`] : []),
  ]
  const cfg = plan.config
  const renamed = Object.entries(cfg.renamed)
  const nothing =
    widen.length === 0 && !plan.trust_changed &&
    cfg.new_required_inputs.length === 0 && cfg.new_optional.length === 0 &&
    cfg.removed.length === 0 && renamed.length === 0 && cfg.rotated.length === 0

  return (
    <div className={styles.diff}>
      {plan.trust_changed && (
        <p className={styles.diffWarn}>⚠ Signed by a different key than the installed version.</p>
      )}
      {widen.length > 0 && (
        <div className={styles.diffGroup}>
          <span className={styles.diffHead}>New permissions requested</span>
          <ul className={styles.diffList}>{widen.map((w) => <li key={w}>{w}</li>)}</ul>
        </div>
      )}
      {(cfg.new_required_inputs.length > 0 || cfg.new_optional.length > 0 || cfg.removed.length > 0 || renamed.length > 0 || cfg.rotated.length > 0) && (
        <div className={styles.diffGroup}>
          <span className={styles.diffHead}>Configuration changes</span>
          <ul className={styles.diffList}>
            {cfg.new_required_inputs.map((k) => <li key={k}>+ {k} (required)</li>)}
            {cfg.new_optional.map((k) => <li key={k}>+ {k}</li>)}
            {renamed.map(([from, to]) => <li key={from}>{from} → {to}</li>)}
            {cfg.removed.map((k) => <li key={k}>− {k}</li>)}
            {cfg.rotated.map((k) => <li key={k}>↻ {k} (secret re-minted)</li>)}
          </ul>
        </div>
      )}
      {nothing && <p className={styles.muted}>No permission or config changes — a clean version bump.</p>}
    </div>
  )
}

function StepIcon({ status }: { status: WizardStep['status'] }) {
  switch (status) {
    case 'done': return <CheckCircle2 size={16} className={styles.stepDone} />
    case 'failed': return <AlertTriangle size={16} className={styles.stepFailed} />
    case 'skipped': return <SkipForward size={16} className={styles.stepSkipped} />
    case 'awaiting_input': return <Hourglass size={16} className={styles.stepAwaiting} />
    default: return <Circle size={16} className={styles.stepPending} />
  }
}

function ActorIcon({ actor }: { actor: WizardStep['actor'] }) {
  if (actor === 'mira') return <span title="MIRA does this" style={{ display: 'inline-flex' }}><Bot size={13} /></span>
  if (actor === 'admin_external') return <span title="You, in another system" style={{ display: 'inline-flex' }}><Terminal size={13} /></span>
  return <span title="You" style={{ display: 'inline-flex' }}><User size={13} /></span>
}

export default function CppInstallWizard({
  file, manifest, component, trust, update, onDone, onClose,
}: {
  file: File
  manifest: PackageSummary
  component: ComponentSummary
  trust: TrustLevel
  /** When set, this is an UPDATE of an installed package, not a fresh install. */
  update?: UpdatePlan
  onDone: () => void
  onClose: () => void
}) {
  const isUpdate = !!update
  const inputFields = useMemo(() => {
    const all = (component.config_schema ?? []).filter((f) => f.source === 'input')
    // On an update, only collect the genuinely-new fields — everything else is
    // seeded from the prior install server-side.
    if (!update) return all
    const wanted = new Set([...update.config.new_required_inputs, ...update.config.new_optional])
    return all.filter((f) => wanted.has(f.key))
  }, [component.config_schema, update])

  const [values, setValues] = useState<Record<string, any>>(() => {
    const init: Record<string, any> = {}
    for (const f of inputFields) init[f.key] = f.default ?? (f.type === 'bool' ? false : '')
    return init
  })
  const [ack, setAck] = useState(false)
  const [capAck, setCapAck] = useState(false)
  const [trustAck, setTrustAck] = useState(false)
  const [state, setState] = useState<WizardState | null>(null)
  // Per-awaiting-step output drafts: { stepId: { outKey: value } }.
  const [outputs, setOutputs] = useState<Record<string, Record<string, string>>>({})

  const visibleFields = inputFields.filter((f) => evalCond(f.visible_when, values))
  const needsAck = trust.level !== 'verified'
  // Update gates: the admin must re-approve a changed signer / widened capability.
  const gatesUnmet =
    (needsAck && !ack) ||
    (!!update?.needs_trust_reapproval && !trustAck) ||
    (!!update?.needs_capability_reapproval && !capAck)

  const beginMut = useMutation({
    mutationFn: () => {
      // Drop hidden fields; required check on visible ones.
      const answers: Record<string, any> = {}
      for (const f of visibleFields) {
        if (f.required && (values[f.key] === '' || values[f.key] == null)) {
          throw new Error(`"${f.label || f.key}" is required`)
        }
        answers[f.key] = values[f.key]
      }
      return isUpdate
        ? packagesApi.cppUpdate(file, answers, { capabilityAck: capAck, trustAck, allowUntrusted: ack })
        : packagesApi.cppInstall(file, answers, ack)
    },
    onSuccess: (s) => handleState(s),
    onError: (e: any) => toast.error(`Couldn't start: ${e?.response?.data?.error ?? e?.message ?? e}`),
  })

  const stepMut = useMutation({
    mutationFn: ({ stepId }: { stepId: string }) =>
      packagesApi.cppStep(manifest.id, stepId, outputs[stepId] ?? {}),
    onSuccess: (s) => handleState(s),
    onError: (e: any) => toast.error(`Step failed: ${e?.response?.data?.error ?? e?.message ?? e}`),
  })

  const cancelMut = useMutation({
    mutationFn: () => packagesApi.cppCancel(manifest.id),
    onSuccess: () => { toast('Install cancelled — nothing left behind.'); onClose() },
    onError: (e: any) => toast.error(`Cancel failed: ${e?.response?.data?.error ?? e?.message ?? e}`),
  })

  function handleState(s: WizardState) {
    setState(s)
    if (s.status === 'complete') {
      ;(s.warnings ?? []).forEach((w) => toast(w, { icon: '⚠️' }))
      toast.success(isUpdate ? `${s.name} updated to v${s.version}.` : `${s.name} installed — the channel is live.`)
      onDone()
    } else if (s.status === 'failed') {
      const failed = s.steps.find((st) => st.status === 'failed')
      toast.error(`Install failed: ${failed?.message ?? 'a step did not pass'}`)
    }
  }

  const awaiting = state?.steps.find((st) => st.status === 'awaiting_input')
  const busy = beginMut.isPending || stepMut.isPending

  // ── the install form (before begin) ────────────────────────────────
  if (!state) {
    return (
      <div className={styles.card}>
        <div className={styles.cardHead}>
          <div>
            <h3 className={styles.title}>
              {isUpdate ? 'Update' : 'Set up'} {manifest.name}{' '}
              {isUpdate
                ? <span className={styles.ver}>v{update!.from_version} → v{update!.to_version}</span>
                : <span className={styles.ver}>v{manifest.version}</span>}
            </h3>
            <p className={styles.desc}>
              {isUpdate
                ? 'MIRA keeps your existing account + secrets and applies only what changed.'
                : "A guided install — MIRA mints the secrets and creates the channel; you'll run a couple of steps on your provider."}
            </p>
          </div>
        </div>

        {isUpdate && <UpdateDiff plan={update!} />}

        <div className={styles.formGrid}>
          {visibleFields.length === 0 && (
            <p className={styles.muted}>{isUpdate ? 'No new settings needed.' : 'No settings needed — click Start.'}</p>
          )}
          {visibleFields.map((f) => (
            <div key={f.key} className={styles.field}>
              <label htmlFor={`cf-${f.key}`} className={styles.fieldLabel}>
                {f.label || f.key}{f.required && <span className={styles.req}> *</span>}
              </label>
              <FieldInput field={f} value={values[f.key]} onChange={(v) => setValues({ ...values, [f.key]: v })} />
              {f.help && <p className={styles.fieldHelp}>{f.help}</p>}
            </div>
          ))}
        </div>

        {update?.needs_trust_reapproval && (
          <label className={styles.ackRow}>
            <input type="checkbox" checked={trustAck} onChange={(e) => setTrustAck(e.target.checked)} />
            <span>This update is signed by a <strong>different key</strong> — re-approve the publisher.</span>
          </label>
        )}
        {update?.needs_capability_reapproval && (
          <label className={styles.ackRow}>
            <input type="checkbox" checked={capAck} onChange={(e) => setCapAck(e.target.checked)} />
            <span>This update <strong>widens what the plugin can do</strong> — approve the new capabilities.</span>
          </label>
        )}
        {needsAck && (
          <label className={styles.ackRow}>
            <input type="checkbox" checked={ack} onChange={(e) => setAck(e.target.checked)} />
            <span>This package is <strong>{trust.level}</strong> — {isUpdate ? 'update' : 'install'} anyway (I trust its publisher).</span>
          </label>
        )}

        <div className={styles.wizActions}>
          <button className={styles.ghostBtn} onClick={onClose} disabled={busy}>Cancel</button>
          <button className={styles.installBtn} disabled={busy || gatesUnmet} onClick={() => beginMut.mutate()}>
            {beginMut.isPending
              ? <><Loader2 size={14} className={styles.spin} /> Starting…</>
              : isUpdate ? 'Apply update' : 'Start setup'}
          </button>
        </div>
      </div>
    )
  }

  // ── the stepper ────────────────────────────────────────────────────
  return (
    <div className={styles.card}>
      <div className={styles.cardHead}>
        <div>
          <h3 className={styles.title}>Setting up {state.name} <span className={styles.ver}>v{state.version}</span></h3>
          <p className={styles.id}>{state.status === 'awaiting_input' ? 'Waiting on you' : state.status === 'failed' ? 'Failed' : 'Working…'}</p>
        </div>
      </div>

      <ol className={styles.stepper}>
        {state.steps.map((st) => (
          <li key={st.id} className={`${styles.step} ${st.status === 'awaiting_input' ? styles.stepActive : ''}`}>
            <span className={styles.stepIcon}><StepIcon status={st.status} /></span>
            <div className={styles.stepBody}>
              <div className={styles.stepTitle}>
                <ActorIcon actor={st.actor} /> {st.title}
                {st.status === 'skipped' && <span className={styles.stepTag}>skipped</span>}
              </div>

              {/* The active human step: instructions + any values to paste back. */}
              {st.status === 'awaiting_input' && (
                <div className={styles.stepAction}>
                  {st.render && <pre className={styles.cmd}>{st.render}</pre>}
                  {(st.awaiting_outputs ?? []).map((key) => (
                    <div key={key} className={styles.field}>
                      <label className={styles.fieldLabel}>{key}</label>
                      <input
                        className={styles.fieldInput}
                        type="text"
                        value={outputs[st.id]?.[key] ?? ''}
                        onChange={(e) => setOutputs({ ...outputs, [st.id]: { ...(outputs[st.id] ?? {}), [key]: e.target.value } })}
                      />
                    </div>
                  ))}
                  <button className={styles.installBtn} disabled={busy} onClick={() => stepMut.mutate({ stepId: st.id })}>
                    {stepMut.isPending ? <><Loader2 size={14} className={styles.spin} /> Continuing…</> : (st.awaiting_outputs?.length ? 'Submit & continue' : 'I\'ve done this — continue')}
                  </button>
                </div>
              )}

              {st.status !== 'awaiting_input' && st.message && (
                <p className={st.status === 'failed' ? styles.stepError : styles.fieldHelp}>{st.message}</p>
              )}
            </div>
          </li>
        ))}
      </ol>

      <div className={styles.wizActions}>
        {state.status === 'failed'
          ? <button className={styles.ghostBtn} onClick={onClose}>Close</button>
          : <button className={styles.ghostBtn} onClick={() => { if (confirm('Cancel this install and undo what was set up?')) cancelMut.mutate() }} disabled={cancelMut.isPending}>
              {cancelMut.isPending ? 'Cancelling…' : 'Cancel install'}
            </button>}
        {!awaiting && state.status !== 'failed' && state.status !== 'complete' && (
          <span className={styles.muted}><Loader2 size={14} className={styles.spin} /> Working…</span>
        )}
      </div>
    </div>
  )
}
