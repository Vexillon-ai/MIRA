// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/api/automations.ts
//
// TypeScript client for the  automations HTTP surface.
// Mirrors `src/server/handlers/automations.rs` — DTO shapes intentionally
// match `serde_json` output so `axios` returns ready-to-render objects.

import { api } from './client'

// ── Enums / unions ──────────────────────────────────────────────────────────

export type OwnerKind = 'user' | 'agent' | 'system'

export type ScheduleStatus =
  | 'active'
  | 'paused'
  | 'pending_approval'
  | 'expired'
  | 'failed'

export type ConversationStrategy = 'existing' | 'new' | 'named'

// ── Trigger ─────────────────────────────────────────────────────────────────

export type TriggerSpec =
  | { kind: 'one_off';  at: number }
  | { kind: 'interval'; every_secs: number }
  | { kind: 'cron';     expr: string }

// ── Action ──────────────────────────────────────────────────────────────────

export interface PromptAction {
  conversation_strategy: ConversationStrategy
  conversation_id?:      string | null
  conversation_name?:    string | null
  channel:               string
  prompt:                string
  tools_allowed?:        string[] | null
  max_iterations?:       number
}

export type Action =
  | ({ kind: 'prompt' } & PromptAction)
  | { kind: 'tool_call'; tool: string; args: unknown }
  | { kind: 'internal';  task: string; args?: unknown }
  | {
      kind:           'http_post'
      url:            string
      headers?:       Record<string, string>
      body_template:  string
      timeout_secs?:  number
    }
  | {
      kind:           'channel_message'
      channel:        string
      to?:            string | null
      text_template:  string
    }

// ── Quiet hours ─────────────────────────────────────────────────────────────

export interface QuietHours {
  start: string  // HH:MM
  end:   string  // HH:MM
}

// ── Schedule ────────────────────────────────────────────────────────────────

export interface Schedule {
  id:             string
  user_id:        string
  owner_kind:     OwnerKind
  name:           string
  description:    string | null
  rationale:      string | null
  trigger:        TriggerSpec
  timezone:       string
  quiet_hours:    QuietHours | null
  action:         Action
  status:         ScheduleStatus
  created_at:     number
  expires_at:     number | null
  last_run_at:    number | null
  next_run_at:    number | null
  run_count:      number
  failure_count:  number
  max_failures:   number
  last_error:     string | null
}

export interface CreateScheduleRequest {
  name:         string
  description?: string | null
  rationale?:   string | null
  trigger:      TriggerSpec
  timezone?:    string
  quiet_hours?: QuietHours | null
  action:       Action
  expires_at?:  number | null
}

export type UpdateScheduleRequest = CreateScheduleRequest

export interface SnoozeRequest {
  /** Unix seconds. Server clamps to ≥ now. */
  until: number
}

// ── Runs ────────────────────────────────────────────────────────────────────

export type RunOutcome = 'success' | 'failure' | 'skipped' | 'coalesced'

// Matches server `crate::automations::AutomationRun`. Field names are wire
// names — keep snake_case to align with the JSON without a transform layer.
export interface AutomationRun {
  id:             string
  source_kind:    string  // "schedule" | "webhook" | "event" | "dead_letter"
  source_id:      string
  user_id:        string
  started_at:     number
  finished_at:    number | null
  outcome:        RunOutcome
  output_snippet: string | null
  error:          string | null
  context:        string | null
}

// ── Responses ───────────────────────────────────────────────────────────────

interface SchedulesListResponse { schedules: Schedule[] }
interface NextFiresResponse { next_fires: number[] }
interface RunsListResponse { runs: AutomationRun[] }
interface AutomationsListResponse {
  items: ({ kind: 'schedule' } & Schedule)[]
}

// ── Webhooks ──────────────────────────────────────────────────────

export type AutomationStatus =
  | 'active'
  | 'paused'
  | 'pending_approval'
  | 'expired'
  | 'failed'

export interface Webhook {
  id:                  string
  user_id:             string
  owner_kind:          OwnerKind
  name:                string
  description:         string | null
  rationale:           string | null
  token:               string
  /** One-time on create + rotate-secret. Read responses omit this field. */
  secret?:             string
  predicate:           unknown | null
  payload_template:    string | null
  action:              Action
  rate_limit_per_min:  number
  debounce_secs:       number | null
  status:              AutomationStatus
  created_at:          number
  expires_at:          number | null
  last_seen_at:        number | null
  last_error:          string | null
}

export interface CreateWebhookRequest {
  name:                string
  description?:        string | null
  rationale?:          string | null
  predicate?:          unknown | null
  payload_template?:   string | null
  action:              Action
  rate_limit_per_min?: number | null
  debounce_secs?:      number | null
  expires_at?:         number | null
}

export type UpdateWebhookRequest = CreateWebhookRequest

export interface WebhookPayload {
  id:           number
  webhook_id:   string
  received_at:  number
  headers_json: string
  body:         string
  matched:      boolean
}

interface WebhooksListResponse { webhooks: Webhook[] }
interface PayloadsListResponse { payloads: WebhookPayload[] }

// ── Event subscriptions ───────────────────────────────────────────

export interface EventSubscription {
  id:            string
  user_id:       string
  owner_kind:    OwnerKind
  name:          string
  description:   string | null
  rationale:     string | null
  event_name:    string
  predicate:     unknown | null
  action:        Action
  status:        AutomationStatus
  created_at:    number
  expires_at:    number | null
  last_fired_at: number | null
  last_error:    string | null
}

export interface CreateSubscriptionRequest {
  name:         string
  description?: string | null
  rationale?:   string | null
  event_name:   string
  predicate?:   unknown | null
  action:       Action
  expires_at?:  number | null
}

export type UpdateSubscriptionRequest = CreateSubscriptionRequest

interface SubscriptionsListResponse { subscriptions: EventSubscription[] }

interface EventNamesResponse { names: string[] }

// ── API ─────────────────────────────────────────────────────────────────────

export const automationsApi = {
  listSchedules(userId?: string): Promise<Schedule[]> {
    const params = userId ? { user_id: userId } : undefined
    return api
      .get<SchedulesListResponse>('/api/schedules', { params })
      .then((r) => r.data.schedules)
  },
  createSchedule(req: CreateScheduleRequest): Promise<Schedule> {
    return api.post<Schedule>('/api/schedules', req).then((r) => r.data)
  },
  getSchedule(id: string): Promise<Schedule> {
    return api
      .get<Schedule>(`/api/schedules/${encodeURIComponent(id)}`)
      .then((r) => r.data)
  },
  updateSchedule(id: string, req: UpdateScheduleRequest): Promise<Schedule> {
    return api
      .put<Schedule>(`/api/schedules/${encodeURIComponent(id)}`, req)
      .then((r) => r.data)
  },
  deleteSchedule(id: string): Promise<void> {
    return api
      .delete(`/api/schedules/${encodeURIComponent(id)}`)
      .then(() => undefined)
  },
  nextFires(id: string, n = 5): Promise<number[]> {
    return api
      .get<NextFiresResponse>(`/api/schedules/${encodeURIComponent(id)}/next-fires`, {
        params: { n },
      })
      .then((r) => r.data.next_fires)
  },
  runNow(id: string): Promise<Schedule> {
    return api
      .post<Schedule>(`/api/schedules/${encodeURIComponent(id)}/run-now`)
      .then((r) => r.data)
  },
  pause(id: string): Promise<Schedule> {
    return api
      .post<Schedule>(`/api/schedules/${encodeURIComponent(id)}/pause`)
      .then((r) => r.data)
  },
  resume(id: string): Promise<Schedule> {
    return api
      .post<Schedule>(`/api/schedules/${encodeURIComponent(id)}/resume`)
      .then((r) => r.data)
  },
  approveSchedule(id: string): Promise<Schedule> {
    return api
      .post<Schedule>(`/api/schedules/${encodeURIComponent(id)}/approve`)
      .then((r) => r.data)
  },
  rejectSchedule(id: string): Promise<void> {
    return api
      .post(`/api/schedules/${encodeURIComponent(id)}/reject`)
      .then(() => undefined)
  },
  snooze(id: string, until: number): Promise<Schedule> {
    return api
      .post<Schedule>(`/api/schedules/${encodeURIComponent(id)}/snooze`, { until })
      .then((r) => r.data)
  },
  listRuns(params: {
    source?:  string
    id?:      string
    outcome?: RunOutcome
    // Cursor: only runs with `started_at < before`. 
    before?:  number
    limit?:   number
  } = {}): Promise<AutomationRun[]> {
    return api
      .get<RunsListResponse>('/api/automations/runs', { params })
      .then((r) => r.data.runs)
  },
  listAutomations(): Promise<AutomationsListResponse['items']> {
    return api
      .get<AutomationsListResponse>('/api/automations')
      .then((r) => r.data.items)
  },

  // ── Webhooks ────────────────────────────────────────────────────────────
  listWebhooks(userId?: string): Promise<Webhook[]> {
    const params = userId ? { user_id: userId } : undefined
    return api
      .get<WebhooksListResponse>('/api/webhooks', { params })
      .then((r) => r.data.webhooks)
  },
  createWebhook(req: CreateWebhookRequest): Promise<Webhook> {
    return api.post<Webhook>('/api/webhooks', req).then((r) => r.data)
  },
  getWebhook(id: string): Promise<Webhook> {
    return api.get<Webhook>(`/api/webhooks/${encodeURIComponent(id)}`).then((r) => r.data)
  },
  updateWebhook(id: string, req: UpdateWebhookRequest): Promise<Webhook> {
    return api.put<Webhook>(`/api/webhooks/${encodeURIComponent(id)}`, req).then((r) => r.data)
  },
  deleteWebhook(id: string): Promise<void> {
    return api.delete(`/api/webhooks/${encodeURIComponent(id)}`).then(() => undefined)
  },
  pauseWebhook(id: string): Promise<Webhook> {
    return api.post<Webhook>(`/api/webhooks/${encodeURIComponent(id)}/pause`).then((r) => r.data)
  },
  resumeWebhook(id: string): Promise<Webhook> {
    return api.post<Webhook>(`/api/webhooks/${encodeURIComponent(id)}/resume`).then((r) => r.data)
  },
  approveWebhook(id: string): Promise<Webhook> {
    return api.post<Webhook>(`/api/webhooks/${encodeURIComponent(id)}/approve`).then((r) => r.data)
  },
  rejectWebhook(id: string): Promise<void> {
    return api.post(`/api/webhooks/${encodeURIComponent(id)}/reject`).then(() => undefined)
  },
  rotateWebhookToken(id: string): Promise<Webhook> {
    return api.post<Webhook>(`/api/webhooks/${encodeURIComponent(id)}/rotate-token`).then((r) => r.data)
  },
  rotateWebhookSecret(id: string): Promise<Webhook> {
    return api.post<Webhook>(`/api/webhooks/${encodeURIComponent(id)}/rotate-secret`).then((r) => r.data)
  },
  listWebhookPayloads(id: string, limit = 20): Promise<WebhookPayload[]> {
    return api
      .get<PayloadsListResponse>(`/api/webhooks/${encodeURIComponent(id)}/payloads`, { params: { limit } })
      .then((r) => r.data.payloads)
  },
  testReplayWebhook(id: string, payloadId: number): Promise<void> {
    return api
      .post(`/api/webhooks/${encodeURIComponent(id)}/test`, { payload_id: payloadId })
      .then(() => undefined)
  },
  webhookUrl(id: string): Promise<{ path: string }> {
    return api
      .get<{ path: string }>(`/api/webhooks/${encodeURIComponent(id)}/url`)
      .then((r) => r.data)
  },

  // ── Event subscriptions ─────────────────────────────────────────────────
  listSubscriptions(userId?: string): Promise<EventSubscription[]> {
    const params = userId ? { user_id: userId } : undefined
    return api
      .get<SubscriptionsListResponse>('/api/event-subscriptions', { params })
      .then((r) => r.data.subscriptions)
  },
  createSubscription(req: CreateSubscriptionRequest): Promise<EventSubscription> {
    return api.post<EventSubscription>('/api/event-subscriptions', req).then((r) => r.data)
  },
  getSubscription(id: string): Promise<EventSubscription> {
    return api.get<EventSubscription>(`/api/event-subscriptions/${encodeURIComponent(id)}`).then((r) => r.data)
  },
  updateSubscription(id: string, req: UpdateSubscriptionRequest): Promise<EventSubscription> {
    return api.put<EventSubscription>(`/api/event-subscriptions/${encodeURIComponent(id)}`, req).then((r) => r.data)
  },
  deleteSubscription(id: string): Promise<void> {
    return api.delete(`/api/event-subscriptions/${encodeURIComponent(id)}`).then(() => undefined)
  },
  pauseSubscription(id: string): Promise<EventSubscription> {
    return api.post<EventSubscription>(`/api/event-subscriptions/${encodeURIComponent(id)}/pause`).then((r) => r.data)
  },
  resumeSubscription(id: string): Promise<EventSubscription> {
    return api.post<EventSubscription>(`/api/event-subscriptions/${encodeURIComponent(id)}/resume`).then((r) => r.data)
  },
  approveSubscription(id: string): Promise<EventSubscription> {
    return api.post<EventSubscription>(`/api/event-subscriptions/${encodeURIComponent(id)}/approve`).then((r) => r.data)
  },
  rejectSubscription(id: string): Promise<void> {
    return api.post(`/api/event-subscriptions/${encodeURIComponent(id)}/reject`).then(() => undefined)
  },
  listEventNames(): Promise<string[]> {
    return api.get<EventNamesResponse>('/api/events/names').then((r) => r.data.names)
  },
  testEmitEvent(name: string, payload: unknown): Promise<void> {
    return api
      .post('/api/events/test', { event_name: name, payload })
      .then(() => undefined)
  },
}
