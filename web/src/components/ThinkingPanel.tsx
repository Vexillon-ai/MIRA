// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState, useMemo } from 'react'
import { ChevronRight, ChevronDown, Wrench, BookOpen, Brain, Loader2 } from 'lucide-react'
import type { ThinkingEntry } from '@/api/types'
import styles from './ThinkingPanel.module.css'

interface ThinkingPanelProps {
  /** Ordered list of events the agent emitted this turn. */
  entries:    ThinkingEntry[]
  /** True while the SSE stream is still active. Controls auto-
   *  expansion, pulse animation, and the "Thinking…" wording. */
  isStreaming?: boolean
}

/**
 * Single rollup that surfaces the agent's tool calls, tool results,
 * model reasoning blocks, and wiki context fetches for one turn.
 *
 * Live mode (isStreaming=true): auto-expanded, summary shows "Thinking…"
 * with a pulse, the entry list grows as SSE events arrive.
 *
 * History mode (isStreaming=false): collapsed by default with a
 * summary like "Thinking (3 steps)"; click to expand. Reads from the
 * persisted metadata so reloading a conversation shows the same trail
 * that streamed live during the original turn.
 *
 * Hidden entirely when there are no entries and we're not streaming
 * (no agent activity to show).
 */
export default function ThinkingPanel({ entries, isStreaming = false }: ThinkingPanelProps) {
  // Default-open during streaming so users see activity as it
  // arrives; collapsed in history mode to keep finished
  // conversations tidy. User toggle wins from then on.
  const [open, setOpen] = useState(isStreaming)

  if (entries.length === 0 && !isStreaming) return null

  const summary = useMemo(() => buildSummary(entries, isStreaming), [entries, isStreaming])

  return (
    <div className={`${styles.panel} ${isStreaming ? styles.panelLive : ''}`}>
      <button
        type="button"
        className={styles.summary}
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
      >
        {open
          ? <ChevronDown size={12} className={styles.chevron} />
          : <ChevronRight size={12} className={styles.chevron} />}
        {isStreaming && <Loader2 size={11} className={styles.pulse} />}
        <span className={styles.summaryLabel}>{summary}</span>
      </button>
      {open && (
        <div className={styles.body}>
          {entries.map((e, i) => (
            <ThinkingRow key={i} entry={e} />
          ))}
          {isStreaming && entries.length === 0 && (
            <div className={styles.empty}>Waiting for activity…</div>
          )}
        </div>
      )}
    </div>
  )
}

function buildSummary(entries: ThinkingEntry[], live: boolean): string {
  if (live) {
    if (entries.length === 0) return 'Thinking…'
    return `Thinking… (${entries.length} step${entries.length === 1 ? '' : 's'})`
  }
  const n = entries.length
  return `Thinking (${n} step${n === 1 ? '' : 's'})`
}

function ThinkingRow({ entry }: { entry: ThinkingEntry }) {
  switch (entry.type) {
    case 'tool_call': {
      const argsStr = formatArgs(entry.args)
      return (
        <div className={styles.row}>
          <Wrench size={11} className={styles.iconCall} />
          <span className={styles.label}>→ {entry.name}</span>
          {argsStr && <span className={styles.args}>{argsStr}</span>}
        </div>
      )
    }
    case 'tool_result': {
      const colour = entry.success ? styles.iconOk : styles.iconFail
      return (
        <div className={styles.row}>
          <Wrench size={11} className={colour} />
          <span className={styles.label}>← {entry.name}</span>
          <span className={styles.output}>{truncate(entry.output, 240)}</span>
        </div>
      )
    }
    case 'reasoning': {
      return (
        <div className={`${styles.row} ${styles.rowReasoning}`}>
          <Brain size={11} className={styles.iconReason} />
          <div className={styles.reasoningBody}>{entry.text}</div>
        </div>
      )
    }
    case 'wiki_context': {
      if (entry.pages.length === 0) return null
      return (
        <div className={styles.row}>
          <BookOpen size={11} className={styles.iconWiki} />
          <span className={styles.label}>wiki context</span>
          <span className={styles.args}>{entry.pages.join(', ')}</span>
        </div>
      )
    }
  }
}

function formatArgs(args: unknown): string {
  if (args == null) return ''
  if (typeof args === 'string') return args.length > 0 ? truncate(args, 120) : ''
  try {
    const s = JSON.stringify(args)
    return truncate(s, 120)
  } catch {
    return ''
  }
}

function truncate(s: string, n: number): string {
  if (s.length <= n) return s
  return s.slice(0, n - 1) + '…'
}
