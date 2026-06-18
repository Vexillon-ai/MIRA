// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useRef, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Download, Copy, Check, Activity, FileText, ShieldCheck, RotateCw } from 'lucide-react'
import { logsApi, type LogLevel as ServerLogLevel } from '@/api/logs'
import { toolAuditApi, type ToolAuditRow } from '@/api/toolAudit'
import { getAccessToken } from '@/api/client'
import { useAuthStore } from '@/store/authStore'
import styles from './LogsPage.module.css'

type LogLevel = 'ALL' | 'INFO' | 'WARN' | 'ERROR' | 'DEBUG'
type LogsTab  = 'stream' | 'audit'

const LEVEL_COLORS: Record<string, string> = {
  ERROR: 'var(--error)',
  WARN:  'var(--warning)',
  INFO:  'var(--info)',
  DEBUG: 'var(--text-muted)',
  TRACE: 'var(--text-muted)',
}

function detectLevel(line: string): string {
  if (line.includes(' ERROR') || line.includes('[ERROR]')) return 'ERROR'
  if (line.includes(' WARN')  || line.includes('[WARN]'))  return 'WARN'
  if (line.includes(' INFO')  || line.includes('[INFO]'))  return 'INFO'
  if (line.includes(' DEBUG') || line.includes('[DEBUG]')) return 'DEBUG'
  return 'TRACE'
}

function passesFilter(line: string, level: LogLevel): boolean {
  if (level === 'ALL') return true
  const lineLevel = detectLevel(line)
  if (level === 'ERROR')  return lineLevel === 'ERROR'
  if (level === 'WARN')   return lineLevel === 'ERROR' || lineLevel === 'WARN'
  if (level === 'INFO')   return ['ERROR', 'WARN', 'INFO'].includes(lineLevel)
  if (level === 'DEBUG')  return ['ERROR', 'WARN', 'INFO', 'DEBUG'].includes(lineLevel)
  return true
}

export default function LogsPage() {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const [tab, setTab] = useState<LogsTab>('stream')

  return (
    <div className={styles.page}>
      <div className={styles.tabBar}>
        <button
          className={`${styles.tab} ${tab === 'stream' ? styles.tabActive : ''}`}
          onClick={() => setTab('stream')}
        >
          <FileText size={13} /> Live logs
        </button>
        {isAdmin && (
          <button
            className={`${styles.tab} ${tab === 'audit' ? styles.tabActive : ''}`}
            onClick={() => setTab('audit')}
          >
            <ShieldCheck size={13} /> Tool audit
          </button>
        )}
      </div>
      {tab === 'stream' ? <StreamPanel isAdmin={isAdmin} /> : <AuditPanel />}
    </div>
  )
}

// ── Live logs panel ─────────────────────────────────────────────────────────

function StreamPanel({ isAdmin }: { isAdmin: boolean }) {
  const [lines, setLines]         = useState<string[]>([])
  const [filter, setFilter]       = useState<LogLevel>('ALL')
  const [autoScroll, setAutoScroll] = useState(true)
  const [connected, setConnected] = useState(false)
  const [copied, setCopied]       = useState(false)
  const [serverLevel, setServerLevel]       = useState<ServerLogLevel | null>(null)
  const [serverLevels, setServerLevels]     = useState<ServerLogLevel[]>([])
  const [serverLevelBusy, setServerLevelBusy] = useState(false)
  const bottomRef = useRef<HTMLDivElement>(null)
  const esRef     = useRef<EventSource | null>(null)

  useEffect(() => {
    if (!isAdmin) return
    let cancelled = false
    logsApi.getLevel()
      .then((res) => {
        if (cancelled) return
        setServerLevel(res.level)
        setServerLevels(res.levels)
      })
      .catch(() => { /* ignore — UI just won't show the dropdown */ })
    return () => { cancelled = true }
  }, [isAdmin])

  const handleServerLevelChange = async (next: ServerLogLevel) => {
    setServerLevelBusy(true)
    try {
      const res = await logsApi.setLevel(next)
      setServerLevel(res.level)
    } catch (e) {
      console.error('Failed to change server log level', e)
    } finally {
      setServerLevelBusy(false)
    }
  }

  useEffect(() => {
    // EventSource can't set Authorization headers, so we pass the
    // JWT as a `?token=` query param — AuthLayer accepts it as a
    // fallback when the bearer header isn't present (see the
    // dual-mode auth in 0.130.0). Without this the SSE connect
    // 401s and the page sits stuck at "Connecting to log stream".
    const token = getAccessToken()
    const url   = `/api/logs/stream${token ? `?token=${encodeURIComponent(token)}` : ''}`
    const es = new EventSource(url)
    esRef.current = es

    es.addEventListener('init', (e) => {
      setConnected(true)
      const incoming = (e as MessageEvent).data.split('\n').filter(Boolean)
      setLines(incoming)
    })

    es.addEventListener('lines', (e) => {
      const incoming = (e as MessageEvent).data.split('\n').filter(Boolean)
      setLines((prev) => {
        const next = [...prev, ...incoming]
        return next.length > 2000 ? next.slice(-2000) : next
      })
    })

    es.onerror = () => setConnected(false)

    return () => { es.close(); esRef.current = null }
  }, [])

  useEffect(() => {
    if (autoScroll) {
      bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
    }
  }, [lines, autoScroll])

  const filtered = lines.filter((l) => passesFilter(l, filter))

  const handleCopy = async () => {
    await navigator.clipboard.writeText(filtered.join('\n'))
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  const handleDownload = () => {
    const blob = new Blob([filtered.join('\n')], { type: 'text/plain' })
    const url  = URL.createObjectURL(blob)
    const a    = document.createElement('a')
    a.href     = url
    a.download = `mira-logs-${new Date().toISOString().slice(0, 10)}.txt`
    a.click()
    URL.revokeObjectURL(url)
  }

  return (
    <>
      <div className={styles.toolbar}>
        <div className={styles.statusRow}>
          <span className={`${styles.connDot} ${connected ? styles.dotLive : styles.dotOff}`} />
          <span className={styles.connLabel}>{connected ? 'Live' : 'Disconnected'}</span>
          <span className={styles.lineCount}>{filtered.length} lines</span>
        </div>

        <div className={styles.filters}>
          {(['ALL', 'INFO', 'WARN', 'ERROR', 'DEBUG'] as LogLevel[]).map((l) => (
            <button
              key={l}
              className={`${styles.filterBtn} ${filter === l ? styles.filterActive : ''}`}
              onClick={() => setFilter(l)}
            >
              {l}
            </button>
          ))}
        </div>

        <div className={styles.actions}>
          {isAdmin && serverLevel && (
            <div
              className={styles.serverLevelGroup}
              title="Live server log level. Resets to config on restart."
            >
              <span className={styles.serverLevelLabel}>Server</span>
              <select
                className={styles.serverLevelSelect}
                value={serverLevel}
                disabled={serverLevelBusy}
                onChange={(e) => handleServerLevelChange(e.target.value as ServerLogLevel)}
              >
                {serverLevels.map((lvl) => (
                  <option key={lvl} value={lvl}>{lvl}</option>
                ))}
              </select>
            </div>
          )}
          <label className={styles.toggleLabel}>
            <input
              type="checkbox"
              className={styles.toggleCheck}
              checked={autoScroll}
              onChange={(e) => setAutoScroll(e.target.checked)}
            />
            <Activity size={13} />
            Auto-scroll
          </label>
          <button className={styles.iconBtn} onClick={handleCopy} title="Copy">
            {copied ? <Check size={14} /> : <Copy size={14} />}
          </button>
          <button className={styles.iconBtn} onClick={handleDownload} title="Download">
            <Download size={14} />
          </button>
        </div>
      </div>

      <div className={styles.output}>
        {filtered.map((line, i) => {
          const level = detectLevel(line)
          return (
            <div key={i} className={styles.logLine} style={{ color: LEVEL_COLORS[level] }}>
              {line}
            </div>
          )
        })}
        {filtered.length === 0 && (
          <p className={styles.empty}>
            {connected ? 'No log lines match the current filter.' : 'Connecting to log stream…'}
          </p>
        )}
        <div ref={bottomRef} />
      </div>
    </>
  )
}

// ── Audit panel ─────────────────────────────────────────────────────────────

const PAGE_SIZE = 100

function AuditPanel() {
  const [outcome, setOutcome]   = useState<'' | 'success' | 'failure' | 'error'>('')
  const [toolFilter, setTool]   = useState('')
  const [actorFilter, setActor] = useState('')
  const [offset, setOffset]     = useState(0)

  const { data, isLoading, refetch, isFetching } = useQuery({
    queryKey: ['tool-audit', { outcome, toolFilter, actorFilter, offset }],
    queryFn: () => toolAuditApi.list({
      limit:  PAGE_SIZE,
      offset,
      actor:  actorFilter || undefined,
      tool:   toolFilter || undefined,
      outcome: outcome || undefined,
    }),
    refetchInterval: 15_000,
  })

  const total = data?.total ?? 0
  const rows  = data?.rows ?? []

  const onFilterChange = () => setOffset(0)

  return (
    <>
      <div className={styles.toolbar}>
        <div className={styles.statusRow}>
          <ShieldCheck size={13} />
          <span className={styles.connLabel}>Tool audit</span>
          <span className={styles.lineCount}>{total} rows total</span>
        </div>

        <div className={styles.filters}>
          {(['', 'success', 'failure', 'error'] as const).map((o) => (
            <button
              key={o || 'all'}
              className={`${styles.filterBtn} ${outcome === o ? styles.filterActive : ''}`}
              onClick={() => { setOutcome(o); onFilterChange() }}
            >
              {o || 'ALL'}
            </button>
          ))}
          <input
            className={styles.filterBtn}
            placeholder="tool…"
            value={toolFilter}
            onChange={(e) => { setTool(e.target.value); onFilterChange() }}
            style={{ minWidth: 100 }}
          />
          <input
            className={styles.filterBtn}
            placeholder="actor…"
            value={actorFilter}
            onChange={(e) => { setActor(e.target.value); onFilterChange() }}
            style={{ minWidth: 120 }}
          />
        </div>

        <div className={styles.actions}>
          <button
            className={styles.iconBtn}
            onClick={() => refetch()}
            title="Refresh"
            disabled={isFetching}
          >
            <RotateCw size={14} />
          </button>
        </div>
      </div>

      <div className={styles.auditWrap}>
        {isLoading ? (
          <p className={styles.empty}>Loading audit rows…</p>
        ) : rows.length === 0 ? (
          <p className={styles.empty}>No audit rows match the current filter.</p>
        ) : (
          <table className={styles.auditTable}>
            <thead>
              <tr>
                <th>When</th>
                <th>Actor</th>
                <th>Tool</th>
                <th>Outcome</th>
                <th>Duration</th>
                <th>Digest</th>
                <th>Output</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => <AuditRowView key={r.id} row={r} />)}
            </tbody>
          </table>
        )}
      </div>

      <div className={styles.pager}>
        <button
          className={styles.pageBtn}
          disabled={offset === 0}
          onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
        >
          ‹ Prev
        </button>
        <span>
          {rows.length === 0 ? 0 : offset + 1}–{offset + rows.length} of {total}
        </span>
        <button
          className={styles.pageBtn}
          disabled={offset + rows.length >= total}
          onClick={() => setOffset(offset + PAGE_SIZE)}
        >
          Next ›
        </button>
      </div>
    </>
  )
}

function AuditRowView({ row }: { row: ToolAuditRow }) {
  const when = new Date(row.started_at).toLocaleString()
  const badge =
    row.outcome === 'success' ? styles.outcomeSuccess :
    row.outcome === 'failure' ? styles.outcomeFailure :
                                styles.outcomeError
  return (
    <tr>
      <td>{when}</td>
      <td>{row.actor}</td>
      <td>{row.tool}</td>
      <td><span className={`${styles.outcomeBadge} ${badge}`}>{row.outcome}</span></td>
      <td>{row.duration_ms} ms</td>
      <td title={row.args_digest}>{row.args_digest.slice(0, 12)}…</td>
      <td><span className={styles.auditSnippet} title={row.truncated_output ?? ''}>
        {row.truncated_output ?? ''}
      </span></td>
    </tr>
  )
}
