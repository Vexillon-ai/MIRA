// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Brain, Plus, Trash2, Pencil, Check, X, Search, User as UserIcon, Users as UsersIcon, Globe } from 'lucide-react'
import { memoryApi, type MemoryItem, type MemoryScope, type MemorySort } from '@/api/memory'
import { groupsApi } from '@/api/groups'
import { useAuthStore } from '@/store/authStore'
import type { Group } from '@/api/types'
import { formatDistanceToNow } from 'date-fns'
import styles from './MemoryPage.module.css'

const CATEGORIES = ['all', 'fact', 'preference', 'skill', 'relationship', 'project'] as const
type CategoryFilter = (typeof CATEGORIES)[number]

const SCOPES = ['all', 'user', 'group', 'system'] as const
type ScopeFilter = (typeof SCOPES)[number]

const SORTS: { value: MemorySort; label: string }[] = [
  { value: 'strength', label: 'Strength' },
  { value: 'recent',   label: 'Recent'   },
]

function StrengthBar({ value, stability }: { value: number; stability: string }) {
  const pct = Math.max(0, Math.min(1, value)) * 100
  const title = `Effective strength: ${(value * 100).toFixed(0)}% (${stability})`
  return (
    <span className={styles.strengthWrap} title={title} data-stability={stability}>
      <span className={styles.strengthFill} style={{ width: `${pct}%` }} />
    </span>
  )
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
  const label = scope === 'group' && groupName ? `group: ${groupName}` : scope

  return (
    <span className={styles.scopeBadge} data-scope={scope} title={`Scope: ${label}`}>
      {icon}
      {label}
    </span>
  )
}

export default function MemoryPage() {
  const qc   = useQueryClient()
  const { user } = useAuthStore()
  const isAdmin = user?.role === 'admin'

  const [search, setSearch]         = useState('')
  const [category, setCategory]     = useState<CategoryFilter>('all')
  const [scopeFilter, setScopeFilter] = useState<ScopeFilter>('all')
  const [sort, setSort] = useState<MemorySort>('strength')
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

  // Pull caller's groups so we can resolve scope_id and render the badge.
  const { data: myGroups = [] } = useQuery<Group[]>({
    queryKey: ['my-groups'],
    queryFn:  groupsApi.listMine,
  })

  const queryParams = {
    q:        search.trim() || undefined,
    category: category    !== 'all' ? category    : undefined,
    scope:    scopeFilter !== 'all' ? scopeFilter : undefined,
    sort,
    limit:    200,
  }

  const { data: memories = [], isLoading } = useQuery({
    queryKey: ['memory', queryParams],
    queryFn:  () => memoryApi.list(queryParams),
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
      setShowCreate(false)
      setNewContent('')
      setNewTags('')
      setNewScope('user')
      setNewGroupId('')
      setError('')
    },
    onError: (err: unknown) => {
      const data = (err as { response?: { data?: { error?: string } } })?.response?.data
      setError(data?.error ?? 'Create failed')
    },
  })

  // Non-admins edit via supersession (append-only). Admins also use supersede
  // here — direct mutation is intentionally not exposed; use DELETE to prune.
  const supersedeMut = useMutation({
    mutationFn: ({ id, content, cat, tags }: { id: number; content: string; cat: string; tags: string }) =>
      memoryApi.supersede(id, {
        content,
        category: cat,
        tags: tags.split(',').map((t) => t.trim()).filter(Boolean),
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['memory'] })
      setEditId(null)
    },
  })

  const deleteMut = useMutation({
    mutationFn: (id: number) => memoryApi.delete(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['memory'] }),
  })

  const startEdit = (m: MemoryItem) => {
    setEditId(m.id)
    setEditContent(m.content)
    setEditCategory(m.category)
    setEditTags(m.tags.join(', '))
  }

  const commitEdit = () => {
    if (!editId) return
    supersedeMut.mutate({ id: editId, content: editContent, cat: editCategory, tags: editTags })
  }

  return (
    <div className={styles.page}>
      {/* Header */}
      <div className={styles.header}>
        <div>
          <h1>Memory</h1>
          <p>{memories.length} item{memories.length !== 1 ? 's' : ''}{category !== 'all' ? ` · ${category}` : ''}{scopeFilter !== 'all' ? ` · ${scopeFilter} scope` : ''}</p>
        </div>
        <button className={styles.btn} onClick={() => setShowCreate(true)}>
          <Plus size={15} />
          Add memory
        </button>
      </div>

      {/* Search + filter bar */}
      <div className={styles.toolbar}>
        <div className={styles.searchBox}>
          <Search size={14} className={styles.searchIcon} />
          <input
            className={styles.searchInput}
            placeholder="Search memories…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </div>
        <div className={styles.chips}>
          {CATEGORIES.map((cat) => (
            <button
              key={cat}
              className={`${styles.chip} ${category === cat ? styles.chipActive : ''}`}
              onClick={() => setCategory(cat)}
            >
              {cat === 'all' ? 'All' : cat}
            </button>
          ))}
        </div>
        <div className={styles.chips}>
          {SCOPES.map((sc) => (
            <button
              key={sc}
              className={`${styles.chip} ${scopeFilter === sc ? styles.chipActive : ''}`}
              onClick={() => setScopeFilter(sc)}
              title={`Scope: ${sc}`}
            >
              {sc === 'all' ? 'All scopes' : sc}
            </button>
          ))}
        </div>
        <div className={styles.chips}>
          {SORTS.map((s) => (
            <button
              key={s.value}
              className={`${styles.chip} ${sort === s.value ? styles.chipActive : ''}`}
              onClick={() => setSort(s.value)}
              title={s.value === 'strength' ? 'Sort by decay-aware strength' : 'Sort by most recent'}
            >
              {s.label}
            </button>
          ))}
        </div>
      </div>

      {/* Create form */}
      {showCreate && (
        <div className={styles.createForm}>
          <textarea
            className={styles.textarea}
            placeholder="Memory content…"
            value={newContent}
            onChange={(e) => setNewContent(e.target.value)}
            rows={3}
            autoFocus
          />
          <div className={styles.formRow}>
            <select
              className={styles.select}
              value={newCategory}
              onChange={(e) => setNewCategory(e.target.value)}
            >
              {CATEGORIES.filter((c) => c !== 'all').map((c) => (
                <option key={c} value={c}>{c}</option>
              ))}
            </select>
            <select
              className={styles.select}
              value={newScope}
              onChange={(e) => {
                setNewScope(e.target.value as MemoryScope)
                setNewGroupId('')
              }}
            >
              <option value="user">Private (you)</option>
              <option value="group" disabled={myGroups.length === 0}>
                {myGroups.length === 0 ? 'Group (no groups yet)' : 'Group'}
              </option>
              {isAdmin && <option value="system">System-wide</option>}
            </select>
            {newScope === 'group' && (
              <select
                className={styles.select}
                value={newGroupId}
                onChange={(e) => setNewGroupId(e.target.value)}
              >
                <option value="">Select group…</option>
                {myGroups.map((g) => (
                  <option key={g.id} value={g.id}>{g.name}</option>
                ))}
              </select>
            )}
            <input
              className={styles.input}
              placeholder="Tags (comma-separated)"
              value={newTags}
              onChange={(e) => setNewTags(e.target.value)}
            />
          </div>
          {error && <p className={styles.error}>{error}</p>}
          <div className={styles.formActions}>
            <button className={styles.btnSecondary} onClick={() => { setShowCreate(false); setError('') }}>Cancel</button>
            <button
              className={styles.btn}
              disabled={
                createMut.isPending ||
                !newContent.trim() ||
                (newScope === 'group' && !newGroupId)
              }
              onClick={() => createMut.mutate()}
            >
              {createMut.isPending ? 'Saving…' : 'Save'}
            </button>
          </div>
        </div>
      )}

      {/* Memory list */}
      <div className={styles.list}>
        {isLoading && <p className={styles.empty}>Loading…</p>}
        {!isLoading && memories.length === 0 && (
          <div className={styles.emptyState}>
            <Brain size={40} />
            <p>No memories yet{search ? ` matching "${search}"` : ''}.</p>
          </div>
        )}

        {memories.map((m) => (
          <div key={m.id} className={styles.card}>
            {editId === m.id ? (
              <div className={styles.editBody}>
                <textarea
                  className={styles.textarea}
                  value={editContent}
                  onChange={(e) => setEditContent(e.target.value)}
                  rows={3}
                />
                <p className={styles.hint}>
                  Saving creates a new memory that supersedes this one. The older version
                  is preserved for historical weight.
                </p>
                <div className={styles.formRow}>
                  <select
                    className={styles.select}
                    value={editCategory}
                    onChange={(e) => setEditCategory(e.target.value)}
                  >
                    {CATEGORIES.filter((c) => c !== 'all').map((c) => (
                      <option key={c} value={c}>{c}</option>
                    ))}
                  </select>
                  <input
                    className={styles.input}
                    value={editTags}
                    onChange={(e) => setEditTags(e.target.value)}
                    placeholder="Tags"
                  />
                </div>
                <div className={styles.editActions}>
                  <button className={styles.iconBtn} onClick={() => setEditId(null)} title="Cancel">
                    <X size={14} />
                  </button>
                  <button className={styles.iconBtnOk} onClick={commitEdit} title="Save as new version" disabled={supersedeMut.isPending}>
                    <Check size={14} />
                  </button>
                </div>
              </div>
            ) : (
              <>
                <div className={styles.cardContent}>{m.content}</div>
                <div className={styles.cardMeta}>
                  <span className={styles.catBadge} data-cat={m.category}>{m.category}</span>
                  <ScopeBadge scope={m.scope} scopeId={m.scope_id} groups={myGroups} />
                  {m.tags.map((t) => (
                    <span key={t} className={styles.tag}>{t}</span>
                  ))}
                  {m.supersedes != null && (
                    <span className={styles.versionNote} title={`Supersedes memory #${m.supersedes}`}>
                      revised
                    </span>
                  )}
                  <StrengthBar value={m.effective_strength} stability={m.stability} />
                  <span className={styles.time}>
                    {formatDistanceToNow(new Date(m.created_at), { addSuffix: true })}
                  </span>
                </div>
                <div className={styles.cardActions}>
                  <button
                    className={styles.iconBtn}
                    onClick={() => startEdit(m)}
                    title="Add revised version (supersede)"
                  >
                    <Pencil size={13} />
                  </button>
                  {isAdmin && (
                    <button
                      className={`${styles.iconBtn} ${styles.danger}`}
                      onClick={() => { if (confirm('Soft-delete this memory? It will be hidden from all users but kept in the audit log.')) deleteMut.mutate(m.id) }}
                      title="Soft delete (admin)"
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
    </div>
  )
}
