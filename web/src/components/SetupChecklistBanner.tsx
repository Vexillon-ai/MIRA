// SPDX-License-Identifier: AGPL-3.0-or-later
//
// "Finish setup" — the slim residue of the first-run setup walkthrough. The
// prominent guided form is the SetupWizard modal; this banner only appears once
// the admin has *skipped* that wizard, as a non-nagging reminder of the steps
// still outstanding (Voice, a Channel, proactive check-ins). It can reopen the
// wizard ("Resume"), deep-link each step, or be dismissed entirely (per
// browser). It auto-hides once every step is done.

import { useState } from 'react'
import { Link } from 'react-router-dom'
import { Sparkles, Check, ChevronDown, ChevronUp, X as XIcon, ArrowRight, Wand2 } from 'lucide-react'
import { useUiStore } from '@/store/uiStore'
import { useSetupChecklist, SETUP_STEPS } from '@/hooks/useSetupChecklist'
import styles from './SetupChecklistBanner.module.css'

export default function SetupChecklistBanner() {
  const setDismissedAt = useUiStore((s) => s.setSetupChecklistDismissedAt)
  const setSkippedAt = useUiStore((s) => s.setSetupChecklistSkippedAt)
  const [expanded, setExpanded] = useState(false)

  const { active, status: data, doneCount, allDone, skipped } = useSetupChecklist()

  if (!active || !data) return null
  if (allDone) return null            // all done → nothing to nag about
  if (!skipped) return null           // wizard is still the active surface

  // Clearing the skip flag makes the wizard the active surface again, which
  // reopens it and hides this banner.
  const resume = () => setSkippedAt(null)

  return (
    <div className={styles.wrap} role="status">
      <div className={styles.header}>
        <button
          className={styles.bar}
          onClick={() => setExpanded((v) => !v)}
          aria-expanded={expanded}
        >
          <Sparkles size={14} className={styles.spark} />
          <span className={styles.title}>Finish setting up MIRA</span>
          <span className={styles.count}>{doneCount}/{SETUP_STEPS.length} done</span>
          {expanded ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
        </button>
        <button
          className={styles.close}
          title="Dismiss"
          aria-label="Dismiss setup checklist"
          onClick={() => setDismissedAt(Date.now())}
        >
          <XIcon size={13} />
        </button>
      </div>

      {expanded && (
        <>
          <ul className={styles.list}>
            {SETUP_STEPS.map((s) => {
              const done = data[s.key]
              return (
                <li key={s.key} className={`${styles.item} ${done ? styles.itemDone : ''}`}>
                  <span className={`${styles.dot} ${done ? styles.dotDone : ''}`}>
                    {done && <Check size={12} />}
                  </span>
                  <span className={styles.itemText}>
                    <span className={styles.itemLabel}>{s.label}</span>
                    <span className={styles.itemDesc}>{s.desc}</span>
                  </span>
                  {!done && (
                    <Link className={styles.action} to={s.to} onClick={() => setExpanded(false)}>
                      Set up <ArrowRight size={12} />
                    </Link>
                  )}
                </li>
              )
            })}
          </ul>
          <div className={styles.footer}>
            <button className={styles.skip} onClick={resume}>
              <Wand2 size={12} /> Resume guided setup
            </button>
          </div>
        </>
      )}
    </div>
  )
}
