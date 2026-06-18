// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

// Wire-type matching src/server/handlers/channel_accounts.rs.
export type ChannelKind = 'signal' | 'telegram' | 'discord' | 'matrix' | 'whatsapp' | 'slack' | 'external'

export interface SignalAccountConfig {
  phone_number: string
  cli_binary?: string
  data_dir?: string
  // Assigned by the server on first daemon launch. Omit on create.
  rest_port?: number | null
  hmac_key?: string | null
}

export interface TelegramAccountConfig {
  bot_token: string
  // "webhook" (default) or "polling".
  mode?: string
  secret_token?: string | null
  // Long-poll hold time (seconds) — only meaningful in polling mode.
  // Defaults to 30 server-side; the UI surfaces a numeric input that
  // only shows when mode === "polling".
  poll_timeout_secs?: number
}

export interface DiscordAccountConfig {
  /** Bot token from Discord Developer Portal → Bot → Reset Token. */
  bot_token: string
  /** Application snowflake from "General Information". Used to skip our
   *  own MESSAGE_CREATE echoes immediately on first message; optional
   *  because we also cache it from the READY event. */
  application_id?: string | null
  /** When true, only respond to messages that @-mention the bot. */
  mention_only?: boolean
}

export interface MatrixAccountConfig {
  /** Homeserver base URL, e.g. "https://matrix.org". */
  homeserver: string
  /** Long-lived access token (Element → Settings → Help & About →
   *  Advanced → Access Token, or one minted via /login). */
  access_token: string
  /** When true, only respond to messages that mention the bot. */
  mention_only?: boolean
}

export interface WhatsAppAccountConfig {
  /** Cloud API phone-number id (Meta app → WhatsApp → API Setup). */
  phone_number_id: string
  /** Permanent access token with whatsapp_business_messaging. */
  access_token: string
  /** App secret for inbound webhook signature verification. Optional but
   *  strongly recommended. */
  app_secret?: string | null
  /** Token MIRA echoes back during the webhook GET handshake — set the
   *  same value in Meta's webhook config. */
  verify_token: string
  /** When true, only respond to messages containing "mira" (group chats). */
  mention_only?: boolean
}

export interface SlackAccountConfig {
  /** Bot User OAuth token (xoxb-…) — needs at least chat:write. */
  bot_token: string
  /** App signing secret (Basic Information → App Credentials). */
  signing_secret: string
  /** When true, only respond to messages containing "mira". */
  mention_only?: boolean
}

export interface ExternalAccountConfig {
  /** Provider slug, e.g. "nctalk" — namespaces the external:<kind> channel. */
  provider_kind: string
  /** Where MIRA POSTs outbound replies (the provider's CPP endpoint). */
  send_url: string
  /** HMAC secret the provider signs inbound webhooks with. Auto-generated
   *  by MIRA on create (shown once, redacted thereafter). */
  inbound_secret?: string
  /** HMAC secret MIRA signs outbound calls with. Auto-generated. */
  outbound_secret?: string
  /** When true, only respond to messages containing "mira". */
  mention_only?: boolean
  /** When true, the provider can play synthesized audio — MIRA offers
   *  voice for this external:<kind> channel and attaches audio on outbound
   *  CPP calls (subject to the user's per-channel voice policy). */
  supports_voice?: boolean
}

export type AnyChannelConfig =
  | SignalAccountConfig
  | TelegramAccountConfig
  | DiscordAccountConfig
  | MatrixAccountConfig
  | WhatsAppAccountConfig
  | SlackAccountConfig
  | ExternalAccountConfig

/** R1+R2 routing mode — how an inbound message picks the MIRA user the
 *  agent runs as. 'personal' = always the bot owner; 'shared' = look the
 *  sender up in the identity table (must link first); 'guest_ok' = like
 *  shared but unlinked senders get a guest session. */
export type RoutingMode = 'personal' | 'shared' | 'guest_ok'

export interface ChannelAccount {
  id: string
  user_id: string
  channel: ChannelKind
  account_label: string
  external_id: string | null
  enabled: boolean
  routing_mode: RoutingMode
  created_at: number
  updated_at: number
  // Typed blob — shape depends on channel. Secrets are redacted server-side.
  config: AnyChannelConfig
}

export interface CreateChannelAccountRequest {
  channel: ChannelKind
  account_label: string
  external_id?: string | null
  enabled?: boolean
  routing_mode?: RoutingMode
  config: AnyChannelConfig
}

export interface UpdateChannelAccountRequest {
  account_label?: string
  external_id?: string | null
  enabled?: boolean
  routing_mode?: RoutingMode
  config?: AnyChannelConfig
}

/** One row in the response of GET /api/channel-accounts/health.
 *  Mirrors `AccountHealth` in src/server/handlers/channel_accounts.rs. */
export interface AccountHealth {
  account_id: string
  channel:    ChannelKind
  alive:      boolean
  /** Probe latency in ms. Absent for telegram + discord (no cheap probe;
   *  liveness comes from the long-lived runtime task on the gateway). */
  latency_ms?: number
  /** One-line reason when `alive === false`. */
  error?:      string
}

export const channelAccountsApi = {
  list: () =>
    api.get<ChannelAccount[]>('/api/channel-accounts').then((r) => r.data),

  /** Per-account daemon liveness. Probes Signal REST, treats Telegram
   *  as alive when enabled. Cheap (~ms each, parallel). Poll from the
   *  UI every few seconds for a live badge. */
  health: () =>
    api.get<AccountHealth[]>('/api/channel-accounts/health').then((r) => r.data),

  create: (body: CreateChannelAccountRequest) =>
    api.post<ChannelAccount>('/api/channel-accounts', body).then((r) => r.data),

  update: (id: string, body: UpdateChannelAccountRequest) =>
    api.put<ChannelAccount>(`/api/channel-accounts/${id}`, body).then((r) => r.data),

  remove: (id: string) =>
    api.delete(`/api/channel-accounts/${id}`),

  /** Per-account daemon lifecycle. `start` errors if already running.
   *  `stop` is idempotent. `restart` = stop + start. Signal accounts
   *  only — Telegram has no local daemon to manage. */
  startDaemon:   (id: string) =>
    api.post<LifecycleResp>(`/api/channel-accounts/${id}/start`).then((r) => r.data),
  stopDaemon:    (id: string) =>
    api.post<LifecycleResp>(`/api/channel-accounts/${id}/stop`).then((r) => r.data),
  restartDaemon: (id: string) =>
    api.post<LifecycleResp>(`/api/channel-accounts/${id}/restart`).then((r) => r.data),

  restartServer: () =>
    api.post('/api/admin/restart'),
}

/** Wire-shape of POST /api/channel-accounts/{id}/{start,stop,restart}.
 *  Mirrors `LifecycleResp` in src/server/handlers/channel_accounts.rs. */
export interface LifecycleResp {
  ok:         boolean
  action:     'start' | 'stop' | 'restart'
  account_id: string
  message:    string
}
