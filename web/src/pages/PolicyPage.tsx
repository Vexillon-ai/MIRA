// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import {
  ShieldCheck, Plus, Trash2, Pencil, X, Save, ToggleLeft, ToggleRight,
} from 'lucide-react'
import {
  policyApi, ALL_EVENT_KINDS, ALL_PREDICATE_TYPES,
  defaultPredicateValue, predicateValueIsNumeric,
  type AdminRule, type AdminRuleInput, type PolicyEventKind, type Predicate,
} from '@/api/policy'
import styles from './PolicyPage.module.css'

// admin UI for the D3 admin-defined rules. Backend
// (POST/GET/PUT/DELETE under /api/policy/rules) shipped in 0.76.0;
// this is the operator-facing surface so policy doesn't require curl.
export default function PolicyPage() {
  const qc = useQueryClient()
  const { data: rules, isLoading, error } = useQuery({
    queryKey: ['policy-rules'],
    queryFn:  policyApi.list,
  })

  const [editing, setEditing] = useState<AdminRule | 'new' | null>(null)

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1>
          <ShieldCheck size={18} style={{ verticalAlign: 'text-bottom', marginRight: 8 }} />
          Policy rules
        </h1>
        <p>
          Admin-defined Deny rules layered on top of the built-in policy
          engine. Rules are evaluated in id order — first-deny-wins.
          Use these to enforce site-specific limits (no calls to a
          particular host, no reads outside <code>/var/data</code>, no
          spawning beyond depth 3, …) without recompiling.
        </p>
      </header>

      <div className={styles.toolbar}>
        <button
          type="button"
          className={styles.primaryBtn}
          onClick={() => setEditing('new')}
          disabled={editing === 'new'}
        >
          <Plus size={13} /> New rule
        </button>
        <span className={styles.count}>
          {rules ? `${rules.length} rule${rules.length === 1 ? '' : 's'}` : ''}
        </span>
      </div>

      <div className={styles.body}>
        {editing === 'new' && (
          <RuleEditor
            initial={null}
            onCancel={() => setEditing(null)}
            onSaved={() => {
              setEditing(null)
              qc.invalidateQueries({ queryKey: ['policy-rules'] })
            }}
          />
        )}

        {isLoading && <div className={styles.empty}>Loading…</div>}
        {error && <div className={styles.empty}>Failed to load rules.</div>}
        {rules && rules.length === 0 && editing !== 'new' && (
          <div className={styles.empty}>
            <strong>No admin rules yet.</strong>
            The built-in engine is active by default (depth cap, session
            budget, network/filesystem/secrets allowlists from Skill
            manifests, per-agent budget). Add custom rules here to
            tighten or extend.
          </div>
        )}

        {rules && rules.length > 0 && (
          <div className={styles.list}>
            {rules.map((r) => (
              editing && editing !== 'new' && editing.id === r.id
                ? <RuleEditor
                    key={r.id}
                    initial={r}
                    onCancel={() => setEditing(null)}
                    onSaved={() => {
                      setEditing(null)
                      qc.invalidateQueries({ queryKey: ['policy-rules'] })
                    }}
                  />
                : <RuleRow
                    key={r.id}
                    rule={r}
                    onEdit={() => setEditing(r)}
                  />
            ))}
          </div>
        )}
      </div>
    </div>
  )
}

// ── Row (read-only view) ─────────────────────────────────────────────

function RuleRow({ rule, onEdit }: { rule: AdminRule; onEdit: () => void }) {
  const qc = useQueryClient()

  const toggleEnabled = useMutation({
    mutationFn: () => policyApi.update(rule.id, {
      ...rule, enabled: !rule.enabled,
    }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['policy-rules'] })
      toast.success(rule.enabled ? 'Rule disabled' : 'Rule enabled')
    },
    onError: (e: Error) => toast.error(`Toggle failed: ${e.message}`),
  })

  const remove = useMutation({
    mutationFn: () => policyApi.delete(rule.id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['policy-rules'] })
      toast.success('Rule deleted')
    },
    onError: (e: Error) => toast.error(`Delete failed: ${e.message}`),
  })

  return (
    <div className={styles.card} data-enabled={rule.enabled}>
      <div className={styles.cardHead}>
        <div className={styles.titleRow}>
          <span className={styles.title}>{rule.name}</span>
          <code className={styles.id}>{rule.id}</code>
          <span className={styles.kindBadge}>{rule.event_kind}</span>
          {!rule.enabled && <span className={styles.disabledBadge}>disabled</span>}
        </div>
        <div className={styles.actions}>
          <button
            type="button"
            className={styles.iconBtn}
            onClick={() => toggleEnabled.mutate()}
            disabled={toggleEnabled.isPending}
            title={rule.enabled ? 'Disable rule' : 'Enable rule'}
          >
            {rule.enabled ? <ToggleRight size={13} /> : <ToggleLeft size={13} />}
          </button>
          <button type="button" className={styles.iconBtn} onClick={onEdit}>
            <Pencil size={11} /> Edit
          </button>
          <button
            type="button"
            className={styles.iconBtn}
            data-variant="danger"
            disabled={remove.isPending}
            onClick={() => {
              if (confirm(`Delete rule "${rule.id}"?`)) remove.mutate()
            }}
          >
            <Trash2 size={11} /> Delete
          </button>
        </div>
      </div>

      <div className={styles.reason}>{rule.reason}</div>

      {rule.predicates.length > 0 && (
        <div className={styles.preds}>
          {rule.predicates.map((p, i) => (
            <span key={i} className={styles.predChip}>
              <code>{p.type}</code> = <strong>{String(p.value)}</strong>
            </span>
          ))}
        </div>
      )}
      {rule.predicates.length === 0 && (
        <div className={styles.preds}>
          <span className={styles.predChip} data-warn="true">
            (no predicates — matches every {rule.event_kind} event)
          </span>
        </div>
      )}
    </div>
  )
}

// ── Editor (create / edit) ───────────────────────────────────────────

function RuleEditor({
  initial, onCancel, onSaved,
}: {
  initial: AdminRule | null
  onCancel: () => void
  onSaved: () => void
}) {
  const isCreate = initial === null
  const [id, setId] = useState(initial?.id ?? '')
  const [name, setName] = useState(initial?.name ?? '')
  const [enabled, setEnabled] = useState(initial?.enabled ?? true)
  const [eventKind, setEventKind] =
    useState<PolicyEventKind>(initial?.event_kind ?? 'spawn_worker')
  const [reason, setReason] = useState(initial?.reason ?? '')
  const [predicates, setPredicates] = useState<Predicate[]>(initial?.predicates ?? [])

  const save = useMutation({
    mutationFn: () => {
      const body: AdminRuleInput = {
        id, name, enabled, event_kind: eventKind, predicates, reason,
      }
      return isCreate
        ? policyApi.create(body)
        : policyApi.update(initial!.id, body)
    },
    onSuccess: () => {
      toast.success(isCreate ? 'Rule created' : 'Rule updated')
      onSaved()
    },
    onError: (e: Error) => toast.error(`Save failed: ${e.message}`),
  })

  const canSave = id.trim() && name.trim() && reason.trim()

  return (
    <div className={`${styles.card} ${styles.editing}`}>
      <div className={styles.cardHead}>
        <div className={styles.titleRow}>
          <span className={styles.title}>
            {isCreate ? 'New rule' : `Editing ${initial!.id}`}
          </span>
        </div>
        <div className={styles.actions}>
          <button type="button" className={styles.iconBtn} onClick={onCancel}>
            <X size={11} /> Cancel
          </button>
          <button
            type="button"
            className={styles.primaryBtn}
            disabled={!canSave || save.isPending}
            onClick={() => save.mutate()}
          >
            <Save size={11} /> Save
          </button>
        </div>
      </div>

      <div className={styles.formGrid}>
        <label className={styles.formField}>
          <span>Rule id</span>
          <input
            type="text"
            value={id}
            onChange={(e) => setId(e.target.value)}
            disabled={!isCreate}
            placeholder="admin/no-evil-net"
            className={styles.input}
          />
          <small className={styles.hint}>Convention: <code>admin/&lt;short-kebab&gt;</code> — surfaces in audit-log denial rows.</small>
        </label>

        <label className={styles.formField}>
          <span>Display name</span>
          <input
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Block evil.com"
            className={styles.input}
          />
        </label>

        <label className={styles.formField}>
          <span>Event kind</span>
          <select
            value={eventKind}
            onChange={(e) => setEventKind(e.target.value as PolicyEventKind)}
            className={styles.input}
          >
            {ALL_EVENT_KINDS.map((k) => <option key={k} value={k}>{k}</option>)}
          </select>
        </label>

        <label className={styles.formField}>
          <span>Enabled</span>
          <label className={styles.toggleLabel}>
            <input
              type="checkbox"
              checked={enabled}
              onChange={(e) => setEnabled(e.target.checked)}
            />
            <span>{enabled ? 'Yes' : 'No (rule will be skipped)'}</span>
          </label>
        </label>

        <label className={`${styles.formField} ${styles.full}`}>
          <span>Deny reason</span>
          <input
            type="text"
            value={reason}
            onChange={(e) => setReason(e.target.value)}
            placeholder="evil.com is denied for this deployment"
            className={styles.input}
          />
          <small className={styles.hint}>Surfaced to the model and into the audit log when this rule fires.</small>
        </label>
      </div>

      <PredicateEditor
        eventKind={eventKind}
        predicates={predicates}
        onChange={setPredicates}
      />
    </div>
  )
}

// ── Predicate editor ─────────────────────────────────────────────────

function PredicateEditor({
  eventKind: _eventKind,
  predicates,
  onChange,
}: {
  eventKind: PolicyEventKind
  predicates: Predicate[]
  onChange: (next: Predicate[]) => void
}) {
  const addPredicate = () => {
    const t = ALL_PREDICATE_TYPES[0]
    onChange([...predicates, { type: t, value: defaultPredicateValue(t) } as Predicate])
  }
  const removePredicate = (i: number) => {
    onChange(predicates.filter((_, idx) => idx !== i))
  }
  const updatePredicate = (i: number, next: Predicate) => {
    onChange(predicates.map((p, idx) => (idx === i ? next : p)))
  }

  return (
    <div className={styles.predEditor}>
      <div className={styles.predEditorHead}>
        <span>Predicates (combined with AND — match all)</span>
        <button type="button" className={styles.iconBtn} onClick={addPredicate}>
          <Plus size={11} /> Add
        </button>
      </div>

      {predicates.length === 0 && (
        <div className={styles.predEmpty}>
          No predicates — this rule matches every event of its kind. Add at
          least one to scope the match.
        </div>
      )}

      {predicates.map((p, i) => (
        <PredicateRow
          key={i}
          predicate={p}
          onChange={(next) => updatePredicate(i, next)}
          onRemove={() => removePredicate(i)}
        />
      ))}
    </div>
  )
}

function PredicateRow({
  predicate, onChange, onRemove,
}: {
  predicate: Predicate
  onChange: (next: Predicate) => void
  onRemove: () => void
}) {
  const isNumeric = predicateValueIsNumeric(predicate.type)
  return (
    <div className={styles.predRow}>
      <select
        value={predicate.type}
        onChange={(e) => {
          const t = e.target.value as Predicate['type']
          // Reset value on type change so we don't carry a number
          // into a string slot or vice versa.
          onChange({ type: t, value: defaultPredicateValue(t) } as Predicate)
        }}
        className={styles.input}
      >
        {ALL_PREDICATE_TYPES.map((t) => <option key={t} value={t}>{t}</option>)}
      </select>
      {isNumeric
        ? <input
            type="number"
            step="any"
            value={String(predicate.value)}
            onChange={(e) => onChange({
              ...predicate, value: Number(e.target.value),
            } as Predicate)}
            className={styles.input}
          />
        : <input
            type="text"
            value={String(predicate.value)}
            onChange={(e) => onChange({
              ...predicate, value: e.target.value,
            } as Predicate)}
            className={styles.input}
            placeholder={placeholderFor(predicate.type)}
          />
      }
      <button type="button" className={styles.iconBtn} data-variant="danger" onClick={onRemove}>
        <X size={11} />
      </button>
    </div>
  )
}

function placeholderFor(t: Predicate['type']): string {
  switch (t) {
    case 'skill_id_equals':    return 'com.example.coding'
    case 'tool_name_equals':   return 'web_fetch'
    case 'provider_equals':    return 'openrouter'
    case 'model_equals':       return 'anthropic/claude-sonnet-4.6'
    case 'secret_name_equals': return 'AWS_SECRET_ACCESS_KEY'
    case 'host_equals':        return 'evil.com'
    case 'host_has_suffix':    return 'evil.com (matches *.evil.com too)'
    case 'path_under':         return '/etc/ssh'
    case 'fs_mode_equals':     return 'write'
    default:                   return ''
  }
}
