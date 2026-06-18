// SPDX-License-Identifier: AGPL-3.0-or-later

import { useNavigate } from 'react-router-dom'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Check, Pause, SkipForward } from 'lucide-react'
import toast from 'react-hot-toast'
import { onboardingApi, onboardingErrorMessage } from '@/api/onboarding'
import styles from './OnboardingProgressStrip.module.css'

interface Props {
  /// Called with a pseudo-message the LLM interprets as a skip directive.
  /// The strip doesn't know which question is "current" — the LLM does —
  /// so we send a natural-language signal and let it call the right tool.
  onSendSkipMessage: (text: string) => void
}

// Status shown on each dot. Current = the first incomplete group.
type DotStatus = 'done' | 'current' | 'upcoming'

export default function OnboardingProgressStrip({ onSendSkipMessage }: Props) {
  const navigate = useNavigate()
  const qc       = useQueryClient()

  const { data: state } = useQuery({
    queryKey: ['onboarding-state'],
    queryFn:  onboardingApi.state,
    // Short stale time — progress updates as tools fire during the turn.
    staleTime: 2_000,
    refetchOnWindowFocus: false,
  })

  // Backstop for reasoning-distilled local models that narrate "all done"
  // without firing `complete_onboarding`. Invalidating `onboarding-state`
  // on success lets OnboardingCompleteModal see the onboarded_at flip and
  // take over from here.
  const finalizeMut = useMutation({
    mutationFn: onboardingApi.finalize,
    onSuccess: () => { qc.invalidateQueries({ queryKey: ['onboarding-state'] }) },
    onError:   (err) => toast.error(onboardingErrorMessage(err, 'Could not finish onboarding')),
  })

  // Hide the strip the moment `onboarded_at` flips — don't leave skip/pause
  // buttons on a conversation that's already done, even if the DB row still
  // carries mode=onboarding.
  if (!state || state.onboarded_at) return null

  const completed = new Set(state.completed_groups)
  const firstRemaining = state.remaining_groups[0] ?? null

  const dotStatus = (id: string): DotStatus => {
    if (completed.has(id)) return 'done'
    if (id === firstRemaining) return 'current'
    return 'upcoming'
  }

  const currentGroup = state.groups.find((g) => g.id === firstRemaining) ?? null
  const doneCount    = state.completed_groups.length

  const liveText = currentGroup
    ? `On section ${doneCount + 1} of ${state.total_groups}: ${currentGroup.label}`
    : `Wrapping up — ${doneCount} of ${state.total_groups} sections done`

  return (
    <div className={styles.strip} role="region" aria-label="Onboarding progress">
      <div className={styles.left}>
        <div className={styles.label} aria-live="polite" aria-atomic="true">
          {/* Screen-reader-only full sentence so group transitions announce
              cleanly; the visible chips carry the compact version. */}
          <span className={styles.srOnly}>{liveText}</span>
          {currentGroup ? (
            <>
              <span className={styles.groupLabel} aria-hidden="true">{currentGroup.label}</span>
              <span className={styles.count}    aria-hidden="true">{doneCount} / {state.total_groups}</span>
            </>
          ) : (
            <span className={styles.groupLabel} aria-hidden="true">Wrapping up…</span>
          )}
        </div>
        <div className={styles.dots} aria-hidden="true">
          {state.groups.map((g) => {
            const status = dotStatus(g.id)
            return (
              <span
                key={g.id}
                className={`${styles.dot} ${styles[`dot_${status}`]}`}
                title={`${g.label}${g.optional ? ' (optional)' : ''} — ${status}`}
              />
            )
          })}
        </div>
      </div>

      <div className={styles.actions}>
        {firstRemaining === null ? (
          <button
            className={`${styles.btn} ${styles.btn_primary}`}
            onClick={() => finalizeMut.mutate()}
            disabled={finalizeMut.isPending}
            title="Finish onboarding now"
          >
            <Check size={12} />
            <span>{finalizeMut.isPending ? 'Finishing…' : 'Finish'}</span>
          </button>
        ) : (
          <>
            <button
              className={styles.btn}
              onClick={() => onSendSkipMessage('[skip this question]')}
              title="Skip the current question"
            >
              <SkipForward size={12} />
              <span>Skip question</span>
            </button>
            <button
              className={styles.btn}
              onClick={() => onSendSkipMessage('[skip this group]')}
              title="Skip the rest of this group"
            >
              <SkipForward size={12} />
              <span>Skip group</span>
            </button>
          </>
        )}
        <button
          className={styles.btn}
          onClick={() => navigate('/chat')}
          title="Pause onboarding — you can resume later"
        >
          <Pause size={12} />
          <span>Pause</span>
        </button>
      </div>
    </div>
  )
}
