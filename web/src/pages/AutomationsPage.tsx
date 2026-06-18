// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Bot, Clock, Play, Pause as PauseIcon, RotateCcw, Trash2, Plus, X,
  AlarmClock, MoonStar, Eye, RefreshCw, AlertTriangle, History,
  Webhook as WebhookIcon, Zap, Copy, KeyRound, Send, Check,
} from 'lucide-react'
import toast from 'react-hot-toast'
import {
  automationsApi,
  type Schedule,
  type ScheduleStatus,
  type TriggerSpec,
  type Action,
  type CreateScheduleRequest,
  type AutomationRun,
  type RunOutcome,
  type Webhook,
  type EventSubscription,
  type WebhookPayload,
  type CreateWebhookRequest,
  type CreateSubscriptionRequest,
  type AutomationStatus,
} from '@/api/automations'
import styles from './AutomationsPage.module.css'

type AutomationsTab = 'schedules' | 'webhooks' | 'triggers' | 'history'

const STATUS_BADGE: Record<AutomationStatus, string> = {
  active:           'Active',
  paused:           'Paused',
  pending_approval: 'Pending',
  expired:          'Expired',
  failed:           'Failed',
}

// ── Helpers ─────────────────────────────────────────────────────────────────

function fmtTs(secs: number | null | undefined, allowEmpty = '—'): string {
  if (!secs) return allowEmpty
  return new Date(secs * 1000).toLocaleString(undefined, {
    dateStyle: 'medium',
    timeStyle: 'short',
  })
}

// Render a unix-second timestamp in a specific named timezone (the schedule's
// own tz, not the browser's). Used to show the *intent* of the schedule next
// to the local-time fmtTs — e.g. "tomorrow 9 AM (Europe/Lisbon)" makes more
// sense to the user than only the converted local time.
function fmtTsInTz(secs: number | null | undefined, tz: string): string {
  if (!secs) return '—'
  try {
    return new Date(secs * 1000).toLocaleString(undefined, {
      dateStyle: 'medium',
      timeStyle: 'short',
      timeZone:  tz,
    })
  } catch {
    return fmtTs(secs)
  }
}

function browserTz(): string {
  try { return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC' } catch { return 'UTC' }
}

// Visual hint that a schedule runs in a tz other than the user's browser.
// Returns null when they match so the noisy badge doesn't show up on every
// row for users who never leave their default tz.
function TzBadge({ tz }: { tz: string }) {
  if (tz === browserTz()) return null
  return <span className={styles.tzBadge} title={`Schedule timezone: ${tz}`}>{tz}</span>
}

function fmtRelative(secs: number | null | undefined): string {
  if (!secs) return '—'
  const diff = secs - Math.floor(Date.now() / 1000)
  const abs  = Math.abs(diff)
  const mins = Math.round(abs / 60)
  const hrs  = Math.round(abs / 3600)
  const days = Math.round(abs / 86400)
  let s: string
  if (abs < 60)         s = `${abs}s`
  else if (abs < 3600)  s = `${mins}m`
  else if (abs < 86400) s = `${hrs}h`
  else                  s = `${days}d`
  return diff < 0 ? `${s} ago` : `in ${s}`
}

function describeTrigger(t: TriggerSpec): string {
  switch (t.kind) {
    case 'one_off':  return `Once at ${fmtTs(t.at)}`
    case 'interval': return `Every ${humanizeSecs(t.every_secs)}`
    case 'cron':     return humanizeCron(t.expr)
  }
}

// Best-effort human-readable cron summary for the common shapes our presets
// emit. Falls back to the raw expression for anything we don't recognise so
// the user is never lied to. Quartz-style 6-field input
// (`sec min hour dom mon dow`).
function humanizeCron(expr: string): string {
  const parts = expr.trim().split(/\s+/)
  if (parts.length !== 6) return `cron: ${expr}`
  const [sec, min, hour, dom, mon, dow] = parts
  const wildAll = (dom === '*' && mon === '*' && dow === '*')
  const fmtTime = (h: string, m: string) => {
    const hh = Number(h), mm = Number(m)
    if (Number.isNaN(hh) || Number.isNaN(mm)) return null
    const pad = (n: number) => String(n).padStart(2, '0')
    return `${pad(hh)}:${pad(mm)}`
  }

  // every minute / every N minutes
  if (sec === '0' && hour === '*' && wildAll) {
    if (min === '*')  return 'Every minute'
    const m = /^\*\/(\d+)$/.exec(min)
    if (m)            return `Every ${m[1]} minutes`
  }
  // hourly / every N hours
  if (sec === '0' && min === '0' && wildAll) {
    if (hour === '*') return 'Every hour'
    const m = /^\*\/(\d+)$/.exec(hour)
    if (m)            return `Every ${m[1]} hours`
  }
  // daily at H:MM
  if (sec === '0' && wildAll && !min.includes('*') && !hour.includes('*')) {
    const t = fmtTime(hour, min)
    if (t) return `Daily at ${t}`
  }
  // weekly at H:MM on a single day
  if (sec === '0' && dom === '*' && mon === '*' &&
      !min.includes('*') && !hour.includes('*') && /^[A-Z]{3}$/.test(dow)) {
    const t = fmtTime(hour, min)
    if (t) return `${dow} at ${t}`
  }
  // weekday range MON-FRI at H:MM
  if (sec === '0' && dom === '*' && mon === '*' &&
      !min.includes('*') && !hour.includes('*') && /^[A-Z]{3}-[A-Z]{3}$/.test(dow)) {
    const t = fmtTime(hour, min)
    if (t) return `${dow} at ${t}`
  }
  // monthly on a fixed day at H:MM
  if (sec === '0' && mon === '*' && dow === '*' &&
      !min.includes('*') && !hour.includes('*') && /^\d+$/.test(dom)) {
    const t = fmtTime(hour, min)
    if (t) return `Day ${dom} of month at ${t}`
  }
  return `cron: ${expr}`
}

function humanizeSecs(s: number): string {
  if (s % 86400 === 0) return `${s / 86400}d`
  if (s % 3600  === 0) return `${s / 3600}h`
  if (s % 60    === 0) return `${s / 60}m`
  return `${s}s`
}

function describeAction(a: Action): string {
  switch (a.kind) {
    case 'prompt':          return `Prompt → ${a.channel}`
    case 'tool_call':       return `Tool: ${a.tool}`
    case 'internal':        return `Internal: ${a.task}`
    case 'http_post':       return `HTTP POST ${a.url}`
    case 'channel_message': return `Message → ${a.channel}`
  }
}

const STATUS_LABEL: Record<ScheduleStatus, string> = {
  active:           'Active',
  paused:           'Paused',
  pending_approval: 'Pending',
  expired:          'Expired',
  failed:           'Failed',
}

// Local TZ guess — best-effort for the create form's default.
function guessTimezone(): string {
  try { return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC' } catch { return 'UTC' }
}

// Cron presets — common Quartz-style cron expressions surfaced as chips.
const CRON_PRESETS: { label: string; expr: string }[] = [
  { label: 'Every minute',   expr: '0 * * * * *'   },
  { label: 'Every 5 min',    expr: '0 */5 * * * *' },
  { label: 'Hourly',         expr: '0 0 * * * *'   },
  { label: 'Daily 09:00',    expr: '0 0 9 * * *'   },
  { label: 'Mon 09:00',      expr: '0 0 9 * * MON' },
  { label: 'Weekday 18:00',  expr: '0 0 18 * * MON-FRI' },
  { label: '1st of month',   expr: '0 0 9 1 * *'   },
]

const INTERVAL_PRESETS: { label: string; secs: number }[] = [
  { label: '1 min',  secs: 60     },
  { label: '5 min',  secs: 300    },
  { label: '15 min', secs: 900    },
  { label: '1 hour', secs: 3600   },
  { label: '6 hour', secs: 21600  },
  { label: '1 day',  secs: 86400  },
]

// ── Editor draft type ───────────────────────────────────────────────────────

type TriggerKind = 'cron' | 'interval' | 'one_off'

interface EditorDraft {
  id?:           string
  name:          string
  description:   string
  rationale:     string
  triggerKind:   TriggerKind
  cronExpr:      string
  intervalSecs:  number
  oneOffAt:      number    // ms epoch (ts from datetime-local)
  timezone:      string
  quietEnabled:  boolean
  quietStart:    string
  quietEnd:      string
  // Action — page only edits Prompt actions for now (the most common UI shape).
  // Tool/Internal/HTTP/Channel actions remain editable via the API/agent.
  channel:       string
  prompt:        string
  conversationStrategy: 'existing' | 'new' | 'named'
  conversationName: string
}

function blankDraft(): EditorDraft {
  return {
    name:          '',
    description:   '',
    rationale:     '',
    triggerKind:   'cron',
    cronExpr:      '0 0 9 * * *',
    intervalSecs:  3600,
    oneOffAt:      Date.now() + 60 * 60_000,
    timezone:      guessTimezone(),
    quietEnabled:  false,
    quietStart:    '22:00',
    quietEnd:      '07:00',
    channel:       'web',
    prompt:        '',
    conversationStrategy: 'named',
    conversationName: '',
  }
}

function draftFromSchedule(s: Schedule): EditorDraft {
  const d = blankDraft()
  d.id          = s.id
  d.name        = s.name
  d.description = s.description ?? ''
  d.rationale   = s.rationale   ?? ''
  d.timezone    = s.timezone

  if (s.trigger.kind === 'cron') {
    d.triggerKind = 'cron'
    d.cronExpr    = s.trigger.expr
  } else if (s.trigger.kind === 'interval') {
    d.triggerKind  = 'interval'
    d.intervalSecs = s.trigger.every_secs
  } else {
    d.triggerKind = 'one_off'
    d.oneOffAt    = s.trigger.at * 1000
  }

  if (s.quiet_hours) {
    d.quietEnabled = true
    d.quietStart   = s.quiet_hours.start
    d.quietEnd     = s.quiet_hours.end
  }

  if (s.action.kind === 'prompt') {
    d.channel              = s.action.channel
    d.prompt               = s.action.prompt
    d.conversationStrategy = s.action.conversation_strategy
    d.conversationName     = s.action.conversation_name ?? ''
  }
  return d
}

function buildTrigger(d: EditorDraft): TriggerSpec {
  if (d.triggerKind === 'cron')     return { kind: 'cron', expr: d.cronExpr.trim() }
  if (d.triggerKind === 'interval') return { kind: 'interval', every_secs: d.intervalSecs }
  return { kind: 'one_off', at: Math.floor(d.oneOffAt / 1000) }
}

function buildAction(d: EditorDraft): Action {
  return {
    kind:                  'prompt',
    conversation_strategy: d.conversationStrategy,
    conversation_name:     d.conversationName || null,
    channel:               d.channel,
    prompt:                d.prompt,
    max_iterations:        10,
  }
}

function buildRequest(d: EditorDraft): CreateScheduleRequest {
  return {
    name:        d.name.trim(),
    description: d.description.trim() || null,
    rationale:   d.rationale.trim() || null,
    trigger:     buildTrigger(d),
    timezone:    d.timezone || 'UTC',
    quiet_hours: d.quietEnabled
      ? { start: d.quietStart, end: d.quietEnd }
      : null,
    action: buildAction(d),
  }
}

function toLocalInput(ms: number): string {
  const d = new Date(ms)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`
}
function fromLocalInput(s: string): number { return new Date(s).getTime() }

// ── Page ────────────────────────────────────────────────────────────────────

export default function AutomationsPage() {
  const [tab, setTab] = useState<AutomationsTab>('schedules')

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1>
            <Bot size={18} style={{ verticalAlign: 'middle', marginRight: 6 }} />
            Automations
          </h1>
          <p>
            Schedules, webhooks, and event subscriptions — three ways the agent
            reacts to time, the outside world, and itself.
          </p>
        </div>
      </div>

      <div className={styles.tabs}>
        <button
          className={`${styles.tab} ${tab === 'schedules' ? styles.tabActive : ''}`}
          onClick={() => setTab('schedules')}
        >
          <Clock size={13} style={{ verticalAlign: 'middle', marginRight: 4 }} />
          Schedules
        </button>
        <button
          className={`${styles.tab} ${tab === 'webhooks' ? styles.tabActive : ''}`}
          onClick={() => setTab('webhooks')}
        >
          <WebhookIcon size={13} style={{ verticalAlign: 'middle', marginRight: 4 }} />
          Webhooks
        </button>
        <button
          className={`${styles.tab} ${tab === 'triggers' ? styles.tabActive : ''}`}
          onClick={() => setTab('triggers')}
        >
          <Zap size={13} style={{ verticalAlign: 'middle', marginRight: 4 }} />
          Triggers
        </button>
        <button
          className={`${styles.tab} ${tab === 'history' ? styles.tabActive : ''}`}
          onClick={() => setTab('history')}
        >
          <History size={13} style={{ verticalAlign: 'middle', marginRight: 4 }} />
          History
        </button>
      </div>

      {tab === 'schedules' && <SchedulesPanel />}
      {tab === 'webhooks'  && <WebhooksPanel />}
      {tab === 'triggers'  && <TriggersPanel />}
      {tab === 'history'   && <HistoryPanel />}
    </div>
  )
}

function SchedulesPanel() {
  const qc = useQueryClient()
  const [editor, setEditor] = useState<EditorDraft | null>(null)
  const [selected, setSelected] = useState<Schedule | null>(null)

  const { data: schedules = [], isLoading, error } = useQuery({
    queryKey: ['schedules'],
    queryFn:  () => automationsApi.listSchedules(),
    refetchInterval: 15_000,
  })

  const invalidate = () => {
    qc.invalidateQueries({ queryKey: ['schedules'] })
    if (selected) qc.invalidateQueries({ queryKey: ['next-fires', selected.id] })
  }

  const createMut = useMutation({
    mutationFn: (req: CreateScheduleRequest) => automationsApi.createSchedule(req),
    onSuccess:  () => { invalidate(); toast.success('Schedule created'); setEditor(null) },
    onError:    (e: any) => toast.error(`Create failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const updateMut = useMutation({
    mutationFn: ({ id, req }: { id: string; req: CreateScheduleRequest }) =>
      automationsApi.updateSchedule(id, req),
    onSuccess: () => { invalidate(); toast.success('Saved'); setEditor(null) },
    onError:   (e: any) => toast.error(`Save failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const deleteMut = useMutation({
    mutationFn: (id: string) => automationsApi.deleteSchedule(id),
    onSuccess:  () => { invalidate(); toast.success('Deleted'); setSelected(null) },
    onError:    (e: any) => toast.error(`Delete failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const pauseMut = useMutation({
    mutationFn: (id: string) => automationsApi.pause(id),
    onSuccess: (s) => {
      invalidate()
      toast.success('Paused')
      setSelected((cur) => (cur && cur.id === s.id ? s : cur))
    },
    onError: (e: any) => toast.error(`Pause failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const resumeMut = useMutation({
    mutationFn: (id: string) => automationsApi.resume(id),
    onSuccess: (s) => {
      invalidate()
      toast.success('Resumed')
      setSelected((cur) => (cur && cur.id === s.id ? s : cur))
    },
    onError: (e: any) => toast.error(`Resume failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const runNowMut = useMutation({
    mutationFn: (id: string) => automationsApi.runNow(id),
    onSuccess: (s) => {
      invalidate()
      toast.success('Fired')
      setSelected((cur) => (cur && cur.id === s.id ? s : cur))
    },
    onError: (e: any) => toast.error(`Run failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const snoozeMut = useMutation({
    mutationFn: ({ id, until }: { id: string; until: number }) =>
      automationsApi.snooze(id, until),
    onSuccess: (s) => {
      invalidate()
      toast.success(`Snoozed until ${fmtTs(s.next_run_at)}`)
      setSelected((cur) => (cur && cur.id === s.id ? s : cur))
    },
    onError: (e: any) => toast.error(`Snooze failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const approveMut = useMutation({
    mutationFn: (id: string) => automationsApi.approveSchedule(id),
    onSuccess: (s) => {
      invalidate()
      toast.success('Approved')
      setSelected((cur) => (cur && cur.id === s.id ? s : cur))
    },
    onError: (e: any) => toast.error(`Approve failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const rejectMut = useMutation({
    mutationFn: (id: string) => automationsApi.rejectSchedule(id),
    onSuccess: () => { invalidate(); toast.success('Rejected'); setSelected(null) },
    onError:   (e: any) => toast.error(`Reject failed: ${e?.response?.data ?? e?.message ?? e}`),
  })

  const sorted = useMemo(() => {
    return [...schedules].sort((a, b) => {
      // Active first, then by next_run_at ascending; nulls last.
      const sa = a.status === 'active' ? 0 : 1
      const sb = b.status === 'active' ? 0 : 1
      if (sa !== sb) return sa - sb
      const na = a.next_run_at ?? Number.POSITIVE_INFINITY
      const nb = b.next_run_at ?? Number.POSITIVE_INFINITY
      return na - nb
    })
  }, [schedules])

  const submitEditor = () => {
    if (!editor) return
    if (!editor.name.trim()) { toast.error('Name is required'); return }
    if (!editor.prompt.trim()) { toast.error('Prompt is required'); return }
    if (editor.triggerKind === 'cron' && !editor.cronExpr.trim()) {
      toast.error('Cron expression is required'); return
    }
    if (editor.triggerKind === 'interval' && editor.intervalSecs < 1) {
      toast.error('Interval must be ≥ 1 second'); return
    }
    const req = buildRequest(editor)
    if (editor.id) updateMut.mutate({ id: editor.id, req })
    else           createMut.mutate(req)
  }

  return (
    <>
      <div className={styles.headerActions} style={{ marginBottom: 12, display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
        <button className={styles.iconBtn} onClick={() => qc.invalidateQueries({ queryKey: ['schedules'] })}>
          <RefreshCw size={13} /> Refresh
        </button>
        <button className={styles.primaryBtn} onClick={() => setEditor(blankDraft())}>
          <Plus size={14} /> New schedule
        </button>
      </div>

      {error && (
        <div className={styles.error}>
          <AlertTriangle size={13} /> {(error as Error).message}
        </div>
      )}

      <div className={styles.listWrap}>
        {isLoading && <div className={styles.empty}>Loading…</div>}

        {!isLoading && sorted.length === 0 && (
          <div className={styles.empty}>
            No schedules yet. Click <strong>New schedule</strong> to author one.
          </div>
        )}

        {sorted.map((s) => (
          <ScheduleRow
            key={s.id}
            sched={s}
            onOpen={() => setSelected(s)}
            onRunNow={() => runNowMut.mutate(s.id)}
            onPause={() => pauseMut.mutate(s.id)}
            onResume={() => resumeMut.mutate(s.id)}
            onApprove={() => approveMut.mutate(s.id)}
            onReject={() => {
              if (confirm(`Reject "${s.name}"? The schedule will be deleted.`)) {
                rejectMut.mutate(s.id)
              }
            }}
            onEdit={() => setEditor(draftFromSchedule(s))}
            onDelete={() => {
              if (confirm(`Delete "${s.name}"? This cannot be undone.`)) {
                deleteMut.mutate(s.id)
              }
            }}
            disabled={runNowMut.isPending || pauseMut.isPending || resumeMut.isPending}
          />
        ))}
      </div>

      {/* ── Detail drawer ── */}
      {selected && (
        <DetailDrawer
          sched={selected}
          onClose={() => setSelected(null)}
          onRunNow={() => runNowMut.mutate(selected.id)}
          onPause={() => pauseMut.mutate(selected.id)}
          onResume={() => resumeMut.mutate(selected.id)}
          onEdit={() => { setEditor(draftFromSchedule(selected)); setSelected(null) }}
          onSnooze={(mins) =>
            snoozeMut.mutate({
              id:    selected.id,
              until: Math.floor(Date.now() / 1000) + mins * 60,
            })
          }
          onDelete={() => {
            if (confirm(`Delete "${selected.name}"? This cannot be undone.`)) {
              deleteMut.mutate(selected.id)
            }
          }}
        />
      )}

      {/* ── Editor modal ── */}
      {editor && (
        <EditorModal
          draft={editor}
          setDraft={setEditor}
          onClose={() => setEditor(null)}
          onSubmit={submitEditor}
          submitting={createMut.isPending || updateMut.isPending}
        />
      )}
    </>
  )
}

// ── List row ────────────────────────────────────────────────────────────────

function ScheduleRow({
  sched, onOpen, onRunNow, onPause, onResume, onApprove, onReject, onEdit, onDelete, disabled,
}: {
  sched: Schedule
  onOpen: () => void
  onRunNow: () => void
  onPause: () => void
  onResume: () => void
  onApprove: () => void
  onReject: () => void
  onEdit: () => void
  onDelete: () => void
  disabled: boolean
}) {
  const isSystem  = sched.owner_kind === 'system'
  const isPaused  = sched.status === 'paused'
  const isPending = sched.status === 'pending_approval'
  return (
    <div className={styles.row} onClick={onOpen}>
      <div className={styles.rowMain}>
        <div className={styles.rowTitle}>
          <span className={styles.name}>{sched.name}</span>
          <span className={`${styles.badge} ${styles[`status_${sched.status}`] ?? ''}`}>
            {STATUS_LABEL[sched.status]}
          </span>
          {isSystem && <span className={styles.systemTag}>system</span>}
        </div>
        <div className={styles.rowMeta}>
          <span><Clock size={11} /> {describeTrigger(sched.trigger)}</span>
          <span>·</span>
          <span>{describeAction(sched.action)}</span>
          <TzBadge tz={sched.timezone} />
          {sched.quiet_hours && (
            <>
              <span>·</span>
              <span><MoonStar size={11} /> {sched.quiet_hours.start}–{sched.quiet_hours.end}</span>
            </>
          )}
        </div>
        <div className={styles.rowMeta}>
          <span>Next: {fmtTs(sched.next_run_at)} ({fmtRelative(sched.next_run_at)})</span>
          <span>·</span>
          <span>Runs: {sched.run_count}</span>
          {sched.failure_count > 0 && (
            <>
              <span>·</span>
              <span className={styles.errText}>Failures: {sched.failure_count}</span>
            </>
          )}
        </div>
      </div>
      <div className={styles.rowActions} onClick={(e) => e.stopPropagation()}>
        {isPending ? (
          <>
            <button
              className={styles.actionBtn}
              title="Approve"
              onClick={onApprove}
              disabled={disabled}
            >
              <Check size={13} />
            </button>
            <button
              className={`${styles.actionBtn} ${styles.dangerBtn}`}
              title="Reject"
              onClick={onReject}
              disabled={disabled}
            >
              <X size={13} />
            </button>
          </>
        ) : (
          <>
            <button
              className={styles.actionBtn}
              title="Run now"
              onClick={onRunNow}
              disabled={disabled}
            >
              <Play size={13} />
            </button>
            {isPaused ? (
              <button
                className={styles.actionBtn}
                title="Resume"
                onClick={onResume}
                disabled={disabled}
              >
                <RotateCcw size={13} />
              </button>
            ) : (
              <button
                className={styles.actionBtn}
                title="Pause"
                onClick={onPause}
                disabled={disabled}
              >
                <PauseIcon size={13} />
              </button>
            )}
            <button className={styles.actionBtn} title="Edit" onClick={onEdit}>
              <Eye size={13} />
            </button>
            {!isSystem && (
              <button
                className={`${styles.actionBtn} ${styles.dangerBtn}`}
                title="Delete"
                onClick={onDelete}
              >
                <Trash2 size={13} />
              </button>
            )}
          </>
        )}
      </div>
    </div>
  )
}

// ── Detail drawer ───────────────────────────────────────────────────────────

function DetailDrawer({
  sched, onClose, onRunNow, onPause, onResume, onEdit, onSnooze, onDelete,
}: {
  sched: Schedule
  onClose: () => void
  onRunNow: () => void
  onPause: () => void
  onResume: () => void
  onEdit: () => void
  onSnooze: (mins: number) => void
  onDelete: () => void
}) {
  const isPaused = sched.status === 'paused'
  const isSystem = sched.owner_kind === 'system'

  const { data: nextFires = [] } = useQuery({
    queryKey: ['next-fires', sched.id],
    queryFn:  () => automationsApi.nextFires(sched.id, 3),
    enabled:  sched.trigger.kind !== 'one_off',
  })

  const { data: runs = [] } = useQuery({
    queryKey: ['schedule-runs', sched.id],
    queryFn:  () => automationsApi.listRuns({ source: 'schedule', id: sched.id, limit: 10 }),
    refetchInterval: 10_000,
  })

  return (
    <div className={styles.drawerBackdrop} onClick={onClose}>
      <aside className={styles.drawer} onClick={(e) => e.stopPropagation()}>
        <div className={styles.drawerHeader}>
          <h2>{sched.name}</h2>
          <button className={styles.closeBtn} onClick={onClose}><X size={16} /></button>
        </div>

        <div className={styles.drawerSection}>
          <div className={styles.detailLine}>
            <strong>Status</strong>
            <span className={`${styles.badge} ${styles[`status_${sched.status}`] ?? ''}`}>
              {STATUS_LABEL[sched.status]}
            </span>
          </div>
          <div className={styles.detailLine}>
            <strong>Owner</strong>
            <span style={{ textTransform: 'capitalize' }}>{sched.owner_kind}</span>
          </div>
          <div className={styles.detailLine}>
            <strong>Trigger</strong>
            <span>{describeTrigger(sched.trigger)}</span>
          </div>
          <div className={styles.detailLine}>
            <strong>Timezone</strong>
            <span>{sched.timezone}</span>
          </div>
          <div className={styles.detailLine}>
            <strong>Action</strong>
            <span>{describeAction(sched.action)}</span>
          </div>
          {sched.quiet_hours && (
            <div className={styles.detailLine}>
              <strong>Quiet hours</strong>
              <span>{sched.quiet_hours.start} – {sched.quiet_hours.end}</span>
            </div>
          )}
          {sched.description && (
            <div className={styles.detailLine}>
              <strong>Description</strong>
              <span style={{ whiteSpace: 'pre-wrap' }}>{sched.description}</span>
            </div>
          )}
          {sched.rationale && (
            <div className={styles.detailLine}>
              <strong>Rationale</strong>
              <span style={{ whiteSpace: 'pre-wrap' }}>{sched.rationale}</span>
            </div>
          )}
          <div className={styles.detailLine}>
            <strong>Last run</strong>
            <span>{fmtTs(sched.last_run_at)}</span>
          </div>
          <div className={styles.detailLine}>
            <strong>Next run</strong>
            <span>{fmtTs(sched.next_run_at)} ({fmtRelative(sched.next_run_at)})</span>
          </div>
          <div className={styles.detailLine}>
            <strong>Run count</strong>
            <span>{sched.run_count} (failed: {sched.failure_count}/{sched.max_failures})</span>
          </div>
          {sched.last_error && (
            <div className={styles.detailLine}>
              <strong>Last error</strong>
              <span className={styles.errText} style={{ whiteSpace: 'pre-wrap' }}>{sched.last_error}</span>
            </div>
          )}
        </div>

        {nextFires.length > 0 && (
          <div className={styles.drawerSection}>
            <h3>Next 3 fires</h3>
            <ul className={styles.fireList}>
              {nextFires.map((t, i) => (
                <li key={i}>
                  <Clock size={11} /> {fmtTs(t)} <span className={styles.relTime}>({fmtRelative(t)})</span>
                  {sched.timezone !== browserTz() && (
                    <span className={styles.relTime}> · {fmtTsInTz(t, sched.timezone)} {sched.timezone}</span>
                  )}
                </li>
              ))}
            </ul>
          </div>
        )}

        <div className={styles.drawerSection}>
          <h3><History size={13} style={{ verticalAlign: 'middle', marginRight: 4 }} /> Recent runs</h3>
          {runs.length === 0
            ? <p className={styles.muted}>No runs yet.</p>
            : <RunsTable runs={runs} />
          }
        </div>

        <div className={styles.drawerActions}>
          <button className={styles.iconBtn} onClick={onRunNow}>
            <Play size={12} /> Run now
          </button>
          {isPaused
            ? <button className={styles.iconBtn} onClick={onResume}>
                <RotateCcw size={12} /> Resume
              </button>
            : <button className={styles.iconBtn} onClick={onPause}>
                <PauseIcon size={12} /> Pause
              </button>
          }
          <SnoozeMenu onSnooze={onSnooze} />
          <button className={styles.iconBtn} onClick={onEdit}>Edit</button>
          {!isSystem && (
            <button className={`${styles.iconBtn} ${styles.dangerBtn}`} onClick={onDelete}>
              <Trash2 size={12} /> Delete
            </button>
          )}
        </div>
      </aside>
    </div>
  )
}

function SnoozeMenu({ onSnooze }: { onSnooze: (mins: number) => void }) {
  const [open, setOpen] = useState(false)
  const presets = [
    { label: '15m', mins: 15  },
    { label: '1h',  mins: 60  },
    { label: '4h',  mins: 240 },
    { label: '1d',  mins: 1440 },
  ]
  return (
    <div className={styles.snoozeWrap}>
      <button className={styles.iconBtn} onClick={() => setOpen((v) => !v)}>
        <AlarmClock size={12} /> Snooze
      </button>
      {open && (
        <div className={styles.snoozeMenu} onMouseLeave={() => setOpen(false)}>
          {presets.map((p) => (
            <button
              key={p.label}
              className={styles.snoozeItem}
              onClick={() => { onSnooze(p.mins); setOpen(false) }}
            >
              {p.label}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

function runDuration(r: AutomationRun): string {
  if (r.finished_at == null) return '—'
  const ms = (r.finished_at - r.started_at) * 1000
  if (ms < 1000)  return `${ms}ms`
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`
  return `${Math.round(ms / 1000)}s`
}

function RunsTable({ runs }: { runs: AutomationRun[] }) {
  return (
    <table className={styles.runsTable}>
      <thead>
        <tr>
          <th>Started</th>
          <th>Outcome</th>
          <th>Duration</th>
          <th>Detail</th>
        </tr>
      </thead>
      <tbody>
        {runs.map((r) => {
          const detail = r.error ?? r.output_snippet ?? ''
          return (
            <tr key={r.id}>
              <td>{fmtTs(r.started_at)}</td>
              <td>
                <span className={`${styles.badge} ${styles[`outcome_${r.outcome}`] ?? ''}`}>
                  {r.outcome}
                </span>
              </td>
              <td>{runDuration(r)}</td>
              <td className={styles.runDetail} title={detail}>{detail}</td>
            </tr>
          )
        })}
      </tbody>
    </table>
  )
}

// ── History panel ─────────────────────────────────────────────────

const HISTORY_PAGE_SIZE = 50

type HistorySourceFilter  = 'all' | 'schedule' | 'webhook' | 'event' | 'dead_letter'
type HistoryOutcomeFilter = 'all' | RunOutcome

function HistoryPanel() {
  const [source,  setSource]  = useState<HistorySourceFilter>('all')
  const [outcome, setOutcome] = useState<HistoryOutcomeFilter>('all')
  // Pages we've already pulled. We don't put them in react-query because the
  // cursor changes per-fetch; a flat in-component array is the right shape
  // for an append-only "load more" feed.
  const [pages, setPages] = useState<AutomationRun[][]>([])
  const [loading, setLoading] = useState(false)
  const [done, setDone] = useState(false)

  const flat = useMemo(() => pages.flat(), [pages])

  const fetchPage = async (before: number | undefined) => {
    setLoading(true)
    try {
      const rows = await automationsApi.listRuns({
        source:  source  === 'all' ? undefined : source,
        outcome: outcome === 'all' ? undefined : outcome,
        before,
        limit:   HISTORY_PAGE_SIZE,
      })
      setPages((p) => before === undefined ? [rows] : [...p, rows])
      if (rows.length < HISTORY_PAGE_SIZE) setDone(true)
    } catch (e: unknown) {
      toast.error(`Failed to load runs: ${(e as Error).message}`)
    } finally {
      setLoading(false)
    }
  }

  // Reset when filters change. The empty-deps pattern would miss filter
  // changes; the deps trigger a fresh fetch each time.
  useEffect(() => {
    setPages([])
    setDone(false)
    fetchPage(undefined)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [source, outcome])

  const oldest = flat.length > 0 ? flat[flat.length - 1].started_at : undefined

  return (
    <div className={styles.historyPanel}>
      <div className={styles.historyFilters}>
        <label>
          Source
          <select value={source} onChange={(e) => setSource(e.target.value as HistorySourceFilter)}>
            <option value="all">All</option>
            <option value="schedule">Schedules</option>
            <option value="webhook">Webhooks</option>
            <option value="event">Events</option>
            <option value="dead_letter">Dead-letter</option>
          </select>
        </label>
        <label>
          Outcome
          <select value={outcome} onChange={(e) => setOutcome(e.target.value as HistoryOutcomeFilter)}>
            <option value="all">All</option>
            <option value="success">Success</option>
            <option value="failure">Failure</option>
            <option value="skipped">Skipped</option>
            <option value="coalesced">Coalesced</option>
          </select>
        </label>
        <button
          type="button"
          className={styles.actionBtn}
          onClick={() => { setPages([]); setDone(false); fetchPage(undefined) }}
          disabled={loading}
          title="Refresh"
        >
          <RefreshCw size={13} />
        </button>
      </div>

      {flat.length === 0 && !loading && (
        <p className={styles.emptyState}>No runs match these filters.</p>
      )}

      {flat.length > 0 && (
        <table className={styles.runsTable}>
          <thead>
            <tr>
              <th>Started</th>
              <th>Source</th>
              <th>Outcome</th>
              <th>Duration</th>
              <th>Detail</th>
            </tr>
          </thead>
          <tbody>
            {flat.map((r) => {
              const detail = r.error ?? r.output_snippet ?? ''
              return (
                <tr key={r.id}>
                  <td>{fmtTs(r.started_at)}</td>
                  <td>
                    <span className={styles.systemTag}>{r.source_kind}</span>
                    <span className={styles.relTime}> {r.source_id.slice(0, 8)}</span>
                  </td>
                  <td>
                    <span className={`${styles.badge} ${styles[`outcome_${r.outcome}`] ?? ''}`}>
                      {r.outcome}
                    </span>
                  </td>
                  <td>{runDuration(r)}</td>
                  <td className={styles.runDetail} title={detail}>{detail}</td>
                </tr>
              )
            })}
          </tbody>
        </table>
      )}

      <div className={styles.historyFooter}>
        {!done && flat.length > 0 && (
          <button
            type="button"
            className={styles.actionBtn}
            onClick={() => fetchPage(oldest)}
            disabled={loading}
          >
            {loading ? 'Loading…' : 'Load more'}
          </button>
        )}
        {done && flat.length > 0 && (
          <span className={styles.emptyState}>End of history.</span>
        )}
      </div>
    </div>
  )
}

// ── Editor modal ────────────────────────────────────────────────────────────

function EditorModal({
  draft, setDraft, onClose, onSubmit, submitting,
}: {
  draft: EditorDraft
  setDraft: (d: EditorDraft | null) => void
  onClose: () => void
  onSubmit: () => void
  submitting: boolean
}) {
  // Live preview of next fires for cron/interval — peek at the spec without
  // saving. Skipped for one_off (the next fire is just `oneOffAt`).
  const previewSpec = useMemo<TriggerSpec>(() => buildTrigger(draft), [draft])
  const previewKey  = JSON.stringify({ s: previewSpec, tz: draft.timezone })
  const { data: preview = [] } = useQuery({
    queryKey: ['preview-cron', previewKey],
    queryFn:  async () => {
      // Server-side preview for an existing schedule; for the editor we
      // approximate via a temporary one — but there's no preview-without-save
      // endpoint. Show client-derived "next" only for interval/one_off.
      if (previewSpec.kind === 'interval') {
        const out = []
        const start = Date.now()
        for (let i = 1; i <= 3; i++) {
          out.push(Math.floor(start / 1000) + previewSpec.every_secs * i)
        }
        return out
      }
      if (previewSpec.kind === 'one_off') return [previewSpec.at]
      return []  // Cron preview requires server eval.
    },
    enabled: draft.triggerKind !== 'cron',
  })

  const update = (patch: Partial<EditorDraft>) => setDraft({ ...draft, ...patch })

  return (
    <div className={styles.modalBackdrop} onClick={onClose}>
      <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
        <div className={styles.modalHeader}>
          <h2>{draft.id ? 'Edit schedule' : 'New schedule'}</h2>
          <button className={styles.closeBtn} onClick={onClose}><X size={16} /></button>
        </div>

        <div className={styles.formField}>
          <label>Name</label>
          <input
            value={draft.name}
            autoFocus
            onChange={(e) => update({ name: e.target.value })}
            placeholder="Daily morning briefing"
          />
        </div>

        <div className={styles.formField}>
          <label>Description (optional)</label>
          <input
            value={draft.description}
            onChange={(e) => update({ description: e.target.value })}
            placeholder="What this schedule does"
          />
        </div>

        <div className={styles.formField}>
          <label>Trigger</label>
          <div className={styles.segmentedRow}>
            {(['cron', 'interval', 'one_off'] as TriggerKind[]).map((k) => (
              <button
                key={k}
                type="button"
                className={`${styles.segmented} ${draft.triggerKind === k ? styles.segmentedActive : ''}`}
                onClick={() => update({ triggerKind: k })}
              >
                {k === 'one_off' ? 'One-off' : k[0].toUpperCase() + k.slice(1)}
              </button>
            ))}
          </div>
        </div>

        {draft.triggerKind === 'cron' && (
          <>
            <div className={styles.formField}>
              <label>Cron expression (6-field, sec min hr dom mon dow)</label>
              <input
                value={draft.cronExpr}
                onChange={(e) => update({ cronExpr: e.target.value })}
                placeholder="0 0 9 * * *"
                style={{ fontFamily: 'monospace' }}
              />
              <div className={styles.presetRow}>
                {CRON_PRESETS.map((p) => (
                  <button
                    key={p.expr}
                    type="button"
                    className={styles.presetChip}
                    onClick={() => update({ cronExpr: p.expr })}
                  >
                    {p.label}
                  </button>
                ))}
              </div>
            </div>
            {draft.cronExpr.trim() && (
              <p className={styles.hint}>
                <strong>{humanizeCron(draft.cronExpr)}</strong>
                {' '}— preview times computed by the server after the schedule is saved.
              </p>
            )}
          </>
        )}

        {draft.triggerKind === 'interval' && (
          <div className={styles.formField}>
            <label>Every</label>
            <div className={styles.intervalRow}>
              <input
                type="number"
                min={1}
                value={draft.intervalSecs}
                onChange={(e) => update({ intervalSecs: Math.max(1, parseInt(e.target.value || '0', 10)) })}
              />
              <span>seconds</span>
            </div>
            <div className={styles.presetRow}>
              {INTERVAL_PRESETS.map((p) => (
                <button
                  key={p.secs}
                  type="button"
                  className={styles.presetChip}
                  onClick={() => update({ intervalSecs: p.secs })}
                >
                  {p.label}
                </button>
              ))}
            </div>
          </div>
        )}

        {draft.triggerKind === 'one_off' && (
          <div className={styles.formField}>
            <label>Fire at</label>
            <input
              type="datetime-local"
              value={toLocalInput(draft.oneOffAt)}
              onChange={(e) => update({ oneOffAt: fromLocalInput(e.target.value) })}
            />
          </div>
        )}

        {preview.length > 0 && (
          <div className={styles.previewBox}>
            <strong>Preview:</strong>
            <ul>
              {preview.map((t, i) => (
                <li key={i}>{fmtTs(t)} <span className={styles.relTime}>({fmtRelative(t)})</span></li>
              ))}
            </ul>
          </div>
        )}

        <div className={styles.formField}>
          <label>Timezone</label>
          <input
            value={draft.timezone}
            onChange={(e) => update({ timezone: e.target.value })}
            placeholder="UTC"
          />
        </div>

        <div className={styles.checkboxRow}>
          <input
            type="checkbox"
            id="quiet-enabled"
            checked={draft.quietEnabled}
            onChange={(e) => update({ quietEnabled: e.target.checked })}
          />
          <label htmlFor="quiet-enabled">Enable quiet hours (skip user-visible actions in window)</label>
        </div>
        {draft.quietEnabled && (
          <div className={styles.formRow}>
            <div className={styles.formField}>
              <label>Quiet from</label>
              <input
                type="time"
                value={draft.quietStart}
                onChange={(e) => update({ quietStart: e.target.value })}
              />
            </div>
            <div className={styles.formField}>
              <label>Quiet until</label>
              <input
                type="time"
                value={draft.quietEnd}
                onChange={(e) => update({ quietEnd: e.target.value })}
              />
            </div>
          </div>
        )}

        <hr className={styles.divider} />

        <h3 className={styles.subhead}>Action — drop a prompt into a conversation</h3>

        <div className={styles.formRow}>
          <div className={styles.formField}>
            <label>Channel</label>
            <select
              value={draft.channel}
              onChange={(e) => update({ channel: e.target.value })}
            >
              <option value="web">Web</option>
              <option value="tui">TUI</option>
              <option value="telegram">Telegram</option>
              <option value="signal">Signal</option>
            </select>
          </div>
          <div className={styles.formField}>
            <label>Conversation</label>
            <select
              value={draft.conversationStrategy}
              onChange={(e) => update({ conversationStrategy: e.target.value as EditorDraft['conversationStrategy'] })}
            >
              <option value="named">Named (find or create)</option>
              <option value="new">Always new</option>
              {/* "existing" is only meaningful when paired with a conversation_id,
                  which this form doesn't collect. We keep it selectable only when
                  the loaded row was already authored that way (e.g. by the agent),
                  so editing such a row preserves the value. */}
              {draft.conversationStrategy === 'existing' && (
                <option value="existing">Existing (uses saved conversation id)</option>
              )}
            </select>
          </div>
        </div>

        {draft.conversationStrategy === 'existing' ? (
          <p className={styles.formHint}>
            This schedule reuses a specific conversation by id. Switch to
            <strong> Named</strong> or <strong>New</strong> above if you want to
            change which thread it posts into.
          </p>
        ) : (
          <div className={styles.formField}>
            <label>Conversation name {draft.conversationStrategy === 'named' ? '(required)' : '(optional title)'}</label>
            <input
              value={draft.conversationName}
              onChange={(e) => update({ conversationName: e.target.value })}
              placeholder="Morning briefing"
            />
          </div>
        )}

        <div className={styles.formField}>
          <label>Prompt</label>
          <textarea
            value={draft.prompt}
            onChange={(e) => update({ prompt: e.target.value })}
            placeholder="Summarize today's calendar and any unread notes."
          />
        </div>

        <div className={styles.modalActions}>
          <div />
          <div className={styles.modalActionsRight}>
            <button className={styles.iconBtn} onClick={onClose}>Cancel</button>
            <button
              className={styles.primaryBtn}
              onClick={onSubmit}
              disabled={submitting}
            >
              {draft.id ? 'Save' : 'Create'}
            </button>
          </div>
        </div>
      </div>
    </div>
  )
}

// ── Webhooks panel ─────────────────────────────────────────────────────────

interface WebhookDraft {
  name:               string
  description:        string
  channel:            string
  prompt:             string
  conversationName:   string
  predicate:          string
  rateLimitPerMin:    number
}

function blankWebhookDraft(): WebhookDraft {
  return {
    name:             '',
    description:      '',
    channel:          'web',
    prompt:           '',
    conversationName: '',
    predicate:        '',
    rateLimitPerMin:  60,
  }
}

function buildWebhookRequest(d: WebhookDraft): CreateWebhookRequest {
  let predicate: unknown | null = null
  if (d.predicate.trim()) {
    try { predicate = JSON.parse(d.predicate) }
    catch { throw new Error('Predicate is not valid JSON') }
  }
  const action: Action = {
    kind:                  'prompt',
    conversation_strategy: 'named',
    conversation_name:     d.conversationName || null,
    channel:               d.channel,
    prompt:                d.prompt,
    max_iterations:        10,
  }
  return {
    name:               d.name.trim(),
    description:        d.description.trim() || null,
    predicate,
    action,
    rate_limit_per_min: d.rateLimitPerMin,
  }
}

function WebhooksPanel() {
  const qc = useQueryClient()
  const [editor, setEditor]     = useState<WebhookDraft | null>(null)
  const [selected, setSelected] = useState<Webhook | null>(null)
  // Webhook objects stash the secret on response only at create/rotate time.
  // Surfacing it once is the whole point — keep a side-table keyed by id.
  const [revealedSecrets, setRevealedSecrets] = useState<Record<string, string>>({})

  const { data: webhooks = [], isLoading, error } = useQuery({
    queryKey: ['webhooks'],
    queryFn:  () => automationsApi.listWebhooks(),
    refetchInterval: 30_000,
  })

  const invalidate = () => qc.invalidateQueries({ queryKey: ['webhooks'] })

  const stashSecret = (w: Webhook) => {
    if (w.secret) {
      setRevealedSecrets((m) => ({ ...m, [w.id]: w.secret! }))
      toast.success('Secret revealed once — copy now', { duration: 6000 })
    }
  }

  const createMut = useMutation({
    mutationFn: (req: CreateWebhookRequest) => automationsApi.createWebhook(req),
    onSuccess: (w) => {
      invalidate(); setEditor(null); stashSecret(w); setSelected(w)
      toast.success('Webhook created')
    },
    onError: (e: any) => toast.error(`Create failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const deleteMut = useMutation({
    mutationFn: (id: string) => automationsApi.deleteWebhook(id),
    onSuccess: () => { invalidate(); toast.success('Deleted'); setSelected(null) },
    onError:   (e: any) => toast.error(`Delete failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const pauseMut = useMutation({
    mutationFn: (id: string) => automationsApi.pauseWebhook(id),
    onSuccess: (w) => { invalidate(); setSelected((cur) => cur && cur.id === w.id ? w : cur) },
  })
  const resumeMut = useMutation({
    mutationFn: (id: string) => automationsApi.resumeWebhook(id),
    onSuccess: (w) => { invalidate(); setSelected((cur) => cur && cur.id === w.id ? w : cur) },
  })
  const rotateTokenMut = useMutation({
    mutationFn: (id: string) => automationsApi.rotateWebhookToken(id),
    onSuccess: (w) => { invalidate(); setSelected(w); toast.success('Token rotated') },
  })
  const rotateSecretMut = useMutation({
    mutationFn: (id: string) => automationsApi.rotateWebhookSecret(id),
    onSuccess: (w) => { invalidate(); setSelected(w); stashSecret(w) },
  })
  const approveMut = useMutation({
    mutationFn: (id: string) => automationsApi.approveWebhook(id),
    onSuccess: (w) => {
      invalidate()
      toast.success('Approved')
      setSelected((cur) => (cur && cur.id === w.id ? w : cur))
    },
    onError: (e: any) => toast.error(`Approve failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const rejectMut = useMutation({
    mutationFn: (id: string) => automationsApi.rejectWebhook(id),
    onSuccess: () => { invalidate(); toast.success('Rejected'); setSelected(null) },
    onError:   (e: any) => toast.error(`Reject failed: ${e?.response?.data ?? e?.message ?? e}`),
  })

  const submit = () => {
    if (!editor) return
    if (!editor.name.trim())   { toast.error('Name is required'); return }
    if (!editor.prompt.trim()) { toast.error('Prompt is required'); return }
    let req: CreateWebhookRequest
    try { req = buildWebhookRequest(editor) }
    catch (e: any) { toast.error(e.message); return }
    createMut.mutate(req)
  }

  return (
    <>
      <div className={styles.headerActions} style={{ marginBottom: 12, display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
        <button className={styles.iconBtn} onClick={() => qc.invalidateQueries({ queryKey: ['webhooks'] })}>
          <RefreshCw size={13} /> Refresh
        </button>
        <button className={styles.primaryBtn} onClick={() => setEditor(blankWebhookDraft())}>
          <Plus size={14} /> New webhook
        </button>
      </div>

      {error && (
        <div className={styles.error}>
          <AlertTriangle size={13} /> {(error as Error).message}
        </div>
      )}

      <div className={styles.listWrap}>
        {isLoading && <div className={styles.empty}>Loading…</div>}
        {!isLoading && webhooks.length === 0 && (
          <div className={styles.empty}>
            No webhooks yet. Click <strong>New webhook</strong> to mint a public
            URL the agent will react to.
          </div>
        )}
        {webhooks.map((w) => (
          <div key={w.id} className={styles.row} onClick={() => setSelected(w)}>
            <div className={styles.rowMain}>
              <div className={styles.rowTitle}>
                <span className={styles.name}>{w.name}</span>
                <span className={`${styles.badge} ${styles[`status_${w.status}`] ?? ''}`}>
                  {STATUS_BADGE[w.status]}
                </span>
              </div>
              <div className={styles.rowMeta}>
                <span><WebhookIcon size={11} /> /webhook/incoming/{w.token.slice(0, 8)}…</span>
                <span>·</span>
                <span>{describeAction(w.action)}</span>
                <span>·</span>
                <span>≤ {w.rate_limit_per_min}/min</span>
              </div>
              <div className={styles.rowMeta}>
                <span>Last seen: {fmtTs(w.last_seen_at)}</span>
                {w.last_error && (
                  <>
                    <span>·</span>
                    <span className={styles.errText}>err: {w.last_error}</span>
                  </>
                )}
              </div>
            </div>
            <div className={styles.rowActions} onClick={(e) => e.stopPropagation()}>
              {w.status === 'pending_approval' ? (
                <>
                  <button className={styles.actionBtn} onClick={() => approveMut.mutate(w.id)} title="Approve">
                    <Check size={13} />
                  </button>
                  <button
                    className={`${styles.actionBtn} ${styles.dangerBtn}`}
                    onClick={() => { if (confirm(`Reject "${w.name}"? The webhook will be deleted.`)) rejectMut.mutate(w.id) }}
                    title="Reject"
                  >
                    <X size={13} />
                  </button>
                </>
              ) : (
                <>
                  {w.status === 'paused'
                    ? <button className={styles.actionBtn} onClick={() => resumeMut.mutate(w.id)} title="Resume"><RotateCcw size={13} /></button>
                    : <button className={styles.actionBtn} onClick={() => pauseMut.mutate(w.id)}  title="Pause"><PauseIcon size={13} /></button>}
                  <button
                    className={`${styles.actionBtn} ${styles.dangerBtn}`}
                    onClick={() => { if (confirm(`Delete "${w.name}"?`)) deleteMut.mutate(w.id) }}
                    title="Delete"
                  >
                    <Trash2 size={13} />
                  </button>
                </>
              )}
            </div>
          </div>
        ))}
      </div>

      {selected && (
        <WebhookDetail
          webhook={selected}
          revealedSecret={revealedSecrets[selected.id]}
          onClose={() => setSelected(null)}
          onRotateToken={() => rotateTokenMut.mutate(selected.id)}
          onRotateSecret={() => rotateSecretMut.mutate(selected.id)}
        />
      )}

      {editor && (
        <WebhookEditorModal
          draft={editor}
          setDraft={setEditor}
          onClose={() => setEditor(null)}
          onSubmit={submit}
          submitting={createMut.isPending}
        />
      )}
    </>
  )
}

function WebhookDetail({
  webhook, revealedSecret, onClose, onRotateToken, onRotateSecret,
}: {
  webhook:        Webhook
  revealedSecret: string | undefined
  onClose:        () => void
  onRotateToken:  () => void
  onRotateSecret: () => void
}) {
  const { data: urlInfo } = useQuery({
    queryKey: ['webhook-url', webhook.id],
    queryFn:  () => automationsApi.webhookUrl(webhook.id),
  })
  const { data: payloads = [] } = useQuery({
    queryKey: ['webhook-payloads', webhook.id],
    queryFn:  () => automationsApi.listWebhookPayloads(webhook.id, 20),
    refetchInterval: 5_000,
  })
  const fullUrl = useMemo(() => {
    if (!urlInfo) return ''
    if (urlInfo.path.startsWith('http')) return urlInfo.path
    return `${window.location.origin}${urlInfo.path}`
  }, [urlInfo])

  const copy = (s: string) => {
    navigator.clipboard?.writeText(s).then(
      () => toast.success('Copied'),
      () => toast.error('Copy failed'),
    )
  }

  return (
    <div className={styles.drawerBackdrop} onClick={onClose}>
      <aside className={styles.drawer} onClick={(e) => e.stopPropagation()}>
        <div className={styles.drawerHeader}>
          <h2>{webhook.name}</h2>
          <button className={styles.closeBtn} onClick={onClose}><X size={16} /></button>
        </div>

        <div className={styles.drawerSection}>
          <div className={styles.detailLine}>
            <strong>URL</strong>
            <span style={{ wordBreak: 'break-all', fontFamily: 'monospace', fontSize: 12 }}>
              {fullUrl || '—'}
            </span>
          </div>
          <div style={{ display: 'flex', gap: 6 }}>
            <button className={styles.iconBtn} onClick={() => copy(fullUrl)} disabled={!fullUrl}>
              <Copy size={12} /> Copy URL
            </button>
            <button className={styles.iconBtn} onClick={onRotateToken}>
              <RotateCcw size={12} /> Rotate token
            </button>
          </div>
        </div>

        {revealedSecret && (
          <div className={styles.drawerSection}>
            <div className={styles.detailLine}>
              <strong>Signing secret</strong>
              <span style={{ fontFamily: 'monospace', fontSize: 12, wordBreak: 'break-all' }}>
                {revealedSecret}
              </span>
            </div>
            <p className={styles.hint}>
              <KeyRound size={12} style={{ verticalAlign: 'middle', marginRight: 4 }} />
              This is shown once. Copy it now — refreshing this view will hide it.
              Senders compute <code>HMAC-SHA256(secret, body)</code> and pass it
              as <code>X-Webhook-Signature</code> (optionally prefixed
              <code>sha256=</code>).
            </p>
            <button className={styles.iconBtn} onClick={() => copy(revealedSecret)}>
              <Copy size={12} /> Copy secret
            </button>
          </div>
        )}
        {!revealedSecret && (
          <div className={styles.drawerSection}>
            <button className={styles.iconBtn} onClick={onRotateSecret}>
              <KeyRound size={12} /> Rotate signing secret
            </button>
            <p className={styles.hint}>
              The current secret hash is server-side; rotating mints a new one
              shown once.
            </p>
          </div>
        )}

        <div className={styles.drawerSection}>
          <div className={styles.detailLine}>
            <strong>Status</strong>
            <span className={`${styles.badge} ${styles[`status_${webhook.status}`] ?? ''}`}>
              {STATUS_BADGE[webhook.status]}
            </span>
          </div>
          <div className={styles.detailLine}>
            <strong>Action</strong>
            <span>{describeAction(webhook.action)}</span>
          </div>
          <div className={styles.detailLine}>
            <strong>Rate limit</strong>
            <span>{webhook.rate_limit_per_min} requests / minute</span>
          </div>
          {webhook.predicate != null && (
            <div className={styles.detailLine}>
              <strong>Predicate</strong>
              <span style={{ fontFamily: 'monospace', whiteSpace: 'pre-wrap', fontSize: 11 }}>
                {JSON.stringify(webhook.predicate, null, 2)}
              </span>
            </div>
          )}
          <div className={styles.detailLine}>
            <strong>Last seen</strong>
            <span>{fmtTs(webhook.last_seen_at)}</span>
          </div>
        </div>

        <div className={styles.drawerSection}>
          <h3><History size={13} style={{ verticalAlign: 'middle', marginRight: 4 }} /> Recent payloads</h3>
          {payloads.length === 0
            ? <p className={styles.muted}>No deliveries yet — POST to the URL above to test.</p>
            : (
              <ul className={styles.fireList}>
                {payloads.map((p) => (
                  <PayloadRow key={p.id} webhookId={webhook.id} payload={p} />
                ))}
              </ul>
            )
          }
        </div>
      </aside>
    </div>
  )
}

function PayloadRow({ webhookId, payload }: { webhookId: string; payload: WebhookPayload }) {
  const [expanded, setExpanded] = useState(false)
  const replayMut = useMutation({
    mutationFn: () => automationsApi.testReplayWebhook(webhookId, payload.id),
    onSuccess:  () => toast.success('Replay dispatched'),
    onError:    (e: any) => toast.error(`Replay failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  return (
    <li>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: 8 }}>
        <div>
          <Clock size={11} /> {fmtTs(payload.received_at)}
          {' '}
          <span className={`${styles.badge}`} style={{ marginLeft: 6 }}>
            {payload.matched ? 'matched' : 'no-match'}
          </span>
        </div>
        <div style={{ display: 'flex', gap: 6 }}>
          <button className={styles.actionBtn} onClick={() => setExpanded((v) => !v)} title="Toggle">
            <Eye size={12} />
          </button>
          <button
            className={styles.actionBtn}
            onClick={() => replayMut.mutate()}
            disabled={replayMut.isPending}
            title="Replay"
          >
            <Send size={12} />
          </button>
        </div>
      </div>
      {expanded && (
        <pre style={{ fontSize: 11, maxHeight: 240, overflow: 'auto', marginTop: 6 }}>
          {payload.body}
        </pre>
      )}
    </li>
  )
}

function WebhookEditorModal({
  draft, setDraft, onClose, onSubmit, submitting,
}: {
  draft:      WebhookDraft
  setDraft:   (d: WebhookDraft | null) => void
  onClose:    () => void
  onSubmit:   () => void
  submitting: boolean
}) {
  const update = (patch: Partial<WebhookDraft>) => setDraft({ ...draft, ...patch })
  return (
    <div className={styles.modalBackdrop} onClick={onClose}>
      <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
        <div className={styles.modalHeader}>
          <h2>New webhook</h2>
          <button className={styles.closeBtn} onClick={onClose}><X size={16} /></button>
        </div>

        <div className={styles.formField}>
          <label>Name</label>
          <input
            autoFocus
            value={draft.name}
            onChange={(e) => update({ name: e.target.value })}
            placeholder="GitHub deploy notifier"
          />
        </div>

        <div className={styles.formField}>
          <label>Description (optional)</label>
          <input
            value={draft.description}
            onChange={(e) => update({ description: e.target.value })}
            placeholder="What this webhook does"
          />
        </div>

        <div className={styles.formField}>
          <label>Predicate JSON (optional — match payloads before firing)</label>
          <textarea
            value={draft.predicate}
            onChange={(e) => update({ predicate: e.target.value })}
            placeholder='{"==": [{"path": "payload.action"}, "opened"]}'
            style={{ fontFamily: 'monospace', minHeight: 80 }}
          />
          <p className={styles.hint}>
            Leave blank to fire on every valid POST. Reference the payload via
            <code> payload.foo </code> path lookups.
          </p>
        </div>

        <div className={styles.formField}>
          <label>Rate limit (requests / minute)</label>
          <input
            type="number"
            min={1}
            value={draft.rateLimitPerMin}
            onChange={(e) => update({ rateLimitPerMin: Math.max(1, parseInt(e.target.value || '0', 10)) })}
          />
        </div>

        <hr className={styles.divider} />

        <h3 className={styles.subhead}>Action — drop a prompt into a conversation</h3>

        <div className={styles.formRow}>
          <div className={styles.formField}>
            <label>Channel</label>
            <select value={draft.channel} onChange={(e) => update({ channel: e.target.value })}>
              <option value="web">Web</option>
              <option value="tui">TUI</option>
              <option value="telegram">Telegram</option>
              <option value="signal">Signal</option>
            </select>
          </div>
          <div className={styles.formField}>
            <label>Conversation name</label>
            <input
              value={draft.conversationName}
              onChange={(e) => update({ conversationName: e.target.value })}
              placeholder="github-events"
            />
          </div>
        </div>

        <div className={styles.formField}>
          <label>Prompt template</label>
          <textarea
            value={draft.prompt}
            onChange={(e) => update({ prompt: e.target.value })}
            placeholder="A new deploy event arrived: {{payload.repo}} → {{payload.ref}}. Draft a one-line summary."
            style={{ minHeight: 80 }}
          />
          <p className={styles.hint}>
            Reference fields with <code>{'{{payload.field}}'}</code>.
          </p>
        </div>

        <div className={styles.modalActions}>
          <div />
          <div className={styles.modalActionsRight}>
            <button className={styles.iconBtn} onClick={onClose}>Cancel</button>
            <button className={styles.primaryBtn} onClick={onSubmit} disabled={submitting}>
              Create
            </button>
          </div>
        </div>
      </div>
    </div>
  )
}

// ── Triggers (event subscriptions) panel ───────────────────────────────────

interface SubscriptionDraft {
  name:             string
  description:      string
  eventName:        string
  channel:          string
  prompt:           string
  conversationName: string
  predicate:        string
}

function blankSubscriptionDraft(eventNames: string[]): SubscriptionDraft {
  return {
    name:             '',
    description:      '',
    eventName:        eventNames[0] ?? '',
    channel:          'web',
    prompt:           '',
    conversationName: '',
    predicate:        '',
  }
}

function buildSubscriptionRequest(d: SubscriptionDraft): CreateSubscriptionRequest {
  let predicate: unknown | null = null
  if (d.predicate.trim()) {
    try { predicate = JSON.parse(d.predicate) }
    catch { throw new Error('Predicate is not valid JSON') }
  }
  const action: Action = {
    kind:                  'prompt',
    conversation_strategy: 'named',
    conversation_name:     d.conversationName || null,
    channel:               d.channel,
    prompt:                d.prompt,
    max_iterations:        10,
  }
  return {
    name:        d.name.trim(),
    description: d.description.trim() || null,
    event_name:  d.eventName.trim(),
    predicate,
    action,
  }
}

function TriggersPanel() {
  const qc = useQueryClient()
  const [editor, setEditor]     = useState<SubscriptionDraft | null>(null)
  const [selected, setSelected] = useState<EventSubscription | null>(null)

  const { data: eventNames = [] } = useQuery({
    queryKey: ['event-names'],
    queryFn:  () => automationsApi.listEventNames(),
    staleTime: 5 * 60_000,
  })
  const { data: subs = [], isLoading, error } = useQuery({
    queryKey: ['event-subscriptions'],
    queryFn:  () => automationsApi.listSubscriptions(),
    refetchInterval: 30_000,
  })

  const invalidate = () => qc.invalidateQueries({ queryKey: ['event-subscriptions'] })

  const createMut = useMutation({
    mutationFn: (req: CreateSubscriptionRequest) => automationsApi.createSubscription(req),
    onSuccess: () => { invalidate(); toast.success('Trigger created'); setEditor(null) },
    onError:   (e: any) => toast.error(`Create failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const deleteMut = useMutation({
    mutationFn: (id: string) => automationsApi.deleteSubscription(id),
    onSuccess: () => { invalidate(); toast.success('Deleted'); setSelected(null) },
    onError:   (e: any) => toast.error(`Delete failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const pauseMut = useMutation({
    mutationFn: (id: string) => automationsApi.pauseSubscription(id),
    onSuccess: (s) => { invalidate(); setSelected((cur) => cur && cur.id === s.id ? s : cur) },
  })
  const resumeMut = useMutation({
    mutationFn: (id: string) => automationsApi.resumeSubscription(id),
    onSuccess: (s) => { invalidate(); setSelected((cur) => cur && cur.id === s.id ? s : cur) },
  })
  const testEmitMut = useMutation({
    mutationFn: ({ name, payload }: { name: string; payload: unknown }) =>
      automationsApi.testEmitEvent(name, payload),
    onSuccess: () => toast.success('Synthetic event emitted'),
    onError:   (e: any) => toast.error(`Emit failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const approveMut = useMutation({
    mutationFn: (id: string) => automationsApi.approveSubscription(id),
    onSuccess: (s) => {
      invalidate()
      toast.success('Approved')
      setSelected((cur) => (cur && cur.id === s.id ? s : cur))
    },
    onError: (e: any) => toast.error(`Approve failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const rejectMut = useMutation({
    mutationFn: (id: string) => automationsApi.rejectSubscription(id),
    onSuccess: () => { invalidate(); toast.success('Rejected'); setSelected(null) },
    onError:   (e: any) => toast.error(`Reject failed: ${e?.response?.data ?? e?.message ?? e}`),
  })

  const submit = () => {
    if (!editor) return
    if (!editor.name.trim())      { toast.error('Name is required'); return }
    if (!editor.eventName.trim()) { toast.error('Event is required'); return }
    if (!editor.prompt.trim())    { toast.error('Prompt is required'); return }
    let req: CreateSubscriptionRequest
    try { req = buildSubscriptionRequest(editor) }
    catch (e: any) { toast.error(e.message); return }
    createMut.mutate(req)
  }

  return (
    <>
      <div className={styles.headerActions} style={{ marginBottom: 12, display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
        <button className={styles.iconBtn} onClick={() => qc.invalidateQueries({ queryKey: ['event-subscriptions'] })}>
          <RefreshCw size={13} /> Refresh
        </button>
        <button
          className={styles.primaryBtn}
          onClick={() => setEditor(blankSubscriptionDraft(eventNames))}
          disabled={eventNames.length === 0}
        >
          <Plus size={14} /> New trigger
        </button>
      </div>

      {error && (
        <div className={styles.error}>
          <AlertTriangle size={13} /> {(error as Error).message}
        </div>
      )}

      <div className={styles.listWrap}>
        {isLoading && <div className={styles.empty}>Loading…</div>}
        {!isLoading && subs.length === 0 && (
          <div className={styles.empty}>
            No event subscriptions yet. Click <strong>New trigger</strong> to
            react when MIRA emits something interesting.
          </div>
        )}
        {subs.map((s) => (
          <div key={s.id} className={styles.row} onClick={() => setSelected(s)}>
            <div className={styles.rowMain}>
              <div className={styles.rowTitle}>
                <span className={styles.name}>{s.name}</span>
                <span className={`${styles.badge} ${styles[`status_${s.status}`] ?? ''}`}>
                  {STATUS_BADGE[s.status]}
                </span>
              </div>
              <div className={styles.rowMeta}>
                <span><Zap size={11} /> {s.event_name}</span>
                <span>·</span>
                <span>{describeAction(s.action)}</span>
              </div>
              <div className={styles.rowMeta}>
                <span>Last fired: {fmtTs(s.last_fired_at)}</span>
                {s.last_error && (
                  <>
                    <span>·</span>
                    <span className={styles.errText}>err: {s.last_error}</span>
                  </>
                )}
              </div>
            </div>
            <div className={styles.rowActions} onClick={(e) => e.stopPropagation()}>
              {s.status === 'pending_approval' ? (
                <>
                  <button
                    className={styles.actionBtn}
                    onClick={() => approveMut.mutate(s.id)}
                    title="Approve"
                  >
                    <Check size={13} />
                  </button>
                  <button
                    className={`${styles.actionBtn} ${styles.dangerBtn}`}
                    onClick={() => { if (confirm(`Reject "${s.name}"? The subscription will be deleted.`)) rejectMut.mutate(s.id) }}
                    title="Reject"
                  >
                    <X size={13} />
                  </button>
                </>
              ) : (
                <>
                  <button
                    className={styles.actionBtn}
                    title="Emit synthetic event (admin)"
                    onClick={() => testEmitMut.mutate({ name: s.event_name, payload: {} })}
                  >
                    <Play size={13} />
                  </button>
                  {s.status === 'paused'
                    ? <button className={styles.actionBtn} onClick={() => resumeMut.mutate(s.id)} title="Resume"><RotateCcw size={13} /></button>
                    : <button className={styles.actionBtn} onClick={() => pauseMut.mutate(s.id)}  title="Pause"><PauseIcon size={13} /></button>}
                  <button
                    className={`${styles.actionBtn} ${styles.dangerBtn}`}
                    onClick={() => { if (confirm(`Delete "${s.name}"?`)) deleteMut.mutate(s.id) }}
                    title="Delete"
                  >
                    <Trash2 size={13} />
                  </button>
                </>
              )}
            </div>
          </div>
        ))}
      </div>

      {selected && (
        <SubscriptionDetail
          sub={selected}
          onClose={() => setSelected(null)}
          onTestEmit={(payload) => testEmitMut.mutate({ name: selected.event_name, payload })}
        />
      )}

      {editor && (
        <SubscriptionEditorModal
          draft={editor}
          eventNames={eventNames}
          setDraft={setEditor}
          onClose={() => setEditor(null)}
          onSubmit={submit}
          submitting={createMut.isPending}
        />
      )}
    </>
  )
}

function SubscriptionDetail({
  sub, onClose, onTestEmit,
}: {
  sub:        EventSubscription
  onClose:    () => void
  onTestEmit: (payload: unknown) => void
}) {
  const [payloadText, setPayloadText] = useState('{}')
  const fire = () => {
    try {
      const p = JSON.parse(payloadText)
      onTestEmit(p)
    } catch {
      toast.error('Payload is not valid JSON')
    }
  }
  return (
    <div className={styles.drawerBackdrop} onClick={onClose}>
      <aside className={styles.drawer} onClick={(e) => e.stopPropagation()}>
        <div className={styles.drawerHeader}>
          <h2>{sub.name}</h2>
          <button className={styles.closeBtn} onClick={onClose}><X size={16} /></button>
        </div>

        <div className={styles.drawerSection}>
          <div className={styles.detailLine}>
            <strong>Event</strong>
            <span style={{ fontFamily: 'monospace' }}>{sub.event_name}</span>
          </div>
          <div className={styles.detailLine}>
            <strong>Status</strong>
            <span className={`${styles.badge} ${styles[`status_${sub.status}`] ?? ''}`}>
              {STATUS_BADGE[sub.status]}
            </span>
          </div>
          <div className={styles.detailLine}>
            <strong>Action</strong>
            <span>{describeAction(sub.action)}</span>
          </div>
          {sub.predicate != null && (
            <div className={styles.detailLine}>
              <strong>Predicate</strong>
              <span style={{ fontFamily: 'monospace', whiteSpace: 'pre-wrap', fontSize: 11 }}>
                {JSON.stringify(sub.predicate, null, 2)}
              </span>
            </div>
          )}
          <div className={styles.detailLine}>
            <strong>Last fired</strong>
            <span>{fmtTs(sub.last_fired_at)}</span>
          </div>
          {sub.last_error && (
            <div className={styles.detailLine}>
              <strong>Last error</strong>
              <span className={styles.errText} style={{ whiteSpace: 'pre-wrap' }}>{sub.last_error}</span>
            </div>
          )}
        </div>

        <div className={styles.drawerSection}>
          <h3>Test emit (admin only)</h3>
          <p className={styles.hint}>
            Synthesise a <code>{sub.event_name}</code> event with the JSON below.
            Predicate, ownership, and dispatch run end-to-end.
          </p>
          <textarea
            value={payloadText}
            onChange={(e) => setPayloadText(e.target.value)}
            style={{ width: '100%', fontFamily: 'monospace', minHeight: 100 }}
          />
          <button className={styles.primaryBtn} onClick={fire} style={{ marginTop: 6 }}>
            <Send size={12} /> Emit
          </button>
        </div>
      </aside>
    </div>
  )
}

function SubscriptionEditorModal({
  draft, eventNames, setDraft, onClose, onSubmit, submitting,
}: {
  draft:      SubscriptionDraft
  eventNames: string[]
  setDraft:   (d: SubscriptionDraft | null) => void
  onClose:    () => void
  onSubmit:   () => void
  submitting: boolean
}) {
  const update = (patch: Partial<SubscriptionDraft>) => setDraft({ ...draft, ...patch })
  return (
    <div className={styles.modalBackdrop} onClick={onClose}>
      <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
        <div className={styles.modalHeader}>
          <h2>New trigger</h2>
          <button className={styles.closeBtn} onClick={onClose}><X size={16} /></button>
        </div>

        <div className={styles.formField}>
          <label>Name</label>
          <input
            autoFocus
            value={draft.name}
            onChange={(e) => update({ name: e.target.value })}
            placeholder="Notify me when a tool fails"
          />
        </div>

        <div className={styles.formField}>
          <label>Description (optional)</label>
          <input
            value={draft.description}
            onChange={(e) => update({ description: e.target.value })}
          />
        </div>

        <div className={styles.formField}>
          <label>Event</label>
          <select
            value={draft.eventName}
            onChange={(e) => update({ eventName: e.target.value })}
          >
            {eventNames.map((n) => (
              <option key={n} value={n}>{n}</option>
            ))}
          </select>
        </div>

        <div className={styles.formField}>
          <label>Predicate JSON (optional)</label>
          <textarea
            value={draft.predicate}
            onChange={(e) => update({ predicate: e.target.value })}
            placeholder='{"==": [{"path": "payload.tool"}, "calendar"]}'
            style={{ fontFamily: 'monospace', minHeight: 80 }}
          />
        </div>

        <hr className={styles.divider} />

        <h3 className={styles.subhead}>Action — drop a prompt into a conversation</h3>

        <div className={styles.formRow}>
          <div className={styles.formField}>
            <label>Channel</label>
            <select value={draft.channel} onChange={(e) => update({ channel: e.target.value })}>
              <option value="web">Web</option>
              <option value="tui">TUI</option>
              <option value="telegram">Telegram</option>
              <option value="signal">Signal</option>
            </select>
          </div>
          <div className={styles.formField}>
            <label>Conversation name</label>
            <input
              value={draft.conversationName}
              onChange={(e) => update({ conversationName: e.target.value })}
              placeholder="alerts"
            />
          </div>
        </div>

        <div className={styles.formField}>
          <label>Prompt template</label>
          <textarea
            value={draft.prompt}
            onChange={(e) => update({ prompt: e.target.value })}
            placeholder="A {{event.name}} arrived. Inspect: {{payload}}"
            style={{ minHeight: 80 }}
          />
        </div>

        <div className={styles.modalActions}>
          <div />
          <div className={styles.modalActionsRight}>
            <button className={styles.iconBtn} onClick={onClose}>Cancel</button>
            <button className={styles.primaryBtn} onClick={onSubmit} disabled={submitting}>
              Create
            </button>
          </div>
        </div>
      </div>
    </div>
  )
}
