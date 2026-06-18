// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState, useMemo, type FormEvent } from 'react'
import { useNavigate, useSearchParams, Link } from 'react-router-dom'
import { useQuery } from '@tanstack/react-query'
import { authApi } from '@/api/auth'
import { useAuthStore } from '@/store/authStore'
import miraLogo from '@/assets/mira-logo.svg'
import styles from './LoginPage.module.css'

export default function SignupPage() {
  const navigate = useNavigate()
  const [params] = useSearchParams()
  const inviteToken = params.get('invite') ?? ''

  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [email, setEmail] = useState('')
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)
  const [pending, setPending] = useState(false)

  // Invite validation (only when a token is present).
  const { data: invite, isLoading: inviteLoading } = useQuery({
    queryKey: ['invite-info', inviteToken],
    queryFn:  () => authApi.inviteInfo(inviteToken),
    enabled:  inviteToken.length > 0,
    retry:    false,
  })

  // Open-signup policy (only consulted when there's no invite).
  const { data: cfg, isLoading: cfgLoading } = useQuery({
    queryKey: ['signup-config'],
    queryFn:  authApi.signupConfig,
    enabled:  inviteToken.length === 0,
    retry:    false,
  })

  const mode: 'invite' | 'open' | 'closed' | 'loading' = useMemo(() => {
    if (inviteToken) {
      if (inviteLoading) return 'loading'
      return invite?.valid ? 'invite' : 'closed'
    }
    if (cfgLoading) return 'loading'
    return cfg?.open_signup ? 'open' : 'closed'
  }, [inviteToken, inviteLoading, invite, cfgLoading, cfg])

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    setError('')
    setLoading(true)
    try {
      const res = await authApi.signup({
        username,
        password,
        email: email || undefined,
        invite_token: inviteToken || undefined,
      })
      if (res.status === 'active') {
        // The signup set the refresh cookie; recover the full session.
        await useAuthStore.getState().refresh()
        navigate('/', { replace: true })
      } else {
        setPending(true)
      }
    } catch (err: unknown) {
      const data = (err as { response?: { data?: unknown } })?.response?.data
      setError(typeof data === 'string' ? data : 'Sign-up failed')
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className={styles.page}>
      <div className={styles.card}>
        <div className={styles.header}>
          <img src={miraLogo} alt="" className={styles.logoImg} />
          <h1 className={styles.title}>Create your account</h1>
          {mode === 'invite' && invite?.role === 'admin' && (
            <p className={styles.subtitle}>You were invited as an administrator</p>
          )}
          {mode === 'open' && cfg?.require_approval && (
            <p className={styles.subtitle}>An admin will approve your account before you can sign in</p>
          )}
        </div>

        {pending ? (
          <div className={styles.notice}>
            <p>Your account was created and is <strong>awaiting administrator approval</strong>.
            You'll be able to sign in once an admin approves it.</p>
            <Link className={styles.ssoBtn} to="/login">Back to sign in</Link>
          </div>
        ) : mode === 'loading' ? (
          <p className={styles.subtitle}>Loading…</p>
        ) : mode === 'closed' ? (
          <div className={styles.notice}>
            <p>{inviteToken
              ? 'This invite link is invalid or has expired.'
              : 'Sign-ups are invite-only. Ask an administrator for an invite link.'}</p>
            <Link className={styles.ssoBtn} to="/login">Back to sign in</Link>
          </div>
        ) : (
          <form className={styles.form} onSubmit={handleSubmit}>
            <div className={styles.field}>
              <label className={styles.label} htmlFor="username">Username</label>
              <input id="username" type="text" className={styles.input}
                value={username} onChange={(e) => setUsername(e.target.value)}
                autoComplete="username" autoFocus required />
            </div>
            <div className={styles.field}>
              <label className={styles.label} htmlFor="email">
                Email {mode === 'open' ? '' : '(optional)'}
              </label>
              <input id="email" type="email" className={styles.input}
                value={email} onChange={(e) => setEmail(e.target.value)}
                autoComplete="email"
                placeholder={invite?.email_hint ?? undefined}
                required={mode === 'open'} />
            </div>
            <div className={styles.field}>
              <label className={styles.label} htmlFor="password">Password</label>
              <input id="password" type="password" className={styles.input}
                value={password} onChange={(e) => setPassword(e.target.value)}
                autoComplete="new-password" minLength={8} required />
            </div>

            {error && <p className={styles.error}>{error}</p>}

            <button type="submit" className={styles.btn} disabled={loading}>
              {loading ? 'Creating…' : 'Create account'}
            </button>
            <Link className={styles.altLink} to="/login">Already have an account? Sign in</Link>
          </form>
        )}
      </div>
    </div>
  )
}
