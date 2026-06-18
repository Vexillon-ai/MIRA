// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { useNavigate } from 'react-router-dom'
import { UserPlus, Trash2, Shield, User as UserIcon, KeyRound, Copy, Pencil, Calendar, ShieldCheck, Mail, Check, X, LogOut } from 'lucide-react'
import { api } from '@/api/client'
import { invitesApi, type CreateInviteResponse } from '@/api/invites'
import type { User, CreateUserRequest, ResetPasswordResponse } from '@/api/types'
import { formatDistanceToNow } from 'date-fns'
import Avatar from '@/components/Avatar'
import UserEditDialog from '@/components/UserEditDialog'
import CapabilityEditor from '@/components/CapabilityEditor'
import styles from './UsersPage.module.css'

export default function UsersPage() {
  const qc = useQueryClient()
  const navigate = useNavigate()
  const [showCreate, setShowCreate] = useState(false)
  const [form, setForm] = useState<CreateUserRequest>({ username: '', password: '', role: 'user' })
  const [createError, setCreateError] = useState('')
  const [resetResult, setResetResult] = useState<{ userId: string; password: string } | null>(null)
  const [editing, setEditing] = useState<User | null>(null)
  const [capsFor, setCapsFor] = useState<User | null>(null)
  const [showInvite, setShowInvite] = useState(false)
  const [inviteResult, setInviteResult] = useState<CreateInviteResponse | null>(null)

  const { data: users = [], isLoading } = useQuery<User[]>({
    queryKey: ['users'],
    queryFn: () => api.get('/api/users').then((r) => r.data),
  })

  const { data: pending = [] } = useQuery<User[]>({
    queryKey: ['pending-users'],
    queryFn:  invitesApi.pending,
  })

  const approveMut = useMutation({
    mutationFn: (id: string) => invitesApi.approve(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['pending-users'] })
      qc.invalidateQueries({ queryKey: ['users'] })
    },
  })

  const createMut = useMutation({
    mutationFn: (data: CreateUserRequest) => api.post('/api/users', data).then((r) => r.data),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['users'] })
      setShowCreate(false)
      setForm({ username: '', password: '', role: 'user' })
      setCreateError('')
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error ?? 'Create failed'
      setCreateError(msg)
    },
  })

  const deleteMut = useMutation({
    mutationFn: (id: string) => api.delete(`/api/users/${id}`),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['users'] }),
  })

  const resetMut = useMutation({
    mutationFn: (id: string) =>
      api.post<ResetPasswordResponse>(`/api/users/${id}/reset-password`).then((r) => r.data),
    onSuccess: (data, id) => {
      setResetResult({ userId: id, password: data.new_password })
    },
  })

  const revokeMut = useMutation({
    mutationFn: (id: string) =>
      api.post<{ revoked: number }>(`/api/users/${id}/revoke-sessions`).then((r) => r.data),
    onSuccess: (data) => {
      alert(`Signed out ${data.revoked} session${data.revoked !== 1 ? 's' : ''}. The user is locked out within ~15 minutes.`)
    },
  })

  if (isLoading) return <div className={styles.loading}>Loading users…</div>

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1>Users</h1>
          <p>{users.length} user{users.length !== 1 ? 's' : ''}</p>
        </div>
        <div style={{ display: 'flex', gap: 8 }}>
          <button className={styles.btnSecondary} onClick={() => { setInviteResult(null); setShowInvite(true) }}>
            <Mail size={15} />
            Invite
          </button>
          <button className={styles.btn} onClick={() => setShowCreate(true)}>
            <UserPlus size={15} />
            Add user
          </button>
        </div>
      </div>

      {pending.length > 0 && (
        <div className={styles.pendingPanel}>
          <h3>{pending.length} pending approval{pending.length !== 1 ? 's' : ''}</h3>
          {pending.map((u) => (
            <div key={u.id} className={styles.pendingRow}>
              <span className={styles.pendingName}>
                {u.display_name ?? u.username} <span className={styles.nameHint}>@{u.username}</span>
                {u.email && <span className={styles.nameHint}> · {u.email}</span>}
              </span>
              <button
                className={styles.approveBtn}
                onClick={() => approveMut.mutate(u.id)}
                disabled={approveMut.isPending}
                title="Approve"
              >
                <Check size={14} /> Approve
              </button>
              <button
                className={`${styles.iconBtn} ${styles.danger}`}
                onClick={() => { if (confirm(`Reject and delete "${u.username}"?`)) deleteMut.mutate(u.id) }}
                title="Reject"
              >
                <X size={15} />
              </button>
            </div>
          ))}
        </div>
      )}

      {showInvite && (
        <InviteModal
          onClose={() => setShowInvite(false)}
          result={inviteResult}
          onCreated={setInviteResult}
        />
      )}

      {resetResult && (
        <div className={styles.resetBanner}>
          <span>New password for user:</span>
          <code className={styles.resetCode}>{resetResult.password}</code>
          <button
            className={styles.iconBtn}
            onClick={() => navigator.clipboard.writeText(resetResult.password)}
            title="Copy"
          >
            <Copy size={14} />
          </button>
          <button className={styles.btnSecondary} onClick={() => setResetResult(null)}>Dismiss</button>
        </div>
      )}

      {showCreate && (
        <div className={styles.createForm}>
          <h3>New User</h3>
          <div className={styles.formRow}>
            <input
              className={styles.input}
              placeholder="Username"
              value={form.username}
              onChange={(e) => setForm((f) => ({ ...f, username: e.target.value }))}
            />
            <input
              className={styles.input}
              type="password"
              placeholder="Password"
              value={form.password}
              onChange={(e) => setForm((f) => ({ ...f, password: e.target.value }))}
            />
            <input
              className={styles.input}
              placeholder="Display name (optional)"
              value={form.display_name ?? ''}
              onChange={(e) => setForm((f) => ({ ...f, display_name: e.target.value || undefined }))}
            />
            <input
              className={styles.input}
              type="email"
              placeholder="Email (optional)"
              value={form.email ?? ''}
              onChange={(e) => setForm((f) => ({ ...f, email: e.target.value || undefined }))}
            />
            <select
              className={styles.select}
              value={form.role}
              onChange={(e) => setForm((f) => ({ ...f, role: e.target.value as 'admin' | 'user' }))}
            >
              <option value="user">User</option>
              <option value="admin">Admin</option>
            </select>
          </div>
          {createError && <p className={styles.error}>{createError}</p>}
          <div className={styles.formActions}>
            <button className={styles.btnSecondary} onClick={() => setShowCreate(false)}>Cancel</button>
            <button
              className={styles.btn}
              disabled={createMut.isPending || !form.username || !form.password}
              onClick={() => createMut.mutate(form)}
            >
              {createMut.isPending ? 'Creating…' : 'Create'}
            </button>
          </div>
        </div>
      )}

      <div className={styles.list}>
        {users.map((user) => (
          <div key={user.id} className={styles.item} data-inactive={!user.is_active || undefined}>
            <div className={styles.avatar}>
              <Avatar user={user} size={36} />
            </div>
            <div className={styles.info}>
              <span className={styles.name}>
                {user.display_name ?? user.username}
                {user.display_name && <span className={styles.nameHint}> @{user.username}</span>}
              </span>
              <span className={styles.meta}>
                {user.email && <>{user.email} · </>}
                Created {formatDistanceToNow(new Date(user.created_at), { addSuffix: true })}
                {user.last_login && <> · Last login {formatDistanceToNow(new Date(user.last_login), { addSuffix: true })}</>}
                {!user.is_active && <> · <span className={styles.inactive}>inactive</span></>}
              </span>
            </div>
            <div className={styles.roleBadge} data-role={user.role}>
              {user.role === 'admin' ? <Shield size={12} /> : <UserIcon size={12} />}
              {user.role}
            </div>
            <button
              className={styles.iconBtn}
              onClick={() => navigate(`/calendar?userId=${encodeURIComponent(user.id)}`)}
              title="View calendar"
            >
              <Calendar size={15} />
            </button>
            <button
              className={styles.iconBtn}
              onClick={() => setEditing(user)}
              title="Edit user"
            >
              <Pencil size={15} />
            </button>
            <button
              className={styles.iconBtn}
              onClick={() => setCapsFor(user)}
              title="Capabilities (RBAC)"
            >
              <ShieldCheck size={15} />
            </button>
            <button
              className={styles.iconBtn}
              onClick={() => {
                if (confirm(`Reset password for "${user.username}"?`)) resetMut.mutate(user.id)
              }}
              title="Reset password"
              disabled={resetMut.isPending}
            >
              <KeyRound size={15} />
            </button>
            <button
              className={styles.iconBtn}
              onClick={() => {
                if (confirm(`Sign "${user.username}" out of all sessions? They'll be logged out everywhere within ~15 minutes.`)) revokeMut.mutate(user.id)
              }}
              title="Sign out everywhere"
              disabled={revokeMut.isPending}
            >
              <LogOut size={15} />
            </button>
            <button
              className={`${styles.iconBtn} ${styles.danger}`}
              onClick={() => {
                if (confirm(`Delete user "${user.username}"?`)) deleteMut.mutate(user.id)
              }}
              title="Delete"
            >
              <Trash2 size={15} />
            </button>
          </div>
        ))}
      </div>

      <UserEditDialog
        open={editing !== null}
        user={editing}
        onClose={() => setEditing(null)}
      />

      {capsFor && (
        <CapabilityEditor
          scope="user"
          id={capsFor.id}
          name={capsFor.display_name ?? capsFor.username}
          onClose={() => setCapsFor(null)}
        />
      )}
    </div>
  )
}

function InviteModal({
  onClose, result, onCreated,
}: {
  onClose: () => void
  result: CreateInviteResponse | null
  onCreated: (r: CreateInviteResponse) => void
}) {
  const [role, setRole] = useState('user')
  const [maxUses, setMaxUses] = useState(1)
  const [expiresHours, setExpiresHours] = useState(168) // 7 days
  const [copied, setCopied] = useState(false)

  const createMut = useMutation({
    mutationFn: () => invitesApi.create({
      role,
      max_uses: maxUses,
      expires_in_hours: expiresHours || undefined,
    }),
    onSuccess: onCreated,
  })

  const fullUrl = result ? `${window.location.origin}${result.url}` : ''

  return (
    <div className={styles.overlay} onClick={onClose}>
      <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
        <div className={styles.modalHead}>
          <h3>Invite a user</h3>
          <button className={styles.iconBtn} onClick={onClose}><X size={16} /></button>
        </div>

        {result ? (
          <div className={styles.inviteResult}>
            <p>Share this single link with the person you're inviting. It works once
            {result.role === 'admin' ? ' and grants admin' : ''}:</p>
            <div className={styles.linkRow}>
              <code className={styles.linkCode}>{fullUrl}</code>
              <button
                className={styles.btn}
                onClick={() => { navigator.clipboard.writeText(fullUrl); setCopied(true) }}
              >
                {copied ? 'Copied' : 'Copy'}
              </button>
            </div>
            <button className={styles.btnSecondary} onClick={onClose}>Done</button>
          </div>
        ) : (
          <div className={styles.inviteForm}>
            <label className={styles.invLabel}>
              Role
              <select className={styles.select} value={role} onChange={(e) => setRole(e.target.value)}>
                <option value="user">User</option>
                <option value="admin">Admin</option>
              </select>
            </label>
            <label className={styles.invLabel}>
              Max uses
              <input type="number" min={1} className={styles.input}
                value={maxUses} onChange={(e) => setMaxUses(Math.max(1, +e.target.value))} />
            </label>
            <label className={styles.invLabel}>
              Expires in (hours, 0 = never)
              <input type="number" min={0} className={styles.input}
                value={expiresHours} onChange={(e) => setExpiresHours(Math.max(0, +e.target.value))} />
            </label>
            <button className={styles.btn} disabled={createMut.isPending} onClick={() => createMut.mutate()}>
              {createMut.isPending ? 'Creating…' : 'Create invite link'}
            </button>
          </div>
        )}
      </div>
    </div>
  )
}
