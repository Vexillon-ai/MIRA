// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

// Wire types matching src/server/handlers/channel_links.rs (R1+R2).
//
// These power the "My Channels" self-serve linking panel: a user on a
// shared/guest_ok bot generates a one-time LINK-XXXX-XXXX code, sends it
// to the bot, and the bot claims their identity. Links can be listed and
// revoked here too.

export interface ChannelLink {
  id: string
  channel: string
  external_id: string
  created_at: number
  verified_at: number
}

export interface IssueCodeResponse {
  code: string
  channel: string
  expires_at: number
  ttl_seconds: number
}

export const channelLinksApi = {
  /** Every channel identity link the current user owns. */
  list: () =>
    api.get<ChannelLink[]>('/api/me/channel-links').then((r) => r.data),

  /** Remove one of the caller's links (frees the external id for re-use). */
  remove: (id: string) =>
    api.delete(`/api/me/channel-links/${id}`),

  /** Issue a fresh one-time link code for the given channel. Replaces any
   *  pending code for the same (user, channel) pair. */
  issueCode: (channel: string) =>
    api.post<IssueCodeResponse>('/api/me/channel-links/codes', { channel })
      .then((r) => r.data),
}
