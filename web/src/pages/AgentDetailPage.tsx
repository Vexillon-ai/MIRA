// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useRef, useState } from 'react'
import { useParams, useNavigate } from 'react-router-dom'
import { useQuery } from '@tanstack/react-query'
import { ArrowLeft } from 'lucide-react'
import { agentsApi, type AgentActivity, type AgentStdoutChunk } from '@/api/agents'
import styles from './AgentDetailPage.module.css'

/// 0.113.0 — "jump into the agent's terminal". Two-pane view: top
/// shows structured activity (audit + progress + state), bottom
/// shows the raw stdout tail like a tmux pane. Toggle poll vs SSE
/// and prettify; defaults pulled from /api/config.

type ViewMode = 'poll' | 'sse'

export default function AgentDetailPage() {
  const { id } = useParams<{ id: string }>()
  const navigate = useNavigate()
  const [viewMode, setViewMode] = useState<ViewMode>('poll')
  const [prettify, setPrettify] = useState(false)
  const [pollIntervalMs, setPollIntervalMs] = useState(1500)
  const [whichStream, setWhichStream] = useState<'stdout' | 'stderr'>('stdout')

  // Load defaults from /api/config on mount.
  useEffect(() => {
    fetch('/api/config').then(r => r.json()).then((cfg: any) => {
      const d = cfg?.agent?.detail
      if (d?.view_mode === 'poll' || d?.view_mode === 'sse') setViewMode(d.view_mode)
      if (typeof d?.poll_interval_ms === 'number') setPollIntervalMs(d.poll_interval_ms)
      if (typeof d?.prettify_output === 'boolean') setPrettify(d.prettify_output)
    }).catch(() => {})
  }, [])

  if (!id) return <div className={styles.empty}>No agent id in URL.</div>

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <button type="button" className={styles.backBtn} onClick={() => navigate('/agents')}>
          <ArrowLeft size={14} /> Back to agents
        </button>
        <div className={styles.titleRow}>
          <h1>Agent <span className={styles.idMono}>{id.slice(0, 8)}</span></h1>
          <div className={styles.toolbar}>
            <label className={styles.toolbarItem}>
              View
              <select value={viewMode} onChange={(e) => setViewMode(e.target.value as ViewMode)}>
                <option value="poll">Poll</option>
                <option value="sse">SSE</option>
              </select>
            </label>
            {viewMode === 'poll' && (
              <label className={styles.toolbarItem}>
                Interval
                <select value={pollIntervalMs} onChange={(e) => setPollIntervalMs(Number(e.target.value))}>
                  <option value={500}>0.5s</option>
                  <option value={1000}>1s</option>
                  <option value={1500}>1.5s</option>
                  <option value={3000}>3s</option>
                  <option value={5000}>5s</option>
                </select>
              </label>
            )}
            <label className={styles.toolbarItem}>
              <input type="checkbox" checked={prettify} onChange={(e) => setPrettify(e.target.checked)} />
              Prettify JSON
            </label>
            <label className={styles.toolbarItem}>
              Stream
              <select value={whichStream} onChange={(e) => setWhichStream(e.target.value as any)}>
                <option value="stdout">stdout</option>
                <option value="stderr">stderr</option>
              </select>
            </label>
          </div>
        </div>
      </header>

      <div className={styles.panes}>
        <ActivityPane id={id} viewMode={viewMode} pollIntervalMs={pollIntervalMs} />
        <StdoutPane   id={id} viewMode={viewMode} pollIntervalMs={pollIntervalMs}
                              prettify={prettify} which={whichStream} />
      </div>
    </div>
  )
}

// ── Top pane: structured activity ─────────────────────────────────────────

function ActivityPane({ id, viewMode, pollIntervalMs }: {
  id: string; viewMode: ViewMode; pollIntervalMs: number
}) {
  const [sseData, setSseData] = useState<AgentActivity | null>(null)
  // Poll mode via React Query
  const pollQ = useQuery({
    queryKey: ['agent-activity', id],
    queryFn:  () => agentsApi.activity(id),
    enabled: viewMode === 'poll',
    refetchInterval: viewMode === 'poll' ? pollIntervalMs : false,
  })

  // SSE mode: open an EventSource and update on each message
  useEffect(() => {
    if (viewMode !== 'sse') return
    const es = new EventSource(`/api/agents/${encodeURIComponent(id)}/activity/stream`, { withCredentials: true })
    es.onmessage = (ev) => {
      try { setSseData(JSON.parse(ev.data)) } catch (_) {}
    }
    es.onerror = () => { /* let it retry */ }
    return () => es.close()
  }, [id, viewMode])

  const data: AgentActivity | undefined = viewMode === 'sse' ? (sseData ?? undefined) : pollQ.data
  if (!data) return <div className={styles.pane}><div className={styles.empty}>Loading activity…</div></div>

  const { agent, audit, progress } = data
  return (
    <div className={styles.pane}>
      <div className={styles.paneHead}>
        <strong>Activity</strong>
        {agent && (
          <span className={styles.agentMeta}>
            <span className={`${styles.statusBadge} ${styles[`status_${agent.status}`] ?? ''}`}>{agent.status}</span>
            {' · '}skill <code>{agent.skill_id ?? '—'}</code>
            {' · '}spent <strong>${agent.spent_usd.toFixed(4)}</strong>
            {agent.max_usd != null && <> / ${agent.max_usd.toFixed(2)}</>}
            {agent.llm_choice && <> · model <code>{agent.llm_choice.model ?? agent.llm_choice.alias}</code></>}
          </span>
        )}
      </div>
      <div className={styles.paneBody}>
        {agent?.current_step && (
          <div className={styles.currentStep}>
            <strong>Now:</strong> {agent.current_step}
          </div>
        )}
        <ActivityFeed audit={audit} progress={progress} />
        {agent?.result_summary && (
          <div className={styles.outcomeOk}>
            <strong>Result:</strong> {agent.result_summary}
          </div>
        )}
        {agent?.failure_reason && (
          <div className={styles.outcomeFail}>
            <strong>Failed{agent.fault?.code ? ` (${agent.fault.code})` : ''}:</strong> {agent.failure_reason}
          </div>
        )}
      </div>
    </div>
  )
}

/// Merge audit + progress into one chronological stream.
function ActivityFeed({ audit, progress }: { audit: any[]; progress: any[] }) {
  type Row = { ts_ms: number; kind: string; body: string }
  const rows: Row[] = []
  for (const a of audit) rows.push({
    ts_ms: a.ts_ms, kind: a.kind,
    body: typeof a.detail === 'string' ? a.detail : JSON.stringify(a.detail),
  })
  for (const p of progress) rows.push({
    ts_ms: p.ts_ms, kind: 'progress',
    body: p.summary + (p.llm_spend_usd ? ` ($${p.llm_spend_usd.toFixed(4)})` : ''),
  })
  rows.sort((a, b) => a.ts_ms - b.ts_ms)
  if (rows.length === 0) return <div className={styles.empty}>No activity yet.</div>
  return (
    <div className={styles.feed}>
      {rows.map((r, i) => (
        <div key={i} className={`${styles.feedRow} ${styles[`feed_${r.kind}`] ?? ''}`}>
          <span className={styles.feedTime}>{new Date(r.ts_ms).toLocaleTimeString()}</span>
          <span className={styles.feedKind}>{r.kind}</span>
          <span className={styles.feedBody}>{r.body}</span>
        </div>
      ))}
    </div>
  )
}

// ── Bottom pane: raw stdout terminal ───────────────────────────────────────

function StdoutPane({ id, viewMode, pollIntervalMs, prettify, which }: {
  id: string; viewMode: ViewMode; pollIntervalMs: number
  prettify: boolean; which: 'stdout' | 'stderr'
}) {
  const [content, setContent] = useState('')
  const [offset, setOffset] = useState(0)
  const [running, setRunning] = useState(false)
  const containerRef = useRef<HTMLPreElement>(null)
  // Reset on which-stream change
  useEffect(() => { setContent(''); setOffset(0) }, [which, id])

  // Polling loop
  useEffect(() => {
    if (viewMode !== 'poll') return
    let cancelled = false
    const tick = async () => {
      try {
        const resp = await agentsApi.stdout(id, offset === 0 ? { tail: 64 * 1024, which } : { offset, which })
        if (cancelled) return
        if (resp.content) {
          setContent(prev => prev + resp.content)
          setOffset(resp.offset + resp.content.length)
        } else {
          // First fetch can have content but match start_offset; still record where we are
          setOffset(prev => Math.max(prev, resp.offset + resp.content.length))
        }
        setRunning(resp.running)
      } catch (_) { /* swallow */ }
    }
    tick()
    const handle = setInterval(tick, pollIntervalMs)
    return () => { cancelled = true; clearInterval(handle) }
  }, [id, viewMode, pollIntervalMs, offset, which])

  // SSE loop
  useEffect(() => {
    if (viewMode !== 'sse') return
    const es = new EventSource(
      `/api/agents/${encodeURIComponent(id)}/stdout/stream?which=${which}`,
      { withCredentials: true },
    )
    es.onmessage = (ev) => {
      try {
        const payload = JSON.parse(ev.data) as AgentStdoutChunk
        if (payload.content) setContent(prev => prev + payload.content)
        if (typeof payload.running === 'boolean') setRunning(payload.running)
      } catch (_) {}
    }
    return () => es.close()
  }, [id, viewMode, which])

  // Auto-scroll to bottom on new content
  useEffect(() => {
    const el = containerRef.current
    if (!el) return
    // Only auto-scroll if user is near the bottom (within 100px)
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 100
    if (nearBottom) el.scrollTop = el.scrollHeight
  }, [content])

  const rendered = prettify ? prettifyJsonLines(content) : content

  return (
    <div className={`${styles.pane} ${styles.termPane}`}>
      <div className={styles.paneHead}>
        <strong>Raw {which}</strong>
        <span className={styles.termMeta}>
          {running ? <span className={styles.runningDot}>● live</span> : <span className={styles.idleDot}>● idle</span>}
          {' · '}{content.length.toLocaleString()} chars
        </span>
      </div>
      <pre ref={containerRef} className={styles.term}>{rendered || '(no output yet)'}</pre>
    </div>
  )
}

/// Best-effort: any line that's valid JSON gets indented. Anything
/// else is passed through untouched. Cheap enough to redo on each
/// content update for typical sizes.
function prettifyJsonLines(text: string): string {
  return text.split('\n').map(line => {
    const trimmed = line.trim()
    if (!trimmed.startsWith('{') && !trimmed.startsWith('[')) return line
    try {
      return JSON.stringify(JSON.parse(trimmed), null, 2)
    } catch {
      return line
    }
  }).join('\n')
}
