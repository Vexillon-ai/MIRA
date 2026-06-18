// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Plus, Trash2, Users as UsersIcon, UserPlus, X, ShieldCheck } from 'lucide-react'
import { groupsApi } from '@/api/groups'
import { api } from '@/api/client'
import type { Group, GroupMember, User, CreateGroupRequest } from '@/api/types'
import { formatDistanceToNow } from 'date-fns'
import CapabilityEditor from '@/components/CapabilityEditor'
import styles from './GroupsPage.module.css'

export default function GroupsPage() {
  const qc = useQueryClient()
  const [showCreate, setShowCreate]   = useState(false)
  const [form, setForm]               = useState<CreateGroupRequest>({ name: '', description: '' })
  const [createError, setCreateError] = useState('')
  const [expanded, setExpanded]       = useState<string | null>(null)
  const [capsFor, setCapsFor]         = useState<Group | null>(null)

  const { data: groups = [], isLoading } = useQuery<Group[]>({
    queryKey: ['groups'],
    queryFn:  groupsApi.list,
  })

  const createMut = useMutation({
    mutationFn: (data: CreateGroupRequest) => groupsApi.create(data),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['groups'] })
      setShowCreate(false)
      setForm({ name: '', description: '' })
      setCreateError('')
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: string } })?.response?.data ?? 'Create failed'
      setCreateError(typeof msg === 'string' ? msg : 'Create failed')
    },
  })

  const deleteMut = useMutation({
    mutationFn: (id: string) => groupsApi.delete(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['groups'] }),
  })

  if (isLoading) return <div className={styles.loading}>Loading groups…</div>

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1>Groups</h1>
          <p>{groups.length} group{groups.length !== 1 ? 's' : ''} — shared memory containers</p>
        </div>
        <button className={styles.btn} onClick={() => setShowCreate(true)}>
          <Plus size={15} />
          New group
        </button>
      </div>

      {showCreate && (
        <div className={styles.createForm}>
          <h3>New Group</h3>
          <div className={styles.formRow}>
            <input
              className={styles.input}
              placeholder="Group name"
              value={form.name}
              onChange={(e) => setForm((f) => ({ ...f, name: e.target.value }))}
            />
            <input
              className={styles.input}
              placeholder="Description (optional)"
              value={form.description ?? ''}
              onChange={(e) => setForm((f) => ({ ...f, description: e.target.value || undefined }))}
            />
          </div>
          {createError && <p className={styles.error}>{createError}</p>}
          <div className={styles.formActions}>
            <button className={styles.btnSecondary} onClick={() => setShowCreate(false)}>Cancel</button>
            <button
              className={styles.btn}
              disabled={createMut.isPending || !form.name.trim()}
              onClick={() => createMut.mutate(form)}
            >
              {createMut.isPending ? 'Creating…' : 'Create'}
            </button>
          </div>
        </div>
      )}

      <div className={styles.list}>
        {groups.length === 0 && !showCreate && (
          <p className={styles.empty}>
            No groups yet. Groups are shared memory containers — create one to scope memories
            across a set of users.
          </p>
        )}
        {groups.map((group) => (
          <div key={group.id} className={styles.groupCard}>
            <div
              className={styles.groupHeader}
              onClick={() => setExpanded((e) => (e === group.id ? null : group.id))}
            >
              <div className={styles.avatar}><UsersIcon size={15} /></div>
              <div className={styles.info}>
                <span className={styles.name}>{group.name}</span>
                <span className={styles.meta}>
                  {group.description && <>{group.description} · </>}
                  Created {formatDistanceToNow(new Date(group.created_at), { addSuffix: true })}
                </span>
              </div>
              <button
                className={styles.iconBtn}
                onClick={(e) => { e.stopPropagation(); setCapsFor(group) }}
                title="Capabilities (RBAC)"
              >
                <ShieldCheck size={15} />
              </button>
              <button
                className={`${styles.iconBtn} ${styles.danger}`}
                onClick={(e) => {
                  e.stopPropagation()
                  if (confirm(`Delete group "${group.name}"? Memberships will be removed.`)) {
                    deleteMut.mutate(group.id)
                  }
                }}
                title="Delete"
              >
                <Trash2 size={15} />
              </button>
            </div>

            {expanded === group.id && <MemberPanel groupId={group.id} />}
          </div>
        ))}
      </div>

      {capsFor && (
        <CapabilityEditor
          scope="group"
          id={capsFor.id}
          name={capsFor.name}
          onClose={() => setCapsFor(null)}
        />
      )}
    </div>
  )
}

function MemberPanel({ groupId }: { groupId: string }) {
  const qc = useQueryClient()
  const [selectedUser, setSelectedUser] = useState('')

  const { data: members = [] } = useQuery<GroupMember[]>({
    queryKey: ['group-members', groupId],
    queryFn:  () => groupsApi.listMembers(groupId),
  })

  const { data: allUsers = [] } = useQuery<User[]>({
    queryKey: ['users'],
    queryFn:  () => api.get('/api/users').then((r) => r.data),
  })

  const addMut = useMutation({
    mutationFn: (userId: string) => groupsApi.addMember(groupId, userId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['group-members', groupId] })
      setSelectedUser('')
    },
  })

  const removeMut = useMutation({
    mutationFn: (userId: string) => groupsApi.removeMember(groupId, userId),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['group-members', groupId] }),
  })

  const memberIds = new Set(members.map((m) => m.id))
  const candidates = allUsers.filter((u) => !memberIds.has(u.id) && u.is_active)

  return (
    <div className={styles.memberPanel}>
      <div className={styles.memberHead}>
        <span>{members.length} member{members.length !== 1 ? 's' : ''}</span>
        <div className={styles.addRow}>
          <select
            className={styles.select}
            value={selectedUser}
            onChange={(e) => setSelectedUser(e.target.value)}
          >
            <option value="">Add user…</option>
            {candidates.map((u) => (
              <option key={u.id} value={u.id}>
                {u.display_name ?? u.username} (@{u.username})
              </option>
            ))}
          </select>
          <button
            className={styles.btnSmall}
            disabled={!selectedUser || addMut.isPending}
            onClick={() => addMut.mutate(selectedUser)}
          >
            <UserPlus size={13} />
            Add
          </button>
        </div>
      </div>

      {members.length === 0 ? (
        <p className={styles.emptyInline}>No members yet.</p>
      ) : (
        <ul className={styles.memberList}>
          {members.map((m) => (
            <li key={m.id} className={styles.memberItem}>
              <span className={styles.memberAvatar}>{m.username[0]?.toUpperCase() ?? '?'}</span>
              <span className={styles.memberName}>
                {m.display_name ?? m.username}
                <span className={styles.memberHandle}>@{m.username}</span>
              </span>
              <span className={styles.memberRole} data-role={m.role}>{m.role}</span>
              <button
                className={`${styles.iconBtn} ${styles.danger}`}
                onClick={() => removeMut.mutate(m.id)}
                title="Remove member"
              >
                <X size={14} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}
