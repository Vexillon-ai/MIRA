// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export type WikiWriter = 'user' | 'agent' | 'both'
export type WikiOpStatus = 'pending' | 'applied' | 'rejected' | 'failed'
export type WikiLogKind = 'ingest' | 'promote' | 'supersede' | 'lint' | 'note'

export interface PageSummary {
  path: string
  title: string | null
  writer: WikiWriter
  tags: string[]
  valid_from: string | null
  valid_to: string | null
  is_special: boolean
}

export interface ProvenanceView {
  source: string
  turn_id: string | null
  conversation_id: string | null
  extracted_at: number   // epoch ms
}

export interface PageDetail {
  path: string
  title: string | null
  writer: WikiWriter
  tags: string[]
  valid_from: string | null
  valid_to: string | null
  confidence: number | null
  body: string
  provenance: ProvenanceView[]
}

export interface NavBundle {
  profile: string
  index: string
  schema: string
  log: string
}

/**
 * Lossy view of an op envelope as it appears in the audit log. The `op`
 * field carries the raw op shape (write_page, append_section, log_entry,
 * etc.) — the review queue UI inspects it to render previews of the
 * proposed change.
 */
export interface OpView {
  op_id: string
  status: WikiOpStatus
  kind: string
  target_path: string
  scope: 'user' | 'system'
  user_id: string | null
  provenance_source: string
  provenance_actor: string
  conversation_id: string | null
  turn_id: string | null
  created_at: number       // epoch ms
  applied_at: number | null
  reviewed_at: number | null
  reviewed_by: string | null
  failure: string | null
  /** Extractor confidence [0,1]; null for direct UI/tool writes. */
  confidence: number | null
  op: Record<string, unknown>
}

export interface PutPageRequest {
  path: string
  title?: string
  tags?: string[]
  body: string
  writer?: WikiWriter
}

export interface AppendSectionRequest {
  path: string
  section: string
  body: string
}

export interface RejectOpRequest {
  reason?: string
}

export interface RecentOpsQuery {
  since?: number
  limit?: number
}

export interface LogEntryRequest {
  kind?: WikiLogKind
  summary: string
  page_refs?: string[]
}

export const wikiApi = {
  listPages: () =>
    api.get<PageSummary[]>('/api/wiki/pages').then((r) => r.data),

  getPage: (path: string) =>
    api.get<PageDetail>('/api/wiki/page', { params: { path } }).then((r) => r.data),

  putPage: (body: PutPageRequest) =>
    api.put<OpView>('/api/wiki/page', body).then((r) => r.data),

  deletePage: (path: string) =>
    api.delete<OpView>('/api/wiki/page', { params: { path } }).then((r) => r.data),

  appendSection: (body: AppendSectionRequest) =>
    api.post<OpView>('/api/wiki/page/append-section', body).then((r) => r.data),

  addLogEntry: (body: LogEntryRequest) =>
    api.post<OpView>('/api/wiki/log', body).then((r) => r.data),

  getNav: () =>
    api.get<NavBundle>('/api/wiki/nav').then((r) => r.data),

  listPending: () =>
    api.get<OpView[]>('/api/wiki/ops/pending').then((r) => r.data),

  listRecent: (params?: RecentOpsQuery) =>
    api.get<OpView[]>('/api/wiki/ops', { params }).then((r) => r.data),

  approve: (op_id: string) =>
    api.post<OpView>(`/api/wiki/ops/${op_id}/approve`).then((r) => r.data),

  reject: (op_id: string, body: RejectOpRequest = {}) =>
    api.post<OpView>(`/api/wiki/ops/${op_id}/reject`, body).then((r) => r.data),

  /** Bulk-approve pending ops; pass `min_confidence` to approve only the
   *  high-confidence ones. Returns the number applied. */
  approveAll: (min_confidence?: number) =>
    api.post<{ approved: number }>('/api/wiki/ops/approve-all',
      min_confidence == null ? {} : { min_confidence }).then((r) => r.data.approved),

  /** Bulk-reject pending ops; pass `max_confidence` to reject only the
   *  low-confidence ones. Returns the number rejected. */
  rejectAll: (opts?: { reason?: string; max_confidence?: number }) =>
    api.post<{ rejected: number }>('/api/wiki/ops/reject-all', opts ?? {})
      .then((r) => r.data.rejected),

  /** Slice H — save a conversation as a wiki page. */
  saveThread: (conversation_id: string, opts?: { path?: string; title?: string; max_messages?: number }) =>
    api.post<OpView>('/api/wiki/save-thread', { conversation_id, ...opts }).then((r) => r.data),
}

// ── Git + import / export (Slice G) ──────────────────────────────────────────

export interface GitStatus {
  initialized: boolean
  branch: string | null
  head_short: string | null
  remote_url: string | null
  modified: number
  untracked: number
  deleted: number
  ahead: number | null
  behind: number | null
}

export interface GitOpResponse {
  ok: boolean
  output: string
}

export interface ImportResponse {
  ok: boolean
  entries: number
  message: string
}

export const wikiGitApi = {
  status: () =>
    api.get<GitStatus>('/api/wiki/git/status').then((r) => r.data),

  commit: (message?: string) =>
    api.post<GitOpResponse>('/api/wiki/git/commit', { message }).then((r) => r.data),

  setRemote: (url: string) =>
    api.post<GitOpResponse>('/api/wiki/git/remote', { url }).then((r) => r.data),

  push: () =>
    api.post<GitOpResponse>('/api/wiki/git/push').then((r) => r.data),

  pull: () =>
    api.post<GitOpResponse>('/api/wiki/git/pull').then((r) => r.data),

  /**
   * Trigger a tarball download. Uses the browser's native navigation so
   * the file actually saves; axios + blob would also work but is more
   * fiddly with auth cookies.
   */
  exportUrl: () => '/api/wiki/export',

  import: (file: File) => {
    const fd = new FormData()
    fd.append('file', file)
    return api.post<ImportResponse>('/api/wiki/import', fd).then((r) => r.data)
  },
}

// ── System wiki (admin-only) — Slice F ───────────────────────────────────────

export interface AdminReloadResponse {
  reloaded: boolean
  message: string
}

export const wikiAdminApi = {
  listPages: () =>
    api.get<PageSummary[]>('/api/admin/wiki/pages').then((r) => r.data),

  getPage: (path: string) =>
    api.get<PageDetail>('/api/admin/wiki/page', { params: { path } }).then((r) => r.data),

  putPage: (body: PutPageRequest) =>
    api.put<OpView>('/api/admin/wiki/page', body).then((r) => r.data),

  deletePage: (path: string) =>
    api.delete<OpView>('/api/admin/wiki/page', { params: { path } }).then((r) => r.data),

  appendSection: (body: AppendSectionRequest) =>
    api.post<OpView>('/api/admin/wiki/page/append-section', body).then((r) => r.data),

  getNav: () =>
    api.get<NavBundle>('/api/admin/wiki/nav').then((r) => r.data),

  listRecent: (params?: RecentOpsQuery) =>
    api.get<OpView[]>('/api/admin/wiki/ops', { params }).then((r) => r.data),

  reloadPrompt: () =>
    api.post<AdminReloadResponse>('/api/admin/wiki/reload-prompt').then((r) => r.data),
}

