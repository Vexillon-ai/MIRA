// SPDX-License-Identifier: AGPL-3.0-or-later

import { useMemo, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { ScrollText, Download, RefreshCw, AlertTriangle, ShieldCheck } from 'lucide-react'
import { agentsApi, ALL_AUDIT_KINDS, type AuditKind, type AuditRow } from '@/api/agents'
import styles from './AuditPage.module.css'

/// Slice D4 — surfaces the B9 audit log (HMAC-chained) plus the
/// PolicyDecision rows D1/D2/D3 added on top. Filter / search / export.
export default function AuditPage() {
  const [agentFilter, setAgentFilter]   = useState('')
  const [activeKinds, setActiveKinds]   = useState<Set<AuditKind>>(new Set())
  const [searchTerm, setSearchTerm]     = useState('')
  const [limit, setLimit]               = useState(200)

  const { data, isLoading, error, refetch, isRefetching } = useQuery({
    queryKey: ['audit', agentFilter, Array.from(activeKinds).sort(), limit],
    queryFn:  () => agentsApi.audit({
      agent_id: agentFilter.trim() || undefined,
      kinds:    activeKinds.size > 0 ? Array.from(activeKinds) : undefined,
      limit,
    }),
    // Polled rather than SSE: the audit log is append-only and the
    // typical workflow is "go look at what happened in the last hour"
    // — no point burning a websocket on a snapshot view.
    refetchInterval: 5_000,
  })

  // Client-side substring search across kind, agent_id (truncated),
  // and the JSON-stringified event payload. The server-side query is
  // already filtered by kind / agent_id, so the typical dataset is
  // ~hundreds of rows — fast to filter in memory.
  const visible = useMemo<AuditRow[]>(() => {
    const rows = data?.rows ?? []
    if (!searchTerm.trim()) return rows
    const needle = searchTerm.toLowerCase()
    return rows.filter((r) => {
      const eventJson = JSON.stringify(r.event).toLowerCase()
      return r.kind.includes(needle)
          || r.agent_id.toLowerCase().includes(needle)
          || eventJson.includes(needle)
    })
  }, [data?.rows, searchTerm])

  const toggleKind = (k: AuditKind) => {
    setActiveKinds((prev) => {
      const next = new Set(prev)
      if (next.has(k)) next.delete(k); else next.add(k)
      return next
    })
  }

  const exportJson = () => {
    if (!visible.length) {
      toast.error('No rows to export.')
      return
    }
    downloadFile(
      `mira-audit-${new Date().toISOString().slice(0, 19)}.json`,
      'application/json',
      JSON.stringify(visible, null, 2),
    )
    toast.success(`Exported ${visible.length} row${visible.length === 1 ? '' : 's'}`)
  }

  const exportCsv = () => {
    if (!visible.length) {
      toast.error('No rows to export.')
      return
    }
    downloadFile(
      `mira-audit-${new Date().toISOString().slice(0, 19)}.csv`,
      'text/csv',
      rowsToCsv(visible),
    )
    toast.success(`Exported ${visible.length} row${visible.length === 1 ? '' : 's'}`)
  }

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1>
          <ScrollText size={18} style={{ verticalAlign: 'text-bottom', marginRight: 8 }} />
          Audit log
        </h1>
        <p>Append-only HMAC-chained record of every spawn, status change, budget kill, interrupt, and policy decision. Each row links to the previous via HMAC; the chain is verified on every read so tampering surfaces immediately.</p>

        {data && (
          <div className={styles.chainStatus} data-ok={data.chain_ok}>
            {data.chain_ok ? (
              <><ShieldCheck size={14} /> Chain verified ({data.rows.length} rows)</>
            ) : (
              <><AlertTriangle size={14} /> CHAIN BROKEN — {data.chain_break ?? 'no detail'}</>
            )}
          </div>
        )}
      </header>

      <div className={styles.controls}>
        <div className={styles.filterRow}>
          <input
            type="text"
            className={styles.input}
            placeholder="Filter by agent UUID (full id)…"
            value={agentFilter}
            onChange={(e) => setAgentFilter(e.target.value)}
          />
          <input
            type="text"
            className={styles.input}
            placeholder="Search across kind, agent, payload…"
            value={searchTerm}
            onChange={(e) => setSearchTerm(e.target.value)}
          />
          <select
            className={styles.input}
            value={limit}
            onChange={(e) => setLimit(Number(e.target.value))}
            aria-label="Row limit"
          >
            <option value={50}>50 rows</option>
            <option value={200}>200 rows</option>
            <option value={500}>500 rows</option>
            <option value={1000}>1000 rows</option>
          </select>
          <button
            type="button"
            className={styles.iconBtn}
            onClick={() => refetch()}
            disabled={isRefetching}
            title="Refetch now"
          >
            <RefreshCw size={13} className={isRefetching ? styles.spin : ''} />
          </button>
          <button
            type="button"
            className={styles.iconBtn}
            onClick={exportCsv}
            disabled={!visible.length}
            title="Export visible rows as CSV"
          >
            <Download size={13} /> CSV
          </button>
          <button
            type="button"
            className={styles.iconBtn}
            onClick={exportJson}
            disabled={!visible.length}
            title="Export visible rows as JSON"
          >
            <Download size={13} /> JSON
          </button>
        </div>
        <div className={styles.kindFilters}>
          {ALL_AUDIT_KINDS.map((k) => {
            const active = activeKinds.has(k)
            return (
              <button
                type="button"
                key={k}
                className={`${styles.kindChip} ${active ? styles.active : ''}`}
                data-kind={k}
                onClick={() => toggleKind(k)}
              >
                {k}
              </button>
            )
          })}
          {activeKinds.size > 0 && (
            <button
              type="button"
              className={styles.clearKinds}
              onClick={() => setActiveKinds(new Set())}
            >
              clear
            </button>
          )}
        </div>
      </div>

      <div className={styles.body}>
        {isLoading && <div className={styles.empty}>Loading…</div>}
        {error && <div className={styles.empty}>Failed to load audit log.</div>}

        {data && visible.length === 0 && (
          <div className={styles.empty}>
            <strong>No matching rows.</strong>
            {searchTerm || activeKinds.size > 0 || agentFilter
              ? 'Try widening the filters or clearing the search.'
              : 'The audit log is empty — nothing has happened yet that the supervisor records.'}
          </div>
        )}

        {data && visible.length > 0 && (
          <table className={styles.table}>
            <thead>
              <tr>
                <th>Time</th>
                <th>Kind</th>
                <th>Agent</th>
                <th>Event detail</th>
              </tr>
            </thead>
            <tbody>
              {visible.map((row) => (
                <AuditRowItem key={row.id} row={row} />
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  )
}

function AuditRowItem({ row }: { row: AuditRow }) {
  const [expanded, setExpanded] = useState(false)
  return (
    <>
      <tr className={styles.row} data-kind={row.kind} onClick={() => setExpanded((v) => !v)}>
        <td className={styles.tsCell}>{formatTs(row.ts_ms)}</td>
        <td>
          <span className={styles.kindBadge} data-kind={row.kind}>{row.kind}</span>
        </td>
        <td className={styles.agentCell}>
          <code>{row.agent_id.slice(0, 8)}…</code>
        </td>
        <td className={styles.summaryCell}>
          {summariseEvent(row)}
        </td>
      </tr>
      {expanded && (
        <tr className={styles.detailRow}>
          <td colSpan={4}>
            <pre className={styles.detailPre}>
{JSON.stringify(row.event, null, 2)}
            </pre>
            <div className={styles.hmacRow}>
              <span title="prev_hmac"><strong>prev:</strong> <code>{row.prev_hmac}</code></span>
              <span title="hmac"><strong>hmac:</strong> <code>{row.hmac}</code></span>
            </div>
          </td>
        </tr>
      )}
    </>
  )
}

// ── Helpers (pure) ────────────────────────────────────────────────────

function formatTs(ms: number): string {
  // Local time, ISO-like for grep-ability.
  const d = new Date(ms)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}

/// Build a one-line human-readable summary from a row's event payload.
/// We deliberately don't enumerate every field — the expanded JSON
/// view is one click away. The summary just gives enough context to
/// scan the table.
function summariseEvent(row: AuditRow): string {
  const e = row.event as Record<string, unknown>
  switch (row.kind) {
    case 'spawn_requested':
      return `requested ${asString(e.skill_id)} (budget $${asNumber(e.budget_usd).toFixed(2)})`
    case 'spawn_approved':
      return `approved ${asString(e.skill_id)} → child ${asString(e.child_id).slice(0, 8)}…`
    case 'spawn_denied':
      return `DENIED ${asString(e.skill_id)} — ${asString(e.reason)}`
    case 'status_change':
      return `${asString(e.from)} → ${asString(e.to)}`
    case 'agent_budget_exceeded':
      return `agent over budget: $${asNumber(e.spent_usd).toFixed(4)} / $${asNumber(e.cap_usd).toFixed(2)}`
    case 'session_budget_exceeded':
      return `session over budget: $${asNumber(e.session_spent_usd).toFixed(4)} / $${asNumber(e.session_cap_usd).toFixed(2)}`
    case 'interrupted':
      return `interrupted (${asString(e.reason)})`
    case 'policy_decision': {
      const granted = e.granted as boolean
      const rule    = asString(e.rule)
      const detail  = asString(e.detail)
      return `${granted ? 'allowed' : 'DENIED'} by rule '${rule}'${detail ? `: ${detail}` : ''}`
    }
    default:
      return JSON.stringify(e)
  }
}

function asString(v: unknown): string {
  return v == null ? '' : String(v)
}
function asNumber(v: unknown): number {
  return typeof v === 'number' ? v : Number(v ?? 0)
}

function rowsToCsv(rows: AuditRow[]): string {
  // Header + one row per audit row. event_json is the raw payload so
  // post-processing in a spreadsheet still has everything available.
  const header = ['id', 'ts_iso', 'kind', 'agent_id', 'event_json', 'hmac', 'prev_hmac']
  const lines: string[] = [header.join(',')]
  for (const r of rows) {
    lines.push([
      String(r.id),
      new Date(r.ts_ms).toISOString(),
      r.kind,
      r.agent_id,
      csvEscape(JSON.stringify(r.event)),
      r.hmac,
      r.prev_hmac,
    ].join(','))
  }
  return lines.join('\n')
}

/// Quote-and-escape per RFC 4180. Values containing comma, quote, or
/// newline get wrapped in double-quotes; embedded quotes are doubled.
function csvEscape(value: string): string {
  if (/[,"\n\r]/.test(value)) {
    return `"${value.replace(/"/g, '""')}"`
  }
  return value
}

function downloadFile(filename: string, mime: string, content: string) {
  const blob = new Blob([content], { type: mime })
  const url  = URL.createObjectURL(blob)
  const a    = document.createElement('a')
  a.href = url
  a.download = filename
  document.body.appendChild(a)
  a.click()
  document.body.removeChild(a)
  // setTimeout because some browsers clean up too early otherwise.
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
