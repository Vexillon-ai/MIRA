// SPDX-License-Identifier: AGPL-3.0-or-later

import { useQuery } from '@tanstack/react-query'
import { RefreshCw, Server, Database, Clock, Activity, Brain, MessageSquare, Wifi } from 'lucide-react'
import { providersApi } from '@/api/providers'
import styles from './StatusPage.module.css'

function formatUptime(secs: number): string {
  const d = Math.floor(secs / 86400)
  const h = Math.floor((secs % 86400) / 3600)
  const m = Math.floor((secs % 3600) / 60)
  if (d > 0) return `${d}d ${h}h ${m}m`
  if (h > 0) return `${h}h ${m}m`
  return `${m}m ${secs % 60}s`
}

export default function StatusPage() {
  const { data: status, isLoading, refetch, dataUpdatedAt } = useQuery({
    queryKey: ['status'],
    queryFn:  providersApi.status,
    refetchInterval: 15_000,
  })

  const { data: health = [] } = useQuery({
    queryKey: ['providers/health'],
    queryFn:  providersApi.health,
    refetchInterval: 30_000,
  })

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1>Status</h1>
          <p>System health overview</p>
        </div>
        <button className={styles.refreshBtn} onClick={() => refetch()}>
          <RefreshCw size={14} />
          Refresh
        </button>
      </div>

      {isLoading ? (
        <p className={styles.loading}>Loading…</p>
      ) : status ? (
        <div className={styles.body}>
          {/* System info */}
          <div className={styles.grid}>
            <StatCard icon={<Server size={20} />} label="Version" value={`v${status.version}`} accent />
            <StatCard icon={<Clock size={20} />} label="Uptime" value={formatUptime(status.uptime_secs)} />
            <StatCard icon={<Activity size={20} />} label="Provider" value={status.provider_name ?? '—'} />
            <StatCard icon={<Wifi size={20} />} label="Active Sessions" value={status.active_sessions != null ? String(status.active_sessions) : '—'} />
          </div>

          {/* Database stats */}
          <h2 className={styles.section}>Database</h2>
          <div className={styles.grid}>
            <StatCard
              icon={<Brain size={20} />}
              label="Memories"
              value={status.memory_count != null ? String(status.memory_count) : '—'}
              sublabel="stored facts"
            />
            <StatCard
              icon={<MessageSquare size={20} />}
              label="Conversations"
              value={status.conversation_count != null ? String(status.conversation_count) : '—'}
            />
            <StatCard
              icon={<Database size={20} />}
              label="Messages"
              value={status.message_count != null ? String(status.message_count) : '—'}
            />
          </div>

          {/* Provider health */}
          {health.length > 0 && (
            <>
              <h2 className={styles.section}>Providers</h2>
              <div className={styles.healthList}>
                {health.map((p) => (
                  <div key={p.name} className={styles.healthRow}>
                    <span className={`${styles.healthDot} ${p.healthy ? styles.dotOk : styles.dotBad}`} />
                    <span className={styles.healthName}>{p.name}</span>
                    <span className={styles.healthModel}>{p.model}</span>
                    {p.latency_ms != null && (
                      <span className={styles.healthLatency}>{p.latency_ms}ms</span>
                    )}
                    <span className={`${styles.healthStatus} ${p.healthy ? styles.statusOk : styles.statusBad}`}>
                      {p.healthy ? 'healthy' : 'unreachable'}
                    </span>
                  </div>
                ))}
              </div>
            </>
          )}

          {dataUpdatedAt > 0 && (
            <p className={styles.lastUpdated}>
              Last updated {new Date(dataUpdatedAt).toLocaleTimeString()}
            </p>
          )}
        </div>
      ) : (
        <p className={styles.loading}>Could not load status.</p>
      )}
    </div>
  )
}

function StatCard({ icon, label, value, sublabel, accent }: {
  icon: React.ReactNode
  label: string
  value: string
  sublabel?: string
  accent?: boolean
}) {
  return (
    <div className={`${styles.card} ${accent ? styles.cardAccent : ''}`}>
      <div className={styles.cardIcon}>{icon}</div>
      <div className={styles.cardBody}>
        <div className={styles.cardValue}>{value}</div>
        <div className={styles.cardLabel}>{label}</div>
        {sublabel && <div className={styles.cardSub}>{sublabel}</div>}
      </div>
    </div>
  )
}
