// SPDX-License-Identifier: AGPL-3.0-or-later

import { useMemo, useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  BookOpen, Check, FileText, Inbox, Pencil, Plus, RefreshCw, Search, Settings2, Tag, Trash2, User as UserIcon, X,
} from 'lucide-react'
import { formatDistanceToNow } from 'date-fns'
import toast from 'react-hot-toast'
import { useRef } from 'react'
import {
  wikiApi, wikiAdminApi, wikiGitApi,
  type OpView, type PageDetail, type WikiWriter, type GitStatus,
} from '@/api/wiki'
import { Cloud, Download, GitBranch, GitCommit, Upload } from 'lucide-react'
import { useAuthStore } from '@/store/authStore'
import styles from './WikiPage.module.css'

type Tab = 'pages' | 'review' | 'activity' | 'settings'
export type WikiScope = 'user' | 'system'

/**
 * Bundle the scope-specific endpoints into a single object so the rest
 * of the page can stay scope-agnostic. The shapes are identical; only
 * the URLs differ.
 */
interface ScopedApi {
  listPages:    typeof wikiApi.listPages
  getPage:      typeof wikiApi.getPage
  putPage:      typeof wikiApi.putPage
  deletePage:   typeof wikiApi.deletePage
  listRecent:   typeof wikiApi.listRecent
}

function apiFor(scope: WikiScope): ScopedApi {
  return scope === 'system' ? wikiAdminApi : wikiApi
}

function tabLabel(tab: Tab, count: number | null) {
  switch (tab) {
    case 'pages':    return 'Pages'
    case 'review':   return count != null && count > 0 ? `Review (${count})` : 'Review'
    case 'activity': return 'Activity'
    case 'settings': return 'Settings'
  }
}

function formatRelative(ms: number) {
  try { return formatDistanceToNow(new Date(ms), { addSuffix: true }) }
  catch { return '' }
}

export default function WikiPage() {
  const qc = useQueryClient()
  const { user } = useAuthStore()
  const isAdmin = user?.role === 'admin'
  const [scope, setScope] = useState<WikiScope>('user')
  const [tab, setTab] = useState<Tab>('pages')

  // Auto-snap back to 'pages' if the user is currently on the 'review'
  // tab and switches into system scope (which has no review queue).
  const effectiveTab: Tab = scope === 'system' && tab === 'review' ? 'pages' : tab

  // Pending counts only apply to the user scope.
  const { data: pending = [] } = useQuery({
    queryKey: ['wiki', 'pending'],
    queryFn: wikiApi.listPending,
    refetchInterval: 8_000,
    enabled: scope === 'user',
  })

  const reloadPromptMut = useMutation({
    mutationFn: wikiAdminApi.reloadPrompt,
    onSuccess: (r) => {
      toast.success(r.message)
      qc.invalidateQueries({ queryKey: ['wiki'] })
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error
      toast.error(msg ?? 'Reload failed')
    },
  })

  const tabs: Tab[] = scope === 'system'
    ? ['pages', 'activity']
    : ['pages', 'review', 'activity', 'settings']

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1><BookOpen size={18} style={{ verticalAlign: 'middle', marginRight: 6 }} />Wiki</h1>
          <p>
            {scope === 'user'
              ? 'Your personal markdown knowledge base. Pages are stored as files on disk; agent and extractor writes wait for your approval in the review queue.'
              : 'Shared MIRA persona and runbooks. Admin edits to persona.md hot-reload the runtime system prompt — no restart needed.'}
          </p>
        </div>
        {isAdmin && (
          <div className={styles.scopeSwitch}>
            <button
              className={`${styles.scopeBtn} ${scope === 'user' ? styles.scopeBtnActive : ''}`}
              onClick={() => setScope('user')}
              title="Your personal wiki"
            >
              <UserIcon size={12} /> Personal
            </button>
            <button
              className={`${styles.scopeBtn} ${scope === 'system' ? styles.scopeBtnActive : ''}`}
              onClick={() => setScope('system')}
              title="Shared MIRA persona + runbooks"
            >
              <Settings2 size={12} /> System
            </button>
            {scope === 'system' && (
              <button
                className={styles.iconBtn}
                onClick={() => reloadPromptMut.mutate()}
                disabled={reloadPromptMut.isPending}
                title="Re-read persona.md and apply at runtime"
              >
                <RefreshCw size={12} />
              </button>
            )}
          </div>
        )}
      </div>

      <div className={styles.tabs}>
        {tabs.map((t) => (
          <button
            key={t}
            className={`${styles.tab} ${effectiveTab === t ? styles.tabActive : ''}`}
            onClick={() => setTab(t)}
          >
            {tabLabel(t, t === 'review' && scope === 'user' ? pending.length : null)}
          </button>
        ))}
      </div>

      {effectiveTab === 'pages' && <PagesTab qc={qc} scope={scope} />}
      {effectiveTab === 'review' && <ReviewTab pending={pending} qc={qc} />}
      {effectiveTab === 'activity' && <ActivityTab scope={scope} />}
      {effectiveTab === 'settings' && <SettingsTab qc={qc} />}
    </div>
  )
}

// ── Pages tab ────────────────────────────────────────────────────────────────

function PagesTab({ qc, scope }: { qc: ReturnType<typeof useQueryClient>; scope: WikiScope }) {
  const [search, setSearch] = useState('')
  const [selectedPath, setSelectedPath] = useState<string | null>(null)
  const [showCreate, setShowCreate] = useState(false)
  const scopedApi = apiFor(scope)

  const { data: pages = [], isLoading } = useQuery({
    queryKey: ['wiki', scope, 'pages'],
    queryFn: scopedApi.listPages,
  })

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase()
    if (!q) return pages
    return pages.filter((p) => {
      return (
        p.path.toLowerCase().includes(q) ||
        (p.title ?? '').toLowerCase().includes(q) ||
        p.tags.some((t) => t.toLowerCase().includes(q))
      )
    })
  }, [pages, search])

  // Keep selection valid when the list refreshes.
  const selected = selectedPath && pages.find((p) => p.path === selectedPath) ? selectedPath : null

  return (
    <div className={styles.pagesLayout}>
      <aside className={styles.pageList}>
        <div className={styles.listToolbar}>
          <div className={styles.searchBox}>
            <Search size={12} className={styles.searchIcon} />
            <input
              className={styles.searchInput}
              placeholder="Filter pages…"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
            />
          </div>
          <button
            className={styles.iconBtn}
            title="New page"
            onClick={() => { setSelectedPath(null); setShowCreate(true) }}
          >
            <Plus size={14} />
          </button>
        </div>

        {isLoading && <div className={styles.empty}>Loading…</div>}
        {!isLoading && filtered.length === 0 && <div className={styles.empty}>No pages</div>}

        <ul className={styles.list}>
          {filtered.map((p) => (
            <li key={p.path}>
              <button
                className={`${styles.listItem} ${selected === p.path ? styles.listItemActive : ''}`}
                onClick={() => { setSelectedPath(p.path); setShowCreate(false) }}
              >
                <FileText size={12} />
                <div className={styles.listItemBody}>
                  <span className={styles.listItemTitle}>{p.title ?? p.path}</span>
                  <span className={styles.listItemPath}>{p.path}</span>
                  {p.tags.length > 0 && (
                    <span className={styles.tagRow}>
                      {p.tags.slice(0, 3).map((t) => (
                        <span key={t} className={styles.tag}><Tag size={9} /> {t}</span>
                      ))}
                    </span>
                  )}
                </div>
                {p.is_special && <span className={styles.specialBadge}>core</span>}
              </button>
            </li>
          ))}
        </ul>
      </aside>

      <section className={styles.editorPane}>
        {showCreate
          ? <PageEditor key="new" mode="create" qc={qc} scope={scope} onClose={() => setShowCreate(false)} onSaved={(p) => { setShowCreate(false); setSelectedPath(p) }} />
          : selected
          ? <PageEditor key={`${scope}:${selected}`} mode="edit" path={selected} qc={qc} scope={scope} onDeleted={() => setSelectedPath(null)} />
          : <div className={styles.emptyHint}>Select a page on the left, or create a new one.</div>
        }
      </section>
    </div>
  )
}

// ── Page editor ──────────────────────────────────────────────────────────────

interface EditorProps {
  qc: ReturnType<typeof useQueryClient>
  scope: WikiScope
  mode: 'create' | 'edit'
  path?: string
  onClose?: () => void
  onSaved?: (path: string) => void
  onDeleted?: () => void
}

function PageEditor({ qc, scope, mode, path, onClose, onSaved, onDeleted }: EditorProps) {
  const isEdit = mode === 'edit'
  const scopedApi = apiFor(scope)

  const { data: page, isLoading } = useQuery<PageDetail>({
    queryKey: ['wiki', scope, 'page', path],
    queryFn: () => scopedApi.getPage(path!),
    enabled: isEdit && !!path,
  })

  const [draftPath, setDraftPath] = useState(path ?? 'pages/new.md')
  const [title, setTitle] = useState('')
  const [tags, setTags] = useState('')
  const [writer, setWriter] = useState<WikiWriter>('user')
  const [body, setBody] = useState('')
  const [editingMeta, setEditingMeta] = useState(false)

  // Hydrate state when the page loads.
  useMemo(() => {
    if (page) {
      setDraftPath(page.path)
      setTitle(page.title ?? '')
      setTags(page.tags.join(', '))
      setWriter(page.writer)
      setBody(page.body)
    }
  }, [page])

  const saveMut = useMutation({
    mutationFn: () => scopedApi.putPage({
      path: draftPath.trim(),
      title: title.trim() || undefined,
      tags: tags.split(',').map((t) => t.trim()).filter(Boolean),
      writer,
      body,
    }),
    onSuccess: () => {
      const isPersona = draftPath.trim() === 'persona.md' && scope === 'system'
      toast.success(isPersona
        ? 'persona.md saved — runtime prompt reloaded'
        : (isEdit ? 'Page saved' : 'Page created'))
      qc.invalidateQueries({ queryKey: ['wiki', scope] })
      if (!isEdit) onSaved?.(draftPath)
      setEditingMeta(false)
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error
      toast.error(msg ?? 'Save failed')
    },
  })

  const deleteMut = useMutation({
    mutationFn: () => scopedApi.deletePage(path!),
    onSuccess: () => {
      toast.success('Page archived')
      qc.invalidateQueries({ queryKey: ['wiki', scope] })
      onDeleted?.()
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error
      toast.error(msg ?? 'Delete failed')
    },
  })

  if (isEdit && isLoading) return <div className={styles.emptyHint}>Loading page…</div>

  const special = page?.path && /^(profile|SCHEMA|index|log|persona)\.md$/.test(page.path)

  return (
    <div className={styles.editor}>
      <div className={styles.editorHeader}>
        <div className={styles.editorMeta}>
          {!isEdit || editingMeta ? (
            <>
              <input
                className={styles.metaInput}
                placeholder="path (e.g. pages/projects/pong.md)"
                value={draftPath}
                onChange={(e) => setDraftPath(e.target.value)}
                disabled={isEdit /* path is the primary key */}
              />
              <input
                className={styles.metaInput}
                placeholder="Title"
                value={title}
                onChange={(e) => setTitle(e.target.value)}
              />
              <input
                className={styles.metaInput}
                placeholder="tags, comma, separated"
                value={tags}
                onChange={(e) => setTags(e.target.value)}
              />
              <select
                className={styles.metaInput}
                value={writer}
                onChange={(e) => setWriter(e.target.value as WikiWriter)}
                title="Who is allowed to mutate this page"
              >
                <option value="user">writer: user only</option>
                <option value="agent">writer: agent only</option>
                <option value="both">writer: both</option>
              </select>
            </>
          ) : (
            <>
              <span className={styles.metaPath}>{page?.path}</span>
              <span className={styles.metaTitle}>{page?.title ?? '(no title)'}</span>
              <span className={styles.metaTags}>
                {(page?.tags ?? []).map((t) => (
                  <span key={t} className={styles.tag}><Tag size={9} /> {t}</span>
                ))}
              </span>
              <span className={styles.writerBadge} data-writer={page?.writer}>{page?.writer}</span>
            </>
          )}
        </div>

        <div className={styles.editorActions}>
          {isEdit && !editingMeta && (
            <button className={styles.iconBtn} onClick={() => setEditingMeta(true)} title="Edit metadata">
              <Pencil size={13} />
            </button>
          )}
          {isEdit && !special && (
            <button
              className={styles.iconBtn}
              onClick={() => { if (confirm('Archive this page?')) deleteMut.mutate() }}
              disabled={deleteMut.isPending}
              title="Archive page"
            >
              <Trash2 size={13} />
            </button>
          )}
          {!isEdit && (
            <button className={styles.iconBtn} onClick={onClose} title="Cancel">
              <X size={13} />
            </button>
          )}
        </div>
      </div>

      <textarea
        className={styles.bodyTextarea}
        placeholder="# Markdown body…"
        value={body}
        onChange={(e) => setBody(e.target.value)}
      />

      <div className={styles.editorFooter}>
        {page?.provenance && page.provenance.length > 0 && (
          <span className={styles.provenance}>
            {page.provenance.length} provenance entr{page.provenance.length === 1 ? 'y' : 'ies'} —
            latest from <strong>{page.provenance[page.provenance.length - 1].source}</strong> {formatRelative(page.provenance[page.provenance.length - 1].extracted_at)}
          </span>
        )}
        <button
          className={styles.primaryBtn}
          onClick={() => saveMut.mutate()}
          disabled={saveMut.isPending || !draftPath.trim() || !body.trim()}
        >
          <Check size={13} /> {isEdit ? 'Save' : 'Create'}
        </button>
      </div>
    </div>
  )
}

// ── Review tab ───────────────────────────────────────────────────────────────

function ReviewTab({ pending, qc }: { pending: OpView[]; qc: ReturnType<typeof useQueryClient> }) {
  const approveMut = useMutation({
    mutationFn: (id: string) => wikiApi.approve(id),
    onSuccess: () => {
      toast.success('Approved')
      qc.invalidateQueries({ queryKey: ['wiki'] })
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error
      toast.error(msg ?? 'Approve failed')
    },
  })
  const rejectMut = useMutation({
    mutationFn: ({ id, reason }: { id: string; reason: string }) =>
      wikiApi.reject(id, { reason }),
    onSuccess: () => {
      toast.success('Rejected')
      qc.invalidateQueries({ queryKey: ['wiki'] })
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error
      toast.error(msg ?? 'Reject failed')
    },
  })

  // Bulk actions — clear the queue without hand-approving every entry.
  const bulkMut = useMutation({
    mutationFn: (action: { kind: 'approve'; minConfidence?: number } | { kind: 'reject' }) =>
      action.kind === 'approve'
        ? wikiApi.approveAll(action.minConfidence).then((n) => `Approved ${n}`)
        : wikiApi.rejectAll().then((n) => `Rejected ${n}`),
    onSuccess: (msg) => {
      toast.success(msg)
      qc.invalidateQueries({ queryKey: ['wiki'] })
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error
      toast.error(msg ?? 'Bulk action failed')
    },
  })

  if (pending.length === 0) {
    return (
      <div className={styles.emptyState}>
        <Inbox size={28} />
        <h3>Nothing to review</h3>
        <p>When the extractor or an agent tool proposes a change to your wiki, it lands here for your approval.</p>
      </div>
    )
  }

  const HIGH = 0.85
  const highConfCount = pending.filter((o) => o.confidence != null && o.confidence >= HIGH).length
  const busy = approveMut.isPending || rejectMut.isPending || bulkMut.isPending

  return (
    <div className={styles.reviewList}>
      <div
        style={{ display: 'flex', flexWrap: 'wrap', gap: 8, alignItems: 'center', marginBottom: 4 }}
      >
        <span style={{ fontSize: 13, opacity: 0.7, marginRight: 'auto' }}>
          {pending.length} pending
        </span>
        {highConfCount > 0 && (
          <button
            className={styles.primaryBtn}
            disabled={busy}
            onClick={() => bulkMut.mutate({ kind: 'approve', minConfidence: HIGH })}
            title={`Approve the ${highConfCount} op(s) the extractor was at least ${HIGH} confident about`}
          >
            <Check size={12} /> Approve all ≥ {HIGH} ({highConfCount})
          </button>
        )}
        <button
          className={styles.secondaryBtn}
          disabled={busy}
          onClick={() => {
            if (window.confirm(`Approve and apply all ${pending.length} pending changes?`))
              bulkMut.mutate({ kind: 'approve' })
          }}
        >
          <Check size={12} /> Approve all ({pending.length})
        </button>
        <button
          className={styles.dangerBtn}
          disabled={busy}
          onClick={() => {
            if (window.confirm(`Reject and discard all ${pending.length} pending changes?`))
              bulkMut.mutate({ kind: 'reject' })
          }}
        >
          <X size={12} /> Reject all
        </button>
      </div>
      {pending.map((op) => (
        <OpCard
          key={op.op_id}
          op={op}
          onApprove={() => approveMut.mutate(op.op_id)}
          onReject={(reason) => rejectMut.mutate({ id: op.op_id, reason })}
          busy={busy}
        />
      ))}
    </div>
  )
}

interface OpCardProps {
  op: OpView
  onApprove: () => void
  onReject: (reason: string) => void
  busy: boolean
}

function OpCard({ op, onApprove, onReject, busy }: OpCardProps) {
  const [showRejectReason, setShowRejectReason] = useState(false)
  const [reason, setReason] = useState('')

  return (
    <div className={styles.opCard}>
      <div className={styles.opHead}>
        <span className={styles.opKindBadge} data-kind={op.kind}>{op.kind.replace(/_/g, ' ')}</span>
        <span className={styles.opTarget}>{op.target_path}</span>
        <span className={styles.opSource}>
          from <strong>{op.provenance_source}</strong> · {op.provenance_actor}
        </span>
        {op.confidence != null && (
          <span
            className={styles.opKindBadge}
            title="Extractor confidence"
            style={{ opacity: 0.85 }}
          >
            {Math.round(op.confidence * 100)}%
          </span>
        )}
        <span className={styles.opTime}>{formatRelative(op.created_at)}</span>
      </div>
      <OpPreview op={op.op} />
      <div className={styles.opActions}>
        {showRejectReason ? (
          <>
            <input
              className={styles.metaInput}
              placeholder="Reason (optional)"
              value={reason}
              onChange={(e) => setReason(e.target.value)}
              autoFocus
            />
            <button className={styles.dangerBtn} onClick={() => onReject(reason)} disabled={busy}>
              Reject
            </button>
            <button className={styles.iconBtn} onClick={() => { setShowRejectReason(false); setReason('') }}>
              <X size={12} />
            </button>
          </>
        ) : (
          <>
            <button className={styles.primaryBtn} onClick={onApprove} disabled={busy}>
              <Check size={12} /> Approve
            </button>
            <button className={styles.secondaryBtn} onClick={() => setShowRejectReason(true)} disabled={busy}>
              <X size={12} /> Reject
            </button>
          </>
        )}
      </div>
    </div>
  )
}

function OpPreview({ op }: { op: Record<string, unknown> }) {
  // Best-effort preview — we don't know the exact variant shape at compile
  // time, so reach into common fields.
  const kind = (op.op as string | undefined) ?? '(unknown)'
  const body =
    (op.body as string | undefined) ??
    (op.summary as string | undefined) ??
    ''
  const section = op.section as string | undefined
  const title = op.title as string | undefined

  return (
    <div className={styles.opPreview}>
      {title && <div className={styles.opPreviewMeta}>title: <strong>{title}</strong></div>}
      {section && <div className={styles.opPreviewMeta}>section: <strong>## {section}</strong></div>}
      {body && (
        <pre className={styles.opPreviewBody}>{body.length > 1200 ? body.slice(0, 1200) + '…' : body}</pre>
      )}
      {!body && !title && !section && (
        <div className={styles.opPreviewMeta}><em>op kind: {kind}</em></div>
      )}
    </div>
  )
}

// ── Settings tab (Slice G — git + import/export) ─────────────────────────────

function SettingsTab({ qc }: { qc: ReturnType<typeof useQueryClient> }) {
  const fileInputRef = useRef<HTMLInputElement>(null)
  const [remoteUrl, setRemoteUrl] = useState('')

  const { data: git, isLoading } = useQuery<GitStatus>({
    queryKey: ['wiki', 'git', 'status'],
    queryFn: wikiGitApi.status,
    refetchInterval: 12_000,
  })

  // Pre-fill the remote-URL input from the current value once it loads.
  useMemo(() => {
    if (git?.remote_url && !remoteUrl) setRemoteUrl(git.remote_url)
  }, [git?.remote_url]) // eslint-disable-line react-hooks/exhaustive-deps

  const commitMut = useMutation({
    mutationFn: () => wikiGitApi.commit(),
    onSuccess: (r) => { toast.success(r.output); qc.invalidateQueries({ queryKey: ['wiki', 'git'] }) },
    onError: (e: unknown) => toast.error(extractMsg(e, 'Commit failed')),
  })
  const setRemoteMut = useMutation({
    mutationFn: () => wikiGitApi.setRemote(remoteUrl.trim()),
    onSuccess: (r) => { toast.success(r.output); qc.invalidateQueries({ queryKey: ['wiki', 'git'] }) },
    onError: (e: unknown) => toast.error(extractMsg(e, 'Set remote failed')),
  })
  const pushMut = useMutation({
    mutationFn: () => wikiGitApi.push(),
    onSuccess: () => { toast.success('Pushed'); qc.invalidateQueries({ queryKey: ['wiki', 'git'] }) },
    onError: (e: unknown) => toast.error(extractMsg(e, 'Push failed')),
  })
  const pullMut = useMutation({
    mutationFn: () => wikiGitApi.pull(),
    onSuccess: () => { toast.success('Pulled'); qc.invalidateQueries({ queryKey: ['wiki'] }) },
    onError: (e: unknown) => toast.error(extractMsg(e, 'Pull failed')),
  })
  const importMut = useMutation({
    mutationFn: (file: File) => wikiGitApi.import(file),
    onSuccess: (r) => { toast.success(r.message); qc.invalidateQueries({ queryKey: ['wiki'] }) },
    onError: (e: unknown) => toast.error(extractMsg(e, 'Import failed')),
  })

  return (
    <div className={styles.settingsPane}>
      <section className={styles.settingsSection}>
        <h3><GitBranch size={14} /> Git sync</h3>
        {isLoading && <div className={styles.emptyHint}>Loading git status…</div>}
        {git && !git.initialized && (
          <p className={styles.emptyHint}>Git is not initialised for this wiki — enable it in <code>config.wiki.git</code> and restart.</p>
        )}
        {git?.initialized && (
          <>
            <dl className={styles.gitStatusGrid}>
              <dt>Branch</dt><dd>{git.branch ?? '(detached)'}</dd>
              <dt>HEAD</dt><dd><code>{git.head_short ?? '-'}</code></dd>
              <dt>Working tree</dt>
              <dd>
                {git.modified + git.untracked + git.deleted === 0
                  ? <span className={styles.gitClean}>clean</span>
                  : <span className={styles.gitDirty}>
                      {git.modified} modified, {git.untracked} new, {git.deleted} deleted
                    </span>}
              </dd>
              {git.remote_url && (<><dt>Remote</dt><dd className={styles.gitRemote}>{git.remote_url}</dd></>)}
              {(git.ahead !== null || git.behind !== null) && (
                <>
                  <dt>vs upstream</dt>
                  <dd>↑ {git.ahead ?? 0} ↓ {git.behind ?? 0}</dd>
                </>
              )}
            </dl>
            <div className={styles.settingsRow}>
              <input
                className={styles.metaInput}
                placeholder="git@github.com:you/wiki.git"
                value={remoteUrl}
                onChange={(e) => setRemoteUrl(e.target.value)}
              />
              <button className={styles.secondaryBtn}
                onClick={() => setRemoteMut.mutate()}
                disabled={setRemoteMut.isPending || !remoteUrl.trim()}>
                Set remote
              </button>
            </div>
            <div className={styles.settingsRow}>
              <button className={styles.primaryBtn}
                onClick={() => commitMut.mutate()}
                disabled={commitMut.isPending}>
                <GitCommit size={12} /> Commit
              </button>
              <button className={styles.secondaryBtn}
                onClick={() => pushMut.mutate()}
                disabled={pushMut.isPending || !git.remote_url}>
                <Cloud size={12} /> Push
              </button>
              <button className={styles.secondaryBtn}
                onClick={() => pullMut.mutate()}
                disabled={pullMut.isPending || !git.remote_url}>
                <Cloud size={12} /> Pull
              </button>
            </div>
          </>
        )}
      </section>

      <section className={styles.settingsSection}>
        <h3><Download size={14} /> Backup</h3>
        <p className={styles.emptyHint}>
          Export a <code>.tar.gz</code> snapshot of every page, or restore one. <code>.git</code> and scratch files are excluded.
        </p>
        <div className={styles.settingsRow}>
          <a
            className={styles.primaryBtn}
            href={wikiGitApi.exportUrl()}
            target="_blank"
            rel="noopener noreferrer"
          >
            <Download size={12} /> Export
          </a>
          <button
            className={styles.secondaryBtn}
            onClick={() => fileInputRef.current?.click()}
            disabled={importMut.isPending}
          >
            <Upload size={12} /> Import…
          </button>
          <input
            ref={fileInputRef}
            type="file"
            accept=".tar.gz,.tgz,application/gzip"
            style={{ display: 'none' }}
            onChange={(e) => {
              const file = e.target.files?.[0]
              if (file) importMut.mutate(file)
              e.target.value = ''
            }}
          />
        </div>
      </section>
    </div>
  )
}

function extractMsg(err: unknown, fallback: string): string {
  return (err as { response?: { data?: { error?: string } } })?.response?.data?.error ?? fallback
}

// ── Activity tab ─────────────────────────────────────────────────────────────

function ActivityTab({ scope }: { scope: WikiScope }) {
  const scopedApi = apiFor(scope)
  const { data: ops = [], isLoading } = useQuery({
    queryKey: ['wiki', scope, 'recent'],
    queryFn: () => scopedApi.listRecent({ limit: 100 }),
    refetchInterval: 10_000,
  })
  if (isLoading) return <div className={styles.emptyHint}>Loading…</div>
  if (ops.length === 0) {
    return (
      <div className={styles.emptyState}>
        <Inbox size={28} />
        <h3>No recent activity</h3>
        <p>Wiki edits, extractor proposals, and agent tool writes will show up here as they happen.</p>
      </div>
    )
  }
  return (
    <div className={styles.activityList}>
      {ops.map((op) => (
        <div key={op.op_id} className={styles.activityRow} data-status={op.status}>
          <span className={styles.statusBadge} data-status={op.status}>{op.status}</span>
          <span className={styles.opKindBadge} data-kind={op.kind}>{op.kind.replace(/_/g, ' ')}</span>
          <span className={styles.opTarget}>{op.target_path}</span>
          <span className={styles.opSource}>
            {op.provenance_source} · {op.provenance_actor}
          </span>
          <span className={styles.opTime}>{formatRelative(op.created_at)}</span>
        </div>
      ))}
    </div>
  )
}
