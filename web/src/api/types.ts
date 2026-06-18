// SPDX-License-Identifier: AGPL-3.0-or-later

// Shared API types matching the Rust backend

// ── Voice prefs ───────────────────────────────────────────────────────────
//
// Per-channel response policy + voice id override. Both fields are optional
// at every level — missing entries inherit from the next layer down (user →
// server default → built-in `never`). The map is keyed by the stable channel
// id from `GET /api/channels` (`web`, `tui`, `telegram`, `signal`, plus any
// plugin-registered channels).

export type VoiceResponsePolicy = 'always' | 'on_voice_input' | 'never'

export interface ChannelVoicePrefs {
  response_policy?: VoiceResponsePolicy | null
  voice_id?: string | null
}

export type VoicePrefsMap = Record<string, ChannelVoicePrefs>

export interface ChannelDescriptor {
  id: string
  display_name: string
  supports_voice: boolean
}

export interface User {
  id: string
  username: string
  display_name: string | null
  email: string | null
  role: 'admin' | 'user'
  is_active: boolean
  created_at: number
  updated_at: number
  last_login: number | null
  phone: string | null
  preferred_contact: string | null
  avatar: string | null
  voice_prefs: VoicePrefsMap
}

export interface LoginRequest {
  username: string
  password: string
}

export interface LoginResponse {
  access_token: string
  token_type: string
  expires_in: number
  user: User
}

export interface RefreshResponse {
  access_token: string
  token_type: string
  expires_in: number
}

export interface Conversation {
  id: string
  user_id: string
  title: string | null
  channel: string
  model: string | null
  provider: string | null
  created_at: number
  updated_at: number
  last_message_at?: number | null
  /// Conversation flow. `"chat"` is the default; `"onboarding"` swaps in
  /// the onboarding system prompt and restricts the tool set.
  mode?: string
  /// Slice H — when true, the wiki context-injection hook is skipped
  /// for every turn in this conversation.
  skip_wiki?: boolean
}

// ── Onboarding ────────────────────────────────────────────────────────────

export interface OnboardingGroupSummary {
  id: string
  label: string
  optional: boolean
}

export interface OnboardingState {
  user_id: string
  onboarded_at: number | null
  active_conversation_id: string | null
  completed_groups: string[]
  skipped_keys: string[]
  remaining_groups: string[]
  total_groups: number
  groups: OnboardingGroupSummary[]
}

export interface StartOnboardingResponse {
  conversation_id: string
  resumed: boolean
}

export interface ChannelStats {
  channel: string
  conversations: number
  messages: number
  tokens: number
}

export interface HistoryStats {
  total_conversations: number
  total_messages: number
  user_messages: number
  assistant_messages: number
  tool_messages: number
  estimated_tokens: number
  per_channel: ChannelStats[]
  top_model: string | null
  first_message_at: number | null
  last_message_at: number | null
}

export interface UserSummary {
  id: string
  username: string
  display_name: string | null
}

export interface ConversationGroup {
  owner: UserSummary
  conversations: Conversation[]
  last_activity: number
}

export interface PerUserStats extends UserSummary {
  stats: HistoryStats
}

export interface AdminStatsResponse {
  per_user: PerUserStats[]
  totals: HistoryStats
}

/**
 * One entry in the assistant message's thinking trail. Mirrors the
 * shapes pushed into `Message.metadata.thinking` by the chat
 * handler. The discriminator is the `type` field; additional
 * properties vary by variant.
 */
export type ThinkingEntry =
  | { type: 'tool_call';    name: string; args: unknown; call_id?: string }
  | { type: 'tool_result';  name: string; output: string; success: boolean; call_id?: string }
  | { type: 'reasoning';    text: string }
  | { type: 'wiki_context'; pages: string[] }

/**
 * Q1.3 — non-text input attached to a user message. Today: inline
 * base64 images for the vision-capable providers. Round-trips through
 * /api/chat and into Message.metadata for replay on page reload.
 */
export interface Attachment {
  kind:      'image'
  mime_type: string
  /** Standard base64 (RFC 4648 with `=` padding); no `data:` prefix. */
  data_b64:  string
}

export interface Message {
  id: string
  conversation_id: string
  role: 'user' | 'assistant' | 'system' | 'tool'
  content: string
  content_type: string
  channel: string | null
  created_at: number
  token_count: number | null
  tool_calls: string | null
  /// JSON-encoded metadata blob. The chat handler stores the
  /// thinking trail here as `{"thinking": [...]}`; the client parses
  /// at render time via `parseMessageMetadata`.
  metadata?: string | null
  /// Slice H — wiki pages that fed this assistant turn (client-side
  /// only; never persisted by the server). Used to render context
  /// pills under the assistant message.
  wiki_pages?: string[]
  /// Convenience field populated client-side on the live stream
  /// from SSE events. For history rendering we read from `metadata`
  /// via parseMessageMetadata().
  reasoning?: string
  /// Parsed view of the persisted thinking trail. Populated by
  /// parseMessageMetadata at render time OR set client-side during
  /// streaming. Never round-tripped to the server in this field —
  /// the source of truth is `metadata`.
  thinking?: ThinkingEntry[]
  /// Non-fatal warnings surfaced during the turn — e.g. a provider
  /// failover ("your configured provider was unavailable; replied with X
  /// instead"). Set client-side from `warning` SSE events. Rendered as an
  /// inline callout on the message.
  warnings?: string[]
  /// Q1.3 — attachments parsed from metadata (or set client-side on
  /// the just-sent user message). Renders as image previews under
  /// the bubble. Like `thinking`, the persisted source of truth is
  /// the `metadata` blob.
  attachments?: Attachment[]
}

/**
 * Parse the JSON metadata blob attached to a persisted Message and
 * pull out the thinking trail. Returns an empty array when the blob
 * is missing, malformed, or doesn't carry a `thinking` field — the
 * caller falls back to live-stream events in that case.
 */
export function parseMessageMetadata(m: Message): ThinkingEntry[] {
  if (m.thinking) return m.thinking
  if (!m.metadata) return []
  try {
    const obj = JSON.parse(m.metadata)
    if (obj && Array.isArray(obj.thinking)) return obj.thinking as ThinkingEntry[]
  } catch { /* ignore */ }
  return []
}

/// Non-fatal warnings (e.g. provider failover) for a message. Prefers the
/// client-set field, falls back to the persisted metadata blob on reload.
export function parseMessageWarnings(m: Message): string[] {
  if (m.warnings) return m.warnings
  if (!m.metadata) return []
  try {
    const obj = JSON.parse(m.metadata)
    if (obj && Array.isArray(obj.warnings)) return obj.warnings as string[]
  } catch { /* ignore */ }
  return []
}

/// Extract image attachments from a persisted message. Same pattern as
/// `parseMessageMetadata` — prefers the in-memory field set by the
/// client, falls back to the persisted metadata blob on reload.
export function parseMessageAttachments(m: Message): Attachment[] {
  if (m.attachments) return m.attachments
  if (!m.metadata) return []
  try {
    const obj = JSON.parse(m.metadata)
    if (obj && Array.isArray(obj.attachments)) return obj.attachments as Attachment[]
  } catch { /* ignore */ }
  return []
}

export interface ChatRequest {
  message: string
  conversation_id?: number
  session_id?: string
}

export interface ChatChunk {
  type: 'text' | 'tool_call' | 'tool_result' | 'done' | 'error'
  content?: string
  tool?: string
  error?: string
  conversation_id?: number
  message_id?: number
}

export interface ApiError {
  error: string
  code?: string
}

export interface PaginatedResponse<T> {
  items: T[]
  total: number
  page: number
  per_page: number
}

export interface CreateConversationRequest {
  title?: string
  channel?: string
}

export interface UpdateConversationRequest {
  title?: string
  /// Slice H — toggle wiki context injection for this conversation.
  skip_wiki?: boolean
}

export interface CreateUserRequest {
  username: string
  password: string
  display_name?: string
  email?: string
  role?: 'admin' | 'user'
}

export interface UpdateUserRequest {
  display_name?: string
  email?: string
  role?: 'admin' | 'user'
  is_active?: boolean
  phone?: string | null
  preferred_contact?: string | null
  avatar?: string | null
  /// Replace the user's per-channel voice prefs. Send the full merged map —
  /// the backend overwrites the stored value rather than deep-merging.
  voice_prefs?: VoicePrefsMap
}

export interface ChangePasswordRequest {
  new_password: string
}

export interface ResetPasswordResponse {
  new_password: string
}

// ── Groups ─────────────────────────────────────────────────────────────────

export interface Group {
  id: string
  name: string
  description: string | null
  created_by: string
  created_at: number
  updated_at: number
}

export interface GroupMember {
  id: string
  username: string
  display_name: string | null
  role: 'admin' | 'user'
}

export interface CreateGroupRequest {
  name: string
  description?: string
}

export interface UpdateGroupRequest {
  name?: string
  description?: string
}
