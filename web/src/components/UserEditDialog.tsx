// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useRef, useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { X, Check, Upload, Trash2, Shield, User as UserIcon } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import type { User } from '@/api/types'
import Avatar, { AVATAR_PRESETS } from './Avatar'
import styles from './ProfileDialog.module.css'

interface Props {
  open: boolean
  user: User | null
  onClose: () => void
}

/**
 * Admin-facing edit dialog. Mirrors the shape of ProfileDialog but targets
 * an arbitrary user and exposes role + is_active toggles. Password reset
 * stays on the Users page list (one-click, returns a generated password).
 */
export default function UserEditDialog({ open, user, onClose }: Props) {
  const qc = useQueryClient()

  const [displayName, setDisplayName] = useState('')
  const [email, setEmail]             = useState('')
  const [phone, setPhone]             = useState('')
  const [preferredContact, setPreferredContact] = useState('')
  const [role, setRole]               = useState<'admin' | 'user'>('user')
  const [isActive, setIsActive]       = useState(true)
  const [current, setCurrent]         = useState<User | null>(null)

  useEffect(() => {
    if (!open || !user) return
    setCurrent(user)
    setDisplayName(user.display_name ?? '')
    setEmail(user.email ?? '')
    setPhone(user.phone ?? '')
    setPreferredContact(user.preferred_contact ?? '')
    setRole(user.role)
    setIsActive(user.is_active)
  }, [open, user])

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open, onClose])

  const fileInputRef = useRef<HTMLInputElement>(null)

  const profileMut = useMutation({
    mutationFn: async (body: {
      display_name:      string | null
      email:             string | null
      phone:             string | null
      preferred_contact: string | null
      role:              'admin' | 'user'
      is_active:         boolean
    }) => {
      if (!current) throw new Error('No user')
      const r = await api.put<User>(`/api/users/${current.id}`, body)
      return r.data
    },
    onSuccess: (updated) => {
      setCurrent(updated)
      qc.invalidateQueries({ queryKey: ['users'] })
      toast.success('User updated.')
    },
    onError: (e: unknown) => {
      const msg = (e as { response?: { data?: string } })?.response?.data
      toast.error(msg ? String(msg) : 'Update failed')
    },
  })

  const presetMut = useMutation({
    mutationFn: async (avatar: string | null) => {
      if (!current) throw new Error('No user')
      const r = await api.put<User>(`/api/users/${current.id}`, { avatar })
      return r.data
    },
    onSuccess: (updated) => {
      setCurrent(updated)
      qc.invalidateQueries({ queryKey: ['users'] })
    },
    onError: () => toast.error('Could not update avatar'),
  })

  const uploadMut = useMutation({
    mutationFn: async (file: File) => {
      if (!current) throw new Error('No user')
      const fd = new FormData()
      fd.append('file', file)
      const r = await api.post<User>(`/api/users/${current.id}/avatar`, fd)
      return r.data
    },
    onSuccess: (updated) => {
      setCurrent(updated)
      qc.invalidateQueries({ queryKey: ['users'] })
      toast.success('Avatar uploaded.')
    },
    onError: (e: unknown) => {
      const msg = (e as { response?: { data?: string } })?.response?.data
      toast.error(msg ? String(msg) : 'Upload failed')
    },
  })

  const removeAvatarMut = useMutation({
    mutationFn: async () => {
      if (!current) throw new Error('No user')
      const r = await api.delete<User>(`/api/users/${current.id}/avatar`)
      return r.data
    },
    onSuccess: (updated) => {
      setCurrent(updated)
      qc.invalidateQueries({ queryKey: ['users'] })
    },
    onError: () => toast.error('Could not remove avatar'),
  })

  if (!open || !current) return null

  const dirty =
    (displayName      || '') !== (current.display_name      ?? '') ||
    (email            || '') !== (current.email             ?? '') ||
    (phone            || '') !== (current.phone             ?? '') ||
    (preferredContact || '') !== (current.preferred_contact ?? '') ||
    role      !== current.role ||
    isActive  !== current.is_active

  const currentPresetKey = current.avatar?.startsWith('preset:')
    ? current.avatar.slice('preset:'.length)
    : null
  const hasAvatar = !!current.avatar

  const submit = () => {
    profileMut.mutate({
      display_name:      displayName.trim() || null,
      email:             email.trim()       || null,
      phone:             phone.trim()       || null,
      preferred_contact: preferredContact   || null,
      role,
      is_active:         isActive,
    })
  }

  return (
    <div className={styles.backdrop} onMouseDown={onClose}>
      <div
        className={styles.dialog}
        role="dialog"
        aria-modal="true"
        aria-label={`Edit ${current.username}`}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <header className={styles.header}>
          <h2>Edit user</h2>
          <button className={styles.iconBtn} onClick={onClose} title="Close" aria-label="Close">
            <X size={16} />
          </button>
        </header>

        <div className={styles.body}>
          {/* Identity */}
          <section className={styles.identity}>
            <Avatar user={current} size={72} />
            <div>
              <div className={styles.displayName}>
                {current.display_name ?? current.username}
              </div>
              <div className={styles.loginId} title="Login ID — cannot be changed">
                @{current.username}
                <span className={styles.roleBadge} data-role={current.role}>{current.role}</span>
              </div>
            </div>
          </section>

          {/* Avatar picker */}
          <section className={styles.section}>
            <h3 className={styles.sectionTitle}>Avatar</h3>
            <div className={styles.presetGrid}>
              {AVATAR_PRESETS.map((p) => {
                const { Icon, bg, key, label } = p
                const active = currentPresetKey === key
                return (
                  <button
                    key={key}
                    type="button"
                    className={`${styles.presetCell} ${active ? styles.presetActive : ''}`}
                    style={{ background: bg }}
                    title={label}
                    aria-label={`Use ${label} avatar`}
                    onClick={() => presetMut.mutate(`preset:${key}`)}
                    disabled={presetMut.isPending}
                  >
                    <Icon size={20} color="white" />
                  </button>
                )
              })}
            </div>
            <div className={styles.actions}>
              <button
                className={styles.btnSecondary}
                onClick={() => fileInputRef.current?.click()}
                disabled={uploadMut.isPending}
              >
                <Upload size={14} />
                {uploadMut.isPending ? 'Uploading…' : 'Upload photo'}
              </button>
              {hasAvatar && (
                <button
                  className={styles.btnSecondary}
                  onClick={() => removeAvatarMut.mutate()}
                  disabled={removeAvatarMut.isPending}
                  title="Remove avatar"
                >
                  <Trash2 size={14} />
                  Remove
                </button>
              )}
              <input
                ref={fileInputRef}
                type="file"
                accept="image/png,image/jpeg,image/webp,image/gif"
                style={{ display: 'none' }}
                onChange={(e) => {
                  const f = e.target.files?.[0]
                  if (f) uploadMut.mutate(f)
                  e.target.value = ''
                }}
              />
            </div>
          </section>

          {/* Editable fields */}
          <section className={styles.section}>
            <label className={styles.label}>
              <span>Display name</span>
              <input
                className={styles.input}
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
                placeholder="How their name appears"
              />
            </label>
            <label className={styles.label}>
              <span>Email</span>
              <input
                className={styles.input}
                type="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                placeholder="user@example.com"
              />
            </label>
            <label className={styles.label}>
              <span>Phone</span>
              <input
                className={styles.input}
                type="tel"
                value={phone}
                onChange={(e) => setPhone(e.target.value)}
                placeholder="+1 555 123 4567"
              />
            </label>
            <label className={styles.label}>
              <span>Preferred contact</span>
              <select
                className={styles.input}
                value={preferredContact}
                onChange={(e) => setPreferredContact(e.target.value)}
              >
                <option value="">Don't contact proactively</option>
                <option value="signal">Signal</option>
                <option value="telegram">Telegram</option>
              </select>
            </label>
          </section>

          {/* Admin-only controls */}
          <section className={styles.section}>
            <h3 className={styles.sectionTitle}>Access</h3>
            <label className={styles.label}>
              <span>Role</span>
              <select
                className={styles.input}
                value={role}
                onChange={(e) => setRole(e.target.value as 'admin' | 'user')}
              >
                <option value="user">User</option>
                <option value="admin">Admin</option>
              </select>
            </label>
            <label className={styles.label} style={{ flexDirection: 'row', alignItems: 'center', gap: 8 }}>
              <input
                type="checkbox"
                checked={isActive}
                onChange={(e) => setIsActive(e.target.checked)}
              />
              <span>
                Active
                <span style={{ color: 'var(--text-muted)', marginLeft: 6, fontSize: 11 }}>
                  — inactive users cannot sign in
                </span>
              </span>
            </label>
            <div style={{ fontSize: 11, color: 'var(--text-muted)', display: 'flex', alignItems: 'center', gap: 6 }}>
              {role === 'admin' ? <Shield size={11} /> : <UserIcon size={11} />}
              {role === 'admin'
                ? 'Admins can change server configuration and manage users.'
                : 'Regular users can chat and manage their own profile.'}
            </div>
          </section>
        </div>

        <footer className={styles.footer}>
          <button className={styles.btnSecondary} onClick={onClose}>
            Close
          </button>
          <button
            className={styles.btn}
            onClick={submit}
            disabled={!dirty || profileMut.isPending}
            style={{ marginLeft: 8 }}
          >
            <Check size={14} />
            {profileMut.isPending ? 'Saving…' : 'Save changes'}
          </button>
        </footer>
      </div>
    </div>
  )
}
