// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState, useEffect, type FormEvent } from 'react'
import { useNavigate, Link } from 'react-router-dom'
import { useQuery } from '@tanstack/react-query'
import { useAuthStore } from '@/store/authStore'
import { authApi, type OidcProviderButton } from '@/api/auth'
import miraLogo from '@/assets/mira-logo.svg'
import styles from './LoginPage.module.css'

export default function LoginPage() {
  const navigate = useNavigate()
  const login = useAuthStore((s) => s.login)

  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  // SSO providers (empty when OIDC is off → buttons hidden).
  const { data: providers = [] } = useQuery<OidcProviderButton[]>({
    queryKey: ['oidc-providers'],
    queryFn:  authApi.oidcProviders,
    retry:    false,
    staleTime: 5 * 60_000,
  })

  // Surface an error bounced back from the OIDC callback (?sso_error=…).
  useEffect(() => {
    const p = new URLSearchParams(window.location.search)
    const ssoErr = p.get('sso_error')
    if (ssoErr) {
      setError(ssoErr)
      // Strip the query so a refresh doesn't keep showing it.
      window.history.replaceState({}, '', window.location.pathname)
    }
  }, [])

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    setError('')
    setLoading(true)
    try {
      await login(username, password)
      navigate('/', { replace: true })
    } catch (err: unknown) {
      const msg = (err as { response?: { data?: { error?: string } } })
        ?.response?.data?.error ?? 'Login failed'
      setError(msg)
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className={styles.page}>
      <div className={styles.card}>
        <div className={styles.header}>
          <img src={miraLogo} alt="" className={styles.logoImg} />
          <h1 className={styles.title}>MIRA</h1>
          <p className={styles.subtitle}>Your life's loyal partner</p>
        </div>

        <form className={styles.form} onSubmit={handleSubmit}>
          <div className={styles.field}>
            <label className={styles.label} htmlFor="username">Username</label>
            <input
              id="username"
              type="text"
              className={styles.input}
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              autoComplete="username"
              autoFocus
              required
            />
          </div>

          <div className={styles.field}>
            <label className={styles.label} htmlFor="password">Password</label>
            <input
              id="password"
              type="password"
              className={styles.input}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="current-password"
              required
            />
          </div>

          {error && <p className={styles.error}>{error}</p>}

          <button
            type="submit"
            className={styles.btn}
            disabled={loading}
          >
            {loading ? 'Signing in…' : 'Sign in'}
          </button>
          <Link className={styles.altLink} to="/signup">Have an invite? Create an account</Link>
        </form>

        {providers.length > 0 && (
          <div className={styles.sso}>
            <div className={styles.ssoDivider}><span>or</span></div>
            {providers.map((p) => (
              <a
                key={p.id}
                className={styles.ssoBtn}
                href={`/api/auth/oidc/authorize?provider=${encodeURIComponent(p.id)}`}
              >
                Sign in with {p.display_name}
              </a>
            ))}
          </div>
        )}
      </div>
    </div>
  )
}
