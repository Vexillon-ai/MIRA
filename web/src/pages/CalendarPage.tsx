// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useMemo, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router-dom'
import {
  Calendar as CalendarIcon, ChevronLeft, ChevronRight, Plus, X, Trash2, Eye, Bot, Users, Sunrise,
} from 'lucide-react'
import toast from 'react-hot-toast'
import {
  calendarApi,
  SHARED_OWNER,
  GROUP_OWNER_PREFIX,
  groupIdFromOwner,
  type CalendarEvent,
  type EventInput,
  type EventKind,
} from '@/api/calendar'
import { groupsApi } from '@/api/groups'
import type { Group } from '@/api/types'
import {
  automationsApi,
  type Schedule,
} from '@/api/automations'
import { useAuthStore } from '@/store/authStore'
import { api } from '@/api/client'
import type { User } from '@/api/types'
import styles from './CalendarPage.module.css'

const WEEK_DAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat']

function startOfMonth(d: Date) {
  return new Date(d.getFullYear(), d.getMonth(), 1)
}
function startOfGrid(d: Date) {
  const s = startOfMonth(d)
  s.setDate(s.getDate() - s.getDay())
  return s
}
function isSameDay(a: Date, b: Date) {
  return a.getFullYear() === b.getFullYear()
      && a.getMonth()    === b.getMonth()
      && a.getDate()     === b.getDate()
}
function eventTouchesDay(ev: CalendarEvent, day: Date): boolean {
  const dayStart = new Date(day.getFullYear(), day.getMonth(), day.getDate()).getTime()
  const dayEnd   = dayStart + 24 * 3600_000 - 1
  return ev.starts_at <= dayEnd && ev.ends_at >= dayStart
}
function fmtMonth(d: Date) {
  return d.toLocaleString(undefined, { month: 'long', year: 'numeric' })
}

/** Convert a millisecond timestamp into a value usable by `<input type=datetime-local>`. */
function toLocalInput(ms: number): string {
  const d = new Date(ms)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`
}
function fromLocalInput(s: string): number {
  // The browser parses datetime-local as local time.
  return new Date(s).getTime()
}

function fmtDateTime(ms: number, allDay: boolean) {
  const d = new Date(ms)
  return allDay
    ? d.toLocaleDateString()
    : d.toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' })
}

// ── Per-user calendar connection ──────────────────────────────────────────────

// Lets the signed-in user link THEIR OWN external calendar account (the per-user
// OAuth endpoints are AuthUser-gated, so every user — not just admins — can use
// this). The instance-level setup (which provider, its OAuth app credentials)
// lives in admin Settings → Calendar; this only surfaces the active provider's
// per-account connect/disconnect for the current user.
function CalendarConnectPanel() {
  const qc = useQueryClient()
  const [params, setParams] = useSearchParams()
  const q = useQuery({
    queryKey: ['calendar-oauth-status'],
    queryFn:  () => calendarApi.oauthStatus(),
    retry: false,
  })
  const connect = useMutation({
    mutationFn: (p: 'google' | 'outlook') => calendarApi.oauthStart(p),
    onSuccess:  (d) => { window.location.href = d.authorize_url },
    onError:    (e: unknown) => {
      const msg = (e as { response?: { data?: string } })?.response?.data
      toast.error(typeof msg === 'string' && msg ? msg : 'Could not start the connection.')
    },
  })
  const disconnect = useMutation({
    mutationFn: (p: 'google' | 'outlook') => calendarApi.oauthDisconnect(p),
    onSuccess:  () => { toast.success('Disconnected.'); qc.invalidateQueries({ queryKey: ['calendar-oauth-status'] }) },
    onError:    () => toast.error('Disconnect failed'),
  })
  // Surface the result of the OAuth round-trip (callback redirects here with ?connected=1).
  useEffect(() => {
    if (params.get('connected') === '1') {
      toast.success('Calendar account connected.')
      params.delete('connected')
      setParams(params, { replace: true })
      qc.invalidateQueries({ queryKey: ['calendar-oauth-status'] })
    }
  }, [params, setParams, qc])

  const s = q.data
  if (!s) return null
  if (!s.sync_enabled || s.sync_provider === 'none') {
    return <div className={styles.connectPanel}>🔗 External calendar sync isn’t enabled on this server. Ask an administrator to set it up in Settings → Calendar.</div>
  }
  if (s.sync_provider === 'caldav') {
    return <CalDavConnectPanel connected={s.caldav_connected} />
  }
  const prov: 'google' | 'outlook' = s.sync_provider === 'outlook' ? 'outlook' : 'google'
  const label = prov === 'google' ? 'Google' : 'Microsoft / Outlook'
  const connected  = prov === 'google' ? s.google_connected  : s.outlook_connected
  const configured = prov === 'google' ? s.google_configured : s.outlook_configured
  if (!configured) {
    return <div className={styles.connectPanel}>🔗 An administrator selected {label} calendar sync but hasn’t finished setting it up yet.</div>
  }
  return (
    <div className={styles.connectPanel}>
      <span style={{ flex: 1 }}>
        {connected
          ? <>✅ Your <strong>{label}</strong> calendar is connected — your events sync automatically.</>
          : <>🔗 Connect your <strong>{label}</strong> calendar so your events appear here and MIRA can use them.</>}
      </span>
      {connected ? (
        <button className={styles.iconBtn} disabled={disconnect.isPending} onClick={() => disconnect.mutate(prov)}>
          {disconnect.isPending ? 'Disconnecting…' : 'Disconnect'}
        </button>
      ) : (
        <button className={styles.primaryBtn} disabled={connect.isPending} onClick={() => connect.mutate(prov)}>
          {connect.isPending ? 'Starting…' : `Connect ${label}`}
        </button>
      )}
    </div>
  )
}

// Per-user CalDAV (Nextcloud etc.) — no OAuth, so the user enters their own
// server URL + username + app-password. The server validates by syncing once
// before storing (password encrypted at rest), so a bad credential is rejected
// up front rather than silently failing later.
function CalDavConnectPanel({ connected }: { connected: boolean }) {
  const qc = useQueryClient()
  const [editing, setEditing]   = useState(false)
  const [url, setUrl]           = useState('')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')

  const connect = useMutation({
    mutationFn: () => calendarApi.caldavConnect({ url: url.trim(), username: username.trim(), password }),
    onSuccess:  (d) => {
      toast.success(`Connected — synced ${d.synced} event${d.synced === 1 ? '' : 's'}.`)
      setEditing(false); setPassword('')
      qc.invalidateQueries({ queryKey: ['calendar-oauth-status'] })
      qc.invalidateQueries({ queryKey: ['calendar-events'] })
    },
    onError: (e: unknown) => {
      const msg = (e as { response?: { data?: string } })?.response?.data
      toast.error(typeof msg === 'string' && msg ? msg : 'Could not connect to that CalDAV account.')
    },
  })
  const disconnect = useMutation({
    mutationFn: () => calendarApi.caldavDisconnect(),
    onSuccess:  () => { toast.success('Disconnected.'); qc.invalidateQueries({ queryKey: ['calendar-oauth-status'] }) },
    onError:    () => toast.error('Disconnect failed'),
  })

  if (connected && !editing) {
    return (
      <div className={styles.connectPanel}>
        <span style={{ flex: 1 }}>✅ Your <strong>CalDAV</strong> calendar is connected — your events sync automatically.</span>
        <button className={styles.iconBtn} onClick={() => { setEditing(true); setUrl(''); setUsername('') }}>Update</button>
        <button className={styles.iconBtn} disabled={disconnect.isPending} onClick={() => disconnect.mutate()}>
          {disconnect.isPending ? 'Disconnecting…' : 'Disconnect'}
        </button>
      </div>
    )
  }
  const canSubmit = Boolean(url.trim() && username.trim() && password) && !connect.isPending
  return (
    <div className={`${styles.connectPanel} ${styles.connectForm}`}>
      <span>🔗 Connect your <strong>CalDAV</strong> calendar (e.g. Nextcloud). Use an <strong>app password</strong>, not your login password.</span>
      <input className={styles.connectInput} placeholder="Server URL — e.g. https://cloud.example.com/remote.php/dav/"
             value={url} onChange={(e) => setUrl(e.target.value)} />
      <input className={styles.connectInput} placeholder="Username" autoComplete="username"
             value={username} onChange={(e) => setUsername(e.target.value)} />
      <input className={styles.connectInput} type="password" placeholder="App password" autoComplete="new-password"
             value={password} onChange={(e) => setPassword(e.target.value)} />
      <div className={styles.connectRow}>
        <button className={styles.primaryBtn} disabled={!canSubmit} onClick={() => connect.mutate()}>
          {connect.isPending ? 'Connecting…' : 'Connect'}
        </button>
        {connected && (
          <button className={styles.iconBtn} onClick={() => { setEditing(false); setPassword('') }}>Cancel</button>
        )}
      </div>
    </div>
  )
}

// ── Page ────────────────────────────────────────────────────────────────────

export default function CalendarPage() {
  const qc       = useQueryClient()
  const { user } = useAuthStore()
  const isAdmin  = user?.role === 'admin'
  const [params, setParams] = useSearchParams()

  const targetUserId = params.get('userId') || undefined
  const isViewingOther = !!targetUserId && targetUserId !== user?.id

  const [cursor, setCursor] = useState<Date>(() => startOfMonth(new Date()))
  const [selectedEvent, setSelectedEvent] = useState<CalendarEvent | null>(null)
  const [editorOpen, setEditorOpen] = useState(false)
  const [editorDraft, setEditorDraft] = useState<EventInput | null>(null)
  const [editorEditingId, setEditorEditingId] = useState<string | null>(null)

  // Range to fetch — current visible 6-week grid plus a small overflow.
  const range = useMemo(() => {
    const from = startOfGrid(cursor).getTime()
    const end  = new Date(startOfGrid(cursor))
    end.setDate(end.getDate() + 42)  // 6 weeks
    return { from, to: end.getTime() }
  }, [cursor])

  const { data, isLoading, error } = useQuery({
    queryKey: ['calendar-events', targetUserId ?? user?.id, range.from, range.to],
    queryFn:  () => calendarApi.listEvents({
      from:    range.from,
      to:      range.to,
      limit:   2000,
      user_id: targetUserId,
    }),
    enabled: !!user,
  })

  // Resolve the viewed user's display name, when admin is viewing someone else.
  const { data: viewedUser } = useQuery<User | undefined>({
    queryKey: ['user', targetUserId],
    queryFn:  async () => {
      if (!targetUserId) return undefined
      const all = await api.get<User[]>('/api/users').then((r) => r.data)
      return all.find((u) => u.id === targetUserId)
    },
    enabled: !!targetUserId && isAdmin,
  })

  const events = data?.events ?? []

  // overlay automation fires onto the calendar. We only show the
  // user's own (or, for admin viewing self, system + own) schedules; no
  // cross-user surfacing here. Each schedule contributes its `next_run_at` as
  // a single per-day fire item; recurring schedules show their *next* fire,
  // which is the most user-actionable preview without an extra API.
  const { data: schedules = [] } = useQuery<Schedule[]>({
    queryKey: ['calendar-schedules'],
    queryFn:  () => automationsApi.listSchedules(),
    enabled:  !!user && !isViewingOther,
  })

  const firesByDay = useMemo(() => {
    const map = new Map<string, Schedule[]>()
    for (const s of schedules) {
      if (s.status !== 'active') continue
      if (!s.next_run_at) continue
      const ms = s.next_run_at * 1000
      if (ms < range.from || ms >= range.to) continue
      const key = new Date(ms).toDateString()
      const list = map.get(key) ?? []
      list.push(s)
      map.set(key, list)
    }
    for (const list of map.values()) {
      list.sort((a, b) => (a.next_run_at ?? 0) - (b.next_run_at ?? 0))
    }
    return map
  }, [schedules, range.from, range.to])

  // Agenda overlay (cont.) — also project MIRA's own daily morning briefing onto
  // each upcoming day (read-only), from the user's own briefing settings, so the
  // proactive things MIRA does for you are visible where you already look.
  const { data: briefing } = useQuery<{ enabled: boolean; hour: number }>({
    queryKey: ['my-briefing'],
    queryFn:  () => api.get<{ enabled: boolean; hour: number }>('/api/me/briefing').then((r) => r.data),
    enabled:  !!user && !isViewingOther,
  })
  const briefingsByDay = useMemo(() => {
    const map = new Map<string, number>() // dayKey -> ms the briefing fires
    if (!briefing?.enabled) return map
    const now = Date.now()
    const start = startOfGrid(cursor)
    for (let i = 0; i < 42; i++) {
      const d = new Date(start); d.setDate(start.getDate() + i)
      d.setHours(briefing.hour ?? 8, 0, 0, 0)
      const ms = d.getTime()
      if (ms < now || ms < range.from || ms >= range.to) continue
      map.set(d.toDateString(), ms)
    }
    return map
  }, [briefing, cursor, range.from, range.to])

  // Groups — admins get all (to scope new events), others get their own (to label
  // the group events they can see). Used for the visibility picker + event labels.
  const { data: groups = [] } = useQuery<Group[]>({
    queryKey: ['calendar-groups', isAdmin],
    queryFn:  () => (isAdmin ? groupsApi.list() : groupsApi.listMine()),
    enabled:  !!user,
  })
  const groupName = (id: string) => groups.find((g) => g.id === id)?.name ?? 'group'
  // Visibility of an event from its owner: 'personal' | 'org' | 'group'.
  const isOrg   = (ev: CalendarEvent) => ev.owner_user_id === SHARED_OWNER
  const isGroup = (ev: CalendarEvent) => ev.owner_user_id.startsWith(GROUP_OWNER_PREFIX)

  // Day → events mapping
  const eventsByDay = useMemo(() => {
    const map = new Map<string, CalendarEvent[]>()
    const grid = []
    const start = startOfGrid(cursor)
    for (let i = 0; i < 42; i++) {
      const d = new Date(start)
      d.setDate(start.getDate() + i)
      grid.push(d)
    }
    for (const day of grid) {
      const key  = day.toDateString()
      const list = events.filter((e) => eventTouchesDay(e, day))
      list.sort((a, b) => a.starts_at - b.starts_at)
      map.set(key, list)
    }
    return map
  }, [cursor, events])

  const today = new Date()

  // ── Mutations ─────────────────────────────────────────────────────────────

  const invalidate = () => qc.invalidateQueries({ queryKey: ['calendar-events'] })

  const createMut = useMutation({
    mutationFn: (input: EventInput) => calendarApi.createEvent(input),
    onSuccess:  () => { invalidate(); toast.success('Created'); closeEditor() },
    onError:    (e: any) => toast.error(`Create failed: ${e?.message ?? e}`),
  })
  const updateMut = useMutation({
    mutationFn: ({ id, input }: { id: string; input: EventInput }) =>
      calendarApi.updateEvent(id, input),
    onSuccess: () => { invalidate(); toast.success('Updated'); closeEditor(); setSelectedEvent(null) },
    onError:   (e: any) => toast.error(`Update failed: ${e?.message ?? e}`),
  })
  const deleteMut = useMutation({
    mutationFn: (ev: CalendarEvent) => calendarApi.deleteEvent(ev.id, {
      shared:  ev.owner_user_id === SHARED_OWNER,
      groupId: groupIdFromOwner(ev.owner_user_id),
    }),
    onSuccess:  () => { invalidate(); toast.success('Deleted'); setSelectedEvent(null) },
    onError:    (e: any) => toast.error(`Delete failed: ${e?.message ?? e}`),
  })

  // ── Helpers ───────────────────────────────────────────────────────────────

  const canEdit = (ev: CalendarEvent | null): boolean => {
    if (!ev) return false
    // External (synced) events are read-only on the server; reflect that here.
    if (ev.source !== 'native') return false
    // Admins viewing another user shouldn't mutate their data through this UI.
    if (isViewingOther) return false
    // Shared / org / group events are managed by admins only.
    if ((isOrg(ev) || isGroup(ev)) && !isAdmin) return false
    return true
  }

  const openCreate = (day: Date) => {
    if (isViewingOther) return  // read-only when admin is viewing someone else
    const start = new Date(day)
    start.setHours(9, 0, 0, 0)
    const end = new Date(start)
    end.setHours(10, 0, 0, 0)
    setEditorDraft({
      summary:     '',
      description: '',
      starts_at:   start.getTime(),
      ends_at:     end.getTime(),
      all_day:     false,
      kind:        'event',
      shared:      false,
      group_id:    null,
    })
    setEditorEditingId(null)
    setEditorOpen(true)
  }

  const openEdit = (ev: CalendarEvent) => {
    setEditorDraft({
      summary:     ev.summary,
      description: ev.description ?? '',
      starts_at:   ev.starts_at,
      ends_at:     ev.ends_at,
      all_day:     ev.all_day,
      location:    ev.location ?? '',
      rrule:       ev.rrule ?? '',
      status:      ev.status ?? '',
      kind:        ev.kind,
      shared:      ev.owner_user_id === SHARED_OWNER,
      group_id:    groupIdFromOwner(ev.owner_user_id),
    })
    setEditorEditingId(ev.id)
    setEditorOpen(true)
    setSelectedEvent(null)
  }

  const closeEditor = () => {
    setEditorOpen(false)
    setEditorDraft(null)
    setEditorEditingId(null)
  }

  const submitEditor = () => {
    if (!editorDraft) return
    if (!editorDraft.summary.trim()) {
      toast.error('Summary is required')
      return
    }
    if (editorDraft.ends_at < editorDraft.starts_at) {
      toast.error('End must be after start')
      return
    }
    if (editorEditingId) {
      updateMut.mutate({ id: editorEditingId, input: editorDraft })
    } else {
      createMut.mutate(editorDraft)
    }
  }

  // Reset cursor when switching user
  useEffect(() => { setCursor(startOfMonth(new Date())) }, [targetUserId])

  // ── Render ────────────────────────────────────────────────────────────────

  const grid: Date[] = []
  const gridStart = startOfGrid(cursor)
  for (let i = 0; i < 42; i++) {
    const d = new Date(gridStart)
    d.setDate(gridStart.getDate() + i)
    grid.push(d)
  }

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1>
            <CalendarIcon size={18} style={{ verticalAlign: 'middle', marginRight: 6 }} />
            Calendar
          </h1>
          <p>Events and notes. Click any day to add — click an item to view details.</p>
        </div>
        <div className={styles.headerActions}>
          <button
            className={styles.iconBtn}
            onClick={() => setCursor(startOfMonth(new Date()))}
            title="Jump to today"
          >
            Today
          </button>
          {!isViewingOther && (
            <button className={styles.primaryBtn} onClick={() => openCreate(new Date())}>
              <Plus size={14} /> New
            </button>
          )}
        </div>
      </div>

      {isViewingOther && (
        <div className={styles.viewingBanner}>
          <Eye size={14} />
          Viewing {viewedUser?.display_name || viewedUser?.username || targetUserId}'s
          calendar (read-only)
          <button
            className={styles.iconBtn}
            style={{ marginLeft: 'auto' }}
            onClick={() => { params.delete('userId'); setParams(params, { replace: true }) }}
          >
            Back to mine
          </button>
        </div>
      )}

      {!isViewingOther && <CalendarConnectPanel />}

      {error && <div className={styles.error}>{(error as Error).message}</div>}

      <div className={styles.toolbar}>
        <div className={styles.monthNav}>
          <button
            className={styles.iconBtn}
            onClick={() => {
              const next = new Date(cursor)
              next.setMonth(cursor.getMonth() - 1)
              setCursor(next)
            }}
            aria-label="Previous month"
          >
            <ChevronLeft size={14} />
          </button>
          <span className={styles.monthLabel}>{fmtMonth(cursor)}</span>
          <button
            className={styles.iconBtn}
            onClick={() => {
              const next = new Date(cursor)
              next.setMonth(cursor.getMonth() + 1)
              setCursor(next)
            }}
            aria-label="Next month"
          >
            <ChevronRight size={14} />
          </button>
        </div>
        <div style={{ fontSize: 12, color: 'var(--text-muted)' }}>
          {isLoading ? 'Loading…' : `${events.length} item${events.length === 1 ? '' : 's'}`}
        </div>
      </div>

      <div className={styles.gridWrap}>
        <div className={styles.weekHeader}>
          {WEEK_DAYS.map((d) => <div key={d} className={styles.weekDay}>{d}</div>)}
        </div>
        <div className={styles.grid}>
          {grid.map((day) => {
            const inMonth = day.getMonth() === cursor.getMonth()
            const items   = eventsByDay.get(day.toDateString()) ?? []
            const visible = items.slice(0, 3)
            const more    = items.length - visible.length
            return (
              <div
                key={day.toISOString()}
                className={[
                  styles.cell,
                  inMonth ? '' : styles.cellOtherMonth,
                  isSameDay(day, today) ? styles.cellToday : '',
                ].filter(Boolean).join(' ')}
                onClick={() => openCreate(day)}
              >
                <div className={styles.dateNum}>{day.getDate()}</div>
                <div className={styles.itemList}>
                  {visible.map((ev) => {
                    const org = isOrg(ev), grp = isGroup(ev)
                    const scoped = org || grp
                    const scopeLabel = org ? 'organization' : grp ? `group: ${groupName(groupIdFromOwner(ev.owner_user_id)!)}` : ''
                    return (
                    <div
                      key={ev.id}
                      className={[
                        styles.item,
                        ev.kind === 'note' ? styles.itemNote : '',
                        ev.source !== 'native' ? styles.itemExternal : '',
                        scoped ? styles.itemShared : '',
                      ].filter(Boolean).join(' ')}
                      onClick={(e) => { e.stopPropagation(); setSelectedEvent(ev) }}
                      title={scoped ? `${ev.summary} (${scopeLabel})` : ev.summary}
                    >
                      {scoped && <Users size={9} style={{ opacity: 0.85, flexShrink: 0 }} />}
                      {!ev.all_day && (
                        <span style={{ fontSize: 9, opacity: 0.7 }}>
                          {new Date(ev.starts_at).toLocaleTimeString(undefined,
                            { hour: '2-digit', minute: '2-digit' })}
                        </span>
                      )}
                      {ev.summary}
                    </div>
                  )})}
                  {(firesByDay.get(day.toDateString()) ?? []).map((s) => (
                    <Link
                      key={`fire-${s.id}`}
                      to="/automations"
                      className={`${styles.item} ${styles.itemAutomation}`}
                      onClick={(e) => e.stopPropagation()}
                      title={`${s.name} — fires ${new Date((s.next_run_at ?? 0) * 1000).toLocaleString()}`}
                    >
                      <Bot size={9} style={{ opacity: 0.8 }} />
                      <span style={{ fontSize: 9, opacity: 0.7 }}>
                        {new Date((s.next_run_at ?? 0) * 1000).toLocaleTimeString(undefined,
                          { hour: '2-digit', minute: '2-digit' })}
                      </span>
                      {s.name}
                    </Link>
                  ))}
                  {briefingsByDay.get(day.toDateString()) != null && (
                    <div
                      className={`${styles.item} ${styles.itemBriefing}`}
                      onClick={(e) => e.stopPropagation()}
                      title={`MIRA will send your morning briefing at ${new Date(briefingsByDay.get(day.toDateString())!).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })}`}
                    >
                      <Sunrise size={9} style={{ opacity: 0.85 }} />
                      <span style={{ fontSize: 9, opacity: 0.7 }}>
                        {new Date(briefingsByDay.get(day.toDateString())!).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })}
                      </span>
                      Briefing
                    </div>
                  )}
                  {more > 0 && <div className={styles.itemMore}>+{more} more</div>}
                </div>
              </div>
            )
          })}
        </div>
      </div>

      {/* ── Detail dialog ── */}
      {selectedEvent && (
        <div className={styles.modalBackdrop} onClick={() => setSelectedEvent(null)}>
          <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
            <div className={styles.modalHeader}>
              <h2>{selectedEvent.summary}</h2>
              <button className={styles.closeBtn} onClick={() => setSelectedEvent(null)}>
                <X size={16} />
              </button>
            </div>
            <div className={styles.detailLine}>
              <strong>Kind</strong>
              <span style={{ textTransform: 'capitalize' }}>{selectedEvent.kind}</span>
            </div>
            <div className={styles.detailLine}>
              <strong>Starts</strong>
              <span>{fmtDateTime(selectedEvent.starts_at, selectedEvent.all_day)}</span>
            </div>
            <div className={styles.detailLine}>
              <strong>Ends</strong>
              <span>{fmtDateTime(selectedEvent.ends_at, selectedEvent.all_day)}</span>
            </div>
            {selectedEvent.location && (
              <div className={styles.detailLine}>
                <strong>Location</strong>
                <span>{selectedEvent.location}</span>
              </div>
            )}
            {selectedEvent.description && (
              <div className={styles.detailLine}>
                <strong>Notes</strong>
                <span style={{ whiteSpace: 'pre-wrap' }}>{selectedEvent.description}</span>
              </div>
            )}
            <div className={styles.detailLine}>
              <strong>Source</strong>
              <span className={styles.sourceTag}>
                {isOrg(selectedEvent) ? 'organization'
                  : isGroup(selectedEvent) ? `group: ${groupName(groupIdFromOwner(selectedEvent.owner_user_id)!)}`
                  : selectedEvent.source}
              </span>
            </div>
            <div className={styles.modalActions}>
              <div>
                {canEdit(selectedEvent) && (
                  <button
                    className={styles.dangerBtn}
                    onClick={() => deleteMut.mutate(selectedEvent)}
                    disabled={deleteMut.isPending}
                  >
                    <Trash2 size={12} style={{ marginRight: 4 }} /> Delete
                  </button>
                )}
              </div>
              <div className={styles.modalActionsRight}>
                <button className={styles.iconBtn} onClick={() => setSelectedEvent(null)}>
                  Close
                </button>
                {canEdit(selectedEvent) && (
                  <button className={styles.primaryBtn} onClick={() => openEdit(selectedEvent)}>
                    Edit
                  </button>
                )}
              </div>
            </div>
          </div>
        </div>
      )}

      {/* ── Editor dialog ── */}
      {editorOpen && editorDraft && (
        <div className={styles.modalBackdrop} onClick={closeEditor}>
          <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
            <div className={styles.modalHeader}>
              <h2>{editorEditingId ? 'Edit' : 'New'} {editorDraft.kind === 'note' ? 'note' : 'event'}</h2>
              <button className={styles.closeBtn} onClick={closeEditor}>
                <X size={16} />
              </button>
            </div>

            <div className={styles.formField}>
              <label>Summary</label>
              <input
                value={editorDraft.summary}
                onChange={(e) => setEditorDraft({ ...editorDraft, summary: e.target.value })}
                placeholder="Lunch with Annika"
                autoFocus
              />
            </div>

            <div className={styles.formRow}>
              <div className={styles.formField}>
                <label>Kind</label>
                <select
                  value={editorDraft.kind ?? 'event'}
                  onChange={(e) =>
                    setEditorDraft({ ...editorDraft, kind: e.target.value as EventKind })
                  }
                >
                  <option value="event">Event</option>
                  <option value="note">Note</option>
                </select>
              </div>
              <div className={styles.formField}>
                <label>Status</label>
                <select
                  value={editorDraft.status ?? ''}
                  onChange={(e) =>
                    setEditorDraft({ ...editorDraft, status: e.target.value || null })
                  }
                >
                  <option value="">—</option>
                  <option value="tentative">Tentative</option>
                  <option value="confirmed">Confirmed</option>
                  <option value="cancelled">Cancelled</option>
                </select>
              </div>
            </div>

            <div className={styles.checkboxRow}>
              <input
                type="checkbox"
                id="all-day"
                checked={!!editorDraft.all_day}
                onChange={(e) => setEditorDraft({ ...editorDraft, all_day: e.target.checked })}
              />
              <label htmlFor="all-day">All day</label>
            </div>

            {isAdmin && (
              <div className={styles.formField}>
                <label>
                  <Users size={12} style={{ verticalAlign: 'text-bottom', marginRight: 4 }} />
                  Visibility
                </label>
                <select
                  value={editorDraft.group_id ? `group:${editorDraft.group_id}` : editorDraft.shared ? 'org' : 'personal'}
                  onChange={(e) => {
                    const v = e.target.value
                    if (v === 'personal')  setEditorDraft({ ...editorDraft, shared: false, group_id: null })
                    else if (v === 'org')  setEditorDraft({ ...editorDraft, shared: true,  group_id: null })
                    else                   setEditorDraft({ ...editorDraft, shared: false, group_id: v.slice('group:'.length) })
                  }}
                >
                  <option value="personal">Just me (personal)</option>
                  <option value="org">Everyone — organization event</option>
                  {groups.map((g) => (
                    <option key={g.id} value={`group:${g.id}`}>Group: {g.name}</option>
                  ))}
                </select>
              </div>
            )}

            <div className={styles.formRow}>
              <div className={styles.formField}>
                <label>Starts</label>
                <input
                  type="datetime-local"
                  value={toLocalInput(editorDraft.starts_at)}
                  onChange={(e) =>
                    setEditorDraft({ ...editorDraft, starts_at: fromLocalInput(e.target.value) })
                  }
                />
              </div>
              <div className={styles.formField}>
                <label>Ends</label>
                <input
                  type="datetime-local"
                  value={toLocalInput(editorDraft.ends_at)}
                  onChange={(e) =>
                    setEditorDraft({ ...editorDraft, ends_at: fromLocalInput(e.target.value) })
                  }
                />
              </div>
            </div>

            <div className={styles.formField}>
              <label>Location</label>
              <input
                value={editorDraft.location ?? ''}
                onChange={(e) =>
                  setEditorDraft({ ...editorDraft, location: e.target.value || null })
                }
                placeholder="(optional)"
              />
            </div>

            <div className={styles.formField}>
              <label>Notes</label>
              <textarea
                value={editorDraft.description ?? ''}
                onChange={(e) =>
                  setEditorDraft({ ...editorDraft, description: e.target.value || null })
                }
                placeholder="(optional)"
              />
            </div>

            <div className={styles.modalActions}>
              <div />
              <div className={styles.modalActionsRight}>
                <button className={styles.iconBtn} onClick={closeEditor}>Cancel</button>
                <button
                  className={styles.primaryBtn}
                  onClick={submitEditor}
                  disabled={createMut.isPending || updateMut.isPending}
                >
                  {editorEditingId ? 'Save' : 'Create'}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {!isLoading && events.length === 0 && (
        <div className={styles.empty}>
          No events in {fmtMonth(cursor)}.
          {!isViewingOther && ' Click any day to add one.'}
        </div>
      )}
    </div>
  )
}
