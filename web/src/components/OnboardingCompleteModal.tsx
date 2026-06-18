// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { PartyPopper } from 'lucide-react'
import toast from 'react-hot-toast'
import { onboardingApi, onboardingErrorMessage } from '@/api/onboarding'
import styles from './OnboardingCompleteModal.module.css'

/// Watches the `onboarding-state` query for the `onboarded_at` → truthy
/// transition and, when it flips during this session, shows a completion
/// dialog. OK creates a fresh personalized chat and navigates into it.
///
/// Gated on `active` so we only render on the onboarding conversation —
/// the strip hides as soon as `onboarded_at` is set, so without this gate
/// the modal would appear on every other page too.
export default function OnboardingCompleteModal({ active }: { active: boolean }) {
  const navigate = useNavigate()
  const qc       = useQueryClient()

  const { data: state } = useQuery({
    queryKey: ['onboarding-state'],
    queryFn:  onboardingApi.state,
    staleTime: 2_000,
    refetchOnWindowFocus: false,
  })

  // Remember the value we saw on first render so the modal only fires on a
  // real transition, not on page refresh into an already-onboarded state.
  const startOnboardedRef = useRef<boolean | null>(null)
  const [open, setOpen] = useState(false)

  useEffect(() => {
    if (!state) return
    if (startOnboardedRef.current === null) {
      startOnboardedRef.current = !!state.onboarded_at
      return
    }
    if (!startOnboardedRef.current && state.onboarded_at && active) {
      setOpen(true)
      startOnboardedRef.current = true
    }
  }, [state, active])

  const mut = useMutation({
    mutationFn: onboardingApi.postCompleteChat,
    onSuccess: (r) => {
      qc.invalidateQueries({ queryKey: ['conversations'] })
      setOpen(false)
      navigate(`/chat/${r.conversation_id}`)
    },
    onError: (err) => toast.error(onboardingErrorMessage(err, 'Could not open your first chat')),
  })

  if (!open) return null

  return (
    <div className={styles.backdrop} role="dialog" aria-modal="true" aria-labelledby="onb-complete-title">
      <div className={styles.dialog}>
        <div className={styles.icon}><PartyPopper size={28} /></div>
        <h2 id="onb-complete-title" className={styles.title}>Onboarding complete</h2>
        <p className={styles.body}>
          Thanks for that — I've saved everything you shared and I'm ready to help
          for real now.
        </p>
        <div className={styles.actions}>
          <button className={styles.primary} onClick={() => mut.mutate()} disabled={mut.isPending}>
            {mut.isPending ? 'Opening…' : 'OK'}
          </button>
        </div>
      </div>
    </div>
  )
}
