// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState, useMemo } from 'react'
import { useQuery, useQueryClient, useMutation } from '@tanstack/react-query'
import {
  RefreshCw, Zap, AlertCircle, CheckCircle, Server, Plus, Check, X, Search,
} from 'lucide-react'
import {
  providersApi, type ProviderHealth, type ModelInfo,
} from '@/api/providers'
import { catalogApi, type ModelCatalog, type ModelEntry } from '@/api/catalog'
import { api } from '@/api/client'
import { useChatStore } from '@/store/chatStore'
import styles from './ProvidersPage.module.css'

// Order providers consistently: primary first, then locals, then
// cloud, then catch-all. Keeps the rollup ordering stable across
// renders so the user's scroll position isn't surprised by health
// data arriving in a different order than the page redraws.
const PROVIDER_ORDER = [
  'lmstudio', 'ollama',
  'openrouter', 'anthropic', 'gemini', 'openai',
  'deepseek', 'moonshot', 'groq', 'xai',
  // openai_compat catch-all carries its custom name as the slug, so
  // it always sorts last via the fallback below.
]

function orderProviders(slugs: string[], primary: string): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  for (const s of [primary, ...PROVIDER_ORDER]) {
    if (slugs.includes(s) && !seen.has(s)) {
      out.push(s); seen.add(s)
    }
  }
  for (const s of slugs.sort()) {
    if (!seen.has(s)) { out.push(s); seen.add(s) }
  }
  return out
}

// ─────────────────────────────────────────────────────────────────────────────
// Config-mutation helper
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Minimal shape we need from `/api/config` to drive this page. The
 * full Config type lives in SettingsPage; we only touch
 * `providers.<slug>.{available_models,default_model,enabled,api_key,name}`
 * and `primary_provider`.
 */
interface ProvidersConfigShape {
  primary_provider?: string
  providers?: Record<string, ProviderEntry>
}
interface ProviderEntry {
  enabled?:          boolean
  api_key?:          string | null
  default_model?:    string
  available_models?: string[]
  // The openai_compat catch-all stores its display slug under `name`.
  name?:             string
  // Local providers don't have api_key but do have url; we treat the
  // url's presence as "configured" the same way.
  url?:              string
}

function useConfigMutation() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: async (patch: (cfg: ProvidersConfigShape) => void) => {
      // Read the current full config — we MUST NOT discard fields
      // outside `providers.*` because PUT /api/config replaces the
      // whole document.
      const { data: cfg } = await api.get<ProvidersConfigShape>('/api/config')
      patch(cfg)
      await api.put('/api/config', cfg)
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['config'] })
      qc.invalidateQueries({ queryKey: ['models'] })
      qc.invalidateQueries({ queryKey: ['providers/health'] })
    },
  })
}

// ─────────────────────────────────────────────────────────────────────────────
// Page
// ─────────────────────────────────────────────────────────────────────────────

export default function ProvidersPage() {
  const qc = useQueryClient()
  const selectedModel    = useChatStore((s) => s.selectedModel)
  const setSelectedModel = useChatStore((s) => s.setSelectedModel)
  const activeConvId     = useChatStore((s) => s.activeConversationId)

  const { data: health = [], isLoading: healthLoading, refetch: refetchHealth } = useQuery<ProviderHealth[]>({
    queryKey: ['providers/health'],
    queryFn:  providersApi.health,
    refetchInterval: 30_000,
  })
  const { data: models = [] } = useQuery<ModelInfo[]>({
    queryKey: ['models'],
    queryFn:  providersApi.models,
    staleTime: 60_000,
  })
  const { data: status } = useQuery({
    queryKey: ['status'],
    queryFn:  providersApi.status,
    refetchInterval: 15_000,
  })
  const { data: cfg } = useQuery<ProvidersConfigShape>({
    queryKey: ['config'],
    queryFn:  () => api.get<ProvidersConfigShape>('/api/config').then(r => r.data),
  })

  const refresh = () => {
    void refetchHealth()
    qc.invalidateQueries({ queryKey: ['models'] })
    qc.invalidateQueries({ queryKey: ['status'] })
    qc.invalidateQueries({ queryKey: ['config'] })
  }

  // Pick the slugs to render. A provider is "active" (worth showing)
  // when build_provider_chain would register it: enabled AND
  // (key-present | local url present | openai_compat with name).
  // Mirror the same predicate the backend uses.
  const activeSlugs = useMemo(() => {
    if (!cfg?.providers) return []
    const out: string[] = []
    for (const [slug, p] of Object.entries(cfg.providers)) {
      if (p.enabled === false) continue
      const hasKey = Boolean(p.api_key && p.api_key.length > 0)
      const hasUrl = Boolean(p.url && p.url.length > 0)
      // The catch-all `openai_compat` block uses `name` as its
      // user-facing slug; only show the rollup when the user has
      // configured one.
      const isCatchall = slug === 'openai_compat'
      if (isCatchall) {
        if (!p.name || p.name.length === 0) continue
        if (!hasKey) continue
        // Render under the user-chosen name, not under "openai_compat".
        out.push(p.name)
        continue
      }
      if (!hasKey && !hasUrl) continue
      out.push(slug)
    }
    return orderProviders(out, cfg.primary_provider || '')
  }, [cfg])

  // The catch-all is configured under `providers.openai_compat` but
  // displayed under whatever `name` the user picked. Reverse-lookup
  // map so handlers know which underlying config block to mutate.
  const catchallName = cfg?.providers?.openai_compat?.name || ''
  const configSlug = (displaySlug: string): string =>
    displaySlug === catchallName && catchallName ? 'openai_compat' : displaySlug

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1>Providers</h1>
          <p>Active model list and per-provider catalogs</p>
        </div>
        <button className={styles.refreshBtn} onClick={refresh} title="Refresh">
          <RefreshCw size={14} />
          Refresh
        </button>
      </div>

      {status && (
        <div className={styles.statusStrip}>
          <div className={styles.statusItem}>
            <Server size={13} />
            <span>v{status.version}</span>
          </div>
          <div className={styles.statusItem}>
            <span className={styles.dot} />
            <span>Uptime {formatUptime(status.uptime_secs)}</span>
          </div>
          <div className={styles.statusItem}>
            <span>{status.memory_count} memories</span>
          </div>
          <div className={styles.statusItem}>
            <span>{status.conversation_count} conversations</span>
          </div>
          {activeConvId && (
            <div className={styles.statusItem}>
              <span className={styles.active}>● active session</span>
            </div>
          )}
        </div>
      )}

      <div className={styles.section}>
        <h2 className={styles.sectionTitle}>Provider Health</h2>
        {healthLoading ? (
          <p className={styles.loading}>Checking providers…</p>
        ) : (
          <div className={styles.healthGrid}>
            {health.length === 0 && <p className={styles.empty}>No providers configured</p>}
            {health.map((p) => (
              <div key={p.name} className={`${styles.healthCard} ${p.healthy ? styles.healthOk : styles.healthBad}`}>
                <div className={styles.healthCardTop}>
                  {p.healthy
                    ? <CheckCircle size={18} className={styles.iconOk} />
                    : <AlertCircle size={18} className={styles.iconBad} />}
                  <span className={styles.healthName}>{p.name}</span>
                  {p.latency_ms != null && (
                    <span className={styles.latency}>
                      <Zap size={11} /> {p.latency_ms}ms
                    </span>
                  )}
                </div>
                <div className={styles.healthMeta}>
                  <span className={styles.modelLabel}>{p.model}</span>
                  {p.url && <span className={styles.url}>{p.url}</span>}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      <div className={styles.section}>
        <h2 className={styles.sectionTitle}>Available Models</h2>
        <p className={styles.sectionLead}>
          Models added to each provider. Click one to set it as the
          provider's <strong>active</strong> model — the choice used
          when no per-session model override is picked. The × removes
          a model from the list; remove all and the provider drops
          out of the chat dropdown.
        </p>
        {activeSlugs.length === 0 ? (
          <p className={styles.empty}>No providers enabled. Set up a provider in Settings → Providers first.</p>
        ) : (
          activeSlugs.map((displaySlug) => (
            <AvailableRollup
              key={`avail-${displaySlug}`}
              displaySlug={displaySlug}
              configSlug={configSlug(displaySlug)}
              cfg={cfg}
              models={models}
              selectedModel={selectedModel}
              setSelectedModel={setSelectedModel}
            />
          ))
        )}
      </div>

      <div className={styles.section}>
        <h2 className={styles.sectionTitle}>Model Catalogs</h2>
        <p className={styles.sectionLead}>
          Per-provider catalogs as returned by the upstream API.
          Click <strong>Add</strong> to make a model available in
          this MIRA instance — it'll appear in the section above and
          in the chat-page dropdown. <em>Restart required</em> after
          changes so the new selection becomes routable.
        </p>
        {activeSlugs.length === 0 ? (
          <p className={styles.empty}>Configure a provider first to see its catalog.</p>
        ) : (
          activeSlugs.map((displaySlug) => (
            <CatalogRollup
              key={`cat-${displaySlug}`}
              displaySlug={displaySlug}
              configSlug={configSlug(displaySlug)}
              cfg={cfg}
            />
          ))
        )}
      </div>

      {selectedModel && (
        <div className={styles.selectionNote}>
          <CheckCircle size={13} />
          <span>
            <strong>{selectedModel.id}</strong> will be used for new
            chat messages (session only — not persisted).
          </span>
        </div>
      )}
    </div>
  )
}

// ─────────────────────────────────────────────────────────────────────────────
// Available rollup (per provider)
// ─────────────────────────────────────────────────────────────────────────────

function AvailableRollup({
  displaySlug, configSlug, cfg, models, selectedModel, setSelectedModel,
}: {
  displaySlug:   string
  configSlug:    string
  cfg:           ProvidersConfigShape | undefined
  models:        ModelInfo[]
  selectedModel: { id: string; provider: string } | null
  setSelectedModel: (m: { id: string; provider: string } | null) => void
}) {
  const mutate = useConfigMutation()
  const block = cfg?.providers?.[configSlug]

  // Models the chat dropdown will actually surface for this
  // provider — read directly from /api/providers/models so the
  // fallback-when-empty logic stays in one place (server side).
  const provModels = useMemo(
    () => models.filter((m) => m.provider === displaySlug),
    [models, displaySlug],
  )
  const active = block?.default_model ?? ''
  const availableList = block?.available_models ?? []
  // "Locked" = only one model possible. Either the available list
  // says so, or the available list is empty and there's exactly one
  // catalog entry (see CatalogRollup for the auto-add path that
  // promotes that to available_models).
  const locked = provModels.length === 1 && availableList.length <= 1

  const setActive = (id: string) => {
    mutate.mutate((c) => {
      const b = ensureBlock(c, configSlug)
      b.default_model = id
      // Adding it to available if not already (defensive — keeps
      // active in sync with what the dropdown shows).
      const cur = b.available_models ?? []
      if (!cur.includes(id)) b.available_models = [...cur, id]
    })
  }
  const remove = (id: string) => {
    mutate.mutate((c) => {
      const b = ensureBlock(c, configSlug)
      b.available_models = (b.available_models ?? []).filter((x) => x !== id)
      if (b.default_model === id) {
        b.default_model = b.available_models[0] ?? ''
      }
    })
  }

  return (
    <details className={styles.providerRollup} open>
      <summary className={styles.providerRollupHeader}>
        <span className={styles.providerRollupName}>{displaySlug}</span>
        <span className={styles.providerRollupCount}>
          {provModels.length} model{provModels.length === 1 ? '' : 's'}
          {locked && <span className={styles.lockedBadge} title="Only catalog entry — locked">locked</span>}
        </span>
      </summary>
      <div className={styles.providerRollupBody}>
        {provModels.length === 0 ? (
          <p className={styles.providerRollupEmpty}>
            No models added. Use the catalog below to add one.
          </p>
        ) : (
          provModels.map((m) => {
            const isActive    = m.id === active
            const isSelected  = selectedModel?.id === m.id && selectedModel?.provider === m.provider
            return (
              <div key={`${m.provider}:${m.id}`} className={styles.availRow}>
                <button
                  type="button"
                  className={`${styles.availPick} ${isActive ? styles.availPickActive : ''}`}
                  onClick={() => {
                    if (!isActive) setActive(m.id)
                  }}
                  title={isActive ? 'Currently the active model' : 'Set as active model'}
                >
                  <span className={styles.availRadio}>{isActive ? '●' : '○'}</span>
                  <span className={styles.availId}>{m.id}</span>
                  {isActive && <span className={styles.activeBadge}>ACTIVE</span>}
                </button>
                <button
                  type="button"
                  className={styles.availSession}
                  onClick={() =>
                    setSelectedModel(isSelected ? null : { id: m.id, provider: m.provider })
                  }
                  title={isSelected
                    ? 'Clear the per-session override'
                    : 'Use this model for the current chat session (does not change the active model on disk)'}
                >
                  {isSelected ? 'Session ✓' : 'Use this session'}
                </button>
                {!locked && (
                  <button
                    type="button"
                    className={styles.availRemove}
                    onClick={() => remove(m.id)}
                    title="Remove from this provider's available list"
                    disabled={mutate.isPending}
                  >
                    <X size={12} />
                  </button>
                )}
              </div>
            )
          })
        )}
      </div>
    </details>
  )
}

// ─────────────────────────────────────────────────────────────────────────────
// Catalog rollup (per provider)
// ─────────────────────────────────────────────────────────────────────────────

function CatalogRollup({
  displaySlug, configSlug, cfg,
}: {
  displaySlug: string
  configSlug:  string
  cfg:         ProvidersConfigShape | undefined
}) {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const qc = useQueryClient()
  const mutate = useConfigMutation()
  const block = cfg?.providers?.[configSlug]
  const availableList = block?.available_models ?? []
  const defaultModel  = block?.default_model ?? ''

  // The /api/providers/<slug>/catalog endpoint is keyed on the
  // CONFIG slug (openai_compat for the catch-all), not the display
  // slug. Pass that down.
  const { data: catalog, isLoading, error } = useQuery<ModelCatalog>({
    queryKey: ['provider-catalog', configSlug],
    queryFn:  () => catalogApi.fetch(configSlug),
    enabled:  open, // lazy fetch — only when the rollup is opened
    staleTime: 24 * 60 * 60 * 1000,
    retry:     false,
  })

  const refresh = useMutation({
    mutationFn: () => catalogApi.fetch(configSlug, true),
    onSuccess:  (data) => qc.setQueryData(['provider-catalog', configSlug], data),
  })

  const filtered = useMemo(() => {
    if (!catalog) return []
    const q = query.trim().toLowerCase()
    if (!q) return catalog.entries
    return catalog.entries.filter((e) =>
      e.id.toLowerCase().includes(q)
      || (e.display_name?.toLowerCase().includes(q) ?? false)
    )
  }, [catalog, query])

  // Single-model auto-add: when the catalog returns exactly one
  // entry AND it's not yet in available_models, silently push it in.
  // This matches the design — "if a provider has only one model
  // then by default it is added in the available models list".
  // Fires at most once per session per provider (the catalog query
  // result is cached for 24h).
  useMemo(() => {
    if (!catalog || catalog.entries.length !== 1) return
    const onlyId = catalog.entries[0].id
    if (availableList.includes(onlyId)) return
    mutate.mutate((c) => {
      const b = ensureBlock(c, configSlug)
      const cur = b.available_models ?? []
      if (cur.includes(onlyId)) return
      b.available_models = [...cur, onlyId]
      if (!b.default_model) b.default_model = onlyId
    })
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [catalog?.entries.length])

  const addModel = (id: string) => {
    mutate.mutate((c) => {
      const b = ensureBlock(c, configSlug)
      const cur = b.available_models ?? []
      if (cur.includes(id)) return
      b.available_models = [...cur, id]
      // Auto-promote: first add becomes the active model.
      if (!b.default_model) b.default_model = id
    })
  }

  return (
    <details
      className={styles.providerRollup}
      open={open}
      onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}
    >
      <summary className={styles.providerRollupHeader}>
        <span className={styles.providerRollupName}>{displaySlug} catalog</span>
        <span className={styles.providerRollupCount}>
          {catalog ? `${catalog.entries.length} model${catalog.entries.length === 1 ? '' : 's'}` : '…'}
        </span>
        {open && (
          <button
            type="button"
            className={styles.refreshBtn}
            onClick={(e) => {
              e.preventDefault()
              refresh.mutate()
            }}
            disabled={refresh.isPending}
            title="Force re-fetch from the provider"
          >
            <RefreshCw size={11} className={refresh.isPending ? styles.spinning : ''} />
            {refresh.isPending ? '…' : 'Refresh'}
          </button>
        )}
      </summary>
      <div className={styles.providerRollupBody}>
        {isLoading && <p className={styles.providerRollupEmpty}>Fetching catalog…</p>}
        {error && (
          <p className={styles.providerRollupEmpty}>
            Catalog unavailable: {(error as Error).message}.
          </p>
        )}
        {catalog && catalog.entries.length === 0 && (
          <p className={styles.providerRollupEmpty}>
            Provider returned an empty model list.
          </p>
        )}
        {catalog && catalog.entries.length > 5 && (
          <div className={styles.catalogSearchBox}>
            <Search size={12} className={styles.catalogSearchIcon} />
            <input
              type="text"
              className={styles.catalogSearch}
              placeholder={`Filter ${catalog.entries.length} models…`}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            {query && (
              <button
                className={styles.catalogSearchClear}
                onClick={() => setQuery('')}
                title="Clear filter"
              >
                <X size={12} />
              </button>
            )}
          </div>
        )}
        {catalog && filtered.length > 0 && (
          <div className={styles.catalogTable}>
            <div className={`${styles.catalogRow} ${styles.catalogHead}`}>
              <span>Model</span>
              <span>Context</span>
              <span>In / 1M</span>
              <span>Out / 1M</span>
              <span>Notes</span>
              <span></span>
            </div>
            <div className={styles.catalogScroll}>
              {filtered.map((e) => (
                <CatalogModelRow
                  key={e.id}
                  entry={e}
                  added={availableList.includes(e.id)}
                  isActive={e.id === defaultModel}
                  locked={catalog.entries.length === 1}
                  onAdd={() => addModel(e.id)}
                  disabled={mutate.isPending}
                />
              ))}
            </div>
          </div>
        )}
        {catalog && filtered.length === 0 && query && (
          <p className={styles.providerRollupEmpty}>No models match "{query}".</p>
        )}
      </div>
    </details>
  )
}

function CatalogModelRow({
  entry, added, isActive, locked, onAdd, disabled,
}: {
  entry:    ModelEntry
  added:    boolean
  isActive: boolean
  locked:   boolean
  onAdd:    () => void
  disabled: boolean
}) {
  const fmtPrice = (v: number | undefined) =>
    v != null ? `$${v.toFixed(2)}` : '—'
  const fmtCtx = (v: number | undefined) => {
    if (!v) return '—'
    if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`
    if (v >= 1_000)     return `${Math.round(v / 1_000)}K`
    return String(v)
  }
  return (
    <div className={`${styles.catalogRow} ${added ? styles.catalogRowPinned : ''}`}>
      <span className={styles.catalogId} title={entry.display_name ?? entry.id}>
        {entry.id}
        {entry.display_name && <span className={styles.catalogDisplay}> — {entry.display_name}</span>}
      </span>
      <span>{fmtCtx(entry.context_window)}</span>
      <span>{fmtPrice(entry.input_price_per_1m)}</span>
      <span>{fmtPrice(entry.output_price_per_1m)}</span>
      <span className={styles.catalogNotes}>{entry.notes ?? ''}</span>
      <button
        className={`${styles.pinBtn} ${added ? styles.pinBtnActive : ''}`}
        onClick={onAdd}
        disabled={added || disabled || locked}
        title={
          locked ? 'Only catalog entry — auto-added' :
          added  ? (isActive ? 'Already added — currently the active model' : 'Already in available list')
                 : 'Add to this provider\'s available list'
        }
      >
        {added ? <Check size={12} /> : <Plus size={12} />}
        {added ? 'Added' : 'Add'}
      </button>
    </div>
  )
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Get-or-create a provider entry on a config object. Returns the
 * (mutable) entry so callers can patch in place.
 */
function ensureBlock(cfg: ProvidersConfigShape, slug: string): ProviderEntry {
  if (!cfg.providers) cfg.providers = {}
  if (!cfg.providers[slug]) cfg.providers[slug] = {}
  return cfg.providers[slug]
}

function formatUptime(secs: number): string {
  const h = Math.floor(secs / 3600)
  const m = Math.floor((secs % 3600) / 60)
  if (h > 0) return `${h}h ${m}m`
  return `${m}m`
}
