// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Library, Search, ChevronRight, ChevronLeft } from 'lucide-react'
import { memoryApi, type CatalogFact } from '../api/memory'
import styles from './MemoryCatalog.module.css'

// A short, human label for a trust level when the service didn't send one.
function trustLabel(trust?: string | null, label?: string | null): string {
  if (label && label.trim()) return label
  switch (trust) {
    case 'verified':   return 'verified'
    case 'reported':   return 'reported by the source'
    case 'unverified': return 'found in source, not verified'
    default:           return trust ?? 'unverified'
  }
}

function FactRow({ f }: { f: CatalogFact }) {
  const trust = f.trust ?? 'unknown'
  // An unverified item is a source passage, not an asserted value — show it as a quote.
  const quote = trust === 'unverified'
  return (
    <li className={styles.fact} data-trust={trust}>
      <div className={styles.factHead}>
        {f.relation && <span className={styles.relation}>{f.relation}</span>}
        <span className={styles.tierBadge} data-trust={trust}>
          {trustLabel(f.trust, f.trust_label)}
        </span>
      </div>
      <div className={quote ? styles.passage : styles.value}>{f.content}</div>
    </li>
  )
}

/**
 * Per-user browser for an external memory source's catalog (WHAT IS HELD).
 * Renders nothing unless a catalog-capable source (any memory-SPI HTTP service)
 * is configured for this user. Owner-scoped server-side — it only ever shows the
 * signed-in user's own holdings.
 */
export default function MemoryCatalog() {
  const [filter, setFilter]   = useState('')
  const [page, setPage]       = useState(1)
  const [subject, setSubject] = useState<string | null>(null)

  const term = filter.trim()
  const list = useQuery({
    queryKey: ['memory-catalog', term, page],
    // Always a `contains` filter — the prefix/contains toggle was a dev artefact.
    queryFn:  () => memoryApi.catalog({ page, ...(term ? { contains: term } : {}) }),
    placeholderData: (prev) => prev,
  })

  const detail = useQuery({
    queryKey: ['memory-catalog-subject', subject],
    queryFn:  () => memoryApi.catalog({ subject: subject! }),
    enabled:  !!subject,
  })

  // Hidden entirely when no source is configured for this user.
  if (list.isLoading && !list.data) return null
  if (!list.data?.available) return null

  const cat      = list.data.catalog
  const counts   = cat?.counts
  const byTrust  = counts?.by_trust ?? {}
  const pg       = cat?.page
  const subjects = cat?.subjects ?? []
  const nPages   = pg?.n_pages_subjects ?? 1
  // A configured-but-empty or errored source gets an explicit status line rather
  // than a bare "No subjects".
  const statusLine =
    list.data.error ? `Source error: ${list.data.error}`
    : list.data.state === 'unavailable' ? 'Source unavailable — try again shortly.'
    : (list.data.state === 'absent' || subjects.length === 0) && !term
      ? 'Connected — nothing held for you yet.'
      : null

  const d = detail.data?.catalog?.subject_detail

  return (
    <section className={styles.wrap}>
      <div className={styles.head}>
        <Library size={15} />
        <h2>{list.data.source ? `${list.data.source} catalog` : 'Source catalog'}</h2>
        <span className={styles.sub}>Read-only — what your external memory source holds about you</span>
      </div>

      {/* Counts: subjects, facts, and a breakdown by trust level. */}
      <div className={styles.counts}>
        {typeof counts?.subjects === 'number' && (
          <div className={styles.stat}>
            <span className={styles.statN}>{counts.subjects}</span>
            <span className={styles.statL}>subjects</span>
          </div>
        )}
        {typeof counts?.facts === 'number' && (
          <div className={styles.stat}>
            <span className={styles.statN}>{counts.facts}</span>
            <span className={styles.statL}>facts</span>
          </div>
        )}
        {Object.entries(byTrust).map(([trust, n]) => (
          <div key={trust} className={styles.stat} data-trust={trust}>
            <span className={styles.statN}>{String(n)}</span>
            <span className={styles.statL}>{trustLabel(trust)}</span>
          </div>
        ))}
      </div>

      {statusLine && <p className={styles.status}>{statusLine}</p>}

      {/* Subject filter. */}
      <div className={styles.toolbar}>
        <div className={styles.searchBox}>
          <Search size={14} className={styles.searchIcon} />
          <input
            className={styles.searchInput}
            placeholder="Filter subjects…"
            value={filter}
            onChange={(e) => { setFilter(e.target.value); setPage(1) }}
          />
        </div>
      </div>

      <div className={styles.body}>
        {/* Subject list. */}
        <ul className={styles.subjects}>
          {subjects.length === 0 && !statusLine && (
            <li className={styles.empty}>No subjects{term ? ' match that filter' : ''}.</li>
          )}
          {subjects.map((s) => (
            <li key={s.name}>
              <button
                className={`${styles.subjectBtn} ${subject === s.name ? styles.subjectActive : ''}`}
                onClick={() => setSubject(s.name)}
              >
                <span className={styles.subjectName}>{s.name}</span>
                {s.aliases && s.aliases.length > 0 && (
                  <span className={styles.aka}>{s.aliases[0]}</span>
                )}
                <ChevronRight size={14} className={styles.chevron} />
              </button>
            </li>
          ))}
        </ul>

        {/* Selected subject's held facts, with trust labels. */}
        {subject && (
          <div className={styles.detail}>
            <div className={styles.detailHead}>
              <strong>{subject}</strong>
              <button className={styles.close} onClick={() => setSubject(null)}>×</button>
            </div>
            {detail.isLoading && <p className={styles.muted}>Loading…</p>}
            {d && (d.facts?.length ?? 0) === 0 && (
              <p className={styles.muted}>Nothing held for this subject.</p>
            )}
            {d?.facts && d.facts.length > 0 && (
              <ul className={styles.facts}>
                {d.facts.map((f, i) => <FactRow key={i} f={f} />)}
              </ul>
            )}
            {d?.truncated ? (
              <p className={styles.muted}>Showing a capped subset — the source reported more.</p>
            ) : null}
          </div>
        )}
      </div>

      {/* Pagination over the subject list. */}
      {nPages > 1 && (
        <div className={styles.pager}>
          <button disabled={page <= 1} onClick={() => setPage((p) => Math.max(1, p - 1))}>
            <ChevronLeft size={14} />
          </button>
          <span>{page} / {nPages}</span>
          <button disabled={page >= nPages} onClick={() => setPage((p) => p + 1)}>
            <ChevronRight size={14} />
          </button>
        </div>
      )}
    </section>
  )
}
