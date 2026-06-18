// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'
import type {
  Group, GroupMember, CreateGroupRequest, UpdateGroupRequest,
} from './types'

export const groupsApi = {
  list:   () => api.get<Group[]>('/api/groups').then((r) => r.data),
  get:    (id: string) => api.get<Group>(`/api/groups/${id}`).then((r) => r.data),
  create: (body: CreateGroupRequest) =>
    api.post<Group>('/api/groups', body).then((r) => r.data),
  update: (id: string, body: UpdateGroupRequest) =>
    api.put<Group>(`/api/groups/${id}`, body).then((r) => r.data),
  delete: (id: string) => api.delete(`/api/groups/${id}`),

  listMembers: (id: string) =>
    api.get<GroupMember[]>(`/api/groups/${id}/members`).then((r) => r.data),
  addMember: (groupId: string, userId: string) =>
    api.post(`/api/groups/${groupId}/members`, { user_id: userId }),
  removeMember: (groupId: string, userId: string) =>
    api.delete(`/api/groups/${groupId}/members/${userId}`),

  // Self-service
  listMine: () => api.get<Group[]>('/api/me/groups').then((r) => r.data),
}
