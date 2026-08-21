// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { X, LogOut, Check, AlertCircle, Upload, Trash2, RotateCcw } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import { onboardingApi, onboardingErrorMessage } from '@/api/onboarding'
import { useAuthStore } from '@/store/authStore'
import type { User } from '@/api/types'
import Avatar, { AVATAR_PRESETS } from './Avatar'
import styles from './ProfileDialog.module.css'

interface Props {
  open: boolean
  onClose: () => void
}

export default function ProfileDialog({ open, onClose }: Props) {
  const navigate = useNavigate()
  const qc     = useQueryClient()
  const user   = useAuthStore((s) => s.user)
  const setUser = useAuthStore((s) => s.setUser)
  const logout = useAuthStore((s) => s.logout)

  const [displayName, setDisplayName] = useState('')
  const [email, setEmail]             = useState('')
  const [phone, setPhone]             = useState('')
  const [preferredContact, setPreferredContact] = useState('')
  const [newPw, setNewPw]             = useState('')
  const [confirmPw, setConfirmPw]     = useState('')
  const [pwError, setPwError]         = useState('')
  const [confirmReset, setConfirmReset] = useState(false)

  useEffect(() => {
    if (!open || !user) return
    setDisplayName(user.display_name ?? '')
    setEmail(user.email ?? '')
    setPhone(user.phone ?? '')
    setPreferredContact(user.preferred_contact ?? '')
    setNewPw('')
    setConfirmPw('')
    setPwError('')
    setConfirmReset(false)
  }, [open, user])

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open, onClose])

  const profileMut = useMutation({
    mutationFn: async (body: {
      display_name: string | null
      email: string | null
      phone: string | null
      preferred_contact: string | null
    }) => {
      if (!user) throw new Error('No user')
      const r = await api.put<User>(`/api/users/${user.id}`, body)
      return r.data
    },
    onSuccess: (updated) => {
      setUser(updated)
      qc.invalidateQueries({ queryKey: ['users'] })
      toast.success('Profile updated.')
    },
    onError: () => toast.error('Update failed'),
  })

  const pwMut = useMutation({
    mutationFn: async (new_password: string) => {
      if (!user) throw new Error('No user')
      await api.post(`/api/users/${user.id}/password`, { new_password })
    },
    onSuccess: () => {
      setNewPw('')
      setConfirmPw('')
      setPwError('')
      toast.success('Password changed.')
    },
    onError: () => setPwError('Password change failed. Try again.'),
  })

  const fileInputRef = useRef<HTMLInputElement>(null)

  const presetMut = useMutation({
    mutationFn: async (avatar: string | null) => {
      if (!user) throw new Error('No user')
      const r = await api.put<User>(`/api/users/${user.id}`, { avatar })
      return r.data
    },
    onSuccess: (updated) => {
      setUser(updated)
      qc.invalidateQueries({ queryKey: ['users'] })
    },
    onError: () => toast.error('Could not update avatar'),
  })

  const uploadMut = useMutation({
    mutationFn: async (file: File) => {
      if (!user) throw new Error('No user')
      const fd = new FormData()
      fd.append('file', file)
      const r = await api.post<User>(`/api/users/${user.id}/avatar`, fd)
      return r.data
    },
    onSuccess: (updated) => {
      setUser(updated)
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
      if (!user) throw new Error('No user')
      const r = await api.delete<User>(`/api/users/${user.id}/avatar`)
      return r.data
    },
    onSuccess: (updated) => {
      setUser(updated)
      qc.invalidateQueries({ queryKey: ['users'] })
    },
    onError: () => toast.error('Could not remove avatar'),
  })

  // ── Onboarding revisit ────────────────────────────────────────────────────

  const { data: onbState } = useQuery({
    queryKey: ['onboarding-state'],
    queryFn:  onboardingApi.state,
    enabled:  open && !!user,
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  })

  const restartGroupMut = useMutation({
    mutationFn: async (group_id: string) => {
      await onboardingApi.restartGroup(group_id)
      return onboardingApi.start()
    },
    onSuccess: (r) => {
      qc.invalidateQueries({ queryKey: ['onboarding-state'] })
      qc.invalidateQueries({ queryKey: ['conversations'] })
      navigate(`/chat/${r.conversation_id}`)
      onClose()
    },
    onError: (err) => toast.error(onboardingErrorMessage(err, 'Could not restart group')),
  })

  const resetMut = useMutation({
    mutationFn: async () => {
      await onboardingApi.reset()
      return onboardingApi.start()
    },
    onSuccess: (r) => {
      qc.invalidateQueries({ queryKey: ['onboarding-state'] })
      qc.invalidateQueries({ queryKey: ['conversations'] })
      setConfirmReset(false)
      navigate(`/chat/${r.conversation_id}`)
      onClose()
      toast.success('Onboarding reset — starting fresh.')
    },
    onError: (err) => {
      setConfirmReset(false)
      toast.error(onboardingErrorMessage(err, 'Reset failed'))
    },
  })

  if (!open || !user) return null

  const dirty =
    (displayName      || '') !== (user.display_name      ?? '') ||
    (email            || '') !== (user.email             ?? '') ||
    (phone            || '') !== (user.phone             ?? '') ||
    (preferredContact || '') !== (user.preferred_contact ?? '')

  const currentPresetKey = user.avatar?.startsWith('preset:')
    ? user.avatar.slice('preset:'.length)
    : null
  const hasAvatar = !!user.avatar

  const submitProfile = () => {
    profileMut.mutate({
      display_name:      displayName.trim() || null,
      email:             email.trim()       || null,
      phone:             phone.trim()       || null,
      preferred_contact: preferredContact   || null,
    })
  }

  const submitPassword = () => {
    setPwError('')
    if (newPw.length < 8) { setPwError('Password must be at least 8 characters.'); return }
    if (newPw !== confirmPw) { setPwError('Passwords do not match.'); return }
    pwMut.mutate(newPw)
  }

  return (
    <div className={styles.backdrop} onMouseDown={onClose}>
      <div
        className={styles.dialog}
        role="dialog"
        aria-modal="true"
        aria-label="Profile"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <header className={styles.header}>
          <h2>Profile</h2>
          <button className={styles.iconBtn} onClick={onClose} title="Close" aria-label="Close">
            <X size={16} />
          </button>
        </header>

        <div className={styles.body}>
          {/* Avatar + identity */}
          <section className={styles.identity}>
            <Avatar user={user} size={72} />
            <div>
              <div className={styles.displayName}>
                {user.display_name ?? user.username}
              </div>
              <div className={styles.loginId} title="Your login ID — cannot be changed">
                @{user.username}
                <span className={styles.roleBadge} data-role={user.role}>{user.role}</span>
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
                placeholder="How your name appears"
              />
            </label>
            <label className={styles.label}>
              <span>Email</span>
              <input
                className={styles.input}
                type="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                placeholder="you@example.com"
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
              <span title="Channel the agent will use when reaching out to you proactively">
                Preferred contact
              </span>
              <select
                className={styles.input}
                value={preferredContact}
                onChange={(e) => setPreferredContact(e.target.value)}
              >
                <option value="">Don't contact me proactively</option>
                <option value="signal">Signal</option>
                <option value="telegram">Telegram</option>
              </select>
            </label>

            <div className={styles.actions}>
              <button
                className={styles.btn}
                onClick={submitProfile}
                disabled={!dirty || profileMut.isPending}
              >
                <Check size={14} />
                {profileMut.isPending ? 'Saving…' : 'Save changes'}
              </button>
            </div>
          </section>

          {/* Password */}
          <section className={styles.section}>
            <h3 className={styles.sectionTitle}>Change password</h3>
            <label className={styles.label}>
              <span>New password</span>
              <input
                className={styles.input}
                type="password"
                value={newPw}
                onChange={(e) => setNewPw(e.target.value)}
                autoComplete="new-password"
                placeholder="At least 8 characters"
              />
            </label>
            <label className={styles.label}>
              <span>Confirm new password</span>
              <input
                className={styles.input}
                type="password"
                value={confirmPw}
                onChange={(e) => setConfirmPw(e.target.value)}
                autoComplete="new-password"
              />
            </label>
            {pwError && (
              <p className={styles.error}>
                <AlertCircle size={12} /> {pwError}
              </p>
            )}
            <div className={styles.actions}>
              <button
                className={styles.btnSecondary}
                onClick={submitPassword}
                disabled={!newPw || pwMut.isPending}
              >
                {pwMut.isPending ? 'Updating…' : 'Update password'}
              </button>
            </div>
          </section>

          {/* Onboarding — revisit individual groups or start over */}
          {onbState && (
            <section className={styles.section}>
              <h3 className={styles.sectionTitle}>Onboarding</h3>
              <p className={styles.onbHint}>
                {onbState.onboarded_at
                  ? 'You\'ve finished onboarding. You can revisit any topic to update what I know, or start over from scratch.'
                  : 'Pick up any topic you\'d like to revisit, or start over from scratch.'}
              </p>

              <div className={styles.groupList}>
                {onbState.groups.map((g) => {
                  const done   = onbState.completed_groups.includes(g.id)
                  const status = done ? 'done' : 'todo'
                  const busy   = restartGroupMut.isPending && restartGroupMut.variables === g.id
                  return (
                    <div key={g.id} className={styles.groupRow}>
                      <div className={styles.groupLabel}>
                        <span className={styles.name}>{g.label}</span>
                        {g.optional && <span className={styles.meta}>Optional</span>}
                      </div>
                      <span className={styles.statusBadge} data-status={status}>
                        {done ? 'Done' : 'Not done'}
                      </span>
                      <button
                        className={styles.rowAction}
                        onClick={() => restartGroupMut.mutate(g.id)}
                        disabled={restartGroupMut.isPending || resetMut.isPending}
                        title="Redo this topic in a chat"
                      >
                        <RotateCcw size={12} />
                        {busy ? '…' : 'Revisit'}
                      </button>
                    </div>
                  )
                })}
              </div>

              {!confirmReset ? (
                <div className={styles.actions}>
                  <button
                    className={styles.btnDanger}
                    onClick={() => setConfirmReset(true)}
                    disabled={restartGroupMut.isPending || resetMut.isPending}
                  >
                    Start fresh
                  </button>
                </div>
              ) : (
                <div className={styles.confirmBox}>
                  <p>
                    This clears your onboarding profile, preferences file, and
                    progress — then starts a new onboarding chat. Your chat
                    history and memories are untouched.
                  </p>
                  <div className={styles.actions}>
                    <button
                      className={styles.btnSecondary}
                      onClick={() => setConfirmReset(false)}
                      disabled={resetMut.isPending}
                    >
                      Cancel
                    </button>
                    <button
                      className={styles.btnDanger}
                      onClick={() => resetMut.mutate()}
                      disabled={resetMut.isPending}
                    >
                      {resetMut.isPending ? 'Resetting…' : 'Yes, start fresh'}
                    </button>
                  </div>
                </div>
              )}
            </section>
          )}
        </div>

        <footer className={styles.footer}>
          <button
            className={styles.btnDanger}
            onClick={async () => { await logout(); onClose() }}
          >
            <LogOut size={14} />
            Sign out
          </button>
        </footer>
      </div>
    </div>
  )
}
