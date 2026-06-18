// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

// ── Types ───────────────────────────────────────────────────────────────────

export type EventSource = 'native' | 'caldav' | 'google' | 'outlook'
export type EventKind   = 'event' | 'note'

export interface CalendarEvent {
  id:              string
  owner_user_id:   string
  summary:         string
  description?:    string | null
  starts_at:       number
  ends_at:         number
  all_day:         boolean
  location?:       string | null
  rrule?:          string | null
  status?:         string | null
  source:          EventSource
  kind:            EventKind
  external_id?:    string | null
  last_synced_at?: number | null
  created_at:      number
  updated_at:      number
}

export interface EventInput {
  summary:      string
  description?: string | null
  starts_at:    number
  ends_at:      number
  all_day?:     boolean
  location?:    string | null
  rrule?:       string | null
  status?:      string | null
  kind?:        EventKind
  /** Admin-only: create/update under the shared org owner so everyone sees it. */
  shared?:      boolean
  /** Admin-only: scope to a group (only its members see it). Takes precedence over `shared`. */
  group_id?:    string | null
}

/** Owner sentinel for org-wide shared events (mirrors store::SHARED_OWNER). */
export const SHARED_OWNER = '__org__'
/** Owner prefix for group-scoped events: `grp:<group_id>` (mirrors store::GROUP_OWNER_PREFIX). */
export const GROUP_OWNER_PREFIX = 'grp:'
/** Extract the group id from a `grp:<id>` owner, or null. */
export const groupIdFromOwner = (owner: string): string | null =>
  owner.startsWith(GROUP_OWNER_PREFIX) ? owner.slice(GROUP_OWNER_PREFIX.length) : null

export interface ListEventsParams {
  from?:    number
  to?:      number
  limit?:   number
  /** Admin-only override — view another user's calendar. */
  user_id?: string
}

export interface EventListResponse {
  events: CalendarEvent[]
}

export interface SyncResponse {
  provider: string
  pulled:   number
}

export interface OAuthStartResponse {
  authorize_url: string
}

export interface OAuthStatusResponse {
  google_connected:   boolean
  outlook_connected:  boolean
  caldav_connected:   boolean
  google_configured:  boolean
  outlook_configured: boolean
  sync_provider:      string   // "none" | "caldav" | "google" | "outlook"
  sync_enabled:       boolean
}

export interface CalDavConnectInput {
  url:      string
  username: string
  password: string
}

// ── API ─────────────────────────────────────────────────────────────────────

export const calendarApi = {
  listEvents(params: ListEventsParams = {}): Promise<EventListResponse> {
    return api
      .get<EventListResponse>('/api/calendar/events', { params })
      .then((r) => r.data)
  },
  createEvent(input: EventInput): Promise<CalendarEvent> {
    return api.post<CalendarEvent>('/api/calendar/events', input).then((r) => r.data)
  },
  getEvent(id: string, userId?: string): Promise<CalendarEvent> {
    const params = userId ? { user_id: userId } : undefined
    return api
      .get<CalendarEvent>(`/api/calendar/events/${encodeURIComponent(id)}`, { params })
      .then((r) => r.data)
  },
  updateEvent(id: string, input: EventInput): Promise<CalendarEvent> {
    return api
      .put<CalendarEvent>(`/api/calendar/events/${encodeURIComponent(id)}`, input)
      .then((r) => r.data)
  },
  deleteEvent(id: string, opts?: { shared?: boolean; groupId?: string | null }): Promise<void> {
    const params: Record<string, unknown> = {}
    if (opts?.shared) params.shared = true
    if (opts?.groupId) params.group_id = opts.groupId
    return api
      .delete(`/api/calendar/events/${encodeURIComponent(id)}`, { params })
      .then(() => undefined)
  },
  triggerSync(): Promise<SyncResponse> {
    return api.post<SyncResponse>('/api/calendar/sync').then((r) => r.data)
  },
  oauthStart(provider: 'google' | 'outlook'): Promise<OAuthStartResponse> {
    return api
      .post<OAuthStartResponse>('/api/calendar/oauth/start', null, { params: { provider } })
      .then((r) => r.data)
  },
  oauthStatus(): Promise<OAuthStatusResponse> {
    return api.get<OAuthStatusResponse>('/api/calendar/oauth/status').then((r) => r.data)
  },
  oauthDisconnect(provider: 'google' | 'outlook'): Promise<void> {
    return api
      .post('/api/calendar/oauth/disconnect', null, { params: { provider } })
      .then(() => undefined)
  },
  // Per-user CalDAV (Nextcloud etc.) — validated server-side by an immediate sync.
  caldavConnect(input: CalDavConnectInput): Promise<{ synced: number }> {
    return api.post<{ synced: number }>('/api/calendar/caldav', input).then((r) => r.data)
  },
  caldavDisconnect(): Promise<void> {
    return api.post('/api/calendar/caldav/disconnect').then(() => undefined)
  },
}
