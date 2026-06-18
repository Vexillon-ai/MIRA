// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect } from 'react'
import { useNavigate } from 'react-router-dom'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Sparkles, X } from 'lucide-react'
import toast from 'react-hot-toast'
import { onboardingApi, onboardingErrorMessage } from '@/api/onboarding'
import { useAuthStore } from '@/store/authStore'
import { useUiStore } from '@/store/uiStore'
import styles from './OnboardingWelcomeModal.module.css'

// "Maybe later" puts the modal on a 7-day cooldown. Long enough not to nag,
// short enough that a user who genuinely meant to come back sees it again.
const DISMISS_COOLDOWN_MS = 7 * 24 * 60 * 60 * 1000

export default function OnboardingWelcomeModal() {
  const navigate   = useNavigate()
  const qc         = useQueryClient()
  const isAuthed   = useAuthStore((s) => s.isAuthenticated)
  const dismissedAt         = useUiStore((s) => s.onboardingDismissedAt)
  const setDismissedAt      = useUiStore((s) => s.setOnboardingDismissedAt)

  // Only fetch when the user is logged in; the request requires auth.
  const { data: state } = useQuery({
    queryKey: ['onboarding-state'],
    queryFn:  onboardingApi.state,
    enabled:  isAuthed,
    staleTime: 60_000,
    // State endpoint is cheap but it reads three tables — don't hammer it.
    refetchOnWindowFocus: false,
  })

  const startMut = useMutation({
    mutationFn: onboardingApi.start,
    onSuccess:  (r) => {
      qc.invalidateQueries({ queryKey: ['onboarding-state'] })
      qc.invalidateQueries({ queryKey: ['conversations'] })
      navigate(`/chat/${r.conversation_id}`)
    },
    onError: (err) => toast.error(onboardingErrorMessage(err, 'Could not start onboarding')),
  })

  const cooldownActive = dismissedAt !== null &&
    (Date.now() - dismissedAt) < DISMISS_COOLDOWN_MS

  const eligible = !!state
    && state.onboarded_at === null
    && state.active_conversation_id === null
    && !cooldownActive

  // Escape closes (counts as "Maybe later").
  useEffect(() => {
    if (!eligible) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') handleDismiss()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [eligible])

  const handleStart    = () => startMut.mutate()
  const handleDismiss  = () => setDismissedAt(Date.now())

  if (!eligible) return null

  return (
    <div className={styles.backdrop} role="dialog" aria-modal="true" aria-labelledby="onb-title">
      <div className={styles.dialog}>
        <button
          className={styles.closeBtn}
          onClick={handleDismiss}
          aria-label="Maybe later"
          title="Maybe later (Esc)"
        >
          <X size={14} />
        </button>

        <div className={styles.icon}>
          <Sparkles size={28} />
        </div>

        <h2 id="onb-title" className={styles.title}>Let's get to know each other</h2>

        <p className={styles.body}>
          I'd like to learn a few things about you so I can be more useful —
          how you'd like to be addressed, your timezone, what you're working on,
          and how you'd prefer me to behave. It's a short chat. You can skip any
          question or pause at any time.
        </p>

        <div className={styles.actions}>
          <button
            className={styles.primary}
            onClick={handleStart}
            disabled={startMut.isPending}
          >
            {startMut.isPending ? 'Starting…' : 'Start'}
          </button>
          <button
            className={styles.secondary}
            onClick={handleDismiss}
            disabled={startMut.isPending}
          >
            Maybe later
          </button>
        </div>
      </div>
    </div>
  )
}
