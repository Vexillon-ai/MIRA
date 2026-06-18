// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'
import type {
  AdminStatsResponse, Conversation, ConversationGroup, HistoryStats, Message,
  CreateConversationRequest, UpdateConversationRequest,
} from './types'

export const conversationsApi = {
  list: () =>
    api.get<Conversation[]>('/api/conversations').then((r) => r.data),

  stats: () =>
    api.get<HistoryStats>('/api/conversations/stats').then((r) => r.data),

  // Admin-only cross-user history. Endpoints return 403 for non-admins.
  adminGrouped: () =>
    api.get<ConversationGroup[]>('/api/admin/conversations/grouped').then((r) => r.data),

  adminStats: () =>
    api.get<AdminStatsResponse>('/api/admin/conversations/stats').then((r) => r.data),

  create: (data: CreateConversationRequest = {}) =>
    api.post<Conversation>('/api/conversations', data).then((r) => r.data),

  get: (id: string) =>
    api.get<Conversation>(`/api/conversations/${id}`).then((r) => r.data),

  update: (id: string, data: UpdateConversationRequest) =>
    api.patch<Conversation>(`/api/conversations/${id}`, data).then((r) => r.data),

  delete: (id: string) =>
    api.delete(`/api/conversations/${id}`).then((r) => r.data),

  messages: (id: string) =>
    api.get<Message[]>(`/api/conversations/${id}/messages`).then((r) => r.data),

  deleteMessage: (messageId: string) =>
    api.delete(`/api/messages/${messageId}`).then((r) => r.data),
}
