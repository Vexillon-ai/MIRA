// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useState } from 'react'
import { createPortal } from 'react-dom'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useNavigate } from 'react-router-dom'
import toast from 'react-hot-toast'
import { Activity, RefreshCw, Play } from 'lucide-react'
import {
  healthApi,
  taskFileUrl,
  type ActionPolicy,
  type DetectorReport,
  type DetectorConfigEntry,
  type CustomDetectorRow,
  type WebhookListRow,
} from '@/api/health'
import { guardianApi } from '@/api/guardian'
import styles from './SystemHealthPage.module.css'

type Tab = 'status' | 'incidents' | 'config' | 'bans' | 'custom' | 'webhooks' | 'artifacts'

/// 0.107.0 — admin dashboard for the system_audit subsystem.
/// Backend handlers live at `/api/health/*` (see
/// `src/server/handlers/health_dashboard.rs`). All routes here are
/// admin-only — page is gated by AdminGuard in App.tsx.
export default function SystemHealthPage() {
  const [tab, setTab] = useState<Tab>('status')
  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1>
          <Activity size={18} style={{ verticalAlign: 'text-bottom', marginRight: 8 }} />
          System health
        </h1>
        <p>
          Hourly self-audit of MIRA's process, databases, automations,
          watchdog, agents, auth, and skills. Detectors that fire file
          watchdog incidents through the same pipeline as log-derived
          alerts.
        </p>
      </header>

      <DegradationBanner />
      <GuardianStatusPanel />
      <GuardianProvisionPanel />
      <GuardianActionsPanel />

      <nav className={styles.tabs}>
        <TabButton active={tab === 'status'}    onClick={() => setTab('status')}>Status</TabButton>
        <TabButton active={tab === 'incidents'} onClick={() => setTab('incidents')}>Incidents</TabButton>
        <TabButton active={tab === 'config'}    onClick={() => setTab('config')}>Config</TabButton>
        <TabButton active={tab === 'custom'}    onClick={() => setTab('custom')}>Custom SQL</TabButton>
        <TabButton active={tab === 'webhooks'}  onClick={() => setTab('webhooks')}>Webhooks</TabButton>
        <TabButton active={tab === 'artifacts'} onClick={() => setTab('artifacts')}>Artifacts</TabButton>
        <TabButton active={tab === 'bans'}      onClick={() => setTab('bans')}>IP bans</TabButton>
      </nav>

      {tab === 'status'    && <StatusTab />}
      {tab === 'incidents' && <IncidentsTab />}
      {tab === 'config'    && <ConfigTab />}
      {tab === 'custom'    && <CustomDetectorsTab />}
      {tab === 'webhooks'  && <WebhooksTab />}
      {tab === 'artifacts' && <ArtifactsTab />}
      {tab === 'bans'      && <BansTab />}
    </div>
  )
}

/// Live banner for subsystems currently on a degraded fallback path (LLM
/// provider, TTS, STT, embeddings, reasoning). Polled every 30s + refreshed
/// immediately by the `system_degraded` notification.
function DegradationBanner() {
  const q = useQuery({
    queryKey: ['health-degradations'],
    queryFn:  healthApi.degradations,
    refetchInterval: 30_000,
    retry: false,
  })
  const items = q.data ?? []
  if (items.length === 0) return null
  return (
    <div className={styles.degradeBanner} role="alert">
      <strong>⚠️ {items.length} subsystem{items.length !== 1 ? 's' : ''} degraded</strong>
      <ul>
        {items.map((d) => (
          <li key={d.subsystem}>
            <b>{d.label}</b> fell back from <code>{d.from}</code> to <code>{d.to}</code>
            {' '}— {d.reason}
            {d.count > 1 && <> ({d.count}×)</>}
            {!d.persistent && <span className={styles.degradeTransient}> · transient</span>}
          </li>
        ))}
      </ul>
    </div>
  )
}

// MIRA-Guardian — always-on status. Unlike the provision/actions panels below
// (which render only when there's something to *do*), this is shown whenever the
// status endpoint answers, so the operator can always see the Guardian's mode,
// local-model verdict, watch-loop liveness, and recent actions — even idle.
function GuardianStatusPanel() {
  const q = useQuery({
    queryKey: ['guardian-status'],
    queryFn:  guardianApi.status,
    refetchInterval: 30_000,
    retry: false,
  })
  const s = q.data
  if (!s) return null // non-admin / endpoint unavailable — stay quiet
  const ago = (ts: number | null): string => {
    if (!ts) return 'never'
    const secs = Math.max(0, Math.floor(Date.now() / 1000) - ts)
    if (secs < 60)    return `${secs}s ago`
    if (secs < 3600)  return `${Math.floor(secs / 60)}m ago`
    if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`
    return `${Math.floor(secs / 86400)}d ago`
  }
  const autonomyLive = s.mode === 'Active' && !s.isolation_dry_run
  return (
    <div className={styles.guardianPanel} role="region" aria-label="MIRA-Guardian status">
      <strong>🛡️ MIRA-Guardian — {s.mode}{autonomyLive ? ' · autonomy LIVE' : ''}</strong>
      <ul>
        <li>
          Local model: {s.local_model_ok ? '✓ ok' : '✗ none'}
          {s.guardian_alias_set ? ' · alias bound' : ''}
          <span style={{ opacity: 0.7 }}> — {s.model_check}</span>
        </li>
        <li>
          Watch loop:{' '}
          {s.mode === 'Off'
            ? 'idle (mode off)'
            : `ran ${ago(s.watch.last_run_at)} (every ${Math.round(s.watch_interval_secs / 60)}m)`}
          {s.watch.alerts_total > 0 &&
            ` · ${s.watch.alerts_total} alert${s.watch.alerts_total !== 1 ? 's' : ''} this session`}
        </li>
        {s.watch.last_alert_at != null && (
          <li>
            Last alert {ago(s.watch.last_alert_at)} ({s.watch.last_alert_detectors} detector
            {s.watch.last_alert_detectors !== 1 ? 's' : ''}):
            <span style={{ opacity: 0.85 }}> {s.watch.last_alert_summary}</span>
          </li>
        )}
        {s.recent_actions.length > 0 && (
          <li>
            Recent actions:
            <ul style={{ marginTop: 4 }}>
              {s.recent_actions.slice(0, 5).map((a) => (
                <li key={a.id}>
                  <code>{a.kind}</code> — {a.status}
                  {a.result ? <span style={{ opacity: 0.7 }}> ({a.result})</span> : null}
                </li>
              ))}
            </ul>
          </li>
        )}
      </ul>
    </div>
  )
}

// MIRA-Guardian (P2b) — local-model provisioning. Shown only when the Guardian
// has no resolvable local model; offers a one-click Ollama pull + bind.
function GuardianProvisionPanel() {
  const qc = useQueryClient()
  const q = useQuery({
    queryKey: ['guardian-provision-status'],
    queryFn:  guardianApi.provisionStatus,
    refetchInterval: 30_000,
    retry: false,
  })
  const provision = useMutation({
    mutationFn: () => guardianApi.provision(),
    onSuccess: (d) => {
      toast.success(`Provisioning '${d.model}' — pulling in the background. ${d.note ?? ''}`,
                    { duration: 9000 })
      qc.invalidateQueries({ queryKey: ['guardian-provision-status'] })
    },
    onError: () => toast.error('Provision failed (is Ollama running?)'),
  })
  const s = q.data
  // Nothing to do when the Guardian already has a local model (or status absent).
  if (!s || s.local_model_ok) return null
  return (
    <div className={styles.guardianPanel} role="region" aria-label="MIRA-Guardian model setup">
      <strong>🛡️ MIRA-Guardian needs a local model</strong>
      <ul>
        <li>{s.next_step}</li>
        <li>
          Ollama (<code>{s.ollama.url}</code>):{' '}
          {s.ollama.reachable
            ? <>reachable{s.ollama.version ? <> v{s.ollama.version}</> : null}; model{' '}
                <code>{s.ollama.recommended_model}</code>{' '}
                {s.ollama.model_present ? 'pulled ✓' : 'not pulled'}</>
            : <>not reachable</>}
        </li>
      </ul>
      <div className={styles.guardianBtns}>
        <button type="button" className={styles.guardianApprove}
                disabled={!s.ollama.reachable || provision.isPending}
                onClick={() => provision.mutate()}>
          {s.ollama.model_present ? 'Bind model' : `Pull + bind ${s.ollama.recommended_model}`}
        </button>
      </div>
    </div>
  )
}

// MIRA-Guardian (P4) — pending action proposals awaiting operator approval.
// The Guardian only *proposes*; approving here triggers deterministic
// server-side execution of the bounded action.
function GuardianActionsPanel() {
  const qc = useQueryClient()
  const q = useQuery({
    queryKey: ['guardian-actions-pending'],
    queryFn:  guardianApi.pending,
    refetchInterval: 30_000,
    retry: false,
  })
  const refresh = () => qc.invalidateQueries({ queryKey: ['guardian-actions-pending'] })
  const approve = useMutation({
    mutationFn: (id: string) => guardianApi.approve(id),
    onSuccess: (d) => {
      if (d.status === 'executed') toast.success(`Guardian: executed — ${d.result ?? 'done'}`)
      else toast.error(`Guardian: ${d.status}${d.error ? ' — ' + d.error : ''}`)
      refresh()
    },
    onError: () => toast.error('Approve failed'),
  })
  const decline = useMutation({
    mutationFn: (id: string) => guardianApi.decline(id),
    onSuccess: () => { toast('Guardian proposal declined'); refresh() },
    onError: () => toast.error('Decline failed'),
  })

  const items = q.data ?? []
  if (items.length === 0) return null
  const busy = approve.isPending || decline.isPending
  return (
    <div className={styles.guardianPanel} role="region" aria-label="MIRA-Guardian pending actions">
      <strong>🛡️ MIRA-Guardian — {items.length} action{items.length !== 1 ? 's' : ''} awaiting approval</strong>
      <ul>
        {items.map((a) => (
          <li key={a.id}>
            <span>
              <code>{a.kind}</code>
              {a.target ? <> <code>{a.target}</code></> : null} — {a.reason}
            </span>
            <span className={styles.guardianBtns}>
              <button type="button" className={styles.guardianApprove}
                      disabled={busy} onClick={() => approve.mutate(a.id)}>Approve</button>
              <button type="button" className={styles.guardianDecline}
                      disabled={busy} onClick={() => decline.mutate(a.id)}>Decline</button>
            </span>
          </li>
        ))}
      </ul>
    </div>
  )
}

function TabButton({ active, onClick, children }: {
  active: boolean; onClick: () => void; children: React.ReactNode
}) {
  return (
    <button
      type="button"
      className={`${styles.tab} ${active ? styles.tabActive : ''}`}
      onClick={onClick}
    >
      {children}
    </button>
  )
}

// ── Status tab ──────────────────────────────────────────────────────────────

function StatusTab() {
  const qc = useQueryClient()
  const [autoRefresh, setAutoRefresh] = useState(true)
  const [expanded, setExpanded] = useState<Record<string, boolean>>({})

  const snapshotQ = useQuery({
    queryKey: ['health', 'snapshot'],
    queryFn:  healthApi.snapshot,
    refetchInterval: autoRefresh ? 30_000 : false,
    retry: 1,
  })

  const runMut = useMutation({
    mutationFn: healthApi.runNow,
    onSuccess: () => {
      toast.success('Audit queued — refresh in ~5 seconds')
      // Wait briefly then re-pull. The dispatcher tick is fast.
      setTimeout(() => qc.invalidateQueries({ queryKey: ['health', 'snapshot'] }), 6_000)
    },
    onError: (e: any) => toast.error(`Run audit failed: ${e?.response?.data?.error ?? e.message}`),
  })

  if (snapshotQ.isLoading) return <div className={styles.loading}>Loading snapshot…</div>
  if (snapshotQ.error) return (
    <div className={styles.empty}>
      <strong>Couldn't load snapshot.</strong>
      Try clicking <em>Run audit now</em> — the heartbeat may not have fired yet.
    </div>
  )
  if (!snapshotQ.data) return <div className={styles.empty}>No snapshot recorded yet.</div>

  const snap = snapshotQ.data
  const total    = snap.reports.length
  const greens   = snap.reports.filter(r => r.level === 'green').length
  const yellows  = snap.reports.filter(r => r.level === 'yellow').length
  const reds     = snap.reports.filter(r => r.level === 'red').length
  const takenAgo = formatAgo(snap.taken_at)

  return (
    <>
      <div className={styles.toolbar}>
        <div className={styles.summary}>
          Last audit <strong>{takenAgo}</strong> · {total} detectors ·{' '}
          {greens > 0   && <><span style={{ color: '#22c55e' }}>●</span> {greens} green </>}
          {yellows > 0  && <>· <span style={{ color: '#f59e0b' }}>●</span> {yellows} yellow </>}
          {reds > 0     && <>· <span style={{ color: '#ef4444' }}>●</span> {reds} red </>}
          · {snap.duration_ms}ms
        </div>
        <button
          type="button"
          className={styles.runBtn}
          onClick={() => runMut.mutate()}
          disabled={runMut.isPending}
        >
          <Play size={11} style={{ verticalAlign: 'middle' }} />{' '}
          {runMut.isPending ? 'Queued…' : 'Run audit now'}
        </button>
        <label className={styles.refreshLabel}>
          <input
            type="checkbox"
            checked={autoRefresh}
            onChange={(e) => setAutoRefresh(e.target.checked)}
          />
          <RefreshCw size={11} /> Auto-refresh 30s
        </label>
      </div>
      <div className={styles.body}>
        <div className={styles.detectorList}>
          {/* Sort: red first, then yellow, then green; alphabetical within. */}
          {[...snap.reports]
            .sort((a, b) => sortKey(a) - sortKey(b) || a.name.localeCompare(b.name))
            .map(r => (
              <DetectorCard
                key={r.name}
                report={r}
                expanded={!!expanded[r.name]}
                onToggle={() => setExpanded(s => ({ ...s, [r.name]: !s[r.name] }))}
              />
            ))}
        </div>
      </div>
    </>
  )
}

function sortKey(r: DetectorReport): number {
  return r.level === 'red' ? 0 : r.level === 'yellow' ? 1 : 2
}

function DetectorCard({ report, expanded, onToggle }: {
  report: DetectorReport; expanded: boolean; onToggle: () => void
}) {
  const dotClass =
    report.level === 'red'    ? styles.dotRed    :
    report.level === 'yellow' ? styles.dotYellow :
                                styles.dotGreen
  const an = report.analytics
  const badges: React.ReactNode[] = []
  if (an?.forecast_red_in_hours != null) {
    badges.push(<span key="f" className={styles.severityBadge} style={{ background: 'rgba(239,68,68,0.12)', color: '#dc2626' }} title="Linear-trend forecast">↗ red in ~{an.forecast_red_in_hours.toFixed(1)}h</span>)
  }
  if (an?.anomaly_z != null && Math.abs(an.anomaly_z) >= 2) {
    badges.push(<span key="a" className={styles.severityBadge} style={{ background: 'rgba(245,158,11,0.12)', color: '#d97706' }} title="Z-score vs last 7d">σ {an.anomaly_z.toFixed(1)}</span>)
  }
  if (an?.correlated_detectors && an.correlated_detectors.length > 0) {
    badges.push(<span key="c" className={styles.severityBadge} style={{ background: 'rgba(59,130,246,0.12)', color: '#2563eb' }} title={`Tripped within ±10min of: ${an.correlated_detectors.join(', ')}`}>↔ {an.correlated_detectors.length}</span>)
  }
  return (
    <div className={styles.detectorRow}>
      <span className={`${styles.dot} ${dotClass}`} />
      <span className={styles.detectorName}>{report.name}</span>
      <span className={styles.detectorMessage}>
        {report.message}
        {badges.length > 0 && <span style={{ marginLeft: 8, display: 'inline-flex', gap: 4 }}>{badges}</span>}
      </span>
      <button type="button" className={styles.payloadBtn} onClick={onToggle}>
        {expanded ? 'hide' : 'detail'}
      </button>
      {expanded && (
        <pre className={styles.payload}>
          {JSON.stringify(report.payload, null, 2)}
          {an && '\n\nAnalytics: ' + JSON.stringify(an, null, 2)}
        </pre>
      )}
    </div>
  )
}

// ── Custom detectors tab ────────────────────────────────────────────────────

function CustomDetectorsTab() {
  const qc = useQueryClient()
  const listQ = useQuery({
    queryKey: ['health', 'custom-detectors'],
    queryFn:  healthApi.listCustomDetectors,
  })
  const [editing, setEditing] = useState<Partial<CustomDetectorRow> | null>(null)
  const saveMut = useMutation({
    mutationFn: (row: any) => healthApi.upsertCustomDetector(row),
    onSuccess: () => {
      toast.success('Saved')
      qc.invalidateQueries({ queryKey: ['health', 'custom-detectors'] })
      setEditing(null)
    },
    onError: (e: any) => toast.error(`Save failed: ${e?.response?.data?.error ?? e.message}`),
  })
  const delMut = useMutation({
    mutationFn: (name: string) => healthApi.deleteCustomDetector(name),
    onSuccess: () => {
      toast.success('Deleted')
      qc.invalidateQueries({ queryKey: ['health', 'custom-detectors'] })
    },
    onError: (e: any) => toast.error(`Delete failed: ${e?.response?.data?.error ?? e.message}`),
  })
  const [testResult, setTestResult] = useState<string | null>(null)
  const testMut = useMutation({
    mutationFn: ({ target_db, sql }: { target_db: string; sql: string }) =>
      healthApi.testCustomDetector(target_db, sql),
    onSuccess: (r) => setTestResult(JSON.stringify(r, null, 2)),
    onError:   (e: any) => setTestResult(`error: ${e?.response?.data?.error ?? e.message}`),
  })
  if (listQ.isLoading) return <div className={styles.loading}>Loading…</div>
  const rows = listQ.data ?? []

  return (
    <div className={styles.body}>
      <p style={{ fontSize: 12, color: 'var(--text-muted)', marginBottom: 16 }}>
        Add admin-defined SQL detectors that run alongside the built-in ones.
        SQL must start with <code>SELECT</code> or <code>WITH</code> and return a single
        numeric value. <code>target_db</code> is the DB filename without
        the <code>.db</code> suffix (e.g. <code>automations</code>, <code>auth</code>, <code>history</code>, <code>health</code>).
      </p>
      {!editing && (
        <button type="button" className={styles.runBtn} onClick={() => setEditing({
          name: '', target_db: 'automations', sql: 'SELECT COUNT(*) FROM ',
          direction: 'above', enabled: true,
        })}>+ New custom detector</button>
      )}
      {editing && (
        <div style={{ border: '1px solid var(--border-subtle)', padding: 16, borderRadius: 6, marginBottom: 16 }}>
          <div style={{ display: 'grid', gridTemplateColumns: '120px 1fr', gap: 8, alignItems: 'center', fontSize: 12 }}>
            <label>Name</label>
            <input value={editing.name ?? ''} onChange={(e) => setEditing(s => ({ ...s, name: e.target.value }))} />
            <label>Description</label>
            <input value={editing.description ?? ''} onChange={(e) => setEditing(s => ({ ...s, description: e.target.value }))} />
            <label>Target DB</label>
            <input value={editing.target_db ?? 'automations'} onChange={(e) => setEditing(s => ({ ...s, target_db: e.target.value }))} />
            <label>Yellow ≥</label>
            <input type="number" step="any" value={editing.yellow_at ?? ''} onChange={(e) => setEditing(s => ({ ...s, yellow_at: e.target.value === '' ? null : Number(e.target.value) }))} />
            <label>Red ≥</label>
            <input type="number" step="any" value={editing.red_at ?? ''} onChange={(e) => setEditing(s => ({ ...s, red_at: e.target.value === '' ? null : Number(e.target.value) }))} />
            <label>Direction</label>
            <select value={editing.direction ?? 'above'} onChange={(e) => setEditing(s => ({ ...s, direction: e.target.value }))}>
              <option value="above">above (bigger = worse)</option>
              <option value="below">below (smaller = worse)</option>
            </select>
            <label>SQL</label>
            <textarea
              rows={4}
              style={{ fontFamily: 'var(--font-mono, monospace)', fontSize: 12 }}
              value={editing.sql ?? ''}
              onChange={(e) => setEditing(s => ({ ...s, sql: e.target.value }))}
            />
          </div>
          <div style={{ marginTop: 12, display: 'flex', gap: 8 }}>
            <button type="button" className={styles.runBtn}
                    onClick={() => testMut.mutate({ target_db: editing.target_db ?? '', sql: editing.sql ?? '' })}
                    disabled={testMut.isPending}>Test</button>
            <button type="button" className={styles.runBtn}
                    onClick={() => saveMut.mutate(editing)} disabled={saveMut.isPending}>Save</button>
            <button type="button" className={styles.runBtn}
                    onClick={() => { setEditing(null); setTestResult(null) }}>Cancel</button>
          </div>
          {testResult && <pre className={styles.payload} style={{ marginTop: 12 }}>{testResult}</pre>}
        </div>
      )}
      <table className={styles.table}>
        <thead>
          <tr><th>Name</th><th>DB</th><th>Yellow≥</th><th>Red≥</th><th>Enabled</th><th></th></tr>
        </thead>
        <tbody>
          {rows.map(r => (
            <tr key={r.name}>
              <td className="mono">{r.name}{r.description ? <div className="muted" style={{ fontSize: 11 }}>{r.description}</div> : null}</td>
              <td className="mono">{r.target_db}</td>
              <td className="muted">{r.yellow_at ?? '—'}</td>
              <td className="muted">{r.red_at ?? '—'}</td>
              <td>{r.enabled ? '✓' : '—'}</td>
              <td>
                <button type="button" className={styles.linkBtn} onClick={() => setEditing(r)}>Edit</button>
                {' · '}
                <button type="button" className={styles.linkBtn} onClick={() => {
                  if (confirm(`Delete custom detector ${r.name}?`)) delMut.mutate(r.name)
                }}>Delete</button>
              </td>
            </tr>
          ))}
          {rows.length === 0 && !editing && (
            <tr><td colSpan={6} className="muted" style={{ padding: 24, textAlign: 'center' }}>
              No custom detectors yet.
            </td></tr>
          )}
        </tbody>
      </table>
    </div>
  )
}

// ── Webhooks tab ────────────────────────────────────────────────────────────

function WebhooksTab() {
  const qc = useQueryClient()
  const listQ = useQuery({
    queryKey: ['health', 'webhooks'],
    queryFn:  healthApi.listWebhooks,
  })
  const [editing, setEditing] = useState<Partial<WebhookListRow & { secret?: string }> | null>(null)
  const saveMut = useMutation({
    mutationFn: (row: any) => healthApi.upsertWebhook(row),
    onSuccess: () => {
      toast.success('Saved')
      qc.invalidateQueries({ queryKey: ['health', 'webhooks'] })
      setEditing(null)
    },
    onError: (e: any) => toast.error(`Save failed: ${e?.response?.data?.error ?? e.message}`),
  })
  const delMut = useMutation({
    mutationFn: (id: string) => healthApi.deleteWebhook(id),
    onSuccess: () => {
      toast.success('Deleted')
      qc.invalidateQueries({ queryKey: ['health', 'webhooks'] })
    },
    onError: (e: any) => toast.error(`Delete failed: ${e?.response?.data?.error ?? e.message}`),
  })
  if (listQ.isLoading) return <div className={styles.loading}>Loading…</div>
  const rows = listQ.data ?? []

  return (
    <div className={styles.body}>
      <p style={{ fontSize: 12, color: 'var(--text-muted)', marginBottom: 16 }}>
        Outbound webhooks fire one POST per detector report whose level matches
        the configured filter (default: yellow + red). Body is JSON; HMAC-SHA256
        signed via <code>X-Mira-Signature</code> when a secret is set.
      </p>
      {!editing && (
        <button type="button" className={styles.runBtn} onClick={() => setEditing({
          url: 'https://', enabled: true, levels_csv: 'yellow,red',
        })}>+ New webhook</button>
      )}
      {editing && (
        <div style={{ border: '1px solid var(--border-subtle)', padding: 16, borderRadius: 6, marginBottom: 16 }}>
          <div style={{ display: 'grid', gridTemplateColumns: '120px 1fr', gap: 8, alignItems: 'center', fontSize: 12 }}>
            <label>URL</label>
            <input value={editing.url ?? ''} onChange={(e) => setEditing(s => ({ ...s, url: e.target.value }))} />
            <label>Secret (optional)</label>
            <input type="password" placeholder={editing.has_secret ? '(unchanged)' : ''} onChange={(e) => setEditing(s => ({ ...s, secret: e.target.value }))} />
            <label>Levels CSV</label>
            <input value={editing.levels_csv ?? 'yellow,red'} onChange={(e) => setEditing(s => ({ ...s, levels_csv: e.target.value }))} />
            <label>Description</label>
            <input value={editing.description ?? ''} onChange={(e) => setEditing(s => ({ ...s, description: e.target.value }))} />
            <label>Enabled</label>
            <input type="checkbox" checked={editing.enabled ?? true} onChange={(e) => setEditing(s => ({ ...s, enabled: e.target.checked }))} />
          </div>
          <div style={{ marginTop: 12, display: 'flex', gap: 8 }}>
            <button type="button" className={styles.runBtn}
                    onClick={() => saveMut.mutate(editing)} disabled={saveMut.isPending}>Save</button>
            <button type="button" className={styles.runBtn} onClick={() => setEditing(null)}>Cancel</button>
          </div>
        </div>
      )}
      <table className={styles.table}>
        <thead>
          <tr><th>URL</th><th>Levels</th><th>Last fire</th><th>Last status</th><th>Enabled</th><th></th></tr>
        </thead>
        <tbody>
          {rows.map(w => (
            <tr key={w.id}>
              <td className="mono">{w.url}{w.description ? <div className="muted" style={{ fontSize: 11 }}>{w.description}</div> : null}</td>
              <td className="muted">{w.levels_csv ?? 'yellow,red'}</td>
              <td className="muted">{w.last_fire_at ? formatAgo(w.last_fire_at) : '—'}</td>
              <td className="muted">{w.last_status ?? '—'} {w.last_error ? <span style={{ color: '#dc2626' }} title={w.last_error}>⚠</span> : null}</td>
              <td>{w.enabled ? '✓' : '—'}</td>
              <td>
                <button type="button" className={styles.linkBtn} onClick={() => setEditing(w)}>Edit</button>
                {' · '}
                <button type="button" className={styles.linkBtn} onClick={() => {
                  if (confirm(`Delete webhook ${w.url}?`)) delMut.mutate(w.id)
                }}>Delete</button>
              </td>
            </tr>
          ))}
          {rows.length === 0 && !editing && (
            <tr><td colSpan={6} className="muted" style={{ padding: 24, textAlign: 'center' }}>
              No webhooks configured.
            </td></tr>
          )}
        </tbody>
      </table>
    </div>
  )
}

// ── Incidents tab ───────────────────────────────────────────────────────────

function IncidentsTab() {
  const navigate = useNavigate()
  const incidentsQ = useQuery({
    queryKey: ['health', 'incidents'],
    queryFn:  () => healthApi.incidents(50),
  })

  if (incidentsQ.isLoading) return <div className={styles.loading}>Loading incidents…</div>
  if (incidentsQ.error)     return <div className={styles.empty}>Failed to load incidents.</div>
  const rows = incidentsQ.data ?? []
  if (rows.length === 0) return (
    <div className={styles.empty}>
      <strong>No system-health incidents yet.</strong>
      Incidents appear here when the hourly audit detects a non-green signal.
    </div>
  )
  return (
    <div className={styles.body}>
      <table className={styles.table}>
        <thead>
          <tr>
            <th>When</th>
            <th>Severity</th>
            <th>Detector</th>
            <th>Message</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {rows.map(inc => (
            <tr key={inc.id}>
              <td className="muted">{formatAbsolute(inc.created_at)}</td>
              <td><SeverityBadge sev={inc.severity} /></td>
              <td className="mono">{inc.module.replace(/^health\//, '')}</td>
              <td>{inc.message.split('\n')[0]}</td>
              <td>
                <button
                  type="button"
                  className={styles.linkBtn}
                  onClick={() => navigate(`/incidents/${inc.id}`)}
                >
                  Open →
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

function SeverityBadge({ sev }: { sev: string }) {
  const cls = sev === 'ERROR' ? styles.sevError : sev === 'WARN' ? styles.sevWarn : styles.sevInfo
  return <span className={`${styles.severityBadge} ${cls}`}>{sev}</span>
}

// ── Config tab ──────────────────────────────────────────────────────────────

function ConfigTab() {
  const qc = useQueryClient()
  const configQ = useQuery({
    queryKey: ['health', 'config'],
    queryFn:  healthApi.config,
  })
  const saveMut = useMutation({
    mutationFn: ({ name, policy, snooze_secs }: { name: string; policy: ActionPolicy; snooze_secs?: number }) =>
      healthApi.setPolicy(name, policy, undefined, snooze_secs),
    onSuccess: () => {
      toast.success('Policy updated')
      qc.invalidateQueries({ queryKey: ['health', 'config'] })
    },
    onError: (e: any) => toast.error(`Save failed: ${e?.response?.data?.error ?? e.message}`),
  })

  if (configQ.isLoading) return <div className={styles.loading}>Loading config…</div>
  if (configQ.error)     return <div className={styles.empty}>Failed to load config.</div>
  const rows = configQ.data ?? []

  return (
    <div className={styles.body}>
      <p style={{ fontSize: 12, color: 'var(--text-muted)', marginBottom: 16 }}>
        Each detector has a policy: <strong>Notify</strong> (default — file an
        incident on yellow/red), <strong>Auto-cleanup</strong> (run the
        repair action first, then file with the result), or <strong>Off</strong>
        (skip entirely; the detector still runs but its result is never
        escalated). Use <strong>Snooze</strong> to mute temporarily — the
        detector reverts to its declared policy when the window expires.
      </p>
      <table className={styles.table}>
        <thead>
          <tr>
            <th>Detector</th>
            <th style={{ width: 250 }}>Policy</th>
            <th style={{ width: 130 }}>Snooze</th>
            <th>Last changed</th>
          </tr>
        </thead>
        <tbody>
          {rows.map(row => (
            <ConfigRow
              key={row.detector_name}
              row={row}
              onChange={(p, snooze) => saveMut.mutate({ name: row.detector_name, policy: p, snooze_secs: snooze })}
              busy={saveMut.isPending}
            />
          ))}
        </tbody>
      </table>
    </div>
  )
}

function ConfigRow({ row, onChange, busy }: {
  row: DetectorConfigEntry; onChange: (p: ActionPolicy, snooze_secs?: number) => void; busy: boolean
}) {
  const snoozed = row.snooze_until && row.snooze_until * 1000 > Date.now()
  return (
    <tr>
      <td className="mono">
        {row.detector_name}
        {row.overridden && <span className={styles.overriddenDot} title="Overridden from default" />}
      </td>
      <td>
        <div className={styles.policySegment}>
          {(['disabled', 'notify_only', 'auto_cleanup'] as const).map(p => (
            <button
              key={p}
              type="button"
              className={`${styles.policyOption} ${row.policy === p ? styles.policyActive : ''}`}
              onClick={() => row.policy !== p && onChange(p, undefined)}
              disabled={busy}
            >
              {p === 'disabled' ? 'Off' : p === 'notify_only' ? 'Notify' : 'Auto-cleanup'}
            </button>
          ))}
        </div>
      </td>
      <td>
        {snoozed ? (
          <button
            type="button"
            className={styles.linkBtn}
            onClick={() => onChange(row.policy, 0)}
            disabled={busy}
            title={`Snoozed until ${formatAbsolute(row.snooze_until!)}`}
          >
            Snoozed ({formatRelative(row.snooze_until!)}) · clear
          </button>
        ) : (
          <select
            value=""
            onChange={(e) => {
              const secs = parseInt(e.target.value, 10)
              if (!isNaN(secs) && secs > 0) onChange(row.policy, secs)
            }}
            disabled={busy}
            style={{ fontSize: 12, padding: '4px 6px', borderRadius: 3 }}
          >
            <option value="">Snooze…</option>
            <option value={3600}>1 hour</option>
            <option value={4 * 3600}>4 hours</option>
            <option value={24 * 3600}>1 day</option>
            <option value={7 * 24 * 3600}>1 week</option>
          </select>
        )}
      </td>
      <td className="muted">
        {row.updated_at ? formatAgo(row.updated_at) : '—'}
      </td>
    </tr>
  )
}

// ── IP bans tab ─────────────────────────────────────────────────────────────

function BansTab() {
  const qc = useQueryClient()
  const bansQ = useQuery({
    queryKey: ['health', 'ip-bans'],
    queryFn:  healthApi.ipBans,
  })
  const liftMut = useMutation({
    mutationFn: (ip: string) => healthApi.liftIpBan(ip),
    onSuccess: () => {
      toast.success('Ban lifted')
      qc.invalidateQueries({ queryKey: ['health', 'ip-bans'] })
    },
    onError: (e: any) => toast.error(`Lift failed: ${e?.response?.data?.error ?? e.message}`),
  })

  if (bansQ.isLoading) return <div className={styles.loading}>Loading bans…</div>
  if (bansQ.error)     return <div className={styles.empty}>Failed to load IP bans.</div>
  const rows = bansQ.data ?? []
  if (rows.length === 0) return (
    <div className={styles.empty}>
      <strong>No active IP bans.</strong>
      Bans are issued by the <code>auth.failed_logins_per_ip_1h</code>
      detector when its policy is set to Auto-cleanup.
    </div>
  )
  return (
    <div className={styles.body}>
      <table className={styles.table}>
        <thead>
          <tr>
            <th>IP</th>
            <th>Banned until</th>
            <th>Reason</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {rows.map(b => (
            <tr key={b.ip}>
              <td className="mono">{b.ip}</td>
              <td className="muted">{formatAbsolute(b.banned_until)} ({formatRelative(b.banned_until)})</td>
              <td className="muted">{b.reason ?? '—'}</td>
              <td>
                <button
                  type="button"
                  className={styles.linkBtn}
                  onClick={() => liftMut.mutate(b.ip)}
                  disabled={liftMut.isPending}
                >
                  Lift
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

// ── Artifacts tab (0.111.0) ─────────────────────────────────────────────────

function ArtifactsTab() {
  const qc = useQueryClient()
  const [openTask, setOpenTask] = useState<{ id: string; label: string } | null>(null)
  const listQ = useQuery({
    queryKey: ['health', 'artifacts'],
    queryFn:  healthApi.listArtifacts,
  })
  const delMut = useMutation({
    mutationFn: (name: string) => healthApi.deleteArtifact(name),
    onSuccess: () => {
      toast.success('Deleted')
      qc.invalidateQueries({ queryKey: ['health', 'artifacts'] })
    },
    onError: (e: any) => toast.error(`Delete failed: ${e?.response?.data?.error ?? e.message}`),
  })
  const migrateMut = useMutation({
    mutationFn: () => healthApi.migrateArtifacts(),
    onSuccess: (r) => {
      toast.success(`Migrated ${r.moved} dir(s) into ~/mira-artifacts/migrated/`)
      qc.invalidateQueries({ queryKey: ['health', 'artifacts'] })
    },
    onError: (e: any) => toast.error(`Migrate failed: ${e?.response?.data?.error ?? e.message}`),
  })
  if (listQ.isLoading) return <div className={styles.loading}>Loading…</div>
  const rows = listQ.data ?? []

  return (
    <div className={styles.body}>
      <p style={{ fontSize: 12, color: 'var(--text-muted)', marginBottom: 16 }}>
        Subagent task outputs land under <code>~/mira-artifacts/&lt;skill&gt;/&lt;slug&gt;_&lt;task_id&gt;/</code>.
        Each task gets <code>output/</code> for deliverables, <code>logs/</code> for debug,
        and a <code>MANIFEST.json</code> for metadata.{' '}
        <button type="button" className={styles.linkBtn} onClick={() => migrateMut.mutate()} disabled={migrateMut.isPending}>
          {migrateMut.isPending ? 'Scanning…' : 'Tidy: scan $HOME for legacy agent dirs and move them under migrated/'}
        </button>
      </p>
      <table className={styles.table}>
        <thead>
          <tr>
            <th>Skill</th>
            <th>Slug / Name</th>
            <th>Status</th>
            <th>Brief</th>
            <th>Created</th>
            <th>Size</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {rows.map(r => (
            <tr key={r.name}>
              <td className="mono">{r.skill}</td>
              <td className="mono" title={r.absolute_path}>{r.manifest.slug ?? r.name}</td>
              <td>
                <span className={`${styles.severityBadge} ${
                  r.manifest.status === 'completed' ? styles.sevInfo :
                  r.manifest.status === 'failed'    ? styles.sevError :
                                                     styles.sevWarn
                }`}>{r.manifest.status}</span>
              </td>
              <td className="muted" style={{ maxWidth: 360, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={r.manifest.brief_excerpt}>
                {r.manifest.brief_excerpt.slice(0, 80)}{r.manifest.brief_excerpt.length > 80 ? '…' : ''}
              </td>
              <td className="muted">{formatAgo(r.manifest.created_at)}</td>
              <td className="muted">{(r.size_bytes / 1024).toFixed(1)} KB</td>
              <td>
                <button type="button" className={styles.linkBtn}
                        onClick={() => setOpenTask({ id: r.manifest.task_id, label: r.manifest.slug ?? r.name })}>
                  Open
                </button>
                {' · '}
                <button type="button" className={styles.linkBtn} onClick={() => {
                  if (confirm(
                    `Permanently delete this task's entire artifact directory?\n\n` +
                    `${r.absolute_path}\n\n` +
                    `This removes its output/, logs/, and MANIFEST.json. Cannot be undone.`
                  )) {
                    delMut.mutate(r.name)
                  }
                }}>Delete</button>
              </td>
            </tr>
          ))}
          {rows.length === 0 && (
            <tr><td colSpan={7} className="muted" style={{ padding: 24, textAlign: 'center' }}>
              No task artifacts yet. Spawn a background task and the deliverable will appear here.
            </td></tr>
          )}
        </tbody>
      </table>
      {openTask && (
        <TaskFileBrowser taskId={openTask.id} label={openTask.label} onClose={() => setOpenTask(null)} />
      )}
    </div>
  )
}

const IMG_EXT = ['png', 'jpg', 'jpeg', 'gif', 'svg', 'webp']
const TEXT_EXT = ['txt', 'md', 'markdown', 'json', 'jsonl', 'csv', 'tsv', 'log', 'yaml', 'yml',
  'toml', 'html', 'xml', 'js', 'ts', 'py', 'rs', 'sh', 'css', 'sql', 'env']
function extOf(p: string): string { return p.split('.').pop()?.toLowerCase() ?? '' }

/** A4 — browse + preview/download the files inside one task's artifact dir. */
function TaskFileBrowser({ taskId, label, onClose }: { taskId: string; label: string; onClose: () => void }) {
  const filesQ = useQuery({
    queryKey: ['task-files', taskId],
    queryFn:  () => healthApi.listTaskFiles(taskId),
  })
  const [sel, setSel] = useState<string | null>(null)
  const [text, setText] = useState<{ path: string; body: string } | null>(null)
  const files = filesQ.data ?? []
  const selExt = sel ? extOf(sel) : ''
  const isImg = IMG_EXT.includes(selExt)
  const isText = TEXT_EXT.includes(selExt) || selExt === ''

  useEffect(() => {
    if (!sel || isImg || !isText) { setText(null); return }
    let cancelled = false
    fetch(taskFileUrl(taskId, sel))
      .then(r => r.text())
      .then(b => { if (!cancelled) setText({ path: sel, body: b.slice(0, 200_000) }) })
      .catch(() => { if (!cancelled) setText({ path: sel, body: '(could not load file)' }) })
    return () => { cancelled = true }
  }, [sel, taskId, isImg, isText])

  // Close on Escape (the panel is a fixed overlay portaled to <body>).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  return createPortal(
    <div className={styles.fileBrowserOverlay} onClick={onClose}>
    <div className={styles.fileBrowser} onClick={(e) => e.stopPropagation()}>
      <div className={styles.fileBrowserHead}>
        <strong>Files — {label}</strong>
        <button type="button" className={styles.linkBtn} onClick={onClose}>Close</button>
      </div>
      <div className={styles.fileBrowserBody}>
        <ul className={styles.fileList}>
          {filesQ.isLoading && <li className="muted">Loading…</li>}
          {!filesQ.isLoading && files.length === 0 && <li className="muted">No files.</li>}
          {files.map(f => (
            <li key={f.path} className={sel === f.path ? styles.fileSel : ''}>
              <button type="button" className={styles.fileName} onClick={() => setSel(f.path)} title={f.path}>
                {f.path}
              </button>
              <span className="muted">{(f.size_bytes / 1024).toFixed(1)} KB</span>
              <a className={styles.linkBtn} href={taskFileUrl(taskId, f.path, true)}>↓</a>
            </li>
          ))}
        </ul>
        <div className={styles.filePreview}>
          {!sel && <div className="muted">Select a file to preview.</div>}
          {sel && isImg && <img src={taskFileUrl(taskId, sel)} alt={sel} className={styles.previewImg} />}
          {sel && !isImg && text && <pre className={styles.previewText}>{text.body}</pre>}
          {sel && !isImg && !isText && (
            <div className="muted">Binary file — <a href={taskFileUrl(taskId, sel, true)}>download</a> to view.</div>
          )}
        </div>
      </div>
    </div>
    </div>,
    document.body,
  )
}

// ── Tiny formatters ────────────────────────────────────────────────────────

function formatAgo(ts_secs: number): string {
  const diff = Date.now() / 1000 - ts_secs
  if (diff < 60)    return `${Math.round(diff)}s ago`
  if (diff < 3600)  return `${Math.round(diff / 60)}m ago`
  if (diff < 86400) return `${Math.round(diff / 3600)}h ago`
  return `${Math.round(diff / 86400)}d ago`
}

function formatRelative(ts_secs: number): string {
  const diff = ts_secs - Date.now() / 1000
  if (diff < 0) return 'expired'
  if (diff < 60)    return `in ${Math.round(diff)}s`
  if (diff < 3600)  return `in ${Math.round(diff / 60)}m`
  if (diff < 86400) return `in ${Math.round(diff / 3600)}h`
  return `in ${Math.round(diff / 86400)}d`
}

function formatAbsolute(ts_secs: number): string {
  return new Date(ts_secs * 1000).toLocaleString()
}
