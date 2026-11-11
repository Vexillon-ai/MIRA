// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState, useEffect } from 'react'
import { Link } from 'react-router-dom'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Brain, Plus, Trash2, Pencil, Check, X, Search, User as UserIcon, Users as UsersIcon, Globe, History, Download } from 'lucide-react'
import { memoryApi, type MemoryItem, type MemoryScope, type MemorySort } from '@/api/memory'
import { groupsApi } from '@/api/groups'
import { useAuthStore } from '@/store/authStore'
import type { Group } from '@/api/types'
import { formatDistanceToNow } from 'date-fns'
import MemoryCatalog from '@/components/MemoryCatalog'
import styles from './MemoryPage.module.css'

const CATEGORIES = ['all', 'fact', 'preference', 'skill', 'relationship', 'project'] as const
type CategoryFilter = (typeof CATEGORIES)[number]

const ORIGINS = [
  { value: 'all',            label: 'All origins' },
  { value: 'user_explicit',  label: 'Added by you' },
  { value: 'auto_extracted', label: 'Learned' },
  { value: 'imported',       label: 'Imported' },
] as const
type OriginFilter = (typeof ORIGINS)[number]['value']

const SORTS: { value: MemorySort; label: string }[] = [
  { value: 'strength', label: 'Most used' },
  { value: 'recent',   label: 'Most recent' },
]

type Tab = 'saved' | 'summaries' | 'sources'

// Machine-generated tags are internal plumbing — hide them from the user.
const MACHINE_TAG = /^(auto|heuristic|user_explicit|tool|rollup|rollup:.*|entity:.*|key:.*|confidence:.*|topic:.*)$/
const userTags = (tags: string[]) => tags.filter((t) => !MACHINE_TAG.test(t))
const catLabel = (c: string) =>
  c === 'all' ? 'All' : c === 'relationship' ? 'People & pets' : c.charAt(0).toUpperCase() + c.slice(1)

function originLabel(m: MemoryItem): string {
  switch (m.source_kind) {
    case 'user_explicit':  return 'Added by you'
    case 'auto_extracted': return 'Learned from chat'
    case 'imported':       return 'Imported'
    default:               return 'Added'
  }
}

function ScopeBadge({ scope, scopeId, groups }: {
  scope:    MemoryScope
  scopeId:  string | null
  groups:   Group[]
}) {
  const groupName = scope === 'group' && scopeId
    ? groups.find((g) => g.id === scopeId)?.name ?? scopeId.slice(0, 8)
    : null
  const icon = scope === 'user'   ? <UserIcon size={10} />
             : scope === 'group'  ? <UsersIcon size={10} />
             :                      <Globe size={10} />
  const label = scope === 'user' ? 'Only me'
              : scope === 'group' && groupName ? `group: ${groupName}`
              : scope
  return (
    <span className={styles.scopeBadge} data-scope={scope} title={`Scope: ${label}`}>
      {icon}{label}
    </span>
  )
}

export default function MemoryPage() {
  const qc   = useQueryClient()
  const { user } = useAuthStore()
  const isAdmin = user?.role === 'admin'

  const [tab, setTab]               = useState<Tab>('saved')
  const [search, setSearch]         = useState('')
  const [category, setCategory]     = useState<CategoryFilter>('all')
  const [origin, setOrigin]         = useState<OriginFilter>('all')
  const [sort, setSort]             = useState<MemorySort>('recent')
  const [editId, setEditId]         = useState<number | null>(null)
  const [editContent, setEditContent] = useState('')
  const [editCategory, setEditCategory] = useState('')
  const [editTags, setEditTags]     = useState('')
  const [showCreate, setShowCreate] = useState(false)
  const [newContent, setNewContent] = useState('')
  const [newCategory, setNewCategory] = useState('fact')
  const [newTags, setNewTags]       = useState('')
  const [newScope, setNewScope]     = useState<MemoryScope>('user')
  const [newGroupId, setNewGroupId] = useState('')
  const [error, setError]           = useState('')
  const [historyFor, setHistoryFor] = useState<number | null>(null)
  // In-app confirm (replaces native confirm) + an Undo toast after a delete.
  const [confirmAsk, setConfirmAsk] = useState<{ message: string; confirmLabel: string; onConfirm: () => void } | null>(null)
  const [undoId, setUndoId]         = useState<number | null>(null)

  // The Undo toast auto-dismisses after a few seconds.
  useEffect(() => {
    if (undoId == null) return
    const t = setTimeout(() => setUndoId(null), 7000)
    return () => clearTimeout(t)
  }, [undoId])

  const { data: myGroups = [] } = useQuery<Group[]>({
    queryKey: ['my-groups'],
    queryFn:  groupsApi.listMine,
  })

  // Server-side kind split: the DB decides saved-vs-summary, so the counts are
  // correct past the page size (no more client-side filtering of a capped list).
  const activeKind: 'saved' | 'summary' = tab === 'summaries' ? 'summary' : 'saved'
  const queryParams = {
    q:        search.trim() || undefined,
    category: tab === 'saved' && category !== 'all' ? category : undefined,
    source:   origin !== 'all' ? origin : undefined,
    sort,
    kind:     activeKind,
    limit:    500,
  }

  const { data: page, isLoading } = useQuery({
    queryKey: ['memory', queryParams],
    queryFn:  () => memoryApi.listWithTotal(queryParams),
    refetchOnWindowFocus: true, // reflect changes made in chat (memory_forget etc.)
  })
  const shown = page?.items ?? []

  // Unfiltered per-kind totals drive the tab labels (cheap limit-1 probes that
  // read X-Total-Count), so each tab shows the true count, not the filtered view.
  const { data: savedTotal = 0 } = useQuery({
    queryKey: ['memory-total', 'saved'],
    queryFn:  () => memoryApi.listWithTotal({ kind: 'saved', limit: 1 }).then((r) => r.total),
    refetchOnWindowFocus: true,
  })
  const { data: summaryTotal = 0 } = useQuery({
    queryKey: ['memory-total', 'summary'],
    queryFn:  () => memoryApi.listWithTotal({ kind: 'summary', limit: 1 }).then((r) => r.total),
    refetchOnWindowFocus: true,
  })

  const { data: historyChain = [], isLoading: historyLoading } = useQuery({
    queryKey: ['memory-history', historyFor],
    queryFn:  () => memoryApi.history(historyFor!),
    enabled:  historyFor != null,
  })

  const createMut = useMutation({
    mutationFn: () => memoryApi.create({
      content:  newContent.trim(),
      category: newCategory,
      tags:     newTags.split(',').map((t) => t.trim()).filter(Boolean),
      scope:    newScope,
      scope_id: newScope === 'group' ? newGroupId : undefined,
    }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['memory'] })
      qc.invalidateQueries({ queryKey: ['memory-total'] })
      setShowCreate(false); setNewContent(''); setNewTags(''); setNewScope('user'); setNewGroupId(''); setError('')
    },
    onError: (err: unknown) => {
      const data = (err as { response?: { data?: { error?: string } } })?.response?.data
      setError(data?.error ?? 'Create failed')
    },
  })

  const supersedeMut = useMutation({
    mutationFn: ({ id, content, cat, tags }: { id: number; content: string; cat: string; tags: string }) =>
      memoryApi.supersede(id, { content, category: cat, tags: tags.split(',').map((t) => t.trim()).filter(Boolean) }),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ['memory'] }); setEditId(null) },
    onError:   () => setError('Edit failed'),
  })

  const deleteMut = useMutation({
    mutationFn: (id: number) => memoryApi.delete(id),
    onSuccess:  (_r, id) => {
      qc.invalidateQueries({ queryKey: ['memory'] })
      qc.invalidateQueries({ queryKey: ['memory-total'] })
      setUndoId(id) // offer Undo
    },
    onError:    () => setError('Delete failed'),
  })

  const restoreMut = useMutation({
    mutationFn: (id: number) => memoryApi.restore(id),
    onSuccess:  () => {
      qc.invalidateQueries({ queryKey: ['memory'] })
      qc.invalidateQueries({ queryKey: ['memory-total'] })
      setUndoId(null)
    },
    onError:    () => setError('Undo failed — the memory may already be gone.'),
  })

  const deleteAllMut = useMutation({
    mutationFn: (hard: boolean) => memoryApi.deleteMine(hard),
    onSuccess:  (r) => {
      qc.invalidateQueries({ queryKey: ['memory'] })
      qc.invalidateQueries({ queryKey: ['memory-total'] })
      setError(`Deleted ${r.deleted} of your memories.`)
    },
    onError:    () => setError('Delete-all failed'),
  })

  const startEdit = (m: MemoryItem) => {
    setEditId(m.id); setEditContent(m.content); setEditCategory(m.category); setEditTags(userTags(m.tags).join(', '))
  }
  const commitEdit = () => {
    if (editId == null) return
    supersedeMut.mutate({ id: editId, content: editContent, cat: editCategory, tags: editTags })
  }
  const doExport = async (format: 'json' | 'md') => {
    try {
      const blob = await memoryApi.exportBlob(format)
      const url = URL.createObjectURL(blob)
      const a = document.createElement('a')
      a.href = url; a.download = `mira-memory.${format === 'md' ? 'md' : 'json'}`
      document.body.appendChild(a); a.click(); a.remove(); URL.revokeObjectURL(url)
    } catch { setError('Export failed') }
  }
  // A user may delete their own user-scoped memory; admins any; group creators theirs.
  const canDelete = (m: MemoryItem) =>
    m.scope === 'user' || isAdmin || m.created_by === user?.id

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1>Memory</h1>
          <p>What MIRA remembers about you · {shown.length} item{shown.length !== 1 ? 's' : ''}</p>
        </div>
        <div className={styles.headerActions}>
          <button className={styles.btnSecondary} onClick={() => doExport('json')} title="Download everything as JSON">
            <Download size={14} /> JSON
          </button>
          <button className={styles.btnSecondary} onClick={() => doExport('md')} title="Download everything as Markdown">
            <Download size={14} /> Markdown
          </button>
          <button className={styles.btn} onClick={() => setShowCreate(true)}>
            <Plus size={15} /> Add memory
          </button>
        </div>
      </div>

      {/* Tabs */}
      <div className={styles.tabs} role="tablist">
        {([['saved', `Saved (${savedTotal})`], ['summaries', `Daily summaries (${summaryTotal})`], ['sources', 'Connected sources']] as const).map(([t, label]) => (
          <button
            key={t}
            role="tab"
            aria-selected={tab === t}
            className={`${styles.tab} ${tab === t ? styles.tabActive : ''}`}
            onClick={() => setTab(t)}
          >{label}</button>
        ))}
      </div>

      {error && <p className={styles.error} role="status">{error}</p>}

      {tab === 'sources' ? (
        <MemoryCatalog />
      ) : (
        <>
          {/* Search + filters (Saved + Summaries) */}
          <div className={styles.toolbar}>
            <div className={styles.searchBox}>
              <Search size={14} className={styles.searchIcon} />
              <input
                className={styles.searchInput}
                placeholder="Search memories…"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                aria-label="Search memories"
              />
            </div>
            {tab === 'saved' && (
              <div className={styles.chips}>
                {CATEGORIES.map((cat) => (
                  <button key={cat} className={`${styles.chip} ${category === cat ? styles.chipActive : ''}`} onClick={() => setCategory(cat)}>
                    {catLabel(cat)}
                  </button>
                ))}
              </div>
            )}
            <div className={styles.chips}>
              {ORIGINS.map((o) => (
                <button key={o.value} className={`${styles.chip} ${origin === o.value ? styles.chipActive : ''}`} onClick={() => setOrigin(o.value)}>
                  {o.label}
                </button>
              ))}
            </div>
            <div className={styles.chips}>
              {SORTS.map((s) => (
                <button key={s.value} className={`${styles.chip} ${sort === s.value ? styles.chipActive : ''}`} onClick={() => setSort(s.value)}>
                  {s.label}
                </button>
              ))}
            </div>
          </div>

          {/* Create form */}
          {showCreate && (
            <div className={styles.createForm}>
              <textarea className={styles.textarea} placeholder="Memory content…" value={newContent} onChange={(e) => setNewContent(e.target.value)} rows={3} autoFocus />
              <div className={styles.formRow}>
                <select className={styles.select} value={newCategory} onChange={(e) => setNewCategory(e.target.value)}>
                  {CATEGORIES.filter((c) => c !== 'all').map((c) => <option key={c} value={c}>{catLabel(c)}</option>)}
                </select>
                <select className={styles.select} value={newScope} onChange={(e) => { setNewScope(e.target.value as MemoryScope); setNewGroupId('') }}>
                  <option value="user">Private (you)</option>
                  <option value="group" disabled={myGroups.length === 0}>{myGroups.length === 0 ? 'Group (no groups yet)' : 'Group'}</option>
                  {isAdmin && <option value="system">System-wide</option>}
                </select>
                {newScope === 'group' && (
                  <select className={styles.select} value={newGroupId} onChange={(e) => setNewGroupId(e.target.value)}>
                    <option value="">Select group…</option>
                    {myGroups.map((g) => <option key={g.id} value={g.id}>{g.name}</option>)}
                  </select>
                )}
                <input className={styles.input} placeholder="Tags (comma-separated)" value={newTags} onChange={(e) => setNewTags(e.target.value)} />
              </div>
              <div className={styles.formActions}>
                <button className={styles.btnSecondary} onClick={() => { setShowCreate(false); setError('') }}>Cancel</button>
                <button className={styles.btn} disabled={createMut.isPending || !newContent.trim() || (newScope === 'group' && !newGroupId)} onClick={() => createMut.mutate()}>
                  {createMut.isPending ? 'Saving…' : 'Save'}
                </button>
              </div>
            </div>
          )}

          {/* List */}
          <div className={styles.list}>
            {isLoading && <p className={styles.empty}>Loading…</p>}
            {!isLoading && shown.length === 0 && (
              <div className={styles.emptyState}>
                <Brain size={40} />
                <p>{tab === 'summaries' ? 'No daily summaries yet.' : `No memories yet${search ? ` matching "${search}"` : ''}.`}</p>
              </div>
            )}

            {shown.map((m) => (
              <div key={m.id} className={styles.card}>
                {editId === m.id ? (
                  <div className={styles.editBody}>
                    <textarea className={styles.textarea} value={editContent} onChange={(e) => setEditContent(e.target.value)} rows={3} />
                    <p className={styles.hint}>Saving creates a new version that supersedes this one; the old version is kept (View history).</p>
                    <div className={styles.formRow}>
                      <select className={styles.select} value={editCategory} onChange={(e) => setEditCategory(e.target.value)}>
                        {CATEGORIES.filter((c) => c !== 'all').map((c) => <option key={c} value={c}>{catLabel(c)}</option>)}
                      </select>
                      <input className={styles.input} value={editTags} onChange={(e) => setEditTags(e.target.value)} placeholder="Tags" />
                    </div>
                    <div className={styles.editActions}>
                      <button className={styles.iconBtn} onClick={() => setEditId(null)} aria-label="Cancel edit"><X size={14} /></button>
                      <button className={styles.iconBtnOk} onClick={commitEdit} aria-label="Save new version" disabled={supersedeMut.isPending}><Check size={14} /></button>
                    </div>
                  </div>
                ) : (
                  <>
                    <div className={styles.cardContent}>{m.content}</div>
                    <div className={styles.cardMeta}>
                      {tab === 'saved' && <span className={styles.catBadge} data-cat={m.category}>{catLabel(m.category)}</span>}
                      <ScopeBadge scope={m.scope} scopeId={m.scope_id} groups={myGroups} />
                      {userTags(m.tags).map((t) => <span key={t} className={styles.tag}>{t}</span>)}
                    </div>
                    {/* Provenance (quiet meta line) */}
                    <div className={styles.provenance}>
                      {originLabel(m)}
                      {m.source_channel ? ` · ${m.source_channel}` : ''}
                      {' · '}{formatDistanceToNow(new Date(m.created_at), { addSuffix: true })}
                      {m.supersedes != null && ' · revised'}
                      {m.source_conversation_id && (
                        <> · <Link to={`/chat/${m.source_conversation_id}`} className={styles.provLink}>view chat</Link></>
                      )}
                    </div>
                    <div className={styles.cardActions}>
                      {m.supersedes != null && (
                        <button className={styles.iconBtn} onClick={() => setHistoryFor(m.id)} aria-label="View history" title="View history">
                          <History size={13} />
                        </button>
                      )}
                      <button className={styles.iconBtn} onClick={() => startEdit(m)} aria-label="Edit (supersede)" title="Edit">
                        <Pencil size={13} />
                      </button>
                      {canDelete(m) && (
                        <button
                          className={`${styles.iconBtn} ${styles.danger}`}
                          onClick={() => setConfirmAsk({
                            message: 'Delete this memory? It is hidden from recall immediately, then permanently purged after the retention window (30 days by default). You can Undo right after.',
                            confirmLabel: 'Delete',
                            onConfirm: () => deleteMut.mutate(m.id),
                          })}
                          aria-label="Delete memory" title="Delete"
                        >
                          <Trash2 size={13} />
                        </button>
                      )}
                    </div>
                  </>
                )}
              </div>
            ))}
          </div>

          {/* Delete-all (own memories) */}
          {tab === 'saved' && savedTotal > 0 && (
            <div className={styles.dangerZone}>
              <button
                className={styles.dangerLink}
                disabled={deleteAllMut.isPending}
                onClick={() => setConfirmAsk({
                  message: 'Delete ALL of your own memories? They are hidden from recall immediately, then permanently purged after the retention window (30 days by default). This clears the built-in store — copies held in a connected Files or external memory source, or in the wiki, are not affected.',
                  confirmLabel: 'Delete all',
                  onConfirm: () => deleteAllMut.mutate(false),
                })}
              >
                {deleteAllMut.isPending ? 'Deleting…' : 'Delete all my memories'}
              </button>
              <button
                className={styles.dangerLink}
                disabled={deleteAllMut.isPending}
                onClick={() => setConfirmAsk({
                  message: 'Permanently erase ALL of your own memories right now, with no retention window? This cannot be undone. It clears the built-in store — copies in a connected Files or external source, or the wiki, are not affected.',
                  confirmLabel: 'Erase permanently',
                  onConfirm: () => deleteAllMut.mutate(true),
                })}
              >
                Erase permanently now
              </button>
            </div>
          )}
        </>
      )}

      {/* Confirm modal (replaces native confirm) */}
      {confirmAsk && (
        <div className={styles.modalOverlay} onClick={() => setConfirmAsk(null)}>
          <div className={styles.modal} onClick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
            <p style={{ fontSize: 14, lineHeight: 1.5, color: 'var(--text-primary)', marginBottom: 14 }}>{confirmAsk.message}</p>
            <div className={styles.formActions}>
              <button className={styles.btnSecondary} onClick={() => setConfirmAsk(null)}>Cancel</button>
              <button
                className={styles.btnDanger}
                onClick={() => { confirmAsk.onConfirm(); setConfirmAsk(null) }}
                autoFocus
              >{confirmAsk.confirmLabel}</button>
            </div>
          </div>
        </div>
      )}

      {/* Undo toast after a single delete */}
      {undoId != null && (
        <div className={styles.undoToast} role="status">
          <span>Memory deleted.</span>
          <button
            className={styles.undoBtn}
            disabled={restoreMut.isPending}
            onClick={() => restoreMut.mutate(undoId)}
          >{restoreMut.isPending ? 'Undoing…' : 'Undo'}</button>
        </div>
      )}

      {/* History modal */}
      {historyFor != null && (
        <div className={styles.modalOverlay} onClick={() => setHistoryFor(null)}>
          <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
            <div className={styles.modalHead}>
              <strong>Version history</strong>
              <button className={styles.iconBtn} onClick={() => setHistoryFor(null)} aria-label="Close"><X size={14} /></button>
            </div>
            {historyLoading && <p className={styles.empty}>Loading…</p>}
            {!historyLoading && historyChain.length === 0 && <p className={styles.empty}>No earlier versions.</p>}
            <ol className={styles.history}>
              {historyChain.map((h, i) => (
                <li key={h.id} className={styles.historyItem} data-current={i === historyChain.length - 1}>
                  <div className={styles.cardContent}>{h.content}</div>
                  <div className={styles.provenance}>
                    {i === historyChain.length - 1 ? 'current' : `version ${i + 1}`}
                    {' · '}{formatDistanceToNow(new Date(h.created_at), { addSuffix: true })}
                  </div>
                </li>
              ))}
            </ol>
          </div>
        </div>
      )}
    </div>
  )
}
