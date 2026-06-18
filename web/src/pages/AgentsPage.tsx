// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { Network, Play, Pause as PauseIcon, Square, ExternalLink, Radio } from 'lucide-react'
import { useNavigate } from 'react-router-dom'
import { agentsApi, type AgentDto, type AgentStatus, type AgentsResponse } from '@/api/agents'
import { getAccessToken } from '@/api/client'
import { formatDistanceToNow } from 'date-fns'
import styles from './AgentsPage.module.css'

const TERMINAL_STATUSES: AgentStatus[] = ['completed', 'failed', 'interrupted']

export default function AgentsPage() {
  const qc = useQueryClient()
  // SSE drives live updates; the query is the initial load + a fallback poll
  // that only runs while the stream isn't connected.
  const [live, setLive] = useState(false)
  const { data, isLoading, error } = useQuery({
    queryKey: ['agents'],
    queryFn:  () => agentsApi.list(),
    refetchInterval: live ? false : 2000,
  })

  useEffect(() => {
    const token = getAccessToken()
    const url = `/api/agents/stream${token ? `?token=${encodeURIComponent(token)}` : ''}`
    const es = new EventSource(url)
    es.onmessage = (e) => {
      try {
        qc.setQueryData<AgentsResponse>(['agents'], JSON.parse(e.data))
        setLive(true)
      } catch { /* ignore malformed frame */ }
    }
    es.onerror = () => setLive(false) // fall back to polling; EventSource auto-reconnects
    return () => es.close()
  }, [qc])

  const agg = data?.aggregate

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1>
          <Network size={18} style={{ verticalAlign: 'text-bottom', marginRight: 8 }} />
          Agents
          <span className={live ? styles.liveOn : styles.liveOff} title={live ? 'Live (streaming)' : 'Polling'}>
            <Radio size={12} /> {live ? 'live' : 'polling'}
          </span>
        </h1>
        <p>Live tree of every agent the supervisor is currently managing. Workers spawned by Skills appear here as soon as they start; rows persist briefly after completion so you can see the outcome.</p>
        {agg && agg.total > 0 && (
          <div className={styles.fleetBar}>
            <span className={styles.stat}><b>{agg.total}</b> agents</span>
            {agg.running > 0 && <span className={`${styles.stat} ${styles.statRun}`}><b>{agg.running}</b> running</span>}
            {agg.paused > 0 && <span className={styles.stat}><b>{agg.paused}</b> paused</span>}
            {agg.completed > 0 && <span className={`${styles.stat} ${styles.statOk}`}><b>{agg.completed}</b> done</span>}
            {agg.failed > 0 && <span className={`${styles.stat} ${styles.statFail}`}><b>{agg.failed}</b> failed</span>}
            {agg.interrupted > 0 && <span className={styles.stat}><b>{agg.interrupted}</b> stopped</span>}
            <span className={styles.statSpend}>spend <b>${agg.total_spent_usd.toFixed(2)}</b></span>
          </div>
        )}
        {data && (
          <div className={styles.runtimeMeta}>
            <span>Max recursion depth: <code>{data.max_recursion_depth}</code></span>
            <span>Default session budget: <code>${data.default_session_usd.toFixed(2)}</code></span>
          </div>
        )}
      </header>

      <div className={styles.body}>
        {isLoading && <div className={styles.empty}>Loading…</div>}
        {error && <div className={styles.empty}>Failed to load agents.</div>}

        {data && data.agents.length === 0 && (
          <div className={styles.empty}>
            <strong>No agents are running.</strong>
            Workers spawn here when a Skill executor calls into the supervisor — currently only via tests and the in-progress Phase C adapters. They'll appear automatically as Phase C lands.
          </div>
        )}

        {data && data.agents.length > 0 && (
          <div className={styles.tree}>
            {orderForTree(data.agents).map((agent) => (
              <AgentCard key={agent.id} agent={agent} />
            ))}
          </div>
        )}
      </div>
    </div>
  )
}

/// Sort the flat list so children render right after their parents,
/// preserving the spawn-order siblings come back in. Roots first,
/// then a recursive walk under each.
function orderForTree(flat: AgentDto[]): AgentDto[] {
  const byId = new Map(flat.map((a) => [a.id, a]))
  const childrenOf = (id: string) => {
    const a = byId.get(id)
    if (!a) return []
    return a.child_ids.map((cid) => byId.get(cid)).filter((x): x is AgentDto => Boolean(x))
  }
  const out: AgentDto[] = []
  const walk = (a: AgentDto) => {
    out.push(a)
    for (const child of childrenOf(a.id)) walk(child)
  }
  for (const a of flat) {
    if (!a.parent) walk(a)
  }
  // Anything orphaned (parent missing from the snapshot) — append so
  // it doesn't silently disappear from the UI.
  for (const a of flat) {
    if (a.parent && !out.includes(a)) out.push(a)
  }
  return out
}

function AgentCard({ agent }: { agent: AgentDto }) {
  const qc = useQueryClient()
  const navigate = useNavigate()
  const isTerminal = TERMINAL_STATUSES.includes(agent.status)

  const interruptMut = useMutation({
    mutationFn: () => agentsApi.interrupt(agent.id, { propagate: true }),
    onSuccess: (data) => {
      qc.invalidateQueries({ queryKey: ['agents'] })
      toast.success(`Stopped ${data.signalled} agent${data.signalled === 1 ? '' : 's'}`)
    },
    onError: (e: Error) => toast.error(`Stop failed: ${e.message}`),
  })

  const pauseMut = useMutation({
    mutationFn: () => agentsApi.pause(agent.id),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ['agents'] }); toast.success('Paused') },
    onError: (e: Error) => toast.error(`Pause failed: ${e.message}`),
  })

  const resumeMut = useMutation({
    mutationFn: () => agentsApi.resume(agent.id),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ['agents'] }); toast.success('Resumed') },
    onError: (e: Error) => toast.error(`Resume failed: ${e.message}`),
  })

  const depthClass = agent.depth > 0 ? styles[`depth${Math.min(agent.depth, 5)}` as `depth${1 | 2 | 3 | 4 | 5}`] : ''

  return (
    <div className={`${styles.card} ${depthClass}`} data-status={agent.status}>
      <div className={styles.cardHead}>
        <div className={styles.titleRow}>
          <span className={styles.title}>
            {agent.skill_id ?? '(root agent)'}
          </span>
          <span className={styles.id}>{agent.id.slice(0, 8)}…</span>
          <span className={styles.statusBadge} data-status={agent.status}>
            {agent.status}
          </span>
        </div>
        <div className={styles.actions}>
          <button type="button" className={styles.actionBtn}
                  onClick={() => navigate(`/agents/${agent.id}`)}
                  title="Open detail view — structured activity + raw stdout">
            <ExternalLink size={11} /> Open
          </button>
          {!isTerminal && (agent.status === 'paused'
              ? <button type="button" className={styles.actionBtn}
                        disabled={resumeMut.isPending}
                        onClick={() => resumeMut.mutate()}>
                  <Play size={11} /> Resume
                </button>
              : <button type="button" className={styles.actionBtn}
                        disabled={pauseMut.isPending}
                        onClick={() => pauseMut.mutate()}>
                  <PauseIcon size={11} /> Pause
                </button>)}
          {!isTerminal && (
            <button type="button" className={styles.actionBtn} data-variant="danger"
                    disabled={interruptMut.isPending}
                    onClick={() => interruptMut.mutate()}>
              <Square size={11} /> Stop
            </button>
          )}
        </div>
      </div>

      {agent.current_step && (
        <div className={styles.currentStep}>{agent.current_step}</div>
      )}

      {!isTerminal && agent.percent_done != null && (
        <div className={styles.progressTrack} title={`${Math.round(agent.percent_done * 100)}% done`}>
          <div className={styles.progressFill} style={{ width: `${Math.round(Math.min(1, Math.max(0, agent.percent_done)) * 100)}%` }} />
          <span className={styles.progressPct}>{Math.round(agent.percent_done * 100)}%</span>
        </div>
      )}

      {agent.failure_reason && (
        <div className={styles.failureReason}>
          {agent.fault?.code && <code className={styles.faultCode}>{agent.fault.code}</code>}
          {agent.failure_reason}
        </div>
      )}

      <div className={styles.meta}>
        <span>Depth: <strong>{agent.depth}</strong></span>
        <span>Spent: <strong>${agent.spent_usd.toFixed(4)}</strong>
              {agent.max_usd != null && <> / ${agent.max_usd.toFixed(2)}</>}</span>
        {(() => {
          // Burn rate: live $/min for a running agent that's spent something.
          const ageMin = (Date.now() - agent.created_at_ms) / 60000
          if (!isTerminal && agent.spent_usd > 0 && ageMin > 0.1) {
            return <span title="Live burn rate">Burn: <strong>${(agent.spent_usd / ageMin).toFixed(3)}/min</strong></span>
          }
          return null
        })()}
        <span>Age: <strong>{formatDistanceToNow(new Date(agent.created_at_ms), { addSuffix: false })}</strong></span>
        {agent.parent && (
          <span>Parent: <code>{agent.parent.slice(0, 8)}…</code></span>
        )}
        {agent.llm_choice && (
          <span title={`alias: ${agent.llm_choice.alias}`}>
            LLM: <code>{agent.llm_choice.provider}{agent.llm_choice.model ? `/${agent.llm_choice.model}` : ''}</code>
          </span>
        )}
      </div>
    </div>
  )
}
