// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { createPortal } from 'react-dom'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Save, Check, Palette, Cpu, Bot, Radio, Database, Server, Code2, Upload, Trash2, Wrench, RotateCcw, Loader2, Calendar as CalendarIcon, RefreshCw, Shield, ShieldAlert, Volume2, ChevronDown, Bell, Image as ImageIcon } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import UpdatesCard from '@/components/UpdatesCard'
import RemoteAccessCard from '@/components/RemoteAccessCard'
import FailoverChainEditor from '@/components/FailoverChainEditor'
import { providersApi, type StatusInfo } from '@/api/providers'
import { ttsApi } from '@/api/tts'
import { playBlobWithGain, type PlayHandle } from '@/api/ttsPlayback'
import { calendarApi } from '@/api/calendar'
import { wslApi } from '@/api/wsl'
import { memoryApi, type ConsolidatorRunResult } from '@/api/memory'
import { useThemeStore, THEMES } from '@/store/themeStore'
import { useUiStore } from '@/store/uiStore'
import { useAuthStore } from '@/store/authStore'
import { useRestartServer } from '@/hooks/useRestartServer'
import AgentAvatar, { type AgentAppearance } from '@/components/AgentAvatar'
import NotificationSettings from '@/components/NotificationSettings'
import BackupRestore from '@/components/BackupRestore'
import DailyBriefingSettings from '@/components/DailyBriefingSettings'
import CompanionCheckinTest from '@/components/CompanionCheckinTest'
import WaitlistPanel from '@/components/WaitlistPanel'
import { AVATAR_PRESETS } from '@/components/Avatar'
import VoiceIdPicker from '@/components/VoiceIdPicker'
import ModelSelect from '@/components/ModelSelect'
import { catalogApi } from '@/api/catalog'
import type { ChannelDescriptor } from '@/api/types'
import styles from './SettingsPage.module.css'

// ── Config shape (mirrors Rust structs) ───────────────────────────────────────
interface Config {
  primary_provider?: string
  data_dir?: string
  providers?: {
    ollama?:      { url?: string; default_model?: string; timeout_secs?: number }
    lmstudio?:    { url?: string; default_model?: string; timeout_secs?: number }
    openrouter?:  { api_key?: string; base_url?: string; default_model?: string }
  }
  agent?: {
    tool_mode?: string
    max_tool_rounds?: number
    max_tool_round_tokens?: number
    max_response_tokens?: number
    max_context_turns?: number
    context_length_tokens?: number
    context_safety_margin_tokens?: number
    prompt_cache_enabled?: boolean
    compaction?: {
      enabled?: boolean
      keep_last_turns?: number
      summary_model?: string
      max_summary_tokens?: number
    }
    system_prompt_file?: string
    playful_easter_eggs?: boolean
    tools?: {
      shell?:       { enabled?: boolean }
      filesystem?:  { enabled?: boolean }
      web_fetch?:   {
        enabled?:        boolean
        max_body_bytes?: number
        max_text_chars?: number
        timeout_secs?:   number
        max_redirects?:  number
      }
      url_preview?: {
        enabled?:        boolean
        max_body_bytes?: number
      }
      web_search?: {
        enabled?:  boolean
        default?:  string
        failover?: string[]
        top_k?:    number
        brave?:    { api_key?: string }
        searxng?:  { url?: string }
      }
    }
  }
  channels?: {
    signal?: {
      enabled?: boolean; phone_number?: string; rest_port?: number; hmac_key?: string
      socket_path?: string; cli_binary?: string; data_dir?: string
    }
    telegram?: {
      enabled?: boolean; bot_token?: string; webhook_url?: string; polling?: boolean
    }
  }
  memory?: {
    vector_backend?: string
    similarity_threshold?: number
    per_user_isolation?: boolean
    share_across_channels?: boolean
    qdrant_url?: string
    embedding_dim?: number
    embedding_cache_size?: number
    embedding?: {
      provider?: string; model?: string; provider_url?: string; api_key?: string; model_cache_dir?: string
    }
    indexer?: {
      enabled?:       boolean
      interval_secs?: number
      batch_size?:    number
      skip_roles?:    string[]
    }
    auto_extract?: {
      mode?:               'off' | 'heuristic' | 'llm'
      min_confidence?:     'low' | 'medium' | 'high'
      allowed_categories?: string[]
      llm_channels?:       string[]
    }
    recency?: {
      weight?:         number
      half_life_days?: number
    }
    rollup?: {
      enabled?:               boolean
      interval_secs?:         number
      day_lag_days?:          number
      max_messages?:          number
      max_chars_per_message?: number
    }
  }
  server?: {
    enabled?: boolean; host?: string; port?: number
    max_connections?: number; request_timeout_secs?: number
    allowed_origins?: string[]
    auth_token?: string; webhook_secret?: string
    tls_cert_path?: string | null; tls_key_path?: string | null
    remote_url?: string | null
    update_check?: { enabled?: boolean; source_url?: string; frequency?: string }
    web_apps?: {
      enabled?: boolean
      mode?: string
      host_suffix?: string
      port?: number
      advertised_host?: string | null
    }
  }
  security?: {
    rate_limit_rpm?:      number
    cors_allowed_origins?: string[]
    blocked_ips?:         string[]
    jwt_secret?:          string
    session_days?:        number
    http?: {
      denylist?:          string[]
      allowlist?:         string[]
      allowlist_only?:    boolean
      searxng_exception?: string | null
      rate?: {
        user_per_min?:            number
        user_per_hour?:           number
        user_per_domain_per_min?: number
        search_per_min?:          number
      }
    }
  }
  proxy?: {
    enabled?:           boolean
    nginx_binary?:      string
    config_path?:       string
    pid_path?:          string
    worker_processes?:  string
    websocket_support?: boolean
    tls?: {
      enabled?:    boolean
      cert_path?:  string
      key_path?:   string
      listen_port?: number
    }
  }
  session?: {
    cleanup_interval_secs?: number
    timeout_secs?:          number
    max_turns?:             number
  }
  logging?: {
    level?: string; format?: string; file?: string
    max_file_size_mb?: number; max_files?: number
  }
  tui?: {
    theme?: string; layout?: string; show_timestamps?: boolean; show_token_count?: boolean
  }
  calendar?: {
    enabled?: boolean
    sync_provider?: 'none' | 'caldav' | 'google' | 'outlook'
    sync_interval_mins?: number
    caldav?:  { url?: string; username?: string; password?: string }
    google?:  { client_id?: string; client_secret?: string; redirect_uri?: string; scopes?: string }
    outlook?: { client_id?: string; client_secret?: string; redirect_uri?: string; scopes?: string }
  }
  sandbox?: {
    enabled?:      boolean
    seccomp_mode?: 'denylist' | 'allowlist'
    backend?:      '' | 'auto' | 'namespace' | 'wasm' | 'pyodide'
    code_run?: {
      enabled?:                boolean
      allowed_languages?:      string[]
      max_wall_clock_seconds?: number
      max_memory_mb?:          number
    }
    python?:  { rootfs_path?: string }
    wasm?:    { python_path?: string }
    pyodide?: { enabled?: boolean; prewarm?: string[] }
  }
  image?: {
    default_backend?: '' | 'auto' | 'openai' | 'automatic1111' | 'comfyui'
    automatic1111?: {
      enabled?: boolean; base_url?: string; model?: string; steps?: number
      sampler?: string; width?: number; height?: number; cfg_scale?: number
      negative_prompt?: string
    }
    comfyui?: {
      enabled?: boolean; base_url?: string; workflow_json?: string; model?: string
      steps?: number; width?: number; height?: number; cfg_scale?: number
      negative_prompt?: string
    }
  }
  video?: {
    default_backend?: '' | 'auto' | 'openai' | 'comfyui' | 'wan2gp'
    openai?: { default_model?: string; default_size?: string; default_seconds?: number }
    comfyui?: {
      enabled?: boolean; base_url?: string; workflow_json?: string; model?: string
      steps?: number; width?: number; height?: number; fps?: number; cfg_scale?: number
      negative_prompt?: string
    }
    wan2gp?: { enabled?: boolean; base_url?: string; api_name?: string }
  }
  tts?: {
    enabled?:               boolean
    default_backend?:       string
    default_voice?:         string
    default_speed?:         number
    default_format?:        string
    streaming?:             boolean
    max_chars_per_request?: number
    request_timeout_secs?:  number
    cache?: { enabled?: boolean; max_disk_mb?: number; ttl_days?: number }
    internal?: {
      engine?: string; default_voice?: string
      auto_download_voices?: boolean
      voices_dir?: string; binary_path?: string
    }
    openai?: {
      api_key?: string | null; base_url?: string
      model?: string; default_voice?: string
    }
    openai_compat?: {
      url?: string; api_key?: string | null
      model?: string; default_voice?: string
    }
    elevenlabs?: { api_key?: string | null; model?: string; default_voice_id?: string }
    cartesia?:   { api_key?: string | null; model?: string; default_voice_id?: string }
    routing?: { web?: string; tui?: string; telegram?: string; signal?: string; mobile?: string }
  }
  // MCP servers moved to the dedicated `/mcp` page in 0.157.0; the
  // top-level `mcp` config block (if present) is kept as a one-shot
  // seed for the per-user store, never edited from here.
  [k: string]: unknown
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/** Remove "***" sentinel values so the backend treats absent fields as
 *  "keep the current live value" rather than overwriting with a placeholder. */
function stripSentinels(obj: unknown): unknown {
  if (typeof obj !== 'object' || obj === null) return obj
  if (Array.isArray(obj)) return obj.map(stripSentinels)
  const result: Record<string, unknown> = {}
  for (const [k, v] of Object.entries(obj as Record<string, unknown>)) {
    if (v === '***') continue
    result[k] = stripSentinels(v)
  }
  return result
}

/** After sentinel-stripping, any provider block still carrying a non-empty
 *  `api_key` is one the user *just typed* (an unchanged key was the "***"
 *  sentinel and got stripped). Treat that as intent to use the provider and
 *  flip its `enabled` on, so a freshly-pasted key works without a separate
 *  toggle step. Local providers (ollama/lmstudio) have no key and are unaffected
 *  — and an already-keyed provider the user deliberately disabled stays disabled
 *  (its key is the stripped sentinel, not a real value). Mutates in place. */
function autoEnableKeyedProviders(body: unknown): void {
  if (typeof body !== 'object' || body === null) return
  const providers = (body as { providers?: unknown }).providers
  if (typeof providers !== 'object' || providers === null) return
  for (const blk of Object.values(providers as Record<string, unknown>)) {
    if (typeof blk !== 'object' || blk === null) continue
    const b = blk as { api_key?: unknown; enabled?: unknown }
    if (typeof b.api_key === 'string' && b.api_key.length > 0) {
      b.enabled = true
    }
  }
}

function getPath(obj: Config, path: string): unknown {
  return path.split('.').reduce<unknown>((cur, k) => {
    if (cur && typeof cur === 'object') return (cur as Record<string, unknown>)[k]
    return undefined
  }, obj)
}

function setPath(obj: Config, path: string, value: unknown): Config {
  const parts = path.split('.')
  const clone = { ...obj }
  let cur: Record<string, unknown> = clone as Record<string, unknown>
  for (let i = 0; i < parts.length; i++) {
    const key = parts[i]
    // Per-key prototype-pollution guard, checked immediately before each write.
    // A dotted path like `__proto__.x` would otherwise walk into
    // Object.prototype; no legitimate config key uses these. (Kept per-key
    // inside the loop, not as an upfront array check, so CodeQL's
    // js/prototype-pollution-utility query recognises the barrier.)
    if (key === '__proto__' || key === 'prototype' || key === 'constructor') {
      return obj
    }
    if (i === parts.length - 1) {
      cur[key] = value
    } else {
      const existing = cur[key]
      cur[key] = typeof existing === 'object' && existing !== null ? { ...existing } : {}
      cur = cur[key] as Record<string, unknown>
    }
  }
  return clone
}

// ── Tabs ──────────────────────────────────────────────────────────────────────

type TabId = 'appearance' | 'providers' | 'agent' | 'tools' | 'sandbox' | 'channels' | 'memory' | 'calendar' | 'voice' | 'image' | 'notifications' | 'guardian' | 'server' | 'advanced'

const TABS: { id: TabId; label: string; icon: ReactNode }[] = [
  { id: 'appearance', label: 'Appearance', icon: <Palette size={14} /> },
  { id: 'providers',  label: 'Providers',  icon: <Cpu size={14} />     },
  { id: 'agent',      label: 'Agent',      icon: <Bot size={14} />     },
  { id: 'tools',      label: 'Tools',      icon: <Wrench size={14} />  },
  { id: 'sandbox',    label: 'Sandbox',    icon: <Shield size={14} />  },
  { id: 'channels',   label: 'Channels',   icon: <Radio size={14} />   },
  { id: 'memory',     label: 'Memory',     icon: <Database size={14} />},
  { id: 'calendar',   label: 'Calendar',   icon: <CalendarIcon size={14} /> },
  { id: 'voice',      label: 'Voice',      icon: <Volume2 size={14} /> },
  { id: 'image',      label: 'Image & Video', icon: <ImageIcon size={14} /> },
  { id: 'notifications', label: 'Notifications', icon: <Bell size={14} /> },
  { id: 'guardian',   label: 'Guardian',   icon: <ShieldAlert size={14} /> },
  { id: 'server',     label: 'Server & Security', icon: <Server size={14} />  },
  { id: 'advanced',   label: 'Advanced',   icon: <Code2 size={14} />   },
]

// ── Field primitives ──────────────────────────────────────────────────────────

function Field({
  label, desc, children,
}: { label: string; desc: string; children: ReactNode }) {
  return (
    <div className={styles.field}>
      <div className={styles.fieldMeta}>
        <span className={styles.fieldLabel}>{label}</span>
        <span className={styles.fieldDesc}>{desc}</span>
      </div>
      <div className={styles.fieldControl}>{children}</div>
    </div>
  )
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className={styles.section}>
      <h3 className={styles.sectionTitle}>{title}</h3>
      <div className={styles.sectionBody}>{children}</div>
    </div>
  )
}

/** Collapsible variant of Section. Uses native <details> so it survives
 *  page reloads-from-scroll and needs no JS state. */
function CollapsibleSection({
  title, children, defaultOpen = false,
}: { title: string; children: ReactNode; defaultOpen?: boolean }) {
  return (
    <details className={styles.section} open={defaultOpen}>
      <summary
        className={styles.sectionTitle}
        style={{ cursor: 'pointer', listStyle: 'revert', userSelect: 'none' }}
      >
        {title}
      </summary>
      <div className={styles.sectionBody}>{children}</div>
    </details>
  )
}

/**
 * Provider section header — title + enable toggle + Test button.
 * Wraps a regular Section but threads the `providers.<slug>.enabled`
 * flag through the toggle and renders an inline status badge after a
 * Test click. The Test button fires `/api/providers/<slug>/catalog?refresh=true`
 * which both verifies the provider is reachable AND refreshes the
 * cached model list — so the dropdown a few rows down picks up new
 * models without a separate "reload" click.
 */
function isHttpUrl(s: string): boolean {
  if (!s) return false
  try {
    const u = new URL(s.trim())
    return (u.protocol === 'http:' || u.protocol === 'https:') && Boolean(u.hostname)
  } catch { return false }
}

function ProviderSection({
  title, slug, set, str, children, urlPath, urlDefault,
}: {
  title:   string
  slug:    string
  set:     (path: string, value: unknown) => void
  str:     (path: string, fallback?: string) => string
  children: ReactNode
  /** Config path of the URL field that gates Test (e.g. `providers.ollama.url`).
   *  Test stays disabled until this holds a valid http(s) URL. */
  urlPath?:    string
  /** The URL field's default, so validation matches what the field displays. */
  urlDefault?: string
}) {
  const enabledPath = `providers.${slug}.enabled`
  // Settings JSON stores booleans; absent is treated as `true` to
  // match the Rust serde default (backwards-compatible for older
  // configs that predate this field).
  const enabledRaw = str(enabledPath, 'true')
  const enabled    = enabledRaw === 'false' ? false : true

  // Test only makes sense against a reachable endpoint — gate it on a valid URL
  // in the provider's URL field (the same value the field shows).
  const urlVal   = urlPath ? str(urlPath, urlDefault ?? '') : ''
  const urlValid = !urlPath || isHttpUrl(urlVal)

  type TestStatus =
    | { kind: 'loading' }
    | { kind: 'ok'; count: number; latencyMs: number }
    | { kind: 'error'; message: string }

  const [status, setStatus] = useState<TestStatus | null>(null)
  const qc = useQueryClient()

  const runTest = async () => {
    setStatus({ kind: 'loading' })
    const t0 = performance.now()
    try {
      const cat = await catalogApi.fetch(slug, true)
      // Catalog listing can succeed while the *configured model* is
      // deprecated/quota'd/unauthorized — so also do a real 1-token generation
      // and surface that error (e.g. Gemini "model no longer available").
      const gen = await providersApi.test(slug)
      const ms  = Math.round(performance.now() - t0)
      if (!gen.ok) {
        setStatus({ kind: 'error', message: gen.error || `model ${gen.model} failed to generate` })
      } else {
        setStatus({ kind: 'ok', count: cat.entries.length, latencyMs: ms })
      }
      // ModelSelect listens to this query key — invalidating makes
      // the dropdown re-render with the freshly-fetched catalog.
      qc.invalidateQueries({ queryKey: ['provider-catalog', slug] })
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e)
      // Axios errors stash the server's message under response.data;
      // pull it out so the badge isn't "Request failed with status code 502".
      const axMsg = (e as { response?: { data?: string | { error?: string } } })
        ?.response?.data
      const human = typeof axMsg === 'string'
        ? axMsg
        : typeof axMsg === 'object' && axMsg?.error
          ? axMsg.error
          : msg
      setStatus({ kind: 'error', message: human })
    }
  }

  return (
    <div className={styles.section}>
      <div style={{
        display: 'flex', alignItems: 'center', gap: 12,
        marginBottom: 'var(--space-2)',
      }}>
        <Toggle value={enabled} onChange={(v) => set(enabledPath, v)} />
        <h3 className={styles.sectionTitle} style={{ margin: 0, flexShrink: 0, whiteSpace: 'nowrap' }}>{title}</h3>
        {/* Test + result, right-justified within the card. Shown only when the
            provider is enabled (nothing to test otherwise), and clickable only
            once the URL field holds a valid http(s) URL. */}
        {enabled && (
          <div style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 10, minWidth: 0 }}>
            {status?.kind === 'ok' && (
              <span style={{ color: 'var(--success, #4ade80)', fontSize: 12, whiteSpace: 'nowrap' }}>
                ✓ {status.count} model{status.count === 1 ? '' : 's'} ({status.latencyMs}ms)
              </span>
            )}
            {status?.kind === 'error' && (
              <span
                style={{ color: 'var(--danger, #f87171)', fontSize: 12, maxWidth: 320, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}
                title={status.message}
              >
                ✗ {status.message.length > 80 ? `${status.message.slice(0, 80)}…` : status.message}
              </span>
            )}
            <button
              type="button"
              className={styles.button}
              onClick={runTest}
              disabled={status?.kind === 'loading' || !urlValid}
              title={urlValid
                ? "Verify connectivity and refresh this provider's model catalog."
                : 'Enter a valid URL above before testing.'}
            >
              {status?.kind === 'loading' ? 'Testing…' : 'Test'}
            </button>
          </div>
        )}
      </div>
      <div className={styles.sectionBody} style={{ opacity: enabled ? 1 : 0.55 }}>
        {children}
      </div>
    </div>
  )
}

function TextInput({
  value, onChange, placeholder = '', type = 'text', mono = false, suggestions, disabled = false,
}: {
  value: string; onChange: (v: string) => void
  placeholder?: string; type?: string; mono?: boolean
  /** When provided, renders a `<datalist>` so the input doubles as an
   *  autocomplete combobox while still allowing free entry. */
  suggestions?: string[]
  /** When true, the field renders greyed-out and read-only — used for
   *  knobs that don't apply to the current provider (e.g. the
   *  embedding API key when provider=internal). */
  disabled?: boolean
}) {
  const isSecret = type === 'password'
  const isRedacted = isSecret && value === '***'

  if (isRedacted) {
    return (
      <div style={{ display: 'flex', gap: '6px', alignItems: 'center' }}>
        <input
          className={`${styles.input} ${mono ? styles.inputMono : ''}`}
          type="password"
          value=""
          placeholder="*****"
          style={{ flex: 1, opacity: 0.7 }}
          onChange={(e) => onChange(e.target.value)}
          disabled={disabled}
        />
      </div>
    )
  }

  // Stable per-mount datalist id so multiple inputs on the page don't collide.
  const listId = useId()

  return (
    <>
      <input
        className={`${styles.input} ${mono ? styles.inputMono : ''}`}
        type={type}
        value={value}
        placeholder={placeholder}
        list={suggestions && suggestions.length > 0 ? listId : undefined}
        onChange={(e) => onChange(e.target.value)}
        disabled={disabled}
        style={disabled ? { opacity: 0.55, cursor: 'not-allowed' } : undefined}
      />
      {suggestions && suggestions.length > 0 && (
        <datalist id={listId}>
          {suggestions.map((s) => <option key={s} value={s} />)}
        </datalist>
      )}
    </>
  )
}

// Free-entry input paired with a click-to-open dropdown of suggestions.
// Used where TextInput's native `<datalist>` falls short — once the input
// value exactly matches a suggestion, browsers either hide the datalist or
// filter it down to that one entry, so the user can't browse alternatives
// without first deleting the field. This combobox shows the *full*
// suggestion list whenever the chevron is clicked, regardless of value.
function ComboInput({
  value, onChange, placeholder = '', mono = false, suggestions,
}: {
  value: string; onChange: (v: string) => void
  placeholder?: string; mono?: boolean
  suggestions: string[]
}) {
  const [open, setOpen] = useState(false)
  const wrapRef = useRef<HTMLDivElement>(null)
  const popRef  = useRef<HTMLUListElement>(null)
  // Coordinates of the popup relative to the viewport. Recomputed on open
  // and on scroll/resize so the list tracks the input even mid-scroll.
  // The popup is portaled to document.body so it escapes the section's
  // `overflow: hidden`, otherwise the longer voice lists get clipped.
  const [popRect, setPopRect] = useState<{ top: number; left: number; width: number } | null>(null)

  const reposition = () => {
    const r = wrapRef.current?.getBoundingClientRect()
    if (!r) return
    setPopRect({ top: r.bottom + 2, left: r.left, width: r.width })
  }

  useLayoutEffect(() => {
    if (!open) return
    reposition()
    window.addEventListener('scroll', reposition, true)
    window.addEventListener('resize', reposition)
    return () => {
      window.removeEventListener('scroll', reposition, true)
      window.removeEventListener('resize', reposition)
    }
  }, [open])

  useEffect(() => {
    if (!open) return
    const onDocClick = (e: MouseEvent) => {
      const t = e.target as Node
      if (wrapRef.current?.contains(t)) return
      if (popRef.current?.contains(t))  return
      setOpen(false)
    }
    document.addEventListener('mousedown', onDocClick)
    return () => document.removeEventListener('mousedown', onDocClick)
  }, [open])

  return (
    <div ref={wrapRef} style={{ position: 'relative', display: 'flex', alignItems: 'stretch' }}>
      <input
        className={`${styles.input} ${mono ? styles.inputMono : ''}`}
        type="text"
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        onFocus={() => { if (suggestions.length > 0) setOpen(true) }}
        onKeyDown={(e) => {
          if (e.key === 'Escape') setOpen(false)
          else if (e.key === 'ArrowDown' && !open && suggestions.length > 0) {
            e.preventDefault(); setOpen(true)
          }
        }}
        style={{ flex: 1, paddingRight: 28 }}
      />
      <button
        type="button"
        aria-label="Show suggestions"
        onMouseDown={(e) => { e.preventDefault(); setOpen((v) => !v) }}
        disabled={suggestions.length === 0}
        style={{
          position: 'absolute', right: 4, top: 0, bottom: 0,
          display: 'flex', alignItems: 'center', justifyContent: 'center',
          width: 22, background: 'transparent', border: 'none',
          color: 'var(--text-muted)',
          cursor: suggestions.length === 0 ? 'default' : 'pointer',
          opacity: suggestions.length === 0 ? 0.4 : 1,
        }}
      >
        <ChevronDown size={14} />
      </button>
      {open && suggestions.length > 0 && popRect && createPortal(
        <ul
          ref={popRef}
          role="listbox"
          style={{
            position: 'fixed',
            top: popRect.top, left: popRect.left, width: popRect.width,
            zIndex: 1000,
            margin: 0, padding: '4px 0', maxHeight: 240, overflowY: 'auto',
            background: 'var(--bg-overlay)', border: '1px solid var(--border)',
            borderRadius: 'var(--radius-sm)', listStyle: 'none',
            boxShadow: '0 4px 12px rgba(0,0,0,0.25)',
          }}
        >
          {suggestions.map((s) => (
            <li
              key={s}
              role="option"
              aria-selected={s === value}
              onMouseDown={(e) => { e.preventDefault(); onChange(s); setOpen(false) }}
              style={{
                padding: '6px 10px', cursor: 'pointer',
                fontFamily: mono ? 'var(--font-mono)' : undefined,
                fontSize: 12,
                background: s === value ? 'var(--bg-secondary, transparent)' : 'transparent',
                color: 'var(--text-primary)',
              }}
              onMouseEnter={(e) => { (e.currentTarget as HTMLLIElement).style.background = 'var(--bg-hover, var(--bg-secondary, rgba(255,255,255,0.05)))' }}
              onMouseLeave={(e) => { (e.currentTarget as HTMLLIElement).style.background = s === value ? 'var(--bg-secondary, transparent)' : 'transparent' }}
            >
              {s}
            </li>
          ))}
        </ul>,
        document.body,
      )}
    </div>
  )
}

function NumberInput({ value, onChange, min, max, step = 1 }: {
  value: number; onChange: (v: number) => void; min?: number; max?: number; step?: number
}) {
  return (
    <input
      className={styles.input}
      type="number"
      value={value}
      min={min}
      max={max}
      step={step}
      onChange={(e) => onChange(Number(e.target.value))}
    />
  )
}

function SelectInput({ value, onChange, options }: {
  value: string
  onChange: (v: string) => void
  options: { value: string; label: string }[]
}) {
  return (
    <select className={styles.select} value={value} onChange={(e) => onChange(e.target.value)}>
      {options.map((o) => <option key={o.value} value={o.value}>{o.label}</option>)}
    </select>
  )
}

function Toggle({ value, onChange, label }: { value: boolean; onChange: (v: boolean) => void; label?: string }) {
  return (
    <label className={styles.toggle}>
      <input type="checkbox" checked={value} onChange={(e) => onChange(e.target.checked)} />
      <span className={styles.toggleTrack}>
        <span className={styles.toggleThumb} />
      </span>
      {label && <span className={styles.toggleLabel}>{label}</span>}
    </label>
  )
}

// Slider for a 0–200 % gain. Shown next to a percentage readout so the
// numeric value is legible at a glance — sliders alone are imprecise. Used by
// per-backend TTS volume controls; HTMLAudioElement caps `volume` at 1.0, so
// values >1.0 are applied client-side via Web Audio gain (see ttsPlayback.ts).
function VolumeSlider({ value, onChange }: { value: number; onChange: (v: number) => void }) {
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 220 }}>
      <input
        type="range"
        min={0}
        max={2}
        step={0.05}
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        style={{ flex: 1 }}
      />
      <span style={{
        minWidth: 48, textAlign: 'right',
        fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--text-secondary)',
      }}>
        {Math.round(value * 100)}%
      </span>
    </div>
  )
}

// ── Main page ─────────────────────────────────────────────────────────────────

export default function SettingsPage() {
  const initialTab = (() => {
    if (typeof window === 'undefined') return 'appearance'
    const p = new URLSearchParams(window.location.search).get('tab')
    const allowed: TabId[] = ['appearance','providers','agent','tools','sandbox','channels','memory','calendar','voice','image','notifications','guardian','server','advanced']
    return (allowed as string[]).includes(p ?? '') ? (p as TabId) : 'appearance'
  })()
  const [tab, setTab] = useState<TabId>(initialTab)
  const [draft, setDraft] = useState<Config>({})
  const [rawJson, setRawJson] = useState('')
  const [rawError, setRawError] = useState('')
  const [saved, setSaved] = useState(false)
  const qc = useQueryClient()
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const { data: status } = useQuery<StatusInfo>({
    queryKey: ['status'],
    queryFn:  providersApi.status,
    staleTime: 30_000,
  })
  const supervised = status?.supervised ?? false
  // Sticky banner state — provider-chain changes need a service
  // restart to take effect (build_provider_chain runs once at
  // startup). Set after saveMut detects a providers/primary_provider
  // diff; cleared when restartMut succeeds.
  const [providerRestartRequired, setProviderRestartRequired] = useState(false)
  const restartMut = useRestartServer({
    supervised,
    onSuccess: () => setProviderRestartRequired(false),
  })

  const { data: config, isLoading } = useQuery<Config>({
    queryKey: ['config'],
    queryFn: () => api.get('/api/config').then((r) => r.data),
  })

  useEffect(() => {
    if (config) {
      setDraft(config)
      setRawJson(JSON.stringify(config, null, 2))
    }
  }, [config])

  // The last config payload we tried to save. Held so the install
  // dialog can re-fire the save after the dep finishes downloading,
  // without forcing the user back to the form to click Save again.
  const lastSaveBodyRef = useRef<Config | null>(null)
  // Modal state: when the server returns 422 missing_dep on save, we
  // open a "Download and install now?" prompt. null = closed.
  const [missingDep, setMissingDep] = useState<{
    dep: string
    message: string
    installEndpoint: string
  } | null>(null)

  const saveMut = useMutation({
    mutationFn: (body: Config) => {
      lastSaveBodyRef.current = body
      return api.put('/api/config', body).then((r) => r.data)
    },
    onSuccess: (_data, body) => {
      setSaved(true)
      setTimeout(() => setSaved(false), 2500)
      // The TtsService reloads on PUT /api/config, so the server's
      // routing map and per-backend voice lists may have changed.
      // Invalidate the cached `useTtsVoices` result so any picker
      // that scopes by `channel` (e.g. the ProfileDialog) sees the
      // new routing without waiting out the 5-minute staleTime.
      qc.invalidateQueries({ queryKey: ['tts', 'voices', 'all-backends'] })
      // Detect provider-chain changes that need a restart. Compare
      // the persisted body's providers/primary against the most
      // recent server-snapshot from useQuery(['config']). Stringify-
      // and-compare is good enough — the diff isn't huge and we
      // only run it on save.
      const b = body as Config & { providers?: unknown; primary_provider?: unknown }
      const prevP = JSON.stringify((config as Config & { providers?: unknown })?.providers ?? null)
      const newP  = JSON.stringify(b.providers ?? null)
      const prevPrimary = (config as Config & { primary_provider?: unknown })?.primary_provider ?? null
      const newPrimary  = b.primary_provider ?? null
      if (prevP !== newP || prevPrimary !== newPrimary) {
        setProviderRestartRequired(true)
      }
    },
    onError: (err: unknown) => {
      const e = err as { response?: { status?: number; data?: { error?: string; dep?: string; message?: string; install_endpoint?: string } } }
      const data = e?.response?.data
      if (e?.response?.status === 422 && data?.error === 'missing_dep' && data.dep && data.install_endpoint) {
        setMissingDep({
          dep:             data.dep,
          message:         data.message ?? `Dependency "${data.dep}" is not installed.`,
          installEndpoint: data.install_endpoint,
        })
      }
    },
  })

  const installDepMut = useMutation({
    mutationFn: (endpoint: string) => api.post(endpoint, {}).then((r) => r.data),
    onSuccess: () => {
      toast.success(`${missingDep?.dep ?? 'dependency'} installed`)
      setMissingDep(null)
      // Replay the save the user originally clicked. If they had
      // navigated away from Settings in the meantime the body is
      // still the last thing they tried to save — which is what
      // they wanted persisted in the first place.
      if (lastSaveBodyRef.current) saveMut.mutate(lastSaveBodyRef.current)
    },
    onError: (err: unknown) => {
      const e = err as { response?: { data?: { error?: string } } }
      toast.error(e?.response?.data?.error ?? 'Install failed')
    },
  })

  const set = (path: string, value: unknown) => {
    setDraft((prev) => setPath(prev, path, value))
  }

  const str  = (path: string, fallback = '') => String(getPath(draft, path) ?? fallback)
  const num  = (path: string, fallback = 0)  => Number(getPath(draft, path) ?? fallback)
  const bool = (path: string, fallback = false) => Boolean(getPath(draft, path) ?? fallback)

  const handleSave = () => {
    if (tab === 'advanced') {
      try {
        const parsed = JSON.parse(rawJson)
        const body = stripSentinels(parsed)
        autoEnableKeyedProviders(body)
        saveMut.mutate(body as Config)
      } catch { setRawError('Invalid JSON'); return }
    } else {
      const body = stripSentinels(draft)
      autoEnableKeyedProviders(body)
      saveMut.mutate(body as Config)
    }
    setRawError('')
  }

  if (isLoading) return <div className={styles.loading}>Loading configuration…</div>

  return (
    <div className={styles.page}>
      {/* Header */}
      <div className={styles.header}>
        <div>
          <h1>Settings</h1>
          <p>MIRA configuration — changes take effect after save</p>
        </div>
        <div className={styles.headerActions}>
          {isAdmin && (
            <button
              className={styles.restartBtn}
              onClick={() => {
                const prompt = supervised
                  ? 'Restart the MIRA server now? Active connections will be interrupted.'
                  : 'MIRA is running without a supervisor — clicking Stop will exit the process and you will need to relaunch it manually. Stop now?'
                if (confirm(prompt)) {
                  restartMut.mutate()
                }
              }}
              disabled={restartMut.isPending}
              title={supervised
                ? 'Required for some settings (channels, security, ports) to take effect'
                : 'MIRA is not running under a supervisor — this will stop the process. Run `mira install` to enable auto-restart.'}
            >
              {restartMut.isPending
                ? <Loader2 size={14} className={styles.spin} />
                : <RotateCcw size={14} />}
              {restartMut.isPending
                ? (supervised ? 'Restarting…' : 'Stopping…')
                : (supervised ? 'Restart server' : 'Stop server')}
            </button>
          )}
          <button
            className={`${styles.saveBtn} ${saved ? styles.saveBtnDone : ''}`}
            onClick={handleSave}
            disabled={saveMut.isPending}
          >
            {saved ? <Check size={14} /> : <Save size={14} />}
            {saveMut.isPending ? 'Saving…' : saved ? 'Saved' : 'Save changes'}
          </button>
        </div>
      </div>

      {saveMut.isError && (
        <div className={styles.errorBanner}>
          {(() => {
            const e = saveMut.error as { response?: { data?: { error?: string } | string } }
            const d = e?.response?.data
            return (typeof d === 'object' ? d?.error : d) ?? 'Save failed'
          })()}
        </div>
      )}

      <WslHostUrlBanner isAdmin={isAdmin} />


      {providerRestartRequired && (
        <div className={styles.restartBanner}>
          <span style={{ flex: 1 }}>
            Provider configuration changed. The new provider chain
            takes effect on the next service restart.
          </span>
          {isAdmin && (
            <button
              type="button"
              className={styles.restartBannerBtn}
              onClick={() => {
                const prompt = supervised
                  ? 'Restart the MIRA server now? Active connections will be interrupted.'
                  : 'MIRA is running without a supervisor — clicking Stop will exit the process and you will need to relaunch it manually. Stop now?'
                if (confirm(prompt)) {
                  restartMut.mutate()
                }
              }}
              disabled={restartMut.isPending}
            >
              {restartMut.isPending
                ? <Loader2 size={12} className={styles.spin} />
                : <RotateCcw size={12} />}
              {restartMut.isPending
                ? (supervised ? 'Restarting…' : 'Stopping…')
                : (supervised ? 'Restart now' : 'Stop now')}
            </button>
          )}
          <button
            type="button"
            className={styles.restartBannerDismiss}
            onClick={() => setProviderRestartRequired(false)}
            title="Dismiss — I'll restart later"
          >
            ×
          </button>
        </div>
      )}

      {/* Tab bar */}
      <div className={styles.tabBar}>
        {TABS.map((t) => (
          <button
            key={t.id}
            className={`${styles.tab} ${tab === t.id ? styles.tabActive : ''}`}
            onClick={() => setTab(t.id)}
          >
            {t.icon}
            <span>{t.label}</span>
          </button>
        ))}
      </div>

      {/* Tab content */}
      <div className={styles.tabContent}>
        {tab === 'appearance' && (
          <AppearanceTab set={set} bool={bool} str={str} />
        )}
        {tab === 'providers' && (
          <ProvidersTab set={set} str={str} num={num} draft={draft} />
        )}
        {tab === 'agent' && (
          <AgentTab set={set} str={str} num={num} bool={bool} isAdmin={isAdmin} />
        )}
        {tab === 'tools' && (
          <ToolsTab set={set} str={str} num={num} bool={bool} draft={draft} />
        )}
        {tab === 'sandbox' && (
          <SandboxTab set={set} str={str} num={num} bool={bool} draft={draft} />
        )}
        {tab === 'channels' && (
          <ChannelsTab set={set} str={str} num={num} bool={bool} />
        )}
        {tab === 'memory' && (
          <MemoryTab set={set} str={str} num={num} bool={bool} draft={draft} />
        )}
        {tab === 'calendar' && (
          <CalendarTab set={set} str={str} num={num} bool={bool} isAdmin={isAdmin} />
        )}
        {tab === 'voice' && (
          <VoiceTab set={set} str={str} num={num} bool={bool} />
        )}
        {tab === 'image' && (
          <ImageTab set={set} str={str} num={num} bool={bool} />
        )}
        {tab === 'notifications' && (
          <div className={styles.tabBody}>
            <Section title="Browser / phone push notifications">
              <NotificationSettings />
            </Section>
            <Section title="Daily Briefing">
              <DailyBriefingSettings />
            </Section>
            <Section title="Companion check-in (test)">
              <CompanionCheckinTest />
            </Section>
          </div>
        )}
        {tab === 'guardian' && (
          <GuardianTab set={set} str={str} num={num} bool={bool} />
        )}
        {tab === 'server' && (
          <ServerTab set={set} str={str} num={num} bool={bool} draft={draft} />
        )}
        {tab === 'advanced' && (
          <AdvancedTab
            json={rawJson}
            error={rawError}
            onChange={(v) => { setRawJson(v); setRawError('') }}
            set={set} str={str} num={num}
          />
        )}
      </div>

      {missingDep && (
        <MissingDepModal
          dep={missingDep.dep}
          message={missingDep.message}
          installing={installDepMut.isPending}
          onInstall={() => installDepMut.mutate(missingDep.installEndpoint)}
          onCancel={() => setMissingDep(null)}
        />
      )}
    </div>
  )
}

// ── Missing-dep install modal ────────────────────────────────────────────────
//
// Fired when PUT /api/config returns 422 missing_dep — the user
// picked a config (e.g. embedding.provider=internal) whose native
// dep isn't on disk. Asking before downloading covers two cases:
// the user might have picked the provider by mistake, or they
// might be on a metered connection.
function MissingDepModal({
  dep,
  message,
  installing,
  onInstall,
  onCancel,
}: {
  dep:        string
  message:    string
  installing: boolean
  onInstall:  () => void
  onCancel:   () => void
}) {
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="missing-dep-title"
      style={{
        position: 'fixed', inset: 0, background: 'rgba(0,0,0,0.55)',
        backdropFilter: 'blur(4px)', display: 'flex',
        alignItems: 'center', justifyContent: 'center', zIndex: 220, padding: 24,
      }}
      onClick={(e) => { if (e.target === e.currentTarget && !installing) onCancel() }}
    >
      <div style={{
        width: '100%', maxWidth: 480,
        background: 'var(--bg-surface)', border: '1px solid var(--border)',
        borderRadius: 'var(--radius-lg)', padding: '24px 24px 20px',
        boxShadow: '0 20px 60px rgba(0,0,0,0.45)',
      }}>
        <h2 id="missing-dep-title" style={{ margin: '0 0 12px', fontSize: 18 }}>
          {dep} not installed
        </h2>
        <p style={{ margin: '0 0 16px', fontSize: 14, lineHeight: 1.5 }}>
          {message}
        </p>
        <p style={{ margin: '0 0 20px', fontSize: 13, color: 'var(--text-muted)' }}>
          Download and install the dependency now? This pulls a few MB
          from upstream and verifies the SHA-256 against MIRA's pinned
          manifest before extracting to <code>~/.mira/deps/{dep}/</code>.
        </p>
        <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end' }}>
          <button
            onClick={onCancel}
            disabled={installing}
            style={{
              padding: '8px 16px', borderRadius: 'var(--radius-sm)',
              border: '1px solid var(--border)', background: 'transparent',
              color: 'var(--text-primary)', cursor: installing ? 'not-allowed' : 'pointer',
            }}
          >
            No, cancel
          </button>
          <button
            onClick={onInstall}
            disabled={installing}
            style={{
              padding: '8px 16px', borderRadius: 'var(--radius-sm)',
              border: 'none', background: 'var(--accent)', color: 'white',
              cursor: installing ? 'wait' : 'pointer', fontWeight: 600,
            }}
          >
            {installing ? 'Installing…' : 'Yes, install now'}
          </button>
        </div>
      </div>
    </div>
  )
}

// ── Appearance tab ────────────────────────────────────────────────────────────

function AppearanceTab({
  set, bool, str,
}: {
  set: (p: string, v: unknown) => void
  bool: (p: string, fb?: boolean) => boolean
  str: (p: string, fb?: string) => string
}) {
  const { theme, setTheme } = useThemeStore()
  const { sidebarCollapsed, toggleSidebar } = useUiStore()

  const TUI_THEMES = ['mira-dark', 'mira-light', 'dracula', 'gruvbox', 'nord']

  return (
    <div className={styles.tabBody}>
      <Section title="Web Interface Theme">
        <p className={styles.sectionDesc}>
          Choose the colour scheme for the web UI. Themes are stored locally and don't affect other users.
        </p>
        <div className={styles.themeGrid}>
          {THEMES.map((t) => (
            <button
              key={t.value}
              className={`${styles.themeCard} ${theme === t.value ? styles.themeCardActive : ''}`}
              onClick={() => setTheme(t.value)}
            >
              <div className={styles.themePreview} style={{ background: t.bg }}>
                <div className={styles.themePreviewAccent} style={{ background: t.accent }} />
                <div className={styles.themePreviewBar} style={{ background: t.accent + '30' }} />
                <div className={styles.themePreviewBar} style={{ background: t.accent + '18' }} />
              </div>
              <div className={styles.themeCardLabel}>
                <span>{t.label}</span>
                {theme === t.value && <span className={styles.themeCardCheck}><Check size={11} /></span>}
              </div>
            </button>
          ))}
        </div>
      </Section>

      <Section title="Sidebar">
        <Field label="Default state" desc="Whether the sidebar starts collapsed or expanded when you open the app.">
          <Toggle
            value={sidebarCollapsed}
            onChange={toggleSidebar}
            label={sidebarCollapsed ? 'Collapsed' : 'Expanded'}
          />
        </Field>
      </Section>

      <Section title="TUI Theme">
        <Field label="Terminal theme" desc="Colour scheme used by the terminal user interface (mira tui).">
          <SelectInput
            value={str('tui.theme', 'mira-dark')}
            onChange={(v) => set('tui.theme', v)}
            options={TUI_THEMES.map((n) => ({ value: n, label: n }))}
          />
        </Field>
        <Field label="Layout" desc="TUI panel layout. 'default' shows chat + status bars.">
          <SelectInput
            value={str('tui.layout', 'default')}
            onChange={(v) => set('tui.layout', v)}
            options={[
              { value: 'default',  label: 'Default' },
              { value: 'compact',  label: 'Compact' },
              { value: 'wide',     label: 'Wide' },
            ]}
          />
        </Field>
        <Field label="Show timestamps" desc="Display message timestamps in the TUI.">
          <Toggle value={bool('tui.show_timestamps', true)} onChange={(v) => set('tui.show_timestamps', v)} />
        </Field>
        <Field label="Show token count" desc="Display token usage after each assistant response.">
          <Toggle value={bool('tui.show_token_count', true)} onChange={(v) => set('tui.show_token_count', v)} />
        </Field>
      </Section>
    </div>
  )
}

// ── Providers tab ─────────────────────────────────────────────────────────────

function ProvidersTab({
  set, str, num, draft,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  draft: Config
}) {
  const failoverValue = getPath(draft, 'failover_providers') as string[] | null | undefined
  return (
    <div className={styles.tabBody}>
      <WslUrlHint />
      <Section title="Active Provider">
        <Field label="Primary provider" desc="The AI provider MIRA uses by default for chat and agent tasks. A provider becomes available once it's enabled and has its API key set (or a server URL, for local providers). If the one you pick here isn't set up, MIRA automatically uses the first provider that is.">
          <SelectInput
            value={str('primary_provider', 'lmstudio')}
            onChange={(v) => set('primary_provider', v)}
            options={[
              { value: 'ollama',         label: 'Ollama (local)' },
              { value: 'lmstudio',       label: 'LM Studio (local)' },
              { value: 'openrouter',     label: 'OpenRouter' },
              { value: 'openai',         label: 'OpenAI' },
              { value: 'deepseek',       label: 'DeepSeek' },
              { value: 'moonshot',       label: 'Moonshot (Kimi)' },
              { value: 'groq',           label: 'Groq' },
              { value: 'xai',            label: 'xAI (Grok)' },
              { value: 'anthropic',      label: 'Anthropic (Claude)' },
              { value: 'gemini',         label: 'Google Gemini' },
              { value: 'openai_compat',  label: 'OpenAI-compatible (custom)' },
            ]}
          />
        </Field>
      </Section>

      <Section title="Automatic failover">
        <FailoverChainEditor
          value={failoverValue}
          primary={str('primary_provider', 'lmstudio')}
          openaiCompatUrl={str('providers.openai_compat.base_url')}
          onChange={(list) => set('failover_providers', list)}
        />
      </Section>

      <ProviderSection title="Ollama" slug="ollama" set={set} str={str}
        urlPath="providers.ollama.url" urlDefault="http://localhost:11434">
        <Field label="Base URL" desc="HTTP endpoint of your local Ollama server.">
          <TextInput value={str('providers.ollama.url', 'http://localhost:11434')} onChange={(v) => set('providers.ollama.url', v)} placeholder="http://localhost:11434" mono />
        </Field>
        <Field label="Default model" desc="Picked from Ollama's /api/tags when the server is reachable. Run `ollama pull <model>` to install more.">
          <ModelSelect
            provider="ollama"
            catalogEnabled={str('providers.ollama.enabled', 'true') !== 'false'}
            value={str('providers.ollama.default_model', '')}
            onChange={(v) => set('providers.ollama.default_model', v)}
            placeholder="llama3:8b"
          />
        </Field>
        <Field label="Timeout (seconds)" desc="Request timeout for Ollama completions.">
          <NumberInput value={num('providers.ollama.timeout_secs', 120)} onChange={(v) => set('providers.ollama.timeout_secs', v)} min={5} max={600} />
        </Field>
      </ProviderSection>

      <ProviderSection title="LM Studio" slug="lmstudio" set={set} str={str}
        urlPath="providers.lmstudio.url" urlDefault="http://localhost:1234">
        <Field label="Base URL" desc="HTTP endpoint of your local LM Studio server.">
          <TextInput value={str('providers.lmstudio.url', 'http://localhost:1234')} onChange={(v) => set('providers.lmstudio.url', v)} placeholder="http://localhost:1234" mono />
        </Field>
        <Field label="Default model" desc="Picked from LM Studio's /v1/models when the server is reachable.">
          <ModelSelect
            provider="lmstudio"
            catalogEnabled={str('providers.lmstudio.enabled', 'true') !== 'false'}
            value={str('providers.lmstudio.default_model', '')}
            onChange={(v) => set('providers.lmstudio.default_model', v)}
            placeholder="meta-llama-3-8b-instruct"
          />
        </Field>
        <Field label="Timeout (seconds)" desc="Request timeout for LM Studio completions.">
          <NumberInput value={num('providers.lmstudio.timeout_secs', 120)} onChange={(v) => set('providers.lmstudio.timeout_secs', v)} min={5} max={600} />
        </Field>
      </ProviderSection>

      <ProviderSection title="OpenRouter" slug="openrouter" set={set} str={str}
        urlPath="providers.openrouter.base_url" urlDefault="https://openrouter.ai/api/v1">
        <Field label="API key" desc="Your OpenRouter API key. Stored in the config file — keep it secure.">
          <TextInput value={str('providers.openrouter.api_key')} onChange={(v) => set('providers.openrouter.api_key', v)} placeholder="sk-or-…" type="password" mono />
        </Field>
        <Field label="Default model" desc="Picked from OpenRouter's full catalog (300+ models) when the API key is set. Pricing is fetched live from OpenRouter.">
          <ModelSelect
            provider="openrouter"
            catalogEnabled={str('providers.openrouter.enabled', 'true') !== 'false'}
            value={str('providers.openrouter.default_model', '')}
            onChange={(v) => set('providers.openrouter.default_model', v)}
            placeholder="openai/gpt-4o"
          />
        </Field>
        <Field label="Base URL" desc="Override the OpenRouter API base URL (advanced).">
          <TextInput value={str('providers.openrouter.base_url', 'https://openrouter.ai/api/v1')} onChange={(v) => set('providers.openrouter.base_url', v)} placeholder="https://openrouter.ai/api/v1" mono />
        </Field>
      </ProviderSection>

      <OpenAiCompatProviderSection
        title="OpenAI"
        slug="openai"
        defaultBaseUrl="https://api.openai.com/v1"
        modelPlaceholder="gpt-4o-mini"
        apiKeyHint="Obtain at https://platform.openai.com/api-keys."
        str={str} set={set} num={num}
      />
      <OpenAiCompatProviderSection
        title="DeepSeek"
        slug="deepseek"
        defaultBaseUrl="https://api.deepseek.com/v1"
        modelPlaceholder="deepseek-chat"
        apiKeyHint="Obtain at https://platform.deepseek.com. Use deepseek-reasoner for R1-style chain-of-thought."
        str={str} set={set} num={num}
      />
      <OpenAiCompatProviderSection
        title="Moonshot (Kimi)"
        slug="moonshot"
        defaultBaseUrl="https://api.moonshot.ai/v1"
        modelPlaceholder="kimi-k2-0905-preview"
        apiKeyHint="Obtain at https://platform.moonshot.ai."
        str={str} set={set} num={num}
      />
      <OpenAiCompatProviderSection
        title="Groq"
        slug="groq"
        defaultBaseUrl="https://api.groq.com/openai/v1"
        modelPlaceholder="llama-3.3-70b-versatile"
        apiKeyHint="Obtain at https://console.groq.com. Fast hosted inference for Llama, Mixtral, DeepSeek, and others."
        str={str} set={set} num={num}
      />
      <OpenAiCompatProviderSection
        title="xAI (Grok)"
        slug="xai"
        defaultBaseUrl="https://api.x.ai/v1"
        modelPlaceholder="grok-4"
        apiKeyHint="Obtain at https://x.ai/api."
        str={str} set={set} num={num}
      />

      <ProviderSection title="Google Gemini" slug="gemini" set={set} str={str}>
        <Field
          label="API key"
          desc="Google AI Studio API key. Obtain at https://aistudio.google.com/apikey. Provider stays registered only when the key is set. Uses Gemini's native :generateContent API — separate from the OpenAI-compatible providers above."
        >
          <TextInput
            value={str('providers.gemini.api_key', '')}
            onChange={(v) => set('providers.gemini.api_key', v)}
            placeholder="AIza…"
            type="password"
            mono
          />
        </Field>
        <Field label="Default model" desc="Picked from Gemini's catalog (/v1beta/models) when the API key is set. Examples: gemini-2.5-pro (top-tier), gemini-2.5-flash (fast/cheap), gemini-2.5-flash-lite (cheapest).">
          <ModelSelect
            provider="gemini"
            catalogEnabled={str('providers.gemini.enabled', 'true') !== 'false'}
            value={str('providers.gemini.default_model', '')}
            onChange={(v) => set('providers.gemini.default_model', v)}
            placeholder="gemini-2.5-pro"
          />
        </Field>
        <Field label="Base URL" desc="Override only for Gemini-compatible proxies. Default is the AI Studio endpoint, NOT Vertex AI (which would need OAuth).">
          <TextInput
            value={str('providers.gemini.base_url', 'https://generativelanguage.googleapis.com')}
            onChange={(v) => set('providers.gemini.base_url', v)}
            placeholder="https://generativelanguage.googleapis.com"
            mono
          />
        </Field>
        <Field label="Timeout (seconds)" desc="Request timeout. Long-context Pro requests can take 30–60s on cold caches.">
          <NumberInput
            value={num('providers.gemini.timeout_secs', 120)}
            onChange={(v) => set('providers.gemini.timeout_secs', v)}
            min={5}
            max={600}
          />
        </Field>
      </ProviderSection>

      <ProviderSection title="Anthropic (Claude)" slug="anthropic" set={set} str={str}>
        <Field
          label="API key"
          desc="Anthropic API key. Obtain at https://console.anthropic.com/settings/keys. Provider stays registered only when the key is set. Uses Anthropic's native /v1/messages API — separate from the OpenAI-compatible providers above."
        >
          <TextInput
            value={str('providers.anthropic.api_key', '')}
            onChange={(v) => set('providers.anthropic.api_key', v)}
            placeholder="sk-ant-…"
            type="password"
            mono
          />
        </Field>
        <Field label="Default model" desc="Picked from Anthropic's catalog (/v1/models) when the API key is set. Examples: claude-sonnet-4-5 (balanced), claude-haiku-4-5 (fast/cheap), claude-opus-4-1 (top-tier reasoning).">
          <ModelSelect
            provider="anthropic"
            catalogEnabled={str('providers.anthropic.enabled', 'true') !== 'false'}
            value={str('providers.anthropic.default_model', '')}
            onChange={(v) => set('providers.anthropic.default_model', v)}
            placeholder="claude-sonnet-4-5"
          />
        </Field>
        <Field label="Base URL" desc="Override only for Anthropic-compatible proxies (Vercel AI Gateway, LiteLLM, etc.).">
          <TextInput
            value={str('providers.anthropic.base_url', 'https://api.anthropic.com')}
            onChange={(v) => set('providers.anthropic.base_url', v)}
            placeholder="https://api.anthropic.com"
            mono
          />
        </Field>
        <Field label="Timeout (seconds)" desc="Request timeout. Anthropic's larger models can take 30–60s on long completions.">
          <NumberInput
            value={num('providers.anthropic.timeout_secs', 120)}
            onChange={(v) => set('providers.anthropic.timeout_secs', v)}
            min={5}
            max={600}
          />
        </Field>
      </ProviderSection>

      <ProviderSection title="OpenAI-compatible (custom gateway)" slug="openai_compat" set={set} str={str}
        urlPath="providers.openai_compat.base_url" urlDefault="">
        <Field
          label="Name"
          desc="A short name for this provider, shown in logs and when picking a model — e.g. 'together', 'fireworks', 'perplexity', 'azure', 'vllm-local'. Leave empty to turn this custom provider off."
        >
          <TextInput
            value={str('providers.openai_compat.name', '')}
            onChange={(v) => set('providers.openai_compat.name', v)}
            placeholder="together"
            mono
          />
        </Field>
        <Field label="Base URL" desc="OpenAI-compatible /v1 endpoint (no trailing slash).">
          <TextInput
            value={str('providers.openai_compat.base_url', '')}
            onChange={(v) => set('providers.openai_compat.base_url', v)}
            placeholder="https://api.together.xyz/v1"
            mono
          />
        </Field>
        <Field label="API key" desc="Leave empty only when Auth style below is set to 'None'.">
          <TextInput
            value={str('providers.openai_compat.api_key', '')}
            onChange={(v) => set('providers.openai_compat.api_key', v)}
            placeholder="sk-…"
            type="password"
            mono
          />
        </Field>
        <Field label="Default model" desc="Picked from the gateway's /v1/models once name+base_url+api_key are set.">
          <ModelSelect
            provider="openai_compat"
            catalogEnabled={str('providers.openai_compat.enabled', 'true') !== 'false'}
            value={str('providers.openai_compat.default_model', '')}
            onChange={(v) => set('providers.openai_compat.default_model', v)}
            placeholder="meta-llama/Llama-3.3-70B-Instruct-Turbo"
          />
        </Field>
        <Field label="Auth style" desc="'bearer' for OpenAI/Together/Fireworks/etc., 'azure' for Azure OpenAI (api-key header), 'none' for unsecured local endpoints.">
          <SelectInput
            value={str('providers.openai_compat.auth_style', 'bearer')}
            onChange={(v) => set('providers.openai_compat.auth_style', v)}
            options={[
              { value: 'bearer', label: 'Bearer (default)' },
              { value: 'azure',  label: 'Azure (api-key header)' },
              { value: 'none',   label: 'None (anonymous)' },
            ]}
          />
        </Field>
        <Field label="Timeout (seconds)" desc="Request timeout.">
          <NumberInput
            value={num('providers.openai_compat.timeout_secs', 120)}
            onChange={(v) => set('providers.openai_compat.timeout_secs', v)}
            min={5}
            max={600}
          />
        </Field>
      </ProviderSection>
    </div>
  )
}

// Each OpenAI-compatible cloud provider has the same four-field shape
// (api_key, base_url, default_model, timeout_secs). Factored to a single
// helper component so adding the next OpenAI-shaped gateway is one line.
function OpenAiCompatProviderSection({
  title, slug, defaultBaseUrl, modelPlaceholder, apiKeyHint, str, set, num,
}: {
  title: string
  slug: string
  defaultBaseUrl: string
  modelPlaceholder: string
  apiKeyHint: string
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
}) {
  const root = `providers.${slug}`
  return (
    <ProviderSection title={title} slug={slug} set={set} str={str}
      urlPath={`${root}.base_url`} urlDefault={defaultBaseUrl}>
      <Field label="API key" desc={`${apiKeyHint} Provider stays registered only when the key is set AND the toggle above is enabled.`}>
        <TextInput
          value={str(`${root}.api_key`, '')}
          onChange={(v) => set(`${root}.api_key`, v)}
          placeholder="sk-…"
          type="password"
          mono
        />
      </Field>
      <Field label="Default model" desc="Picked from the provider's catalog when the API key is set; falls back to free-text otherwise. Used when a per-skill alias doesn't override it.">
        <ModelSelect
          provider={slug}
          catalogEnabled={str(`${root}.enabled`, 'true') !== 'false'}
          value={str(`${root}.default_model`, '')}
          onChange={(v) => set(`${root}.default_model`, v)}
          placeholder={modelPlaceholder}
        />
      </Field>
      <Field label="Base URL" desc="Override only for self-hosted proxies or non-default regions.">
        <TextInput
          value={str(`${root}.base_url`, defaultBaseUrl)}
          onChange={(v) => set(`${root}.base_url`, v)}
          placeholder={defaultBaseUrl}
          mono
        />
      </Field>
      <Field label="Timeout (seconds)" desc="Request timeout.">
        <NumberInput
          value={num(`${root}.timeout_secs`, 120)}
          onChange={(v) => set(`${root}.timeout_secs`, v)}
          min={5}
          max={600}
        />
      </Field>
    </ProviderSection>
  )
}

// ── Agent appearance section ──────────────────────────────────────────────────
//
// Lives inside AgentTab but uses dedicated endpoints rather than the draft-
// save flow — uploads and preset-picks apply immediately, which matches how
// the user-profile avatar picker behaves.

function AppearanceAgentSection() {
  const qc = useQueryClient()
  const { data: appearance } = useQuery({
    queryKey: ['agent-appearance'],
    queryFn:  async () => {
      const r = await api.get<AgentAppearance>('/api/agent/appearance')
      return r.data
    },
    staleTime: 30_000,
  })
  const fileInputRef = useRef<HTMLInputElement>(null)

  const invalidate = () => {
    qc.invalidateQueries({ queryKey: ['agent-appearance'] })
    qc.invalidateQueries({ queryKey: ['config'] })
  }

  const setMut = useMutation({
    mutationFn: async (avatar: string | null) => {
      const r = await api.put<AgentAppearance>('/api/config/agent-avatar', { avatar })
      return r.data
    },
    onSuccess: invalidate,
    onError: () => toast.error('Could not update agent avatar'),
  })

  const uploadMut = useMutation({
    mutationFn: async (file: File) => {
      const fd = new FormData()
      fd.append('file', file)
      const r = await api.post<AgentAppearance>('/api/config/agent-avatar', fd)
      return r.data
    },
    onSuccess: () => { invalidate(); toast.success('Agent avatar uploaded.') },
    onError: (e: unknown) => {
      const msg = (e as { response?: { data?: string } })?.response?.data
      toast.error(msg ? String(msg) : 'Upload failed')
    },
  })

  const clearMut = useMutation({
    mutationFn: async () => {
      const r = await api.delete<AgentAppearance>('/api/config/agent-avatar')
      return r.data
    },
    onSuccess: invalidate,
    onError: () => toast.error('Could not reset avatar'),
  })

  const currentKey = appearance?.avatar?.startsWith('preset:')
    ? appearance.avatar.slice('preset:'.length)
    : null
  const hasAvatar = !!appearance?.avatar

  return (
    <Section title="Appearance">
      <p className={styles.sectionDesc}>
        Choose the avatar shown next to the assistant's messages in chat. Changes apply instantly.
      </p>
      <Field label="Current avatar" desc="How MIRA appears in chat conversations.">
        <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
          <AgentAvatar size={56} />
          <span className={styles.fieldDesc}>
            {hasAvatar
              ? appearance?.avatar?.startsWith('upload:')
                ? 'Custom upload'
                : `Preset: ${currentKey}`
              : 'Default MIRA logo'}
          </span>
        </div>
      </Field>
      <Field label="Preset" desc="Pick one of the built-in icon avatars.">
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(6, 1fr)', gap: 8, maxWidth: 320 }}>
          {AVATAR_PRESETS.map((p) => {
            const { Icon, bg, key, label } = p
            const active = currentKey === key
            return (
              <button
                key={key}
                type="button"
                title={label}
                aria-label={`Use ${label} avatar`}
                onClick={() => setMut.mutate(`preset:${key}`)}
                disabled={setMut.isPending}
                style={{
                  aspectRatio: '1 / 1',
                  borderRadius: '50%',
                  border: active ? '2px solid var(--text-primary)' : '2px solid transparent',
                  boxShadow: active ? '0 0 0 2px var(--bg-surface), 0 0 0 4px var(--accent)' : 'none',
                  background: bg,
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'center',
                  cursor: setMut.isPending ? 'wait' : 'pointer',
                  padding: 0,
                }}
              >
                <Icon size={18} color="white" />
              </button>
            )
          })}
        </div>
      </Field>
      <Field label="Custom image" desc="Upload a PNG, JPEG, WebP, or GIF (max 2 MiB).">
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <button
            className={styles.input}
            style={{ display: 'inline-flex', alignItems: 'center', gap: 6, cursor: 'pointer', width: 'auto' }}
            onClick={() => fileInputRef.current?.click()}
            disabled={uploadMut.isPending}
          >
            <Upload size={14} />
            {uploadMut.isPending ? 'Uploading…' : 'Upload image'}
          </button>
          {hasAvatar && (
            <button
              className={styles.input}
              style={{ display: 'inline-flex', alignItems: 'center', gap: 6, cursor: 'pointer', width: 'auto' }}
              onClick={() => clearMut.mutate()}
              disabled={clearMut.isPending}
              title="Reset to MIRA logo"
            >
              <Trash2 size={14} />
              Reset
            </button>
          )}
          <input
            ref={fileInputRef}
            type="file"
            accept="image/png,image/jpeg,image/webp,image/gif"
            style={{ display: 'none' }}
            onChange={(e) => {
              const f = e.target.files?.[0]
              if (f) uploadMut.mutate(f)
              e.target.value = ''
            }}
          />
        </div>
      </Field>
    </Section>
  )
}

// ── Agent tab ─────────────────────────────────────────────────────────────────

function AgentTab({
  set, str, num, bool, isAdmin,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
  isAdmin: boolean
}) {
  return (
    <div className={styles.tabBody}>
      <AppearanceAgentSection />
      <Section title="Core behaviour">
        <Field label="Tool mode" desc="How MIRA invokes tools. 'auto' tries OpenAI structured calls first, falling back to ReAct-style prompting.">
          <SelectInput
            value={str('agent.tool_mode', 'auto')}
            onChange={(v) => set('agent.tool_mode', v)}
            options={[
              { value: 'auto',     label: 'Auto (recommended)' },
              { value: 'openai',   label: 'OpenAI structured' },
              { value: 'react',    label: 'ReAct (plain text)' },
              { value: 'disabled', label: 'Disabled' },
            ]}
          />
        </Field>
        <Field label="Max tool rounds" desc="Maximum tool-call / observe cycles per turn before the agent gives up.">
          <NumberInput value={num('agent.max_tool_rounds', 10)} onChange={(v) => set('agent.max_tool_rounds', v)} min={1} max={50} />
        </Field>
        <Field
          label="Max tool-round tokens"
          desc="Token cap on each non-streaming tool-call probe. Reasoning-distilled models (Qwen, DeepSeek-R1, etc.) can burn this entire budget on internal deliberation before they emit a tool call — bump to 8192–16384 if you see chats stall mid-thought. Default 2048 fits non-reasoning models comfortably."
        >
          <NumberInput value={num('agent.max_tool_round_tokens', 2048)} onChange={(v) => set('agent.max_tool_round_tokens', v)} min={256} max={65536} />
        </Field>
        <Field
          label="Max response tokens"
          desc="Token cap on the final streaming response after tool calls settle. Bump for very long answers (full code listings, multi-page summaries). Default 16384 is plenty for typical chat."
        >
          <NumberInput value={num('agent.max_response_tokens', 16384)} onChange={(v) => set('agent.max_response_tokens', v)} min={512} max={131072} />
        </Field>
        <Field label="Max context turns" desc="Fixed-window fallback: number of recent conversation turns kept in the model's context (1 turn = user + assistant). Used only when 'Context window (tokens)' below is 0.">
          <NumberInput value={num('agent.max_context_turns', 20)} onChange={(v) => set('agent.max_context_turns', v)} min={2} max={200} />
        </Field>
        <Field label="Context window (tokens)" desc="Token-aware context budgeting: your primary model's context window in tokens (e.g. 128000). When set, MIRA fills the window by token budget — carrying far more history when it fits — instead of the fixed turn count above. 0 keeps the legacy fixed-turn behaviour.">
          <NumberInput value={num('agent.context_length_tokens', 0)} onChange={(v) => set('agent.context_length_tokens', v)} min={0} max={2000000} />
        </Field>
        <Field label="Context safety margin (tokens)" desc="Headroom held back from the context budget (only when 'Context window (tokens)' > 0) so a packed prompt can't overflow the model. Default 2048.">
          <NumberInput value={num('agent.context_safety_margin_tokens', 2048)} onChange={(v) => set('agent.context_safety_margin_tokens', v)} min={0} max={32768} />
        </Field>
        <Field label="Prompt caching" desc="Keep the system-prompt prefix byte-stable turn-to-turn by moving per-turn retrieved context (memory + wiki) out of the system prompt and into the current message. A stable prefix lets cloud providers (Anthropic/OpenAI/Gemini) and local KV caches reuse it — ~90% cheaper/faster input on cloud, a free speedup locally. Off by default.">
          <Toggle value={bool('agent.prompt_cache_enabled', false)} onChange={(v) => set('agent.prompt_cache_enabled', v)} />
        </Field>
        <Field label="Auto-compaction" desc="When token budgeting is on and the oldest turns overflow the window, compact them into a rolling anchored summary instead of dropping them. Only active when 'Context window (tokens)' > 0.">
          <Toggle value={bool('agent.compaction.enabled', true)} onChange={(v) => set('agent.compaction.enabled', v)} />
        </Field>
        <Field label="Keep last turns verbatim" desc="How many of the most recent turns (1 turn = user + assistant) are kept word-for-word and never summarized by compaction. Default 6.">
          <NumberInput value={num('agent.compaction.keep_last_turns', 6)} onChange={(v) => set('agent.compaction.keep_last_turns', v)} min={0} max={100} />
        </Field>
        <Field label="Max summary tokens" desc="Soft cap on the rolling compaction summary's size in tokens, so the compacted block can't grow without bound. Default 1024.">
          <NumberInput value={num('agent.compaction.max_summary_tokens', 1024)} onChange={(v) => set('agent.compaction.max_summary_tokens', v)} min={0} max={32768} />
        </Field>
        <Field label="System prompt file" desc="Path to a custom agent.md persona file. Leave blank to use the built-in default.">
          <TextInput value={str('agent.system_prompt_file')} onChange={(v) => set('agent.system_prompt_file', v)} placeholder="~/.mira/agent.md" mono />
        </Field>
        <Field
          label="Playful easter eggs"
          desc="Let MIRA recognise famous pop-culture references and playful prompts (mirror-mirror, 'open the pod bay doors', 'meaning of life', magic-8-ball 'should I…', 'marco', 'I wish…', and more) and play along — improvised, in your own personality/tone and scaled by your playfulness setting, without hijacking a genuine request. A fun delight layer; turn off for a strictly-business assistant."
        >
          <Toggle value={bool('agent.playful_easter_eggs', true)} onChange={(v) => set('agent.playful_easter_eggs', v)} />
        </Field>
        <Field
          label="Show thinking"
          desc="Render a collapsible 'Thinking' rollup on each assistant chat message showing tool calls, tool results, reasoning blocks (R1/extended-thinking models) and wiki context fetched for the turn. The server still records this trail when off, so re-enabling restores history."
        >
          <Toggle value={bool('agent.show_thinking', true)} onChange={(v) => set('agent.show_thinking', v)} />
        </Field>
      </Section>

      {isAdmin && (
        <Section title="Updates">
          <p className={styles.sectionDesc}>
            MIRA checks for new releases and — where the platform allows — can upgrade itself
            in place and roll back if needed. Checking only compares versions; installing an
            update is always a deliberate action here.
          </p>
          <Field
            label="Automatically check for updates"
            desc="Periodically compare your version against the latest release. On by default; a single lightweight request that never downloads or installs anything on its own. Turn off to stop MIRA contacting the release host."
          >
            <Toggle value={bool('server.update_check.enabled', true)} onChange={(v) => set('server.update_check.enabled', v)} />
          </Field>
          <Field label="Check frequency" desc="How often MIRA refreshes the update check in the background. 'Check now' below always checks immediately.">
            <SelectInput
              value={str('server.update_check.frequency', 'daily')}
              onChange={(v) => set('server.update_check.frequency', v)}
              options={[
                { value: 'daily',   label: 'Daily' },
                { value: 'weekly',  label: 'Weekly' },
                { value: 'monthly', label: 'Monthly' },
              ]}
            />
          </Field>
          <UpdatesCard />
        </Section>
      )}

      <Section title="Tool selection (Just-in-Time Tools)">
        <p className={styles.sectionDesc}>
          With many tools/MCP servers installed, sending every tool's schema on every
          request bloats the prompt. <strong>Adaptive</strong> mode sends only a small core
          set plus the tools most relevant to each message (and ones used recently), and lets
          the model pull in anything else on demand via <code>find_tools</code>. Applies live —
          no restart.
        </p>
        <Field label="Mode" desc="'All' sends every enabled tool (default). 'Adaptive' sends only the per-turn relevant subset.">
          <SelectInput
            value={str('agent.tool_selection.mode', 'all')}
            onChange={(v) => set('agent.tool_selection.mode', v)}
            options={[
              { value: 'all',      label: 'All tools (default)' },
              { value: 'adaptive', label: 'Adaptive (Just-in-Time)' },
            ]}
          />
        </Field>
        <Field label="Top-K semantic matches" desc="Max number of message-relevant tools to add per turn (adaptive mode).">
          <NumberInput value={num('agent.tool_selection.top_k', 8)} onChange={(v) => set('agent.tool_selection.top_k', v)} min={0} max={64} />
        </Field>
        <Field label="Min similarity" desc="Minimum cosine similarity (0–1) for a tool to be matched. Higher = stricter / fewer tools.">
          <NumberInput value={num('agent.tool_selection.min_similarity', 0.30)} onChange={(v) => set('agent.tool_selection.min_similarity', v)} min={0} max={1} step={0.05} />
        </Field>
        <Field label="Stickiness turns" desc="Tools used earlier in a conversation stay available for this many later turns.">
          <NumberInput value={num('agent.tool_selection.stickiness_turns', 6)} onChange={(v) => set('agent.tool_selection.stickiness_turns', v)} min={0} max={50} />
        </Field>
        <Field label="Core tools (always loaded)" desc="Comma-separated tool names; trailing * is a prefix glob (e.g. memory_*). Kept available every turn.">
          <TextInput
            value={str('agent.tool_selection.core_tools', 'memory_*, wiki_*, now')}
            onChange={(v) => set('agent.tool_selection.core_tools', v.split(',').map((s) => s.trim()).filter(Boolean))}
            placeholder="memory_*, wiki_*, now"
            mono
          />
        </Field>
        <Field label="Expose find_tools" desc="Let the model load additional tools on demand (progressive disclosure). Recommended on so no capability is ever hidden.">
          <Toggle value={bool('agent.tool_selection.expose_find_tools', true)} onChange={(v) => set('agent.tool_selection.expose_find_tools', v)} />
        </Field>
      </Section>

      <Section title="Tools">
        <p className={styles.sectionDesc}>
          Individual tool toggles, limits, and the shared HTTP policy live on the dedicated <strong>Tools</strong> tab.
        </p>
      </Section>
    </div>
  )
}

// ── WSL host-URL helpers ────────────────────────────────────────────────────────

// On WSL2 NAT, service URLs pointed at the Windows host's LAN IP are unreachable
// from the guest — only the `windows-host` alias works. This banner shows when
// the server has *empirically* found such misrouted URLs (dead at their IP, alive
// via windows-host) and offers a one-click swap via the safe config path.
function WslHostUrlBanner({ isAdmin }: { isAdmin: boolean }) {
  const qc = useQueryClient()
  const q = useQuery({
    queryKey: ['wsl-host-url-check'],
    queryFn:  wslApi.check,
    retry: false,
    refetchInterval: 60_000,
  })
  const fix = useMutation({
    mutationFn: () => wslApi.fix(),
    onSuccess: (d) => {
      toast.success(d.note || 'Updated — restart for it to take effect.', { duration: 8000 })
      qc.invalidateQueries({ queryKey: ['wsl-host-url-check'] })
      qc.invalidateQueries({ queryKey: ['config'] })
    },
    onError: () => toast.error('Fix failed'),
  })
  const findings = q.data?.findings ?? []
  if (findings.length === 0) return null
  return (
    <div className={styles.restartBanner} role="alert">
      <span style={{ flex: 1 }}>
        <strong>⚠️ {findings.length} service URL{findings.length !== 1 ? 's' : ''} can’t reach the Windows host from WSL.</strong>{' '}
        Switch {findings.length > 1 ? 'them' : 'it'} to the <code>windows-host</code> alias?
        <ul style={{ margin: '6px 0 0', paddingLeft: 18, fontSize: 12, opacity: 0.9 }}>
          {findings.map((f) => (
            <li key={f.path}><code>{f.path}</code>: {f.current} → {f.suggested}</li>
          ))}
        </ul>
      </span>
      {isAdmin && (
        <button
          type="button"
          className={styles.restartBannerBtn}
          disabled={fix.isPending}
          onClick={() => fix.mutate()}
        >
          {fix.isPending ? 'Fixing…' : 'Fix → windows-host'}
        </button>
      )}
    </div>
  )
}

// One-line hint shown near host-URL fields when running in WSL — teaches the
// windows-host pattern at the point of entry. Renders nothing off-WSL.
function WslUrlHint() {
  const q = useQuery({ queryKey: ['wsl-host-url-check'], queryFn: wslApi.check, retry: false })
  if (!q.data?.is_wsl) return null
  return (
    <p className={styles.sectionDesc} style={{ marginTop: 0 }}>
      🪟 Running in WSL — for a service on the <strong>Windows host</strong>, use{' '}
      <code>http://windows-host:PORT</code> (the host’s LAN IP isn’t reachable from WSL).
    </p>
  )
}

// ── Guardian tab ──────────────────────────────────────────────────────────────

// MIRA-Guardian — local-model watchdog. Off by default; everything it does is
// local + HMAC-audited. Mode/interval changes apply on the next service restart
// (the Guardian's named-agent resolver reads a startup config snapshot). The
// live status surface lives on the System Health page.
function GuardianTab({
  set, str, num, bool,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
}) {
  return (
    <div className={styles.tabBody}>
      <Section title="MIRA-Guardian">
        <p className={styles.sectionDesc}>
          A co-resident local-model watchdog that monitors MIRA's health and can
          propose bounded, reversible fixes for your approval. Off by default;
          fail-closed local-only and HMAC-audited. Mode and interval changes take
          effect on the next service restart. Its live status (model, watch-loop,
          recent actions) shows on the <strong>System Health</strong> page.
        </p>
        <Field label="Mode" desc="off — disabled. monitor — observe + alert only, never acts. active — may also propose gated fixes (still need approval).">
          <SelectInput
            value={str('guardian.mode', 'off')}
            onChange={(v) => set('guardian.mode', v)}
            options={[
              { value: 'off',     label: 'Off (disabled)' },
              { value: 'monitor', label: 'Monitor (observe + alert)' },
              { value: 'active',  label: 'Active (propose fixes)' },
            ]}
          />
        </Field>
        <Field label="Watch interval (seconds)" desc="How often the proactive watch loop checks the latest health snapshot and, on a new non-green state, raises an alert. Minimum 60; default 900 (15 min).">
          <NumberInput value={num('guardian.watch_interval_secs', 900)} onChange={(v) => set('guardian.watch_interval_secs', v)} min={60} max={86400} />
        </Field>
        <Field label="Provision model" desc="The Ollama-registry model the one-click provisioning flow pulls + binds when no local model is configured.">
          <TextInput value={str('guardian.provision_model', '')} onChange={(v) => set('guardian.provision_model', v)} placeholder="qwen2.5:3b-instruct" mono />
        </Field>
      </Section>

      <Section title="Model tiers">
        <p className={styles.sectionDesc}>
          The Guardian runs a <strong>tiered</strong> local model: a light
          always-on model for routine, low-severity ticks, escalating to a
          stronger model only for real triage (when a detector goes red). Leave a
          tier empty to reuse the Guardian's default model (the <code>guardian</code>
          alias, else the primary provider). Both tiers stay
          <strong> fail-closed local-only</strong> — a cloud provider is refused.
          Changes take effect on the next service restart.
        </p>
        <Field label="Routine provider" desc="Light always-on model for routine ticks. Empty = use the Guardian's default (guardian alias / primary).">
          <SelectInput
            value={str('guardian.routine_provider', '')}
            onChange={(v) => set('guardian.routine_provider', v)}
            options={[
              { value: '',         label: 'Default (guardian alias / primary)' },
              { value: 'lmstudio', label: 'LM Studio' },
              { value: 'ollama',   label: 'Ollama' },
            ]}
          />
        </Field>
        <Field label="Routine model" desc="Model id on the routine provider. Empty = the provider's/alias's default model.">
          <TextInput value={str('guardian.routine_model', '')} onChange={(v) => set('guardian.routine_model', v)} placeholder="qwen2.5:3b-instruct" mono />
        </Field>
        <Field label="Triage provider" desc="Stronger model reached only when a detector goes red. Empty = use the Guardian's default (guardian alias / primary).">
          <SelectInput
            value={str('guardian.triage_provider', '')}
            onChange={(v) => set('guardian.triage_provider', v)}
            options={[
              { value: '',         label: 'Default (guardian alias / primary)' },
              { value: 'lmstudio', label: 'LM Studio' },
              { value: 'ollama',   label: 'Ollama' },
            ]}
          />
        </Field>
        <Field label="Triage model" desc="Model id on the triage provider. Empty = the provider's/alias's default model.">
          <TextInput value={str('guardian.triage_model', '')} onChange={(v) => set('guardian.triage_model', v)} placeholder="a stronger local model" mono />
        </Field>
      </Section>

      <Section title="Liveness sentinel (separate process)">
        <p className={styles.sectionDesc}>
          The Guardian's watch loop runs <em>inside</em> MIRA, so it can't catch
          the one failure that matters most — <strong>MIRA itself going down</strong>.
          The optional <strong>liveness sentinel</strong> is a separate process
          (<code>mira guardian-watch</code>) that probes MIRA's <code>/health</code>
          and, if MIRA is unreachable for a sustained window, sends a
          <strong> direct web-push alarm</strong> to your device — no dependency on
          the down MIRA. Observe-and-alarm only. After enabling it here, install
          it as its own supervised service by running <code>mira guardian-install</code>
          on the host (Linux/systemd today), then restart it. These settings drive it.
        </p>
        <Field label="Enable sentinel" desc="Master switch. The sentinel is a separate service you supervise; this only tells it (and the UI) it's meant to run.">
          <Toggle value={bool('guardian.process.enabled', false)} onChange={(v) => set('guardian.process.enabled', v)} />
        </Field>
        <Field label="Probe interval (seconds)" desc="How often the sentinel probes MIRA's /health. Minimum 5; default 30.">
          <NumberInput value={num('guardian.process.probe_interval_secs', 30)} onChange={(v) => set('guardian.process.probe_interval_secs', v)} min={5} max={3600} />
        </Field>
        <Field label="Down after N misses" desc="Consecutive failed probes before declaring MIRA down and alarming. Default 3 — high enough that a normal restart (which recovers within a window) doesn't alarm.">
          <NumberInput value={num('guardian.process.down_after_failures', 3)} onChange={(v) => set('guardian.process.down_after_failures', v)} min={1} max={100} />
        </Field>
        <Field label="Alarm push recipient (user id)" desc="Whose registered push devices get the 'MIRA is down' alarm. Leave empty and the sentinel only logs (no push). Set it to the household admin so a phone actually buzzes.">
          <TextInput value={str('guardian.process.notify_user_id', '')} onChange={(v) => set('guardian.process.notify_user_id', v)} placeholder="(user id)" mono />
        </Field>
        <Field label="Probe URL (optional)" desc="Override the liveness URL. Empty = http://127.0.0.1:<server port>/health. Set for a non-default bind or reverse proxy.">
          <TextInput value={str('guardian.process.probe_url', '')} onChange={(v) => set('guardian.process.probe_url', v)} placeholder="http://127.0.0.1:8087/health" mono />
        </Field>
      </Section>

      <Section title="Isolation autonomy">
        <p className={styles.sectionDesc}>
          Only relevant in <strong>active</strong> mode. If the Guardian detects it
          can't reach you (a channel delivery fails) and a bounded fix is clearly
          warranted, it can act on its own and reconcile with you afterward. Dry-run
          (the default) only logs + audits what it <em>would</em> do — nothing executes.
        </p>
        <Field label="Dry-run (observe only)" desc="When on, autonomous remediation under isolation is logged + audited but never executed. Turn off to permit real autonomous action.">
          <Toggle value={bool('guardian.isolation_dry_run', true)} onChange={(v) => set('guardian.isolation_dry_run', v)} />
        </Field>
        <Field label="Grace period (seconds)" desc="After a failed approval delivery, how long to wait for a web-side decision before the Guardian may act autonomously. Default 180 (3 min).">
          <NumberInput value={num('guardian.isolation_grace_secs', 180)} onChange={(v) => set('guardian.isolation_grace_secs', v)} min={0} max={3600} />
        </Field>
      </Section>
    </div>
  )
}

// ── Tools tab ─────────────────────────────────────────────────────────────────

function ToolsTab({
  set, str, num, bool, draft,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
  draft: Config
}) {
  const arrVal = (path: string): string[] => {
    const v = getPath(draft, path)
    return Array.isArray(v) ? (v as string[]) : []
  }
  const setArr = (path: string, raw: string) => {
    const items = raw.split(',').map((s) => s.trim()).filter((s) => s.length > 0)
    set(path, items)
  }

  return (
    <div className={styles.tabBody}>
      <Section title="Local tools">
        <p className={styles.sectionDesc}>
          Host-side tools with broad access. Only enable if you trust the model and have a backup.
        </p>
        <Field label="Shell" desc="Allow MIRA to run shell commands on the host machine.">
          <Toggle value={bool('agent.tools.shell.enabled')} onChange={(v) => set('agent.tools.shell.enabled', v)} />
        </Field>
        <Field label="Filesystem" desc="Allow MIRA to read and write files on the local filesystem.">
          <Toggle value={bool('agent.tools.filesystem.enabled')} onChange={(v) => set('agent.tools.filesystem.enabled', v)} />
        </Field>
      </Section>

      <Section title="web_fetch">
        <p className={styles.sectionDesc}>
          Fetch a single URL and return readable text. Routed through the shared HTTP policy (SSRF block, redirect revalidation, rate limits).
        </p>
        <Field label="Enabled" desc="Expose web_fetch to the agent.">
          <Toggle value={bool('agent.tools.web_fetch.enabled', true)} onChange={(v) => set('agent.tools.web_fetch.enabled', v)} />
        </Field>
        <Field label="Max body bytes" desc="Hard cap on bytes read from the origin before readability runs. Default 5 MiB.">
          <NumberInput value={num('agent.tools.web_fetch.max_body_bytes', 5 * 1024 * 1024)} onChange={(v) => set('agent.tools.web_fetch.max_body_bytes', v)} min={1024} step={1024} />
        </Field>
        <Field label="Max text chars" desc="Cap on characters returned to the model after readability extraction. Default 256 Ki.">
          <NumberInput value={num('agent.tools.web_fetch.max_text_chars', 256 * 1024)} onChange={(v) => set('agent.tools.web_fetch.max_text_chars', v)} min={256} step={256} />
        </Field>
        <Field label="Timeout (s)" desc="Per-request timeout in seconds.">
          <NumberInput value={num('agent.tools.web_fetch.timeout_secs', 30)} onChange={(v) => set('agent.tools.web_fetch.timeout_secs', v)} min={1} max={600} />
        </Field>
        <Field label="Max redirects" desc="How many redirects the policy will follow (each hop is re-validated).">
          <NumberInput value={num('agent.tools.web_fetch.max_redirects', 5)} onChange={(v) => set('agent.tools.web_fetch.max_redirects', v)} min={0} max={20} />
        </Field>
      </Section>

      <Section title="url_preview">
        <p className={styles.sectionDesc}>
          Pull OpenGraph / Twitter Card metadata for a URL. Only parses <code>&lt;head&gt;</code>, so the body cap can be small.
        </p>
        <Field label="Enabled" desc="Expose url_preview to the agent.">
          <Toggle value={bool('agent.tools.url_preview.enabled', true)} onChange={(v) => set('agent.tools.url_preview.enabled', v)} />
        </Field>
        <Field label="Max body bytes" desc="Hard cap on bytes downloaded when fetching <head>. Default 128 KiB.">
          <NumberInput value={num('agent.tools.url_preview.max_body_bytes', 128 * 1024)} onChange={(v) => set('agent.tools.url_preview.max_body_bytes', v)} min={1024} step={1024} />
        </Field>
      </Section>

      <Section title="web_search">
        <p className={styles.sectionDesc}>
          Federated search across DuckDuckGo (no key), Brave API, and SearXNG instances. Failover runs in the listed order.
        </p>
        <Field label="Enabled" desc="Expose web_search to the agent.">
          <Toggle value={bool('agent.tools.web_search.enabled', true)} onChange={(v) => set('agent.tools.web_search.enabled', v)} />
        </Field>
        <Field label="Default backend" desc="Backend tried first. The agent can override per-call.">
          <SelectInput
            value={str('agent.tools.web_search.default', 'ddg')}
            onChange={(v) => set('agent.tools.web_search.default', v)}
            options={[
              { value: 'ddg',     label: 'DuckDuckGo (HTML, no key)' },
              { value: 'brave',   label: 'Brave Search API' },
              { value: 'searxng', label: 'SearXNG (self-hosted)' },
            ]}
          />
        </Field>
        <Field label="Failover order" desc="Comma-separated backend list tried after the default. Unconfigured backends are skipped automatically.">
          <TextInput
            value={arrVal('agent.tools.web_search.failover').join(', ')}
            onChange={(v) => setArr('agent.tools.web_search.failover', v)}
            placeholder="ddg, brave, searxng"
            mono
          />
        </Field>
        <Field label="Top K" desc="Default number of hits returned when the agent doesn't specify top_k. Clamped to 1–20.">
          <NumberInput value={num('agent.tools.web_search.top_k', 10)} onChange={(v) => set('agent.tools.web_search.top_k', v)} min={1} max={20} />
        </Field>
      </Section>

      <Section title="Brave Search API">
        <p className={styles.sectionDesc}>
          Used by the <code>brave</code> backend. Sign up at <code>api.search.brave.com</code>. Falls back to the <code>BRAVE_SEARCH_API_KEY</code> env if left blank.
        </p>
        <Field label="API key" desc="Stored in config and sent as the X-Subscription-Token header.">
          <TextInput
            value={str('agent.tools.web_search.brave.api_key')}
            onChange={(v) => set('agent.tools.web_search.brave.api_key', v)}
            placeholder="BSA…"
            type="password"
            mono
          />
        </Field>
      </Section>

      <Section title="SearXNG">
        <p className={styles.sectionDesc}>
          Used by the <code>searxng</code> backend. The host:port of this URL is auto-added to the HTTP policy's private-IP exception.
        </p>
        <Field label="Instance URL" desc="Base URL of your SearXNG instance. Must expose the JSON format (set `search.formats: [html, json]` in SearXNG's settings.yml).">
          <TextInput
            value={str('agent.tools.web_search.searxng.url')}
            onChange={(v) => set('agent.tools.web_search.searxng.url', v)}
            placeholder="http://searxng.example.com:8080"
            mono
          />
        </Field>
      </Section>

      <Section title="HTTP policy">
        <p className={styles.sectionDesc}>
          Shared outbound policy enforced for every Tier 2 tool (web_fetch, url_preview, web_search). SSRF guards are always on; these controls layer on top.
        </p>
        <Field label="Denylist" desc="Comma-separated hosts that tools must never reach. Matches exact or suffix-at-label (e.g. example.com blocks api.example.com).">
          <TextInput
            value={arrVal('security.http.denylist').join(', ')}
            onChange={(v) => setArr('security.http.denylist', v)}
            placeholder="ads.example.com, tracker.net"
            mono
          />
        </Field>
        <Field label="Allowlist" desc="Comma-separated hosts that are allowed when 'Allowlist-only mode' is on.">
          <TextInput
            value={arrVal('security.http.allowlist').join(', ')}
            onChange={(v) => setArr('security.http.allowlist', v)}
            placeholder="wikipedia.org, news.ycombinator.com"
            mono
          />
        </Field>
        <Field label="Allowlist-only mode" desc="If enabled, only hosts on the allowlist may be reached. Enterprise-paranoid mode.">
          <Toggle value={bool('security.http.allowlist_only')} onChange={(v) => set('security.http.allowlist_only', v)} />
        </Field>
        <Field label="SearXNG exception" desc="Manual host:port exemption from the private-IP block. Leave blank — it is auto-derived from the SearXNG URL above.">
          <TextInput
            value={str('security.http.searxng_exception')}
            onChange={(v) => set('security.http.searxng_exception', v || null)}
            placeholder="auto-derived from SearXNG URL"
            mono
          />
        </Field>
      </Section>

      <Section title="Rate limits">
        <p className={styles.sectionDesc}>
          Per-user token buckets. Fetch and search have independent buckets so cheap lookups cannot starve page fetches.
        </p>
        <Field label="Fetch req / minute" desc="Global cap per user for web_fetch + url_preview.">
          <NumberInput value={num('security.http.rate.user_per_min', 60)} onChange={(v) => set('security.http.rate.user_per_min', v)} min={0} max={100000} />
        </Field>
        <Field label="Fetch req / hour" desc="Longer-window cap per user for web_fetch + url_preview.">
          <NumberInput value={num('security.http.rate.user_per_hour', 600)} onChange={(v) => set('security.http.rate.user_per_hour', v)} min={0} max={1000000} />
        </Field>
        <Field label="Per-domain / minute" desc="Cap per (user, domain) pair to keep a single domain from dominating a user's budget.">
          <NumberInput value={num('security.http.rate.user_per_domain_per_min', 10)} onChange={(v) => set('security.http.rate.user_per_domain_per_min', v)} min={0} max={100000} />
        </Field>
        <Field label="Search req / minute" desc="Independent bucket for web_search queries.">
          <NumberInput value={num('security.http.rate.search_per_min', 30)} onChange={(v) => set('security.http.rate.search_per_min', v)} min={0} max={100000} />
        </Field>
      </Section>

      <Section title="MCP servers">
        <p className={styles.sectionDesc}>
          External Model Context Protocol servers moved out of this
          tab in 0.157.0 — manage them on the dedicated{' '}
          <a href="/mcp" style={{ color: 'var(--accent-light)' }}>MCP page</a>{' '}
          in the left sidebar. Each user owns their own servers; admins
          no longer see other users' entries here.
        </p>
      </Section>
    </div>
  )
}


// ── Sandbox tab ───────────────────────────────────────────────────────────────

function SandboxTab({
  set, str, num, bool, draft,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
  draft: Config
}) {
  const sandboxOn = bool('sandbox.enabled', false)
  const arrVal = (path: string): string[] => {
    const v = getPath(draft, path)
    return Array.isArray(v) ? (v as string[]) : []
  }
  const setArr = (path: string, raw: string) => {
    const items = raw.split(',').map((s) => s.trim()).filter((s) => s.length > 0)
    set(path, items)
  }

  return (
    <div className={styles.tabBody}>
      <Section title="Tier 4 sandbox">
        <p className={styles.sectionDesc}>
          Isolated execution backend for the <code>code_run</code> tool. Works on Linux, macOS and Windows: the cross-platform <strong>WASM/WASI</strong> backend (Wasmtime + a bundled WASI CPython) auto-provisions on first use, while Linux can use the higher-fidelity <strong>namespace</strong> backend (user / mount / pid namespaces + pivoted rootfs + seccomp-bpf) when a rootfs is installed via <code>mira sandbox install python</code>. Disabled by default.
        </p>
        <Field label="Enabled" desc="Master switch. When off, no Tier 4 tool is registered regardless of per-tool toggles below.">
          <Toggle value={sandboxOn} onChange={(v) => set('sandbox.enabled', v)} />
        </Field>
        <Field label="Backend" desc="Which isolation backend runs code. 'Auto' uses Linux namespaces when a rootfs is installed, otherwise the cross-platform WASM backend. Force 'WASM' for the portable runtime, or 'Pyodide' to make scientific Python the primary for every call.">
          <SelectInput
            value={str('sandbox.backend', 'auto')}
            onChange={(v) => set('sandbox.backend', v)}
            options={[
              { value: 'auto',      label: 'Auto (namespace on Linux, else WASM)' },
              { value: 'namespace', label: 'Namespace (Linux only, needs rootfs)' },
              { value: 'wasm',      label: 'WASM/WASI (cross-platform)' },
              { value: 'pyodide',   label: 'Pyodide (scientific Python, primary)' },
            ]}
          />
        </Field>
        <Field label="Seccomp filter" desc="Which syscall filter the namespace backend installs per call. 'Allowlist' is stricter — only the syscalls a Python interpreter needs are permitted; 'denylist' blocks the known escape primitives only. (Namespace backend only; ignored by WASM/Pyodide.)">
          <SelectInput
            value={str('sandbox.seccomp_mode', 'allowlist')}
            onChange={(v) => set('sandbox.seccomp_mode', v)}
            options={[
              { value: 'allowlist', label: 'Allowlist (strict, recommended)' },
              { value: 'denylist',  label: 'Denylist (permissive)' },
            ]}
          />
        </Field>
      </Section>

      {!sandboxOn && (
        <div className={styles.inlineError} style={{ marginBottom: 16 }}>
          The sandbox master switch is off — settings below are saved but inert until you enable it above.
        </div>
      )}

      <Section title="code_run tool">
        <p className={styles.sectionDesc}>
          Runs short scripts inside the sandbox and surfaces any image files written to <code>/tmp/output/</code> as inline artifacts in chat. Both this and the master switch above must be on.
        </p>
        <Field label="Enabled" desc="Expose code_run to the agent.">
          <Toggle value={bool('sandbox.code_run.enabled', false)} onChange={(v) => set('sandbox.code_run.enabled', v)} />
        </Field>
        <Field label="Allowed languages" desc="Comma-separated list of language tags the tool will accept. Iteration B ships 'python' only.">
          <TextInput
            value={arrVal('sandbox.code_run.allowed_languages').join(', ')}
            onChange={(v) => setArr('sandbox.code_run.allowed_languages', v)}
            placeholder="python"
            mono
          />
        </Field>
        <Field label="Wall-clock limit (seconds)" desc="Hard ceiling on per-call execution time before the process is killed. Keep this short (5–30s) — long-running scripts belong on the host, not in the sandbox.">
          <NumberInput
            value={num('sandbox.code_run.max_wall_clock_seconds', 5)}
            onChange={(v) => set('sandbox.code_run.max_wall_clock_seconds', v)}
            min={1} max={300}
          />
        </Field>
        <Field label="Memory limit (MB)" desc="Per-call address-space cap (RLIMIT_AS) in megabytes.">
          <NumberInput
            value={num('sandbox.code_run.max_memory_mb', 256)}
            onChange={(v) => set('sandbox.code_run.max_memory_mb', v)}
            min={16} max={8192}
          />
        </Field>
      </Section>

      <Section title="Python rootfs">
        <p className={styles.sectionDesc}>
          The pivoted root filesystem the sandbox runs Python inside. Built with <code>mira sandbox install python</code> and stored under <code>{'<data_dir>'}/sandbox/rootfs/</code> by default.
        </p>
        <Field label="Rootfs path override" desc="Leave blank to use the default location under data_dir. Set an absolute path to point at a custom rootfs (e.g. a hand-built image with extra packages).">
          <TextInput
            value={str('sandbox.python.rootfs_path')}
            onChange={(v) => set('sandbox.python.rootfs_path', v)}
            placeholder="leave blank for default"
            mono
          />
        </Field>
      </Section>

      <Section title="Scientific Python (Pyodide)">
        <p className={styles.sectionDesc}>
          Adds <strong>numpy, pandas, matplotlib, scipy</strong> and friends via Pyodide-on-Node, with on-demand wheel loading. When enabled, scripts that <code>import</code> a scientific package route to Pyodide automatically while plain scripts stay on the lighter backend above; a chart saved to <code>/tmp/output/</code> renders inline in chat. First enable downloads the Pyodide distribution (~6&nbsp;MB) plus the pre-warm wheels in the background — available after the next restart. Note: user code runs in WebAssembly but the Node host process is privileged, so this is a weaker isolation boundary than WASM/WASI — intended for semi-trusted code.
        </p>
        <Field label="Enabled" desc="Turn on the scientific Python backend. Off by default.">
          <Toggle value={bool('sandbox.pyodide.enabled', false)} onChange={(v) => set('sandbox.pyodide.enabled', v)} />
        </Field>
        <Field label="Pre-warm packages" desc="Comma-separated packages to download into the local wheel cache at provision time so the first scientific run is offline-fast. Leave blank for the default trio (numpy, pandas, matplotlib).">
          <TextInput
            value={arrVal('sandbox.pyodide.prewarm').join(', ')}
            onChange={(v) => setArr('sandbox.pyodide.prewarm', v)}
            placeholder="numpy, pandas, matplotlib"
            mono
          />
        </Field>
      </Section>
    </div>
  )
}

// ── Channels tab ──────────────────────────────────────────────────────────────

function ChannelsTab({
  set, str, num, bool,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
}) {
  return (
    <div className={styles.tabBody}>
      <Section title="Telegram">
        <Field
          label="Enabled"
          desc="MIRA-wide kill switch. When off, all Telegram bots stop sending and inbound webhooks are refused — overrides the per-account toggles on the Channels page."
        >
          <Toggle value={bool('channels.telegram.enabled')} onChange={(v) => set('channels.telegram.enabled', v)} />
        </Field>
        <p className={styles.fieldDesc} style={{ marginTop: 8 }}>
          All per-bot settings — bot token, webhook URL, polling vs
          webhook mode, the BotFather <em>secret token</em>, and the
          per-account on/off — live on the <strong>Channels</strong>{' '}
          page in the left sidebar. The only thing this tab owns is the
          global kill switch above.
        </p>
      </Section>

      <Section title="Signal">
        <Field label="Enabled" desc="Enable the Signal channel via signal-cli REST API.">
          <Toggle value={bool('channels.signal.enabled')} onChange={(v) => set('channels.signal.enabled', v)} />
        </Field>
        <Field label="Phone number" desc="The Signal phone number linked to signal-cli (E.164 format, e.g. +1234567890).">
          <TextInput value={str('channels.signal.phone_number')} onChange={(v) => set('channels.signal.phone_number', v)} placeholder="+1234567890" mono />
        </Field>
        <Field label="REST port" desc="Port on which signal-cli's REST API is listening.">
          <NumberInput value={num('channels.signal.rest_port', 8080)} onChange={(v) => set('channels.signal.rest_port', v)} min={1024} max={65535} />
        </Field>
        <Field label="HMAC key" desc="Optional HMAC-SHA256 key to verify the X-Signal-Signature header. Leave blank to disable signature verification.">
          <TextInput value={str('channels.signal.hmac_key')} onChange={(v) => set('channels.signal.hmac_key', v)} placeholder="leave blank to disable" type="password" mono />
        </Field>
      </Section>

      <CollapsibleSection title="Signal — advanced (signal-cli plumbing)">
        <p className={styles.sectionDesc}>
          Override the signal-cli binary, its data directory, and the UNIX socket used in polling mode. Most users leave these blank to inherit MIRA's defaults.
        </p>
        <Field label="signal-cli binary" desc="Name or absolute path of the signal-cli executable. Leave as 'signal-cli' to look it up on PATH.">
          <TextInput value={str('channels.signal.cli_binary', 'signal-cli')} onChange={(v) => set('channels.signal.cli_binary', v)} placeholder="signal-cli" mono />
        </Field>
        <Field label="signal-cli data directory" desc="Where signal-cli stores account state and keys. Supports ~ expansion.">
          <TextInput value={str('channels.signal.data_dir')} onChange={(v) => set('channels.signal.data_dir', v)} placeholder="~/.local/share/signal-cli" mono />
        </Field>
        <Field label="signald socket path" desc="UNIX socket path used in polling mode. Ignored when REST mode is active.">
          <TextInput value={str('channels.signal.socket_path')} onChange={(v) => set('channels.signal.socket_path', v)} placeholder="/var/run/signald/signald.sock" mono />
        </Field>
      </CollapsibleSection>

      <Section title="Discord">
        <Field
          label="Enabled"
          desc="MIRA-wide kill switch. When off, all Discord gateway connections stop and outbound dispatch is refused — overrides the per-account toggles on the Channels page."
        >
          <Toggle value={bool('channels.discord.enabled')} onChange={(v) => set('channels.discord.enabled', v)} />
        </Field>
        <p className={styles.fieldDesc} style={{ marginTop: 8 }}>
          Per-bot settings (token, application id, mention-only, routing
          mode) live on the <strong>Channels</strong> page in the left
          sidebar. This tab owns only the global kill switch above.
        </p>
      </Section>

      <Section title="Matrix">
        <Field
          label="Enabled"
          desc="MIRA-wide kill switch. When off, all Matrix /sync loops stop and outbound dispatch is refused — overrides the per-account toggles on the Channels page."
        >
          <Toggle value={bool('channels.matrix.enabled')} onChange={(v) => set('channels.matrix.enabled', v)} />
        </Field>
        <p className={styles.fieldDesc} style={{ marginTop: 8 }}>
          Per-account settings (homeserver, access token, mention-only,
          routing mode) live on the <strong>Channels</strong> page.
        </p>
      </Section>

      <Section title="WhatsApp">
        <Field
          label="Enabled"
          desc="MIRA-wide kill switch. When off, inbound WhatsApp webhooks are dropped and outbound dispatch is refused — overrides the per-account toggles on the Channels page."
        >
          <Toggle value={bool('channels.whatsapp.enabled')} onChange={(v) => set('channels.whatsapp.enabled', v)} />
        </Field>
        <p className={styles.fieldDesc} style={{ marginTop: 8 }}>
          Per-account settings (phone number id, tokens, app secret,
          routing mode) live on the <strong>Channels</strong> page.
          Proactive messages only work within 24h of the user's last
          message — see the{' '}
          <a href="https://vexillon.ai/docs/guides/connect-a-channel" target="_blank" rel="noopener noreferrer">channel setup guide</a>.
        </p>
      </Section>

      <Section title="Slack">
        <Field
          label="Enabled"
          desc="MIRA-wide kill switch. When off, inbound Slack events are dropped and outbound dispatch is refused — overrides the per-account toggles on the Channels page."
        >
          <Toggle value={bool('channels.slack.enabled')} onChange={(v) => set('channels.slack.enabled', v)} />
        </Field>
        <p className={styles.fieldDesc} style={{ marginTop: 8 }}>
          Per-account settings (bot token, signing secret, routing mode)
          live on the <strong>Channels</strong> page.
        </p>
      </Section>

      <Section title="External plugin channels (CPP)">
        <Field
          label="Enabled"
          desc="MIRA-wide kill switch for all Channel Provider Protocol (CPP) plugin channels. When off, inbound /webhook/external requests are dropped and outbound dispatch is refused — overrides the per-account toggles on the Channels page."
        >
          <Toggle value={bool('channels.external.enabled')} onChange={(v) => set('channels.external.enabled', v)} />
        </Field>
        <p className={styles.fieldDesc} style={{ marginTop: 8 }}>
          Each external provider is added as an <strong>External
          (plugin)</strong> account on the <strong>Channels</strong> page,
          which also shows its webhook URL + the two HMAC secrets to copy
          into your provider. This is the global on/off for all of them.
          See the{' '}
          <a href="https://vexillon.ai/docs/concepts/channels" target="_blank" rel="noopener noreferrer">Channels documentation</a> to write a
          provider.
        </p>
      </Section>
    </div>
  )
}

// ── Memory tab ────────────────────────────────────────────────────────────────

function MemoryTab({
  set, str, num, bool, draft,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
  draft: Config
}) {
  const arrVal = (path: string): string[] => {
    const v = getPath(draft, path)
    return Array.isArray(v) ? (v as string[]) : []
  }
  const setArr = (path: string, raw: string) => {
    const items = raw.split(',').map((s) => s.trim()).filter((s) => s.length > 0)
    set(path, items)
  }
  // Sleep-like consolidator manual trigger (admin-only endpoint). Local state.
  // The button calls /api/admin/consolidator/run-now which runs all three
  // phases for every user regardless of the per-phase config flags below —
  // it's for *testing* what each phase would do without flipping the flag.
  const [consRunning, setConsRunning] = useState(false)
  const [consResult, setConsResult]   = useState<ConsolidatorRunResult | null>(null)
  const runConsolidator = async () => {
    setConsRunning(true)
    try {
      const r = await memoryApi.runConsolidatorNow()
      setConsResult(r)
      const total = r.contradictions_groups + r.entities_merged + r.importance_edges_scored
      if (total === 0) {
        toast.success(`Consolidator ran on ${r.users_processed} user(s) — nothing to do.`)
      } else {
        toast.success(
          `Consolidator: ${r.contradictions_groups} contradictions, ` +
          `${r.entities_merged} entities merged, ` +
          `${r.importance_edges_scored} edges scored across ${r.users_processed} user(s).`
        )
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e)
      toast.error(`Consolidator failed: ${msg}`)
    } finally {
      setConsRunning(false)
    }
  }
  return (
    <div className={styles.tabBody}>
      <Section title="Storage">
        <Field label="Vector backend" desc="Where semantic memory vectors are stored. 'sqlite' is built-in; 'qdrant' connects to an external Qdrant instance.">
          <SelectInput
            value={str('memory.vector_backend', 'sqlite')}
            onChange={(v) => set('memory.vector_backend', v)}
            options={[
              { value: 'sqlite', label: 'SQLite (built-in)' },
              { value: 'qdrant', label: 'Qdrant' },
            ]}
          />
        </Field>
        <Field label="Qdrant URL" desc="Base URL for your Qdrant instance (only used when backend is 'qdrant').">
          <TextInput value={str('memory.qdrant_url', 'http://localhost:6333')} onChange={(v) => set('memory.qdrant_url', v)} placeholder="http://localhost:6333" mono />
        </Field>
      </Section>

      <Section title="Embeddings">
        <Field label="Embedding provider" desc="Which service generates vector embeddings for semantic search.">
          <SelectInput
            value={str('memory.embedding.provider', 'internal')}
            onChange={(v) => {
              set('memory.embedding.provider', v)
              // Auto-default the model + dim when switching TO internal so
              // the user doesn't end up with an LM-Studio model id pointed
              // at fastembed (which silently falls back to BGE-small with
              // a warning). Switching AWAY from internal leaves the model
              // alone — they probably want what they had before.
              if (v === 'internal') {
                const current = str('memory.embedding.model')
                if (!isKnownFastembedModel(current)) {
                  set('memory.embedding.model', 'BGE-small-en-v1.5')
                  set('memory.embedding_dim', 384)
                }
              }
            }}
            options={[
              { value: 'internal',   label: 'Internal (fastembed, local)' },
              { value: 'ollama',     label: 'Ollama' },
              { value: 'lmstudio',   label: 'LM Studio' },
              { value: 'openai',     label: 'OpenAI' },
              { value: 'openrouter', label: 'OpenRouter' },
            ]}
          />
        </Field>
        <EmbeddingModelField
          provider={str('memory.embedding.provider', 'internal')}
          providerUrl={str('memory.embedding.provider_url')}
          apiKey={str('memory.embedding.api_key')}
          model={str('memory.embedding.model')}
          onModelChange={(id, dim) => {
            set('memory.embedding.model', id)
            if (dim !== null && dim !== undefined) set('memory.embedding_dim', dim)
          }}
        />
        <Field label="Provider URL" desc="Override the embedding endpoint (leave blank to use the provider's default URL). Not used by the internal provider — it runs in-process.">
          <TextInput
            value={str('memory.embedding.provider_url')}
            onChange={(v) => set('memory.embedding.provider_url', v)}
            placeholder={str('memory.embedding.provider', 'internal') === 'internal' ? 'not used by internal provider' : 'leave blank for default'}
            mono
            disabled={str('memory.embedding.provider', 'internal') === 'internal'}
          />
        </Field>
        <Field label="API key" desc="Required for the openai and openrouter providers. Ignored for internal/ollama/lmstudio.">
          <TextInput
            value={str('memory.embedding.api_key')}
            onChange={(v) => set('memory.embedding.api_key', v)}
            placeholder={
              ['internal','ollama','lmstudio'].includes(str('memory.embedding.provider', 'internal'))
                ? 'not used by this provider'
                : 'leave blank for keyless providers'
            }
            type="password"
            mono
            disabled={['internal','ollama','lmstudio'].includes(str('memory.embedding.provider', 'internal'))}
          />
        </Field>
        <Field label="Embedding dimensions" desc="Vector size — must match the chosen model exactly (BGE-small/MiniLM = 384, BGE-base = 768, text-embedding-3-small = 1536). Wrong values silently break similarity search.">
          <NumberInput value={num('memory.embedding_dim', 384)} onChange={(v) => set('memory.embedding_dim', v)} min={1} max={8192} />
        </Field>
        <Field label="Embedding LRU cache size" desc="How many recently-computed embedding vectors to keep in memory. Higher = fewer round-trips, more RAM.">
          <NumberInput value={num('memory.embedding_cache_size', 10000)} onChange={(v) => set('memory.embedding_cache_size', v)} min={0} max={1000000} />
        </Field>
        <Field label="Internal model cache dir" desc="Where the 'internal' provider caches downloaded fastembed model files. Leave blank for the default (~/.mira/data/embeddings/). Only used when the internal provider is selected.">
          <TextInput
            value={str('memory.embedding.model_cache_dir')}
            onChange={(v) => set('memory.embedding.model_cache_dir', v)}
            placeholder={str('memory.embedding.provider', 'internal') === 'internal' ? 'leave blank for default' : 'only used by internal provider'}
            mono
            disabled={str('memory.embedding.provider', 'internal') !== 'internal'}
          />
        </Field>
      </Section>

      <Section title="Behaviour">
        <Field label="Similarity threshold" desc="Minimum cosine similarity (0–1) for a memory to appear in semantic search results.">
          <div className={styles.rangeRow}>
            <input
              type="range"
              className={styles.range}
              min={0} max={1} step={0.01}
              value={num('memory.similarity_threshold', 0.7)}
              onChange={(e) => set('memory.similarity_threshold', Number(e.target.value))}
            />
            <span className={styles.rangeValue}>{num('memory.similarity_threshold', 0.7).toFixed(2)}</span>
          </div>
        </Field>
        <Field label="Per-user isolation" desc="Each user gets a separate memory database. Recommended for multi-user deployments.">
          <Toggle value={bool('memory.per_user_isolation', true)} onChange={(v) => set('memory.per_user_isolation', v)} />
        </Field>
        <Field label="Share across channels" desc="Allow memories created in one channel (e.g. Telegram) to surface in others (e.g. web).">
          <Toggle value={bool('memory.share_across_channels', true)} onChange={(v) => set('memory.share_across_channels', v)} />
        </Field>
        <Field label="Recency weight" desc="How much a memory's freshness boosts its recall ranking, blended with semantic similarity. 0 = rank purely by similarity (older but frequently-used facts can dominate); higher surfaces recently-formed facts. 0.25 recommended.">
          <div className={styles.rangeRow}>
            <input
              type="range"
              className={styles.range}
              min={0} max={1} step={0.05}
              value={num('memory.recency.weight', 0.25)}
              onChange={(e) => set('memory.recency.weight', Number(e.target.value))}
            />
            <span className={styles.rangeValue}>{num('memory.recency.weight', 0.25).toFixed(2)}</span>
          </div>
        </Field>
        <Field label="Recency half-life (days)" desc="How fast the freshness boost fades: a memory this old gives half the boost of a brand-new one. Larger = recency matters for longer. Only relevant when recency weight > 0.">
          <NumberInput value={num('memory.recency.half_life_days', 30)} onChange={(v) => set('memory.recency.half_life_days', v)} min={1} max={3650} />
        </Field>
      </Section>

      <CollapsibleSection title="Background indexer (advanced)">
        <p className={styles.sectionDesc}>
          Embeds historical chat messages into the vector store so semantic recall can reach past conversations. Disabling it stops new inserts but leaves existing vectors intact.
        </p>
        <Field label="Enabled" desc="Run the background transcript indexer.">
          <Toggle value={bool('memory.indexer.enabled', true)} onChange={(v) => set('memory.indexer.enabled', v)} />
        </Field>
        <Field label="Idle poll interval (seconds)" desc="Seconds the indexer waits between polls when the previous batch found nothing. Busy passes run back-to-back.">
          <NumberInput value={num('memory.indexer.interval_secs', 30)} onChange={(v) => set('memory.indexer.interval_secs', v)} min={1} max={3600} />
        </Field>
        <Field label="Batch size" desc="Maximum messages embedded per pass. Higher backfills faster on first run but stalls the embedding provider longer.">
          <NumberInput value={num('memory.indexer.batch_size', 64)} onChange={(v) => set('memory.indexer.batch_size', v)} min={1} max={1000} />
        </Field>
        <Field label="Skip roles" desc="Comma-separated message roles the indexer skips. Defaults to 'tool, system' since those messages are not conversationally meaningful.">
          <TextInput
            value={arrVal('memory.indexer.skip_roles').join(', ')}
            onChange={(v) => setArr('memory.indexer.skip_roles', v)}
            placeholder="tool, system"
            mono
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="Auto-extract (post-turn memory writes)">
        <p className={styles.sectionDesc}>
          Decides how MIRA proactively persists facts about the user after each turn. The <strong>onboarding flow</strong> drives the LLM extractor — switch this off and onboarding stops collecting structured facts. The heuristic mode keeps the bundled regex-based extractor.
        </p>
        <Field label="Mode" desc="off = no auto-extract; heuristic = bundled regex extractor; llm = structured pass through the model with confidence gating.">
          <SelectInput
            value={str('memory.auto_extract.mode', 'heuristic')}
            onChange={(v) => set('memory.auto_extract.mode', v)}
            options={[
              { value: 'off',       label: 'Off — no auto-extraction' },
              { value: 'heuristic', label: 'Heuristic (regex, free)' },
              { value: 'llm',       label: 'LLM (structured, costs tokens)' },
            ]}
          />
        </Field>
        <Field label="Minimum confidence (LLM mode)" desc="Lowest confidence tier accepted from the LLM extractor. Only applied when mode = LLM.">
          <SelectInput
            value={str('memory.auto_extract.min_confidence', 'medium')}
            onChange={(v) => set('memory.auto_extract.min_confidence', v)}
            options={[
              { value: 'low',    label: 'Low (more memories, more noise)' },
              { value: 'medium', label: 'Medium (recommended)' },
              { value: 'high',   label: 'High (only confident facts)' },
            ]}
          />
        </Field>
        <Field label="Allowed categories" desc="Comma-separated. 'relationship' is off by default because it can name third parties who haven't consented.">
          <TextInput
            value={arrVal('memory.auto_extract.allowed_categories').join(', ')}
            onChange={(v) => setArr('memory.auto_extract.allowed_categories', v)}
            placeholder="fact, preference, skill, project"
            mono
          />
        </Field>
        <Field label="LLM extractor channels" desc="Comma-separated channels that use the richer LLM extractor regardless of the mode above (unless mode = off). Channel ids: web, telegram, signal, discord, slack, matrix, whatsapp, email. Leave blank to let Mode decide for every channel. Example: 'telegram' runs the LLM extractor on Telegram while everything else stays heuristic.">
          <TextInput
            value={arrVal('memory.auto_extract.llm_channels').join(', ')}
            onChange={(v) => setArr('memory.auto_extract.llm_channels', v)}
            placeholder="(blank — Mode decides)"
            mono
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="Daily rollup">
        <p className={styles.sectionDesc}>
          Background job that consolidates each user's previous UTC day of conversation into a single summary memory tagged <code>rollup</code>. Off by default — costs one extra LLM call per active user per day.
        </p>
        <Field label="Enabled" desc="Run the daily rollup poller.">
          <Toggle value={bool('memory.rollup.enabled', false)} onChange={(v) => set('memory.rollup.enabled', v)} />
        </Field>
        <Field label="Poll interval (seconds)" desc="How often the loop wakes to check for users still missing yesterday's summary. Repeats are idempotent so they only cost a DB lookup.">
          <NumberInput value={num('memory.rollup.interval_secs', 3600)} onChange={(v) => set('memory.rollup.interval_secs', v)} min={60} max={86400} />
        </Field>
        <Field label="Day lag (days)" desc="How many UTC days back to summarise. 1 = yesterday (recommended). 0 = mid-day partial summaries.">
          <NumberInput value={num('memory.rollup.day_lag_days', 1)} onChange={(v) => set('memory.rollup.day_lag_days', v)} min={0} max={30} />
        </Field>
        <Field label="Max messages per summary" desc="Hard cap on messages fed to one summarizer call. Oldest-first truncation keeps the prompt bounded on heavy days.">
          <NumberInput value={num('memory.rollup.max_messages', 200)} onChange={(v) => set('memory.rollup.max_messages', v)} min={1} max={10000} />
        </Field>
        <Field label="Max chars per message" desc="Per-message character cap before concatenation. Long pastes rarely help a day summary.">
          <NumberInput value={num('memory.rollup.max_chars_per_message', 2000)} onChange={(v) => set('memory.rollup.max_chars_per_message', v)} min={1} max={20000} />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="Knowledge graph (experimental)">
        <p className={styles.sectionDesc}>
          Temporal knowledge-graph memory — facts stored as typed, timestamped triples (subject → predicate → object|value) so aggregation/counting questions ("how many plants?", "total bike spend?") resolve against exact set membership instead of fuzzy top-k retrieval. See <a href="https://vexillon.ai/docs/concepts/memory-and-wiki" target="_blank" rel="noopener noreferrer">Memory &amp; the wiki</a>. Adds an extra LLM call per turn for triple extraction when on. Off by default.
        </p>
        <Field label="Enabled" desc="Turn on the graph layer (extraction + retrieval). Required for any of the consolidation passes below to do anything.">
          <Toggle value={bool('memory.graph.enabled', false)} onChange={(v) => set('memory.graph.enabled', v)} />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="Sleep-like consolidation">
        <p className={styles.sectionDesc}>
          Phased nightly clean-up of the knowledge graph — deterministic, MIRA-side, no LLM-as-policy. Runs per active user inside the existing daily memory.rollup tick. Each phase independently togglable. All require <strong>Knowledge graph (above) enabled</strong>; otherwise they're no-ops. See <a href="https://vexillon.ai/docs/concepts/memory-and-wiki" target="_blank" rel="noopener noreferrer">Memory &amp; the wiki</a>.
        </p>
        <Field label="Contradictions (Phase C)" desc="Resolve single-valued-predicate contradictions (works_at, lives_in, married_to, …). When multiple live edges share (subject, predicate), keep the newest and close older edges' valid_to. Pure SQL + curated predicate list. Audit-preserving — older facts are time-bounded, not deleted.">
          <Toggle value={bool('memory.consolidation.contradictions_enabled', false)} onChange={(v) => set('memory.consolidation.contradictions_enabled', v)} />
        </Field>
        <Field label="Entity dedup (Phase A)" desc="Merge near-duplicate entities of the same entity_type via strict-token-subset rule (e.g. 'navy blazer' / 'navy blue blazer'). Re-points edges to the winner, rolls loser name into aliases. Runs after Phase C so the 'more edges wins' tiebreak sees post-resolution counts.">
          <Toggle value={bool('memory.consolidation.entity_dedup_enabled', false)} onChange={(v) => set('memory.consolidation.entity_dedup_enabled', v)} />
        </Field>
        <Field label="Entity dedup ratio" desc="Size-ratio threshold for Phase A merges (|smaller| / |larger| token counts). 0.6 default catches {navy, blazer} ⊂ {navy, blue, blazer} (2/3) while rejecting {plant} ⊂ {peace, lily, plant} (1/3). Higher = stricter; lower = more aggressive.">
          <NumberInput value={num('memory.consolidation.entity_dedup_ratio', 0.6)} onChange={(v) => set('memory.consolidation.entity_dedup_ratio', v)} min={0.0} max={1.0} step={0.05} />
        </Field>
        <Field label="Importance scoring (Phase D)" desc="Score every live edge nightly as ln(1 + access_count) × exp(-age_days / half_life). Retrieval already orders by importance DESC — enabling biases context toward frequently-reinforced + recent facts. Access tracking on retrieval is always-on and free; only the scoring pass is gated. Runs LAST in the nightly tick.">
          <Toggle value={bool('memory.consolidation.importance_enabled', false)} onChange={(v) => set('memory.consolidation.importance_enabled', v)} />
        </Field>
        <Field label="Importance half-life (days)" desc="After this many days of no reinforcement, an edge's score decays to ~50% of un-decayed strength. 30 default (month-scale, matches typical personal context drift).">
          <NumberInput value={num('memory.consolidation.importance_half_life_days', 30)} onChange={(v) => set('memory.consolidation.importance_half_life_days', v)} min={1} max={365} step={1} />
        </Field>
        <Field
          label="Run consolidator now"
          desc="Trigger all three phases (C → A → D) immediately for every user, regardless of the toggles above. Use this to TEST what each phase would do on your real graph without waiting for the hourly rollup tick or enabling the per-phase flags. Reads the ratio + half-life from the saved config — save your changes first if you've adjusted them."
        >
          <div style={{ display: 'flex', gap: 'var(--space-2)', alignItems: 'center', flexWrap: 'wrap' }}>
            <button
              type="button"
              className={styles.button}
              onClick={runConsolidator}
              disabled={consRunning}
            >
              {consRunning ? 'Running…' : 'Run now'}
            </button>
            {consResult && (
              <span style={{ color: 'var(--color-text-muted)', fontSize: 'var(--font-size-sm)' }}>
                Last run: {consResult.users_processed} user(s) ·{' '}
                {consResult.contradictions_groups} contradictions ({consResult.contradictions_edges_closed} edges) ·{' '}
                {consResult.entities_merged} entities merged ({consResult.entity_edges_repointed} edges re-pointed) ·{' '}
                {consResult.importance_edges_scored} edges scored
              </span>
            )}
          </div>
        </Field>
      </CollapsibleSection>
    </div>
  )
}

// ── EmbeddingModelField — combobox backed by /api/admin/embedding-models ─────
//
// fastembed accepts only ~5 model names (anything else falls back to
// BGE-small with a warning), and each has a known dim — dropdown.
//
// HTTP providers expose /v1/models (or /api/tags for Ollama). We
// fetch the list, filter for entries that look like embedding
// models, and render a `<datalist>`-backed combobox so the user can
// pick one OR type their own (some private deployments expose models
// under non-obvious names that won't pass the heuristic filter).

interface EmbeddingModelEntry {
  id:     string
  dim:    number | null
  source: 'hardcoded' | 'upstream'
}

interface EmbeddingModelsResponse {
  provider: string
  models:   EmbeddingModelEntry[]
  error:    string | null
}

// Exact set of canonical fastembed identifiers (normalised: lowercased,
// punctuation stripped). Anything else — even an LM Studio model id
// that happens to contain "minilml12" as a substring — should be
// treated as foreign so the auto-default kicks in on a switch to
// internal. fastembed itself is fuzzy and would coerce non-matches to
// BGE-small with a warning, but that surprises the user when the
// visible config no longer matches what's actually running.
const FASTEMBED_CANONICAL = new Set([
  'bgesmallenv15',
  'bgebaseenv15',
  'allminilml6v2',
  'allminilml12v2',
  'nomicembedtextv15',
])
function isKnownFastembedModel(name: string): boolean {
  const fp = name.toLowerCase().replace(/[-_. ]/g, '')
  return FASTEMBED_CANONICAL.has(fp)
}

function EmbeddingModelField({
  provider,
  providerUrl,
  apiKey,
  model,
  onModelChange,
}: {
  provider:      string
  providerUrl:   string
  apiKey:        string
  model:         string
  onModelChange: (id: string, dim: number | null) => void
}) {
  // Don't fetch openrouter (no embeddings) or when the form is empty.
  // Internal returns a hardcoded list — still hit the endpoint so the
  // dim mapping comes from one place.
  const enabled = provider !== '' && provider !== 'openrouter'

  const { data, isLoading } = useQuery<EmbeddingModelsResponse>({
    queryKey: ['embedding-models', provider, providerUrl, apiKey],
    queryFn: () => api.get<EmbeddingModelsResponse>('/api/admin/embedding-models', {
      params: {
        provider,
        // Send empty params as `undefined` so axios omits them and
        // the server's `Option<String>` resolves to None (lets the
        // server fall back to LiveConfig defaults for `url`).
        url:     providerUrl || undefined,
        api_key: apiKey || undefined,
      },
    }).then((r) => r.data),
    enabled,
    staleTime: 30_000,
    retry:     false,
  })

  const handleSelect = (id: string) => {
    const match = data?.models.find((m) => m.id === id)
    onModelChange(id, match?.dim ?? null)
  }

  const helpDesc =
    provider === 'internal'
      ? 'Pick a fastembed model. Each ships with a fixed vector dimension — picking one auto-sets the dimension field.'
      : provider === 'openrouter'
      ? "OpenRouter doesn't proxy embeddings — pick a different provider."
      : 'Choose from the provider\'s available embedding models, or type a custom name. The dropdown is filtered by name heuristic; type freely if your model isn\'t listed.'

  // Internal is a closed set — disable free typing so the user can't
  // pick a value fastembed will silently coerce to BGE-small.
  const allowFreeText = provider !== 'internal'

  return (
    <Field label="Embedding model" desc={helpDesc}>
      <Combobox
        value={model}
        onChange={handleSelect}
        options={data?.models ?? []}
        placeholder={
          provider === 'openrouter'
            ? '— OpenRouter has no embeddings —'
            : isLoading
            ? 'Loading models…'
            : provider === 'internal'
            ? 'Pick a fastembed model'
            : 'Pick or type a model name'
        }
        allowFreeText={allowFreeText}
        disabled={provider === 'openrouter'}
      />
      {data?.error && (
        <div style={{ marginTop: 6, fontSize: 12, color: 'var(--text-muted)' }}>
          {data.error}{allowFreeText && ' You can still type a model name manually.'}
        </div>
      )}
      {!data?.error && data && data.models.length === 0 && enabled && !isLoading && allowFreeText && (
        <div style={{ marginTop: 6, fontSize: 12, color: 'var(--text-muted)' }}>
          No embedding models found at the configured URL. Check the
          provider URL or type the model name manually.
        </div>
      )}
    </Field>
  )
}

// ── Combobox — click-to-open dropdown with optional free typing ─────────────
//
// `<input list>` only opens on focus when the field is empty in most
// browsers, which surprises users with a populated value (looks like
// the dropdown is broken). This component:
// - opens on click of the input or the chevron button
// - filters options as the user types (case-insensitive substring)
// - selects on click of an option, on Enter while focused, or on
//   blur when allowFreeText is true (the typed value is committed)
// - closes on outside click or Escape
// - when `allowFreeText=false`, typing still filters but the only
//   accepted commit is picking from the list (used for fastembed,
//   where the upstream is a closed set)
function Combobox({
  value,
  onChange,
  options,
  placeholder,
  allowFreeText,
  disabled,
}: {
  value:         string
  onChange:      (id: string) => void
  options:       Array<{ id: string; dim: number | null }>
  placeholder:   string
  allowFreeText: boolean
  disabled:      boolean
}) {
  const [open, setOpen]     = useState(false)
  const [draft, setDraft]   = useState(value)
  const wrapRef             = useRef<HTMLDivElement>(null)

  // Sync `draft` when value changes from the outside (e.g. provider
  // switch auto-defaulted the model). Without this the input would
  // stay on the user's last keystroke.
  useEffect(() => { setDraft(value) }, [value])

  // Outside-click / Escape close the popover.
  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) {
        setOpen(false)
        if (allowFreeText && draft !== value) onChange(draft)
      }
    }
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setOpen(false) }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [open, draft, value, onChange, allowFreeText])

  const filtered = options.filter((o) => {
    if (!draft) return true
    return o.id.toLowerCase().includes(draft.toLowerCase())
  })

  const pick = (id: string, dim: number | null) => {
    setDraft(id)
    onChange(id)
    void dim // dim is propagated by the parent's onChange via the options array
    setOpen(false)
  }

  return (
    <div ref={wrapRef} style={{ position: 'relative', width: '100%' }}>
      <div style={{ display: 'flex', alignItems: 'stretch', width: '100%' }}>
        <input
          type="text"
          value={draft}
          placeholder={placeholder}
          spellCheck={false}
          disabled={disabled}
          readOnly={!allowFreeText && !disabled}
          onChange={(e) => {
            if (!allowFreeText) return
            setDraft(e.target.value)
            if (!open) setOpen(true)
          }}
          onFocus={() => { if (!disabled) setOpen(true) }}
          onClick={() => { if (!disabled) setOpen(true) }}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault()
              if (filtered.length > 0) {
                pick(filtered[0].id, filtered[0].dim)
              } else if (allowFreeText) {
                onChange(draft)
                setOpen(false)
              }
            }
          }}
          style={{
            flex: 1,
            padding: '6px 10px',
            fontFamily: 'var(--font-mono, monospace)',
            fontSize: 13,
            background: 'var(--bg-input)',
            border: '1px solid var(--border)',
            borderRight: 'none',
            borderRadius: 'var(--radius-sm) 0 0 var(--radius-sm)',
            color: 'var(--text-primary)',
            opacity: disabled ? 0.55 : 1,
            cursor: disabled ? 'not-allowed' : (allowFreeText ? 'text' : 'pointer'),
          }}
        />
        <button
          type="button"
          aria-label="Toggle list"
          tabIndex={-1}
          disabled={disabled}
          onClick={(e) => { e.preventDefault(); if (!disabled) setOpen((o) => !o) }}
          style={{
            padding: '0 8px',
            background: 'var(--bg-input)',
            border: '1px solid var(--border)',
            borderRadius: '0 var(--radius-sm) var(--radius-sm) 0',
            color: 'var(--text-muted)',
            cursor: disabled ? 'not-allowed' : 'pointer',
            display: 'flex',
            alignItems: 'center',
          }}
        >
          <ChevronDown size={14} />
        </button>
      </div>
      {open && !disabled && (
        <div
          role="listbox"
          style={{
            position: 'absolute',
            top: '100%', left: 0, right: 0,
            marginTop: 4,
            maxHeight: 240,
            overflowY: 'auto',
            background: 'var(--bg-surface)',
            border: '1px solid var(--border)',
            borderRadius: 'var(--radius-sm)',
            boxShadow: '0 8px 24px rgba(0,0,0,0.25)',
            zIndex: 50,
          }}
        >
          {filtered.length === 0 ? (
            <div style={{ padding: '8px 12px', fontSize: 13, color: 'var(--text-muted)' }}>
              {allowFreeText ? 'No matches — press Enter to use what you typed.' : 'No matches.'}
            </div>
          ) : filtered.map((o) => (
            <button
              key={o.id}
              type="button"
              role="option"
              aria-selected={o.id === value}
              onClick={() => pick(o.id, o.dim)}
              style={{
                display: 'flex', justifyContent: 'space-between', alignItems: 'center',
                width: '100%', textAlign: 'left',
                padding: '6px 12px',
                background: o.id === value ? 'var(--bg-overlay)' : 'transparent',
                border: 'none',
                color: 'var(--text-primary)',
                cursor: 'pointer',
                fontFamily: 'var(--font-mono, monospace)',
                fontSize: 13,
              }}
              onMouseEnter={(e) => { e.currentTarget.style.background = 'var(--bg-overlay)' }}
              onMouseLeave={(e) => { e.currentTarget.style.background = o.id === value ? 'var(--bg-overlay)' : 'transparent' }}
            >
              <span>{o.id}</span>
              {o.dim != null && (
                <span style={{ color: 'var(--text-muted)', fontSize: 11, marginLeft: 12 }}>
                  {o.dim} dim
                </span>
              )}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

// ── Server tab ────────────────────────────────────────────────────────────────

function ServerTab({
  set, str, num, bool, draft,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
  draft: Config
}) {
  const arrVal = (path: string): string[] => {
    const v = getPath(draft, path)
    return Array.isArray(v) ? (v as string[]) : []
  }
  const setArr = (path: string, raw: string) => {
    const items = raw.split(',').map((s) => s.trim()).filter((s) => s.length > 0)
    set(path, items)
  }
  return (
    <div className={styles.tabBody}>
      <Section title="HTTP server">
        <Field label="Enabled" desc="Run the built-in HTTP server for the web UI and API.">
          <Toggle value={bool('server.enabled')} onChange={(v) => set('server.enabled', v)} />
        </Field>
        <Field label="Host" desc="Address the server binds to. Use '0.0.0.0' to accept external connections.">
          <TextInput value={str('server.host', '127.0.0.1')} onChange={(v) => set('server.host', v)} placeholder="127.0.0.1" mono />
        </Field>
        <Field label="Port" desc="TCP port the server listens on.">
          <NumberInput value={num('server.port', 3000)} onChange={(v) => set('server.port', v)} min={1024} max={65535} />
        </Field>
        <Field label="Max connections" desc="Maximum number of simultaneous client connections.">
          <NumberInput value={num('server.max_connections', 100)} onChange={(v) => set('server.max_connections', v)} min={1} max={10000} />
        </Field>
        <Field label="Request timeout (s)" desc="Maximum seconds to wait for a request to complete before returning an error.">
          <NumberInput value={num('server.request_timeout_secs', 60)} onChange={(v) => set('server.request_timeout_secs', v)} min={5} max={600} />
        </Field>
      </Section>

      <Section title="TLS">
        <p className={styles.sectionDesc}>
          Direct HTTPS for the built-in server. Leave blank if you terminate TLS at nginx (see Reverse proxy below) or run behind another reverse proxy.
        </p>
        <Field label="Certificate path" desc="PEM-encoded certificate file. Supports ~ expansion.">
          <TextInput value={str('server.tls_cert_path')} onChange={(v) => set('server.tls_cert_path', v || null)} placeholder="leave blank for plain HTTP" mono />
        </Field>
        <Field label="Key path" desc="PEM-encoded private key. Required when certificate path is set.">
          <TextInput value={str('server.tls_key_path')} onChange={(v) => set('server.tls_key_path', v || null)} placeholder="leave blank for plain HTTP" mono />
        </Field>
      </Section>

      <Section title="Auth & sessions">
        <Field label="Static auth token" desc="Bearer token accepted on all API requests in addition to JWT. Leave blank to disable static-token auth (JWT still works).">
          <TextInput value={str('server.auth_token')} onChange={(v) => set('server.auth_token', v)} placeholder="leave blank to disable" type="password" mono />
        </Field>
        <Field label="JWT signing secret" desc="HS256 secret used to sign and verify access tokens. Auto-generated on first run if blank — set explicitly only when you need stable tokens across restarts (multi-node deployments).">
          <TextInput value={str('security.jwt_secret')} onChange={(v) => set('security.jwt_secret', v)} placeholder="auto-generated" type="password" mono />
        </Field>
        <Field label="Session lifetime (days)" desc="How long a refresh token stays valid before the user has to log in again.">
          <NumberInput value={num('security.session_days', 7)} onChange={(v) => set('security.session_days', v)} min={1} max={365} />
        </Field>
        <Field label="Webhook secret" desc="Secret used to sign outgoing webhook payloads.">
          <TextInput value={str('server.webhook_secret')} onChange={(v) => set('server.webhook_secret', v)} placeholder="optional" type="password" mono />
        </Field>
      </Section>

      <Section title="CORS & access">
        <Field label="Allowed origins" desc="Comma-separated CORS origin list. Use '*' to allow all (not recommended for public servers). Leave blank to deny all cross-origin requests.">
          <TextInput
            value={arrVal('server.allowed_origins').join(', ')}
            onChange={(v) => setArr('server.allowed_origins', v)}
            placeholder="https://example.com, https://other.com"
            mono
          />
        </Field>
        <Field label="Inbound rate limit (req/min)" desc="Per-IP request budget enforced by the security middleware. 0 disables.">
          <NumberInput value={num('security.rate_limit_rpm', 60)} onChange={(v) => set('security.rate_limit_rpm', v)} min={0} max={100000} />
        </Field>
        <Field label="Blocked IPs" desc="Comma-separated IP addresses that are rejected before any other check.">
          <TextInput
            value={arrVal('security.blocked_ips').join(', ')}
            onChange={(v) => setArr('security.blocked_ips', v)}
            placeholder="1.2.3.4, 5.6.7.8"
            mono
          />
        </Field>
      </Section>

      <Section title="Session lifecycle">
        <p className={styles.sectionDesc}>
          Conversation session storage — controls how long an inactive session is kept and how many turns are retained.
        </p>
        <Field label="Inactivity timeout (seconds)" desc="Sessions with no activity beyond this point are eligible for cleanup.">
          <NumberInput value={num('session.timeout_secs', 3600)} onChange={(v) => set('session.timeout_secs', v)} min={60} max={604800} />
        </Field>
        <Field label="Cleanup sweep interval (seconds)" desc="How often the background sweep removes expired sessions.">
          <NumberInput value={num('session.cleanup_interval_secs', 300)} onChange={(v) => set('session.cleanup_interval_secs', v)} min={10} max={86400} />
        </Field>
        <Field label="Max retained turns" desc="Per-session cap on stored conversation turns. Older turns are dropped past this.">
          <NumberInput value={num('session.max_turns', 100)} onChange={(v) => set('session.max_turns', v)} min={1} max={10000} />
        </Field>
      </Section>

      <Section title="Backup & Restore">
        <BackupRestore />
      </Section>

      <Section title="Hosted-MIRA waitlist">
        <WaitlistPanel />
      </Section>

      <CollapsibleSection title="Reverse proxy (nginx)">
        <p className={styles.sectionDesc}>
          When enabled, MIRA generates an <code>nginx.conf</code>, binds its own HTTP server to <code>127.0.0.1</code>, and manages nginx as a subprocess for TLS termination and WebSocket fan-out. Most users leave this off and run nginx (or another proxy) by hand.
        </p>
        <Field label="Enabled" desc="Manage an nginx subprocess. Requires nginx installed on the host.">
          <Toggle value={bool('proxy.enabled')} onChange={(v) => set('proxy.enabled', v)} />
        </Field>
        <Field label="nginx binary" desc="Full path to the nginx executable (e.g. /usr/sbin/nginx).">
          <TextInput value={str('proxy.nginx_binary', '/usr/sbin/nginx')} onChange={(v) => set('proxy.nginx_binary', v)} placeholder="/usr/sbin/nginx" mono />
        </Field>
        <Field label="Generated config path" desc="Where MIRA writes the generated nginx.conf. Parent directory is created if absent. Supports ~ expansion.">
          <TextInput value={str('proxy.config_path')} onChange={(v) => set('proxy.config_path', v)} placeholder="~/.mira/nginx.conf" mono />
        </Field>
        <Field label="PID file" desc="nginx PID file path. Used to detect a running nginx and send reload signals.">
          <TextInput value={str('proxy.pid_path')} onChange={(v) => set('proxy.pid_path', v)} placeholder="~/.mira/nginx.pid" mono />
        </Field>
        <Field label="Worker processes" desc="nginx worker_processes directive. 'auto' lets nginx pick based on CPU cores.">
          <TextInput value={str('proxy.worker_processes', 'auto')} onChange={(v) => set('proxy.worker_processes', v)} placeholder="auto" mono />
        </Field>
        <Field label="WebSocket support" desc="Add proxy config for /api/v1/stream so the browser streaming client works.">
          <Toggle value={bool('proxy.websocket_support', true)} onChange={(v) => set('proxy.websocket_support', v)} />
        </Field>
        <Field label="TLS at nginx" desc="Let nginx terminate TLS. Recommended over server.tls_* when nginx is in front.">
          <Toggle value={bool('proxy.tls.enabled')} onChange={(v) => set('proxy.tls.enabled', v)} />
        </Field>
        <Field label="TLS cert path" desc="PEM certificate served by nginx. Required when TLS at nginx is on.">
          <TextInput value={str('proxy.tls.cert_path')} onChange={(v) => set('proxy.tls.cert_path', v)} placeholder="/etc/letsencrypt/live/example.com/fullchain.pem" mono />
        </Field>
        <Field label="TLS key path" desc="PEM private key served by nginx.">
          <TextInput value={str('proxy.tls.key_path')} onChange={(v) => set('proxy.tls.key_path', v)} placeholder="/etc/letsencrypt/live/example.com/privkey.pem" mono />
        </Field>
        <Field label="HTTPS port" desc="Port nginx listens on for HTTPS.">
          <NumberInput value={num('proxy.tls.listen_port', 443)} onChange={(v) => set('proxy.tls.listen_port', v)} min={1} max={65535} />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="System email (application-initiated outbound)" defaultOpen={false}>
        <p className={styles.sectionDesc}>
          Used by application features that send mail as MIRA itself —
          password reset, admin alerts, waitlist confirmations.
          Distinct from per-user email accounts. Password auth only
          for now (any transactional SMTP relay works: Postmark,
          SendGrid, AWS SES, Fastmail, etc.).
        </p>
        <Field label="Enabled" desc="Off = system_email sends error out cleanly. Turn on once configured.">
          <Toggle value={bool('system_email.enabled')} onChange={(v) => set('system_email.enabled', v)} />
        </Field>
        <Field label="From address" desc="Address the From header carries.">
          <TextInput value={str('system_email.from_address')} onChange={(v) => set('system_email.from_address', v)} placeholder="mira@example.com" mono />
        </Field>
        <Field label="From name" desc="Display name in the From header. Defaults to 'MIRA' when empty.">
          <TextInput value={str('system_email.from_name')} onChange={(v) => set('system_email.from_name', v)} placeholder="MIRA" />
        </Field>
        <Field label="SMTP host" desc="e.g. smtp.postmarkapp.com / email-smtp.us-east-1.amazonaws.com">
          <TextInput value={str('system_email.smtp_host')} onChange={(v) => set('system_email.smtp_host', v)} placeholder="smtp.example.com" mono />
        </Field>
        <Field label="SMTP port" desc="465 → implicit TLS. 587 → STARTTLS. Plaintext rejected.">
          <NumberInput value={num('system_email.smtp_port', 465)} onChange={(v) => set('system_email.smtp_port', v)} min={1} max={65535} />
        </Field>
        <Field label="Use TLS" desc="Required.">
          <Toggle value={bool('system_email.smtp_use_tls', true)} onChange={(v) => set('system_email.smtp_use_tls', v)} />
        </Field>
        <Field label="SMTP username" desc="Often the API key id for transactional providers.">
          <TextInput value={str('system_email.smtp_username')} onChange={(v) => set('system_email.smtp_username', v)} placeholder="apikey / username" mono />
        </Field>
        <Field label="SMTP password" desc="Save changes after editing; field stays blank on next load.">
          <TextInput value={str('system_email.smtp_password')} onChange={(v) => set('system_email.smtp_password', v)} type="password" placeholder="•••••" mono />
        </Field>
      </CollapsibleSection>

      <Section title="Remote access (reach MIRA from away)">
        <p className={styles.sectionDesc}>
          Reach your server from the mobile app when you're away from home — without opening
          router ports. <strong>Tailscale</strong> is the recommended path: install it on this
          host and your phone once, and MIRA auto-detects the tunnel URL and bakes it into the
          pairing QR. Or set an explicit URL below (Cloudflare Tunnel, DDNS, reverse proxy). This
          opens no ports — it's detection + configuration + a link in the QR.
        </p>
        <Field
          label="Remote URL (override)"
          desc="Externally-reachable base URL (e.g. https://mira.my-tailnet.ts.net or a Cloudflare Tunnel / DDNS host). Leave blank to auto-detect Tailscale. Must be an absolute http/https URL."
        >
          <TextInput
            value={str('server.remote_url')}
            onChange={(v) => set('server.remote_url', v || null)}
            placeholder="auto-detect (Tailscale) — or https://mira.example.com"
            mono
          />
        </Field>
        <RemoteAccessCard />
      </Section>

      <Section title="Web apps (built games & tools)">
        <p className={styles.sectionDesc}>
          When the coding agent builds something runnable (a game, a small web tool), MIRA can
          serve it at its own clickable link so you can open it — instead of the assistant
          claiming it opened a tab it can't. Each app gets its own browser origin, isolated from
          the MIRA app itself. Changing the mode/port needs a restart to take effect.
        </p>
        <Field label="Serve built web apps" desc="Master switch. When off, MIRA still tells you where a built app lives on disk, but gives no link.">
          <Toggle value={bool('server.web_apps.enabled', true)} onChange={(v) => set('server.web_apps.enabled', v)} />
        </Field>
        <Field label="Mode" desc="How apps are reached — pick by how your browser reaches MIRA (the server can't auto-detect it). Subdomain: <task>.<suffix> — most isolated, works same-machine or WSL via localhost. Port: a separate listener, reachable over a LAN / WSL-gateway IP. Both: serve both, subdomain link primary.">
          <SelectInput
            value={str('server.web_apps.mode', 'subdomain')}
            onChange={(v) => set('server.web_apps.mode', v)}
            options={[
              { value: 'subdomain', label: 'Subdomain — <task>.<suffix> (most isolated, same machine / WSL-localhost)' },
              { value: 'port',      label: 'Port — separate listener (LAN / WSL-gateway IP)' },
              { value: 'both',      label: 'Both (subdomain link primary)' },
            ]}
          />
        </Field>
        <Field label="Host suffix" desc="Subdomain / Both mode. App served at http://<task_id>.<suffix>:<port>/. 'localhost' resolves to loopback in every browser with no extra port; only works when the browser reaches MIRA's box via that name.">
          <TextInput value={str('server.web_apps.host_suffix', 'localhost')} onChange={(v) => set('server.web_apps.host_suffix', v)} placeholder="localhost" mono />
        </Field>
        <Field label="Port-mode listener port" desc="Port / Both mode. 0 = server port + 1. The separate listener serves apps at http://<host>:<this-port>/a/<task_id>/.">
          <NumberInput value={num('server.web_apps.port', 0)} onChange={(v) => set('server.web_apps.port', v)} min={0} max={65535} />
        </Field>
        <Field label="Advertised host" desc="Port / Both mode. Host put in the returned URL — e.g. a LAN or WSL-gateway IP like 172.22.240.1. Leave blank to derive from public base URL, then the server host.">
          <TextInput value={str('server.web_apps.advertised_host')} onChange={(v) => set('server.web_apps.advertised_host', v || null)} placeholder="auto-derived" mono />
        </Field>
      </Section>
    </div>
  )
}

// ── Calendar tab ──────────────────────────────────────────────────────────────

function CalendarTab({
  set, str, num, bool, isAdmin,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
  isAdmin: boolean
}) {
  const provider = str('calendar.sync_provider', 'none')
  const enabled  = bool('calendar.enabled', true)

  // Per-user account linking (Connect Google/Outlook) lives on the Calendar page
  // now, since it's per-user and the whole Settings page is admin-only. This tab
  // is just the instance-level setup: enable, which provider, its credentials.
  const syncMut = useMutation({
    mutationFn: () => calendarApi.triggerSync(),
    onSuccess:  (r) => toast.success(`Synced ${r.pulled} event${r.pulled === 1 ? '' : 's'} from ${r.provider}.`),
    onError:    (e: unknown) => {
      const d = (e as { response?: { data?: string } })?.response?.data
      toast.error(d ? String(d) : 'Sync failed')
    },
  })

  return (
    <div className={styles.tabBody}>
      <Section title="Native calendar">
        <p className={styles.sectionDesc}>
          MIRA-native events are always available and stored locally. External sync mirrors remote events as read-only rows — you can edit and delete only native events from the UI.
        </p>
        <Field label="Enabled" desc="Enable the MIRA-native calendar store and the four agent tools (list/create/update/delete).">
          <Toggle value={enabled} onChange={(v) => set('calendar.enabled', v)} />
        </Field>
      </Section>

      <Section title="External sync">
        <Field label="Sync provider" desc="Which external calendar to mirror into MIRA. Leave as 'None' to keep MIRA self-contained.">
          <SelectInput
            value={provider}
            onChange={(v) => set('calendar.sync_provider', v)}
            options={[
              { value: 'none',    label: 'None (MIRA-only)' },
              { value: 'caldav',  label: 'CalDAV' },
              { value: 'google',  label: 'Google Calendar' },
              { value: 'outlook', label: 'Outlook / Microsoft 365' },
            ]}
          />
        </Field>
        <Field label="Sync interval (minutes)" desc="How often the background engine polls the external source. Minimum 5 minutes.">
          <NumberInput
            value={num('calendar.sync_interval_mins', 15)}
            onChange={(v) => set('calendar.sync_interval_mins', v)}
            min={5}
            max={1440}
          />
        </Field>
        {isAdmin && provider !== 'none' && (
          <Field label="Sync now" desc="Run one pull immediately against the current provider. Admin only.">
            <button
              className={styles.input}
              style={{ display: 'inline-flex', alignItems: 'center', gap: 6, cursor: 'pointer', width: 'auto' }}
              onClick={() => syncMut.mutate()}
              disabled={syncMut.isPending}
            >
              <RefreshCw size={14} />
              {syncMut.isPending ? 'Syncing…' : 'Run sync'}
            </button>
          </Field>
        )}
      </Section>

      {provider === 'caldav' && (
        <Section title="CalDAV">
          <p className={styles.sectionDesc}>
            Works with iCloud, Fastmail, Nextcloud, Radicale, and any RFC 4791 server.
            CalDAV is <strong>per-user</strong>: each user connects their own account
            (server URL + username + app password) from their own <strong>Calendar</strong>
            page (left nav). Their password is encrypted at rest — there's nothing
            to enter here. Just leave the provider set to CalDAV and tell users to
            connect from their Calendar page.
          </p>
        </Section>
      )}

      {provider === 'google' && (
        <Section title="Google Calendar">
          <p className={styles.sectionDesc}>
            Register an OAuth client at <code>console.cloud.google.com</code>. Add the redirect URI below verbatim, then click Connect below to authorise.
          </p>
          <Field label="Client ID" desc="OAuth 2.0 Client ID from the Google Cloud console.">
            <TextInput
              value={str('calendar.google.client_id')}
              onChange={(v) => set('calendar.google.client_id', v)}
              placeholder="xxxx.apps.googleusercontent.com"
              mono
            />
          </Field>
          <Field label="Client secret" desc="OAuth 2.0 Client secret. Stored in the config file.">
            <TextInput
              value={str('calendar.google.client_secret')}
              onChange={(v) => set('calendar.google.client_secret', v)}
              placeholder="GOCSPX-…"
              type="password"
              mono
            />
          </Field>
          <Field label="Redirect URI" desc="Must match an authorised redirect URI in the Google client exactly. For most setups: https://yourdomain/api/calendar/oauth/callback">
            <TextInput
              value={str('calendar.google.redirect_uri')}
              onChange={(v) => set('calendar.google.redirect_uri', v)}
              placeholder="https://yourdomain/api/calendar/oauth/callback"
              mono
            />
          </Field>
          <p className={styles.sectionDesc}>
            Once this is set up, each user links their own Google account from their
            own <strong>Calendar</strong> page (left nav) — there's nothing per-user to do here.
          </p>
        </Section>
      )}

      {provider === 'outlook' && (
        <Section title="Outlook / Microsoft 365">
          <p className={styles.sectionDesc}>
            Register an app at <code>portal.azure.com</code> (Azure AD → App registrations). Add the redirect URI below, then click Connect to authorise.
          </p>
          <Field label="Client ID" desc="Application (client) ID from the Azure app registration.">
            <TextInput
              value={str('calendar.outlook.client_id')}
              onChange={(v) => set('calendar.outlook.client_id', v)}
              placeholder="00000000-0000-0000-0000-000000000000"
              mono
            />
          </Field>
          <Field label="Client secret" desc="Client secret value (not ID). Stored in the config file.">
            <TextInput
              value={str('calendar.outlook.client_secret')}
              onChange={(v) => set('calendar.outlook.client_secret', v)}
              placeholder="secret value"
              type="password"
              mono
            />
          </Field>
          <Field label="Redirect URI" desc="Must match a registered redirect URI in the Azure app. For most setups: https://yourdomain/api/calendar/oauth/callback">
            <TextInput
              value={str('calendar.outlook.redirect_uri')}
              onChange={(v) => set('calendar.outlook.redirect_uri', v)}
              placeholder="https://yourdomain/api/calendar/oauth/callback"
              mono
            />
          </Field>
          <p className={styles.sectionDesc}>
            Once this is set up, each user links their own Microsoft account from
            their own <strong>Calendar</strong> page (left nav) — there's nothing per-user to do here.
          </p>
        </Section>
      )}
    </div>
  )
}

// ── Voice tab ─────────────────────────────────────────────────────────────────
//
// TTS subsystem. Surfaces the `tts.*` config tree:
// * a status panel that pings `/api/tts/status` for the active backend,
// * shared voice/speed/format defaults,
// * per-backend sections for internal (Piper), OpenAI, OpenAI-compat,
// * a placeholder for ElevenLabs / Cartesia,
// * per-channel routing so the chat 🔊 button, TUI, Telegram, and Signal
//   can each pin a different backend or fall through to the default.

// Static fallback for the Kokoro voice dropdown when the live list isn't
// available (binary built without the `kokoro` feature, or backend not yet
// registered). Mirrors the curated English presets in backend/kokoro.rs.
const KOKORO_FALLBACK_VOICES: { id: string; name: string; is_downloaded: boolean }[] = [
  { id: 'af_heart',   name: 'Heart (US)',    is_downloaded: false },
  { id: 'af_bella',   name: 'Bella (US)',    is_downloaded: false },
  { id: 'am_michael', name: 'Michael (US)',  is_downloaded: false },
  { id: 'am_adam',    name: 'Adam (US)',     is_downloaded: false },
  { id: 'bf_emma',    name: 'Emma (UK)',     is_downloaded: false },
  { id: 'bm_george',  name: 'George (UK)',   is_downloaded: false },
]

const TTS_BACKENDS: { value: string; label: string; privacy: string }[] = [
  { value: 'internal',      label: 'Internal (Piper / eSpeak)', privacy: 'local' },
  { value: 'kokoro',        label: 'Kokoro (local, natural)',   privacy: 'local' },
  { value: 'chatterbox',    label: 'Chatterbox (AMD Vulkan)',   privacy: 'local' },
  { value: 'openai',        label: 'OpenAI (cloud)',            privacy: 'cloud' },
  { value: 'openai_compat', label: 'OpenAI-compatible (self-hosted)', privacy: 'self-hosted' },
  { value: 'elevenlabs',    label: 'ElevenLabs (cloud, config only)', privacy: 'cloud' },
  { value: 'cartesia',      label: 'Cartesia (cloud, config only)',   privacy: 'cloud' },
]

const STT_BACKENDS: { value: string; label: string; privacy: string }[] = [
  { value: 'internal',      label: 'Internal (whisper.cpp)',          privacy: 'local' },
  { value: 'openai',        label: 'OpenAI Whisper (cloud)',          privacy: 'cloud' },
  { value: 'openai_compat', label: 'OpenAI-compatible (self-hosted)', privacy: 'self-hosted' },
]

// Per-channel routing + voice prefs rows are driven off the channel
// registry (`GET /api/channels`) so plugin-defined channels appear without
// any frontend change. Built-in channels are guaranteed to come back; we
// don't bother with a hard-coded fallback.

interface SettingsBag {
  str: (path: string, fallback?: string) => string
  set: (path: string, value: unknown) => void
}

function useRegistryChannels() {
  return useQuery({
    queryKey: ['channels'],
    queryFn:  async () => {
      const r = await api.get<ChannelDescriptor[]>('/api/channels')
      return r.data
    },
    staleTime: 5 * 60_000,
    refetchOnWindowFocus: false,
  })
}

function ChannelRoutingRows({ str, set }: SettingsBag) {
  const { data: channels } = useRegistryChannels()
  const list = channels?.filter((c) => c.supports_voice) ?? []
  return (
    <>
      {list.map((ch) => (
        <Field key={ch.id} label={ch.display_name} desc={`Backend used for tts.routing.${ch.id}.`}>
          <SelectInput
            value={str(`tts.routing.${ch.id}`, '')}
            onChange={(v) => set(`tts.routing.${ch.id}`, v)}
            options={[
              { value: '', label: '(use default)' },
              ...TTS_BACKENDS.map(b => ({ value: b.value, label: b.label })),
            ]}
          />
        </Field>
      ))}
    </>
  )
}

function ChannelVoicePrefsRows({ str, set }: SettingsBag) {
  const { data: channels } = useRegistryChannels()
  const list = channels?.filter((c) => c.supports_voice) ?? []
  return (
    <>
      {list.map((ch) => {
        const policyPath  = `tts.voice_prefs.${ch.id}.response_policy`
        const voicePath   = `tts.voice_prefs.${ch.id}.voice_id`
        const routingPath = `tts.routing.${ch.id}`
        // Drive the voice list off the form-state routing rather than
        // the server's saved routing — otherwise, when the user changes
        // the channel's TTS engine in the row above, the voice
        // dropdown stays stuck on the previously-saved engine's voices
        // (the picker's `channel` lookup hits `/api/tts/status`'s
        // routing map, which is cached for 5 minutes and reflects
        // only saved state). When the routing field is empty (channel
        // inherits the global default), fall back to channel-based
        // resolution.
        const formBackend = str(routingPath, '')
        return (
          <Field
            key={ch.id}
            label={ch.display_name}
            desc={`Default response policy + voice id for the ${ch.display_name} channel.`}
          >
            <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
              <SelectInput
                value={str(policyPath, '')}
                onChange={(v) => set(policyPath, v === '' ? null : v)}
                options={[
                  { value: '',                label: 'Inherit (never)' },
                  { value: 'always',          label: 'Always' },
                  { value: 'on_voice_input',  label: 'On voice input' },
                  { value: 'never',           label: 'Never' },
                ]}
              />
              <VoiceIdPicker
                value={str(voicePath, '')}
                onChange={(v) => set(voicePath, v === '' ? null : v)}
                ariaLabel={`${ch.display_name} default voice id`}
                backend={formBackend || undefined}
                channel={ch.id}
              />
            </div>
          </Field>
        )
      })}
    </>
  )
}

// Curated whisper.cpp models. Mirrors `src/stt/manifest.rs` so the
// settings dropdown matches what the server will actually download.
const WHISPER_MODELS: { value: string; label: string }[] = [
  { value: 'tiny.en',   label: 'tiny.en (~75 MB · English)'        },
  { value: 'tiny',      label: 'tiny (~75 MB · multilingual)'      },
  { value: 'base.en',   label: 'base.en (~142 MB · English) — default' },
  { value: 'base',      label: 'base (~142 MB · multilingual)'     },
  { value: 'small.en',  label: 'small.en (~466 MB · English)'      },
  { value: 'small',     label: 'small (~466 MB · multilingual)'    },
  { value: 'medium.en', label: 'medium.en (~1.5 GB · English)'     },
  { value: 'medium',    label: 'medium (~1.5 GB · multilingual)'   },
  { value: 'large-v3',  label: 'large-v3 (~3.0 GB · multilingual)' },
]

/**
 * Extract just the hostname (or host:no-port) from a URL string so
 * the TTS openai_compat preset buttons can preserve "where the user
 * is pointing today" while swapping port + path. Returns an empty
 * string when the input isn't parseable — caller falls back to
 * `localhost`. We don't try to be clever with non-HTTP schemes
 * because nothing in the TTS path uses them.
 */
function hostFromUrl(s: string): string {
  try {
    const u = new URL(s)
    return u.hostname || ''
  } catch {
    return ''
  }
}

// ── Image & Video tab ─────────────────────────────────────────────────────────

function ImageTab({
  set, str, num, bool,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
}) {
  const comfyOn = bool('image.comfyui.enabled', false)
  const a1111On = bool('image.automatic1111.enabled', false)
  return (
    <div className={styles.tabBody}>
      <Section title="Image generation">
        <p className={styles.sectionDesc}>
          The <code>image_generate</code> tool turns a prompt into an inline image through a pluggable backend. Enable one or more below; the <strong>default backend</strong> picks which runs when the agent doesn't specify one. Local backends (ComfyUI / Automatic1111) need no key. OpenAI uses the key under <strong>Providers</strong>.
        </p>
        <Field label="Default backend" desc="Which backend to use when the request doesn't name one. 'Auto' picks the first enabled (local preferred).">
          <SelectInput
            value={str('image.default_backend', 'auto')}
            onChange={(v) => set('image.default_backend', v)}
            options={[
              { value: 'auto',          label: 'Auto (first enabled, local preferred)' },
              { value: 'comfyui',       label: 'ComfyUI (local)' },
              { value: 'automatic1111', label: 'Automatic1111 / SD WebUI (local)' },
              { value: 'openai',        label: 'OpenAI Images (cloud)' },
            ]}
          />
        </Field>
      </Section>

      <Section title="ComfyUI (local)">
        <p className={styles.sectionDesc}>
          Local ComfyUI server. MIRA runs a node-graph workflow (built-in default SD txt2img, or your own API-format workflow) and fetches the result.
        </p>
        <Field label="Enabled" desc="Use ComfyUI as an image backend.">
          <Toggle value={comfyOn} onChange={(v) => set('image.comfyui.enabled', v)} />
        </Field>
        <Field label="Base URL" desc="e.g. http://127.0.0.1:8188 — or http://windows-host:8188 when ComfyUI runs on the Windows host and MIRA is in WSL.">
          <TextInput value={str('image.comfyui.base_url', 'http://127.0.0.1:8188')} onChange={(v) => set('image.comfyui.base_url', v)} placeholder="http://127.0.0.1:8188" mono />
        </Field>
        <Field label="Checkpoint (model)" desc="Checkpoint filename for the default workflow (e.g. sd_xl_base_1.0.safetensors). Blank = auto-pick the first available.">
          <TextInput value={str('image.comfyui.model')} onChange={(v) => set('image.comfyui.model', v)} placeholder="auto" mono />
        </Field>
        <Field label="Steps" desc="Sampling steps.">
          <NumberInput value={num('image.comfyui.steps', 20)} onChange={(v) => set('image.comfyui.steps', v)} min={1} max={150} />
        </Field>
        <div style={{ display: 'flex', gap: 8 }}>
          <Field label="Width" desc="Default width.">
            <NumberInput value={num('image.comfyui.width', 1024)} onChange={(v) => set('image.comfyui.width', v)} min={64} max={4096} step={64} />
          </Field>
          <Field label="Height" desc="Default height.">
            <NumberInput value={num('image.comfyui.height', 1024)} onChange={(v) => set('image.comfyui.height', v)} min={64} max={4096} step={64} />
          </Field>
          <Field label="CFG scale" desc="Prompt adherence.">
            <NumberInput value={num('image.comfyui.cfg_scale', 7)} onChange={(v) => set('image.comfyui.cfg_scale', v)} min={1} max={30} step={0.5} />
          </Field>
        </div>
        <Field label="Negative prompt" desc="Default things to avoid (used when the call doesn't pass one).">
          <TextInput value={str('image.comfyui.negative_prompt')} onChange={(v) => set('image.comfyui.negative_prompt', v)} placeholder="blurry, low quality, watermark" />
        </Field>
        <Field label="Custom workflow (advanced)" desc="Optional ComfyUI API-format workflow JSON with placeholder tokens {{prompt}} {{negative}} {{seed}} {{width}} {{height}} {{steps}} {{cfg}} {{ckpt}}. Blank = built-in default SD txt2img.">
          <TextInput value={str('image.comfyui.workflow_json')} onChange={(v) => set('image.comfyui.workflow_json', v)} placeholder="(default workflow)" mono />
        </Field>
      </Section>

      <Section title="Automatic1111 / SD WebUI (local)">
        <p className={styles.sectionDesc}>
          Local Stable Diffusion WebUI (incl. Forge). Requires the server launched with <code>--api --listen</code>.
        </p>
        <Field label="Enabled" desc="Use Automatic1111 as an image backend.">
          <Toggle value={a1111On} onChange={(v) => set('image.automatic1111.enabled', v)} />
        </Field>
        <Field label="Base URL" desc="e.g. http://127.0.0.1:7860 — or http://windows-host:7860 from WSL.">
          <TextInput value={str('image.automatic1111.base_url', 'http://127.0.0.1:7860')} onChange={(v) => set('image.automatic1111.base_url', v)} placeholder="http://127.0.0.1:7860" mono />
        </Field>
        <Field label="Checkpoint (model)" desc="Optional checkpoint to switch to per call. Blank = use the WebUI's currently-loaded model.">
          <TextInput value={str('image.automatic1111.model')} onChange={(v) => set('image.automatic1111.model', v)} placeholder="(loaded model)" mono />
        </Field>
        <Field label="Sampler" desc="Sampler name, e.g. 'Euler a' or 'DPM++ 2M'.">
          <TextInput value={str('image.automatic1111.sampler', 'Euler a')} onChange={(v) => set('image.automatic1111.sampler', v)} placeholder="Euler a" mono />
        </Field>
        <Field label="Steps" desc="Sampling steps.">
          <NumberInput value={num('image.automatic1111.steps', 25)} onChange={(v) => set('image.automatic1111.steps', v)} min={1} max={150} />
        </Field>
        <div style={{ display: 'flex', gap: 8 }}>
          <Field label="Width" desc="Default width.">
            <NumberInput value={num('image.automatic1111.width', 1024)} onChange={(v) => set('image.automatic1111.width', v)} min={64} max={4096} step={64} />
          </Field>
          <Field label="Height" desc="Default height.">
            <NumberInput value={num('image.automatic1111.height', 1024)} onChange={(v) => set('image.automatic1111.height', v)} min={64} max={4096} step={64} />
          </Field>
          <Field label="CFG scale" desc="Prompt adherence.">
            <NumberInput value={num('image.automatic1111.cfg_scale', 7)} onChange={(v) => set('image.automatic1111.cfg_scale', v)} min={1} max={30} step={0.5} />
          </Field>
        </div>
        <Field label="Negative prompt" desc="Default things to avoid.">
          <TextInput value={str('image.automatic1111.negative_prompt')} onChange={(v) => set('image.automatic1111.negative_prompt', v)} placeholder="blurry, low quality, watermark" />
        </Field>
      </Section>

      <Section title="OpenAI Images (cloud)">
        <p className={styles.sectionDesc}>
          Uses the OpenAI key + endpoint from the <strong>Providers</strong> tab (or an OpenAI-compatible images endpoint). Enabled automatically when a key is present.
        </p>
        <Field label="Default model" desc="e.g. dall-e-3 or gpt-image-1.">
          <TextInput value={str('image.openai.default_model', 'dall-e-3')} onChange={(v) => set('image.openai.default_model', v)} placeholder="dall-e-3" mono />
        </Field>
      </Section>

      <Section title="Video generation">
        <p className={styles.sectionDesc}>
          The <code>video_generate</code> tool renders a short clip inline as a player, through a pluggable backend (mirrors image). Pick the <strong>default backend</strong>; configure each below.
        </p>
        <Field label="Default backend" desc="Which backend to use when the request doesn't name one. 'Auto' picks the first enabled (local preferred).">
          <SelectInput
            value={str('video.default_backend', 'auto')}
            onChange={(v) => set('video.default_backend', v)}
            options={[
              { value: 'auto',    label: 'Auto (first enabled, local preferred)' },
              { value: 'comfyui', label: 'ComfyUI video (local)' },
              { value: 'wan2gp',  label: 'WAN2GP (local)' },
              { value: 'openai',  label: 'OpenAI Videos / Sora (cloud)' },
            ]}
          />
        </Field>
      </Section>

      <Section title="ComfyUI video (local)">
        <p className={styles.sectionDesc}>
          Runs a ComfyUI <strong>video</strong> workflow (Wan / AnimateDiff / SVD). There's no universal default video workflow, so you must paste your own API-format workflow with placeholder tokens: <code>{'{{prompt}}'} {'{{negative}}'} {'{{seed}}'} {'{{width}}'} {'{{height}}'} {'{{frames}}'} {'{{fps}}'} {'{{steps}}'} {'{{cfg}}'} {'{{ckpt}}'}</code> (<code>{'{{frames}}'}</code> = seconds × fps).
        </p>
        <Field label="Enabled" desc="Use ComfyUI as a video backend.">
          <Toggle value={bool('video.comfyui.enabled', false)} onChange={(v) => set('video.comfyui.enabled', v)} />
        </Field>
        <Field label="Base URL" desc="Usually the same ComfyUI as images, e.g. http://windows-host:8188.">
          <TextInput value={str('video.comfyui.base_url', 'http://127.0.0.1:8188')} onChange={(v) => set('video.comfyui.base_url', v)} placeholder="http://127.0.0.1:8188" mono />
        </Field>
        <Field label="Workflow JSON (required)" desc="ComfyUI API-format video workflow with the placeholder tokens above. Export from ComfyUI (Save (API Format)) and replace the prompt/seed/etc values with tokens.">
          <TextInput value={str('video.comfyui.workflow_json')} onChange={(v) => set('video.comfyui.workflow_json', v)} placeholder="(paste API-format workflow)" mono />
        </Field>
        <Field label="Model / checkpoint" desc="Value for {{ckpt}} (workflow-dependent).">
          <TextInput value={str('video.comfyui.model')} onChange={(v) => set('video.comfyui.model', v)} placeholder="(workflow-dependent)" mono />
        </Field>
        <div style={{ display: 'flex', gap: 8 }}>
          <Field label="Width" desc="Default width."><NumberInput value={num('video.comfyui.width', 512)} onChange={(v) => set('video.comfyui.width', v)} min={64} max={2048} step={16} /></Field>
          <Field label="Height" desc="Default height."><NumberInput value={num('video.comfyui.height', 512)} onChange={(v) => set('video.comfyui.height', v)} min={64} max={2048} step={16} /></Field>
          <Field label="FPS" desc="frames = seconds × fps."><NumberInput value={num('video.comfyui.fps', 16)} onChange={(v) => set('video.comfyui.fps', v)} min={1} max={60} /></Field>
        </div>
        <div style={{ display: 'flex', gap: 8 }}>
          <Field label="Steps" desc="Sampling steps."><NumberInput value={num('video.comfyui.steps', 20)} onChange={(v) => set('video.comfyui.steps', v)} min={1} max={150} /></Field>
          <Field label="CFG scale" desc="Prompt adherence."><NumberInput value={num('video.comfyui.cfg_scale', 7)} onChange={(v) => set('video.comfyui.cfg_scale', v)} min={1} max={30} step={0.5} /></Field>
        </div>
        <Field label="Negative prompt" desc="Default things to avoid.">
          <TextInput value={str('video.comfyui.negative_prompt')} onChange={(v) => set('video.comfyui.negative_prompt', v)} placeholder="blurry, low quality" />
        </Field>
      </Section>

      <Section title="WAN2GP (local)">
        <p className={styles.sectionDesc}>
          Local WAN2GP (Wan2GP) — a Gradio video app. MIRA drives its Gradio API. Set the URL and the API endpoint name (from the app's <code>/config</code>).
        </p>
        <Field label="Enabled" desc="Use WAN2GP as a video backend.">
          <Toggle value={bool('video.wan2gp.enabled', false)} onChange={(v) => set('video.wan2gp.enabled', v)} />
        </Field>
        <Field label="Base URL" desc="e.g. http://windows-host:7862.">
          <TextInput value={str('video.wan2gp.base_url', 'http://127.0.0.1:7862')} onChange={(v) => set('video.wan2gp.base_url', v)} placeholder="http://127.0.0.1:7862" mono />
        </Field>
        <Field label="Gradio API name" desc="The named endpoint to call, e.g. /generate_video (discoverable from the app's /config).">
          <TextInput value={str('video.wan2gp.api_name')} onChange={(v) => set('video.wan2gp.api_name', v)} placeholder="/generate_video" mono />
        </Field>
      </Section>

      <Section title="OpenAI Videos / Sora (cloud)">
        <p className={styles.sectionDesc}>
          Uses the OpenAI key from the <strong>Providers</strong> tab; off until a key is set.
        </p>
        <Field label="Default model" desc="e.g. sora-2 or sora-2-pro (larger sizes need pro).">
          <TextInput value={str('video.openai.default_model', 'sora-2')} onChange={(v) => set('video.openai.default_model', v)} placeholder="sora-2" mono />
        </Field>
        <Field label="Default size" desc="Frame size WIDTHxHEIGHT, e.g. 1280x720.">
          <TextInput value={str('video.openai.default_size', '1280x720')} onChange={(v) => set('video.openai.default_size', v)} placeholder="1280x720" mono />
        </Field>
        <Field label="Default length (seconds)" desc="Clip length in seconds.">
          <NumberInput value={num('video.openai.default_seconds', 4)} onChange={(v) => set('video.openai.default_seconds', v)} min={1} max={60} />
        </Field>
      </Section>
    </div>
  )
}

function VoiceTab({
  set, str, num, bool,
}: {
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
  bool: (p: string, fb?: boolean) => boolean
}) {
  const enabled = bool('tts.enabled', true)
  const backend = str('tts.default_backend', 'internal')
  const qc = useQueryClient()

  // Tracks whether the user has just proved this backend works via the Test
  // button. The server-side `/api/tts/status` probe is independent — it
  // sometimes reports unhealthy for openai_compat servers that 404 on the
  // probe's empty default voice, even though a real synth call with an
  // explicit voice succeeds. So when the user manually verifies, we override
  // the badge. Reset whenever the user picks a different backend.
  const [verifiedBackend, setVerifiedBackend] = useState<string | null>(null)
  const isVerified = verifiedBackend === backend

  const { data: status } = useQuery({
    queryKey: ['tts-status', backend],
    queryFn:  async () => {
      const r = await api.get('/api/tts/status', {
        params: { backend },
      })
      return r.data as {
        enabled: boolean; backend: string; backends: string[]
        healthy: boolean; last_latency_ms: number | null; note: string | null
        cache: { entries: number; total_bytes: number }
      }
    },
    staleTime: 10_000,
    enabled,
  })

  const { data: openaiVoices } = useQuery({
    queryKey: ['tts-voices', 'openai'],
    queryFn:  async () => {
      const r = await api.get('/api/tts/voices', { params: { backend: 'openai' } })
      return r.data as { id: string; name: string }[]
    },
    staleTime: 60_000,
    enabled,
  })

  const { data: piperVoices } = useQuery({
    queryKey: ['tts-voices', 'piper'],
    queryFn:  async () => {
      const r = await api.get('/api/tts/voices', { params: { backend: 'piper' } })
      return r.data as { id: string; name: string; is_downloaded: boolean }[]
    },
    staleTime: 60_000,
    enabled,
  })

  // Kokoro voice list. Served only when the binary was built with the
  // `kokoro` feature and the backend is registered; otherwise the request
  // errors and we fall back to the static preset list below so the dropdown
  // still renders.
  const { data: kokoroVoices } = useQuery({
    queryKey: ['tts-voices', 'kokoro'],
    queryFn:  async () => {
      const r = await api.get('/api/tts/voices', { params: { backend: 'kokoro' } })
      return r.data as { id: string; name: string; is_downloaded: boolean }[]
    },
    staleTime: 60_000,
    enabled:   enabled && bool('tts.kokoro.enabled', false),
    retry:     false,
  })

  // Host hardware probe (K2) — drives the local-voice recommendation card.
  const { data: hardware } = useQuery({
    queryKey: ['system-hardware'],
    queryFn:  async () => {
      const r = await api.get('/api/system/hardware')
      return r.data as {
        os: string; arch: string; is_wsl: boolean
        has_cuda: boolean; has_vulkan: boolean
        gpus: { vendor: string; name: string; source: string }[]
        recommendation: { kind: string; reason: string }
      }
    },
    staleTime: 5 * 60_000,
    enabled,
  })

  // Chatterbox (K3) server health + supervisor state.
  const { data: chatterboxStatus } = useQuery({
    queryKey: ['chatterbox-status'],
    queryFn:  async () => {
      const r = await api.get('/api/system/chatterbox/status')
      return r.data as {
        supervised: boolean; running: boolean; healthy: boolean
        starts?: number; pid?: number | null; last_error?: string | null
        enabled?: boolean; port?: number
      }
    },
    refetchInterval: 10_000,
    enabled:   enabled && (backend === 'chatterbox' || bool('tts.chatterbox.enabled', false)),
    retry:     false,
  })

  // Discover voices on the configured OpenAI-compat server. The backend walks
  // a probe chain (`/voices`, `/audio/voices`, Chatterbox helpers) and returns
  // whatever the server advertises — empty list if none of the shapes match.
  // Re-keyed on URL so editing the server URL refetches automatically.
  // ── Speech-to-Text (STT) ────────────────────────────────────────────────
  // Status panel mirrors the TTS one — pings `/api/stt/status` for the
  // active backend so the user sees OK/DOWN before saving.
  const sttEnabled = bool('stt.enabled', true)
  const sttBackend = str('stt.default_backend', 'internal')

  const { data: sttStatus } = useQuery({
    queryKey: ['stt-status', sttBackend],
    queryFn:  async () => {
      const r = await api.get('/api/stt/status', { params: { backend: sttBackend } })
      return r.data as {
        enabled: boolean; backend: string; backends: string[]
        healthy: boolean; latency_ms: number | null; note: string | null
      }
    },
    staleTime: 10_000,
    enabled:   sttEnabled,
  })

  const compatUrl = str('tts.openai_compat.url', 'http://localhost:8000/v1')
  const compatBackendWired = (status?.backends ?? []).includes('openai_compat')
  const {
    data:    compatVoices,
    isFetching: compatVoicesFetching,
    refetch: refetchCompatVoices,
  } = useQuery({
    queryKey: ['tts-voices', 'openai_compat', compatUrl],
    queryFn:  async () => {
      const r = await api.get('/api/tts/voices', { params: { backend: 'openai_compat' } })
      return r.data as { id: string; name: string }[]
    },
    staleTime: 60_000,
    // Only probe once the backend is actually wired up — otherwise the call
    // returns the configured-default fallback and the dropdown looks empty.
    enabled: enabled && compatBackendWired,
  })

  return (
    <div className={styles.tabBody}>
      <Section title="Status">
        <p className={styles.sectionDesc}>
          The voice (text-to-speech) pipeline powers the speaker button on
          assistant messages and any spoken replies MIRA sends. Disabling turns
          the entire subsystem off.
        </p>
        <Field label="Enabled" desc="Master switch for the TTS subsystem.">
          <Toggle
            value={enabled}
            onChange={(v) => {
              set('tts.enabled', v)
              // Re-probe immediately so the badge reflects the new state
              // instead of waiting out the 10 s staleTime.
              qc.invalidateQueries({ queryKey: ['tts-status'] })
            }}
          />
        </Field>
        {enabled && (
          <Field
            label="Test voice"
            desc="Synthesise a short clip with the backend and voice you have selected here (no Save needed) so you can confirm the configured TTS is working."
          >
            <TestVoiceButton
              qc={qc}
              backend={backend}
              voice={(() => {
                switch (backend) {
                  case 'piper':
                  case 'internal':
                    return str('tts.internal.default_voice', 'en_US-amy-medium')
                  case 'openai':
                    return str('tts.openai.default_voice', 'alloy')
                  case 'openai_compat':
                    return str('tts.openai_compat.default_voice', 'alloy')
                  default:
                    return str('tts.default_voice') || undefined
                }
              })()}
              gain={(() => {
                // Mirror volumeForBackend against the form state so the slider's
                // value applies on the next test click without needing a Save.
                switch (backend) {
                  case 'internal':
                  case 'piper':
                  case 'espeak':
                  case 'kokoro':         return num('tts.internal.volume', 1.0)
                  case 'openai':         return num('tts.openai.volume', 1.0)
                  case 'openai_compat':  return num('tts.openai_compat.volume', 1.0)
                  case 'elevenlabs':     return num('tts.elevenlabs.volume', 1.0)
                  case 'cartesia':       return num('tts.cartesia.volume', 1.0)
                  default:               return 1.0
                }
              })()}
              onVerified={() => setVerifiedBackend(backend)}
            />
          </Field>
        )}
        {enabled && status && (() => {
          // Treat the backend as healthy if either the server probe succeeded
          // OR the user just verified it via the Test button. The probe can
          // fail for benign reasons (openai_compat servers that 404 on an
          // empty default voice) while real synth requests succeed.
          const effectiveHealthy = status.healthy || isVerified
          const desc = status.healthy
            ? `Healthy${status.last_latency_ms != null ? ` · ${status.last_latency_ms} ms last probe` : ''}${status.note ? ` · ${status.note}` : ''}`
            : isVerified
              ? `Verified by Test voice — server probe still reports: ${status.note ?? 'unhealthy'}`
              : `Unhealthy${status.note ? ` — ${status.note}` : ''}`
          return (
            <Field label={`Active: ${status.backend}`} desc={desc}>
              <span style={{
                display: 'inline-block',
                padding: '3px 10px',
                borderRadius: 'var(--radius-full)',
                background: effectiveHealthy ? 'var(--accent-bg, var(--bg-overlay))' : 'var(--bg-overlay)',
                color: effectiveHealthy ? 'var(--accent, var(--text-secondary))' : 'var(--text-muted)',
                fontSize: '11px',
                fontFamily: 'var(--font-mono)',
              }}>
                {effectiveHealthy ? 'OK' : 'DOWN'}
              </span>
            </Field>
          )
        })()}
        {enabled && status && (
          <Field
            label="Cache"
            desc={`${status.cache.entries} entries · ${(status.cache.total_bytes / 1024 / 1024).toFixed(1)} MB on disk`}
          >
            <span style={{ color: 'var(--text-muted)', fontFamily: 'var(--font-mono)', fontSize: '11px' }}>
              {status.backends.join(' · ')}
            </span>
          </Field>
        )}
      </Section>

      <Section title="Defaults">
        <Field label="Default backend" desc="Which TTS backend to use when a request doesn't pin one. Per-channel pinning below overrides this.">
          <SelectInput
            value={backend}
            onChange={(v) => set('tts.default_backend', v)}
            options={TTS_BACKENDS.map(b => ({
              value: b.value,
              label: `${b.label}  (${b.privacy})`,
            }))}
          />
        </Field>
        <Field label="Default voice" desc="Voice id used when the request doesn't specify one. Empty = backend default.">
          <TextInput
            value={str('tts.default_voice')}
            onChange={(v) => set('tts.default_voice', v)}
            placeholder="(backend default)"
            mono
          />
        </Field>
        <Field label="Default speed" desc="Speech rate multiplier. 1.0 = natural; 0.5–2.0 supported.">
          <NumberInput
            value={num('tts.default_speed', 1.0)}
            onChange={(v) => set('tts.default_speed', v)}
            min={0.5} max={2.0} step={0.1}
          />
        </Field>
        <Field label="Default format" desc="Audio container hint. WAV is the safest cross-platform default; MP3/OGG-Opus are smaller for messaging channels.">
          <SelectInput
            value={str('tts.default_format', 'wav')}
            onChange={(v) => set('tts.default_format', v)}
            options={[
              { value: 'wav',      label: 'WAV (uncompressed)' },
              { value: 'mp3',      label: 'MP3 (cloud-default)' },
              { value: 'ogg-opus', label: 'OGG/Opus (small, voice notes)' },
            ]}
          />
        </Field>
        <Field label="Streaming" desc="Sentence-chunked synthesis for chat. Disable to always wait for the full buffer.">
          <Toggle value={bool('tts.streaming', true)} onChange={(v) => set('tts.streaming', v)} />
        </Field>
        <Field label="Max characters per request" desc="Safety cap on a single TTS call.">
          <NumberInput
            value={num('tts.max_chars_per_request', 4000)}
            onChange={(v) => set('tts.max_chars_per_request', v)}
            min={100} max={20000}
          />
        </Field>
        <Field label="Request timeout (seconds)" desc="Per-request timeout against the backend.">
          <NumberInput
            value={num('tts.request_timeout_secs', 30)}
            onChange={(v) => set('tts.request_timeout_secs', v)}
            min={5} max={300}
          />
        </Field>
      </Section>

      {hardware && (
        <Section title="Recommended for your hardware">
          <Field
            label="Detected"
            desc={[
              `${hardware.os}/${hardware.arch}${hardware.is_wsl ? ' · WSL2' : ''}`,
              hardware.gpus.length ? `GPU: ${hardware.gpus.map(g => g.name || g.vendor).join(', ')}` : 'no discrete GPU detected',
              hardware.has_cuda ? 'CUDA' : null,
              hardware.has_vulkan ? 'Vulkan' : null,
            ].filter(Boolean).join(' · ')}
          >
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: '11px', color: 'var(--text-muted)' }}>
              {hardware.recommendation.kind === 'chatterbox_vulkan' ? 'Chatterbox (AMD Vulkan)'
                : hardware.recommendation.kind === 'kokoro_cuda'     ? 'Kokoro (CUDA)'
                : 'Kokoro (CPU)'}
            </span>
          </Field>
          <p style={{ color: 'var(--text-muted)', fontSize: '12px', margin: '4px 0 0' }}>
            {hardware.recommendation.reason}
          </p>
        </Section>
      )}

      <CollapsibleSection title="Internal (Piper)" defaultOpen={backend === 'internal'}>
        <Field label="Engine" desc="Internal TTS engine. Piper is the default; eSpeak is the fallback when Piper download fails.">
          <SelectInput
            value={str('tts.internal.engine', 'piper')}
            onChange={(v) => set('tts.internal.engine', v)}
            options={[
              { value: 'piper',  label: 'Piper (default)' },
              { value: 'espeak', label: 'eSpeak NG (fallback)' },
            ]}
          />
        </Field>
        <Field label="Default voice" desc="Voice id for the internal engine. Curated voices are auto-downloaded on first use.">
          <SelectInput
            value={str('tts.internal.default_voice', 'en_US-amy-medium')}
            onChange={(v) => {
              set('tts.internal.default_voice', v)
              // Eager-fetch the model pair so the user doesn't pay the
              // download latency on the first speak. Pre-existing voices
              // short-circuit; new ones flip the "(not downloaded)" suffix
              // off once the dropdown refetches.
              if (bool('tts.internal.auto_download_voices', true)) {
                const t = toast.loading(`Downloading voice ${v}…`, { id: `dl-${v}` })
                api.post('/api/tts/voices/download', { backend: 'piper', voice_id: v })
                  .then(() => {
                    toast.success(`Voice ${v} ready`, { id: t })
                    qc.invalidateQueries({ queryKey: ['tts-voices', 'piper'] })
                  })
                  .catch((e) => {
                    const msg = e?.response?.data?.error ?? e?.message ?? 'download failed'
                    toast.error(`Voice ${v}: ${msg}`, { id: t })
                  })
              }
            }}
            options={(piperVoices ?? [{ id: 'en_US-amy-medium', name: 'Amy (US English)', is_downloaded: false }]).map(v => ({
              value: v.id,
              label: `${v.name}${v.is_downloaded ? '' : ' (not downloaded)'}`,
            }))}
          />
        </Field>
        <Field label="Auto-download voices" desc="Fetch the model files from huggingface.co on first use. Off = require manual install in voices_dir.">
          <Toggle
            value={bool('tts.internal.auto_download_voices', true)}
            onChange={(v) => set('tts.internal.auto_download_voices', v)}
          />
        </Field>
        <Field label="Voices directory" desc="Override for <data_dir>/tts/voices. Empty = use the default.">
          <TextInput
            value={str('tts.internal.voices_dir')}
            onChange={(v) => set('tts.internal.voices_dir', v)}
            placeholder="(default)"
            mono
          />
        </Field>
        <Field label="Piper binary path" desc="Override for the auto-installed Piper executable. Empty = use the bundled install.">
          <TextInput
            value={str('tts.internal.binary_path')}
            onChange={(v) => set('tts.internal.binary_path', v)}
            placeholder="(default)"
            mono
          />
        </Field>
        <Field label="Playback volume" desc="Web playback gain. 100 % = unaltered, up to 200 % to boost quiet voices. Applied client-side; saved as tts.internal.volume.">
          <VolumeSlider
            value={num('tts.internal.volume', 1.0)}
            onChange={(v) => set('tts.internal.volume', v)}
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="Kokoro (local, natural voice)" defaultOpen={backend === 'kokoro'}>
        <Field label="Enable Kokoro" desc="Run the Kokoro-82M model in-process for natural-sounding speech — no separate server, no API key. Requires a MIRA build with the 'kokoro' feature; on a stock build this toggle has no effect. American/British English only.">
          <Toggle value={bool('tts.kokoro.enabled', false)} onChange={(v) => set('tts.kokoro.enabled', v)} />
        </Field>
        <Field label="Default voice" desc="Kokoro preset voice. Voices ship inside the model download.">
          <SelectInput
            value={str('tts.kokoro.default_voice', 'af_heart')}
            onChange={(v) => set('tts.kokoro.default_voice', v)}
            options={(kokoroVoices ?? KOKORO_FALLBACK_VOICES).map(v => ({
              value: v.id,
              label: `${v.name}${v.is_downloaded ? '' : ' (model not downloaded)'}`,
            }))}
          />
        </Field>
        <Field label="Device" desc="Compute device. Auto picks the fastest available; CUDA/Metal require a MIRA build with the matching GPU feature, otherwise they fall back to CPU.">
          <SelectInput
            value={str('tts.kokoro.device', 'auto')}
            onChange={(v) => set('tts.kokoro.device', v)}
            options={[
              { value: 'auto',  label: 'Auto (fastest available)' },
              { value: 'cpu',   label: 'CPU' },
              { value: 'cuda',  label: 'CUDA (NVIDIA)' },
              { value: 'metal', label: 'Metal (Apple)' },
            ]}
          />
        </Field>
        <Field label="Auto-download model" desc="Fetch the Kokoro-82M weights (~0.3 GB) from HuggingFace on first use. Off = require the model already present in the model directory.">
          <Toggle value={bool('tts.kokoro.auto_download', true)} onChange={(v) => set('tts.kokoro.auto_download', v)} />
        </Field>
        <Field label="Model directory" desc="Override for <data_dir>/tts/kokoro/Kokoro-82M. Empty = use the default.">
          <TextInput
            value={str('tts.kokoro.model_path')}
            onChange={(v) => set('tts.kokoro.model_path', v)}
            placeholder="(default)"
            mono
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="Chatterbox (AMD Vulkan server)" defaultOpen={backend === 'chatterbox'}>
        <p style={{ color: 'var(--text-muted)', fontSize: '12px', margin: '0 0 8px' }}>
          Fast local TTS on AMD Radeon GPUs via Vulkan, from{' '}
          <a href="https://github.com/tarekedOz/Chatterbox_AMDVulkan" target="_blank" rel="noreferrer">
            Chatterbox_AMDVulkan
          </a>. OpenAI-compatible — MIRA talks to it on a local port. On Windows,
          MIRA can install it for you; on WSL2 install it on the Windows side and
          point MIRA at the URL.
        </p>
        <Field label="Enable Chatterbox" desc="Register the Chatterbox backend (client → http://127.0.0.1:{port}/v1).">
          <Toggle value={bool('tts.chatterbox.enabled', false)} onChange={(v) => set('tts.chatterbox.enabled', v)} />
        </Field>
        {chatterboxStatus && (
          <Field label="Server status" desc={chatterboxStatus.supervised ? `Supervised by MIRA · ${chatterboxStatus.starts ?? 0} start(s)` : 'Not supervised — bare liveness probe'}>
            <span style={{
              fontFamily: 'var(--font-mono)', fontSize: '11px',
              color: chatterboxStatus.healthy ? 'var(--ok, #3a3)' : 'var(--text-muted)',
            }}>
              {chatterboxStatus.healthy ? 'HEALTHY' : (chatterboxStatus.running ? 'STARTING' : 'DOWN')}
              {chatterboxStatus.last_error ? ` · ${chatterboxStatus.last_error}` : ''}
            </span>
          </Field>
        )}
        <Field label="Port" desc="Local port the Chatterbox server listens on. Default 8087.">
          <NumberInput value={num('tts.chatterbox.port', 8087)} onChange={(v) => set('tts.chatterbox.port', v)} min={1} max={65535} />
        </Field>
        <Field label="Default voice" desc="Chatterbox preset voice (e.g. Adrian).">
          <TextInput value={str('tts.chatterbox.default_voice', 'Adrian')} onChange={(v) => set('tts.chatterbox.default_voice', v)} mono />
        </Field>
        <Field label="Let MIRA manage the process" desc="Spawn, health-check, and restart the Chatterbox server. Same-host only — leave off if Chatterbox runs on another machine (e.g. the Windows side under WSL2).">
          <Toggle value={bool('tts.chatterbox.supervise', false)} onChange={(v) => set('tts.chatterbox.supervise', v)} />
        </Field>
        {bool('tts.chatterbox.supervise', false) && (
          <Field label="Server binary path" desc="Path to the Chatterbox server executable. Required for supervision.">
            <TextInput value={str('tts.chatterbox.binary_path')} onChange={(v) => set('tts.chatterbox.binary_path', v)} placeholder="(set after install)" mono />
          </Field>
        )}
        <Field label="Install Chatterbox" desc="Run the one-click Windows installer (native or WSL2 → Windows side). Downloads the app + ~1.4 GB of model weights; can take a few minutes.">
          <button
            className="btn"
            onClick={() => {
              const t = toast.loading('Installing Chatterbox… (downloading weights, may take minutes)')
              api.post('/api/system/chatterbox/install')
                .then((r) => {
                  toast.success('Chatterbox installer finished', { id: t })
                  if (r.data?.log) console.log('chatterbox install:', r.data.log)
                  qc.invalidateQueries({ queryKey: ['chatterbox-status'] })
                })
                .catch((e) => {
                  const msg = e?.response?.data?.error ?? e?.message ?? 'install failed'
                  toast.error(`Chatterbox install: ${msg}`, { id: t })
                })
            }}
          >
            Install / update
          </button>
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="OpenAI (cloud)" defaultOpen={backend === 'openai'}>
        <p className={styles.sectionDesc}>
          Sends the text to api.openai.com. Falls back to the
          <code> OPENAI_API_KEY </code> environment variable when no key
          is set here.
        </p>
        <Field label="API key" desc="OpenAI API key. Stored in the config file — keep it secure.">
          <TextInput
            value={str('tts.openai.api_key')}
            onChange={(v) => set('tts.openai.api_key', v)}
            placeholder="sk-…"
            type="password" mono
          />
        </Field>
        <Field label="Base URL" desc="Override the OpenAI API base URL (advanced). Must include the /v1 prefix.">
          <TextInput
            value={str('tts.openai.base_url', 'https://api.openai.com/v1')}
            onChange={(v) => set('tts.openai.base_url', v)}
            placeholder="https://api.openai.com/v1"
            mono
          />
        </Field>
        <Field label="Model" desc="One of tts-1, tts-1-hd, gpt-4o-mini-tts.">
          <SelectInput
            value={str('tts.openai.model', 'tts-1')}
            onChange={(v) => set('tts.openai.model', v)}
            options={[
              { value: 'tts-1',             label: 'tts-1 (fast, cheap)' },
              { value: 'tts-1-hd',          label: 'tts-1-hd (higher quality)' },
              { value: 'gpt-4o-mini-tts',   label: 'gpt-4o-mini-tts (newest, expressive)' },
            ]}
          />
        </Field>
        <Field label="Default voice" desc="OpenAI voice. Sample each in OpenAI's docs before picking.">
          <SelectInput
            value={str('tts.openai.default_voice', 'alloy')}
            onChange={(v) => set('tts.openai.default_voice', v)}
            options={(openaiVoices ?? [
              { id: 'alloy', name: 'Alloy' }, { id: 'echo', name: 'Echo' },
              { id: 'fable', name: 'Fable' }, { id: 'onyx', name: 'Onyx' },
              { id: 'nova', name: 'Nova' },   { id: 'shimmer', name: 'Shimmer' },
            ]).map(v => ({ value: v.id, label: v.name }))}
          />
        </Field>
        <Field label="Playback volume" desc="Web playback gain. 100 % = unaltered, up to 200 % to boost quiet voices. Applied client-side; saved as tts.openai.volume.">
          <VolumeSlider
            value={num('tts.openai.volume', 1.0)}
            onChange={(v) => set('tts.openai.volume', v)}
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="OpenAI-compatible (self-hosted)" defaultOpen={backend === 'openai_compat'}>
        <p className={styles.sectionDesc}>
          Any server speaking OpenAI's <code>/v1/audio/speech</code> spec
          — OpenedAI-Speech, LiteLLM, LocalAI, Chatterbox-TTS-Server,
          Kokoro-FastAPI, etc. Stays on your network.
        </p>
        <Field
          label="Presets"
          desc="One-click defaults for popular self-hosted servers. Overwrites Base URL / Model / Default voice — Host portion is preserved so localhost stays localhost."
        >
          <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
            <button
              type="button"
              className={styles.button}
              onClick={() => {
                const host = hostFromUrl(str('tts.openai_compat.url', '')) || 'localhost'
                set('tts.openai_compat.url', `http://${host}:8004/v1`)
                set('tts.openai_compat.model', 'ChatterboxTurboTTS')
                set('tts.openai_compat.default_voice', 'Abigail.wav')
              }}
              title="Pre-fill for Chatterbox-TTS-Server (default port 8004)."
            >
              Chatterbox
            </button>
            <button
              type="button"
              className={styles.button}
              onClick={() => {
                const host = hostFromUrl(str('tts.openai_compat.url', '')) || 'localhost'
                set('tts.openai_compat.url', `http://${host}:8880/v1`)
                set('tts.openai_compat.model', 'kokoro')
                set('tts.openai_compat.default_voice', 'af_sky')
              }}
              title="Pre-fill for Kokoro-FastAPI (default port 8880)."
            >
              Kokoro
            </button>
            <button
              type="button"
              className={styles.button}
              onClick={() => {
                const host = hostFromUrl(str('tts.openai_compat.url', '')) || 'localhost'
                set('tts.openai_compat.url', `http://${host}:8000/v1`)
                set('tts.openai_compat.model', 'tts-1')
                set('tts.openai_compat.default_voice', 'alloy')
              }}
              title="Pre-fill for OpenedAI-Speech (default port 8000)."
            >
              OpenedAI-Speech
            </button>
          </div>
        </Field>
        <Field label="Base URL" desc="Server URL including the /v1 prefix.">
          <TextInput
            value={str('tts.openai_compat.url', 'http://localhost:8000/v1')}
            onChange={(v) => set('tts.openai_compat.url', v)}
            placeholder="http://localhost:8000/v1"
            mono
          />
        </Field>
        <Field label="API key (optional)" desc="Bearer token. Many self-hosted servers run open inside a LAN.">
          <TextInput
            value={str('tts.openai_compat.api_key')}
            onChange={(v) => set('tts.openai_compat.api_key', v)}
            placeholder="(none)"
            type="password" mono
          />
        </Field>
        <Field label="Model" desc="Model id the server expects.">
          <TextInput
            value={str('tts.openai_compat.model', 'tts-1')}
            onChange={(v) => set('tts.openai_compat.model', v)}
            placeholder="tts-1"
            mono
          />
        </Field>
        <Field
          label="Default voice"
          desc={
            !compatBackendWired
              ? 'Save the URL and reload to discover voices from the server.'
              : compatVoicesFetching
                ? 'Probing server for available voices…'
                : (compatVoices?.length ?? 0) > 1
                  ? `Autocomplete from ${compatVoices!.length} voices the server advertises. Free entry is still allowed.`
                  : 'Server did not advertise a voice list — type the id manually.'
          }
        >
          <div style={{ display: 'flex', gap: '6px', alignItems: 'center' }}>
            <div style={{ flex: 1 }}>
              <ComboInput
                value={str('tts.openai_compat.default_voice', 'alloy')}
                onChange={(v) => set('tts.openai_compat.default_voice', v)}
                placeholder="alloy"
                mono
                suggestions={(compatVoices ?? []).map((v) => v.id)}
              />
            </div>
            <button
              type="button"
              className={styles.button}
              disabled={!compatBackendWired || compatVoicesFetching}
              onClick={() => { void refetchCompatVoices() }}
              title="Re-probe the server for available voices."
            >
              {compatVoicesFetching ? '…' : 'Reload'}
            </button>
          </div>
        </Field>
        <Field label="Playback volume" desc="Web playback gain. 100 % = unaltered, up to 200 % to boost quiet voices. Applied client-side; saved as tts.openai_compat.volume.">
          <VolumeSlider
            value={num('tts.openai_compat.volume', 1.0)}
            onChange={(v) => set('tts.openai_compat.volume', v)}
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="ElevenLabs (cloud)" defaultOpen={backend === 'elevenlabs'}>
        <p className={styles.sectionDesc}>
          Premium expressive voices via api.elevenlabs.io. Backend wiring
          ships in a later stage; configuration here persists so the
          credential is ready when the runtime adapter lands.
        </p>
        <Field label="API key" desc="ElevenLabs API key. Stored in the config file — keep it secure.">
          <TextInput
            value={str('tts.elevenlabs.api_key')}
            onChange={(v) => set('tts.elevenlabs.api_key', v)}
            placeholder="(none)"
            type="password" mono
          />
        </Field>
        <Field label="Model" desc="ElevenLabs model id. eleven_turbo_v2_5 is the low-latency default.">
          <SelectInput
            value={str('tts.elevenlabs.model', 'eleven_turbo_v2_5')}
            onChange={(v) => set('tts.elevenlabs.model', v)}
            options={[
              { value: 'eleven_turbo_v2_5',     label: 'eleven_turbo_v2_5 (fast, English+)' },
              { value: 'eleven_multilingual_v2', label: 'eleven_multilingual_v2 (29 languages)' },
              { value: 'eleven_monolingual_v1', label: 'eleven_monolingual_v1 (legacy)' },
            ]}
          />
        </Field>
        <Field label="Default voice id" desc="ElevenLabs voice id (e.g. Rachel = 21m00Tcm4TlvDq8ikWAM). Browse voices at elevenlabs.io/app/voice-library.">
          <TextInput
            value={str('tts.elevenlabs.default_voice_id', '21m00Tcm4TlvDq8ikWAM')}
            onChange={(v) => set('tts.elevenlabs.default_voice_id', v)}
            placeholder="21m00Tcm4TlvDq8ikWAM"
            mono
          />
        </Field>
        <Field label="Playback volume" desc="Web playback gain. 100 % = unaltered, up to 200 % to boost quiet voices. Applied client-side; saved as tts.elevenlabs.volume.">
          <VolumeSlider
            value={num('tts.elevenlabs.volume', 1.0)}
            onChange={(v) => set('tts.elevenlabs.volume', v)}
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="Cartesia (cloud)" defaultOpen={backend === 'cartesia'}>
        <p className={styles.sectionDesc}>
          Cartesia Sonic — sub-second-latency neural voices via
          api.cartesia.ai. Backend wiring ships in a later stage; the
          credential and voice id persist here so the runtime adapter
          picks them up when it lands.
        </p>
        <Field label="API key" desc="Cartesia API key. Stored in the config file — keep it secure.">
          <TextInput
            value={str('tts.cartesia.api_key')}
            onChange={(v) => set('tts.cartesia.api_key', v)}
            placeholder="(none)"
            type="password" mono
          />
        </Field>
        <Field label="Model" desc="Cartesia model id. sonic-english is the production default; sonic-multilingual covers ~15 languages.">
          <SelectInput
            value={str('tts.cartesia.model', 'sonic-english')}
            onChange={(v) => set('tts.cartesia.model', v)}
            options={[
              { value: 'sonic-english',      label: 'sonic-english (low latency)' },
              { value: 'sonic-multilingual', label: 'sonic-multilingual (~15 languages)' },
            ]}
          />
        </Field>
        <Field label="Default voice id" desc="Cartesia voice UUID. Empty = backend default for the model.">
          <TextInput
            value={str('tts.cartesia.default_voice_id', '')}
            onChange={(v) => set('tts.cartesia.default_voice_id', v)}
            placeholder="(backend default)"
            mono
          />
        </Field>
        <Field label="Playback volume" desc="Web playback gain. 100 % = unaltered, up to 200 % to boost quiet voices. Applied client-side; saved as tts.cartesia.volume.">
          <VolumeSlider
            value={num('tts.cartesia.volume', 1.0)}
            onChange={(v) => set('tts.cartesia.volume', v)}
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="Per-channel routing">
        <p className={styles.sectionDesc}>
          Pin a channel to a specific backend. Leave on <code>(use default)</code>
          to follow the <em>Default backend</em> selection above. Useful when
          you want, say, the chat 🔊 button to use OpenAI-compat but a
          messaging channel to keep using local Piper.
        </p>
        <ChannelRoutingRows str={str} set={set} />
      </CollapsibleSection>

      <CollapsibleSection title="Per-channel voice defaults">
        <p className={styles.sectionDesc}>
          Server-wide defaults for when MIRA replies with voice and which
          voice id to use, per channel. Each user can override these in their
          profile. <em>Inherit</em> means the built-in fallback (
          <code>never</code>) applies — set <code>always</code> if voice
          replies should be the default behavior on a channel.
        </p>
        <ChannelVoicePrefsRows str={str} set={set} />
      </CollapsibleSection>

      <CollapsibleSection title="Cache">
        <Field label="Enabled" desc="Memoise synth results so repeat plays of the same message skip the backend.">
          <Toggle
            value={bool('tts.cache.enabled', true)}
            onChange={(v) => set('tts.cache.enabled', v)}
          />
        </Field>
        <Field label="Disk cap (MB)" desc="Maximum on-disk size before LRU eviction kicks in.">
          <NumberInput
            value={num('tts.cache.max_disk_mb', 100)}
            onChange={(v) => set('tts.cache.max_disk_mb', v)}
            min={10} max={10_000}
          />
        </Field>
        <Field label="TTL (days)" desc="Entries older than this are swept on startup and once a day.">
          <NumberInput
            value={num('tts.cache.ttl_days', 30)}
            onChange={(v) => set('tts.cache.ttl_days', v)}
            min={1} max={365}
          />
        </Field>
      </CollapsibleSection>

      {/* ── Speech-to-Text ──────────────────────────────────── */}
      <Section title="Speech-to-Text">
        <p className={styles.sectionDesc}>
          Powers the mic button in the chat composer and the inbound
          voice-note path on Telegram / Signal. Internal whisper.cpp runs
          on-device with no network call; the cloud backends offer faster
          first-token latency on slower hardware.
        </p>
        <Field label="Enabled" desc="Master switch for the STT subsystem.">
          <Toggle
            value={sttEnabled}
            onChange={(v) => {
              set('stt.enabled', v)
              qc.invalidateQueries({ queryKey: ['stt-status'] })
            }}
          />
        </Field>
        {sttEnabled && sttStatus && (
          <Field
            label={`Active: ${sttStatus.backend}`}
            desc={
              sttStatus.healthy
                ? `Healthy${sttStatus.latency_ms != null ? ` · ${sttStatus.latency_ms} ms last probe` : ''}${sttStatus.note ? ` · ${sttStatus.note}` : ''}`
                : `Unhealthy${sttStatus.note ? ` — ${sttStatus.note}` : ''}`
            }
          >
            <span style={{
              display: 'inline-block',
              padding: '3px 10px',
              borderRadius: 'var(--radius-full)',
              background: sttStatus.healthy ? 'var(--accent-bg, var(--bg-overlay))' : 'var(--bg-overlay)',
              color:      sttStatus.healthy ? 'var(--accent, var(--text-secondary))' : 'var(--text-muted)',
              fontSize:   '11px',
              fontFamily: 'var(--font-mono)',
            }}>
              {sttStatus.healthy ? 'OK' : 'DOWN'}
            </span>
          </Field>
        )}
        {sttEnabled && sttStatus && (
          <Field label="Wired backends" desc="Backends that loaded successfully at startup.">
            <span style={{ color: 'var(--text-muted)', fontFamily: 'var(--font-mono)', fontSize: '11px' }}>
              {sttStatus.backends.join(' · ') || '(none)'}
            </span>
          </Field>
        )}
      </Section>

      <Section title="STT defaults">
        <Field label="Default backend" desc="Which STT backend transcribes when a request doesn't pin one. Per-channel pinning below overrides this.">
          <SelectInput
            value={sttBackend}
            onChange={(v) => set('stt.default_backend', v)}
            options={STT_BACKENDS.map(b => ({
              value: b.value,
              label: `${b.label}  (${b.privacy})`,
            }))}
          />
        </Field>
        <Field label="Default language" desc="ISO 639-1 hint passed to the backend (e.g. 'en', 'fr'). Empty = auto-detect.">
          <TextInput
            value={str('stt.default_language')}
            onChange={(v) => set('stt.default_language', v)}
            placeholder="(auto-detect)"
            mono
          />
        </Field>
        <Field label="Max audio seconds" desc="Hard cap on a single transcription. Anything longer is rejected at the API.">
          <NumberInput
            value={num('stt.max_audio_seconds', 600)}
            onChange={(v) => set('stt.max_audio_seconds', v)}
            min={5} max={3600}
          />
        </Field>
        <Field label="Request timeout (seconds)" desc="Per-request timeout against the backend.">
          <NumberInput
            value={num('stt.request_timeout_secs', 60)}
            onChange={(v) => set('stt.request_timeout_secs', v)}
            min={5} max={600}
          />
        </Field>
      </Section>

      <CollapsibleSection title="Internal (whisper.cpp)" defaultOpen={sttBackend === 'internal'}>
        <p className={styles.sectionDesc}>
          Runs entirely on this machine. The first transcription downloads
          the chosen ggml model from huggingface.co; subsequent calls are
          instant (no network).
        </p>
        <Field label="Model" desc="Trade-off between accuracy and speed. base.en is the default — fast, English-only, good enough for voice notes.">
          <SelectInput
            value={str('stt.internal.model', 'base.en')}
            onChange={(v) => set('stt.internal.model', v)}
            options={WHISPER_MODELS}
          />
        </Field>
        <Field label="Auto-download model" desc="Fetch the ggml file from huggingface.co on first use. Off = require manual install in models_dir.">
          <Toggle
            value={bool('stt.internal.auto_download_model', true)}
            onChange={(v) => set('stt.internal.auto_download_model', v)}
          />
        </Field>
        <Field label="Models directory" desc="Override for <data_dir>/stt/models. Empty = use the default.">
          <TextInput
            value={str('stt.internal.models_dir')}
            onChange={(v) => set('stt.internal.models_dir', v)}
            placeholder="(default)"
            mono
          />
        </Field>
        <Field label="Threads" desc="CPU threads whisper.cpp uses. 0 = auto (one per physical core).">
          <NumberInput
            value={num('stt.internal.threads', 0)}
            onChange={(v) => set('stt.internal.threads', v)}
            min={0} max={64}
          />
        </Field>
        <Field label="Use GPU" desc="Enable GPU offload if whisper.cpp was compiled with CUDA/Metal/Vulkan support. Safe to leave on — it falls back to CPU if unavailable.">
          <Toggle
            value={bool('stt.internal.use_gpu', false)}
            onChange={(v) => set('stt.internal.use_gpu', v)}
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="OpenAI Whisper (cloud)" defaultOpen={sttBackend === 'openai'}>
        <p className={styles.sectionDesc}>
          Uploads the audio to api.openai.com. Falls back to the
          <code> OPENAI_API_KEY </code> environment variable when no key
          is set here.
        </p>
        <Field label="API key" desc="OpenAI API key. Stored in the config file — keep it secure.">
          <TextInput
            value={str('stt.openai.api_key')}
            onChange={(v) => set('stt.openai.api_key', v)}
            placeholder="sk-…"
            type="password" mono
          />
        </Field>
        <Field label="Base URL" desc="Override the OpenAI API base URL (advanced). Must include the /v1 prefix.">
          <TextInput
            value={str('stt.openai.base_url', 'https://api.openai.com/v1')}
            onChange={(v) => set('stt.openai.base_url', v)}
            placeholder="https://api.openai.com/v1"
            mono
          />
        </Field>
        <Field label="Model" desc="Whisper model id. whisper-1 is the canonical OpenAI choice.">
          <SelectInput
            value={str('stt.openai.model', 'whisper-1')}
            onChange={(v) => set('stt.openai.model', v)}
            options={[
              { value: 'whisper-1',                label: 'whisper-1 (default)' },
              { value: 'gpt-4o-transcribe',        label: 'gpt-4o-transcribe (newer, higher quality)' },
              { value: 'gpt-4o-mini-transcribe',   label: 'gpt-4o-mini-transcribe (cheaper)' },
            ]}
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="OpenAI-compatible STT (self-hosted)" defaultOpen={sttBackend === 'openai_compat'}>
        <p className={styles.sectionDesc}>
          Any server speaking OpenAI's <code>/v1/audio/transcriptions</code> spec
          — faster-whisper-server, LiteLLM, LocalAI, etc. Stays on your network.
        </p>
        <Field label="Base URL" desc="Server URL including the /v1 prefix.">
          <TextInput
            value={str('stt.openai_compat.url', 'http://localhost:8080/v1')}
            onChange={(v) => set('stt.openai_compat.url', v)}
            placeholder="http://localhost:8080/v1"
            mono
          />
        </Field>
        <Field label="API key (optional)" desc="Bearer token. Many self-hosted servers run open inside a LAN.">
          <TextInput
            value={str('stt.openai_compat.api_key')}
            onChange={(v) => set('stt.openai_compat.api_key', v)}
            placeholder="(none)"
            type="password" mono
          />
        </Field>
        <Field label="Model" desc="Model id the server expects. Defaults to whisper-1.">
          <TextInput
            value={str('stt.openai_compat.model', 'whisper-1')}
            onChange={(v) => set('stt.openai_compat.model', v)}
            placeholder="whisper-1"
            mono
          />
        </Field>
      </CollapsibleSection>

      <CollapsibleSection title="STT per-channel routing">
        <p className={styles.sectionDesc}>
          Pin a channel to a specific STT backend. Leave on <code>(use default)</code>
          to follow the <em>Default backend</em> selection above.
        </p>
        {([
          ['web',      'Web chat (mic button)'],
          ['tui',      'Terminal'],
          ['telegram', 'Telegram (voice notes)'],
          ['signal',   'Signal (voice notes)'],
        ] as const).map(([key, label]) => (
          <Field key={key} label={label} desc={`Backend used for stt.routing.${key}.`}>
            <SelectInput
              value={str(`stt.routing.${key}`, '')}
              onChange={(v) => set(`stt.routing.${key}`, v)}
              options={[
                { value: '', label: '(use default)' },
                ...STT_BACKENDS.map(b => ({ value: b.value, label: b.label })),
              ]}
            />
          </Field>
        ))}
      </CollapsibleSection>
    </div>
  )
}

// ── TTS test button ───────────────────────────────────────────────────────────
//
// Hits `/api/tts/speak` with a fixed short phrase using the active backend
// (channel `web` is default), plays the resulting audio, and toasts the
// outcome. Lets the user confirm a config change actually wired the backend
// through, instead of waiting for the next assistant reply to find out.
//
// Also nudges the `tts-status` query to refresh on success so the OK / DOWN
// badge mirrors what the speak call just proved.

function TestVoiceButton({
  qc, backend, voice, gain = 1.0, onVerified,
}: {
  qc: ReturnType<typeof useQueryClient>
  // backend/voice come from the in-memory form state, not the saved config —
  // that's intentional, so the user hears what they just picked even before
  // saving. The TtsService hot-reloads on save, but voice/backend overrides
  // on the request itself bypass live config entirely for this synth call.
  backend?: string
  voice?: string
  // Linear playback gain for the chosen backend. >1.0 amplifies; routed
  // through Web Audio because HTMLAudioElement caps `volume` at 1.0.
  gain?: number
  onVerified?: () => void
}) {
  const [busy, setBusy] = useState(false)
  const playRef = useRef<PlayHandle | null>(null)

  const onClick = async () => {
    if (busy) return
    setBusy(true)
    const t = toast.loading('Synthesising test clip…')
    try {
      const blob = await ttsApi.speak({
        text:    'MIRA voice test successful.',
        channel: 'web',
        backend: backend || undefined,
        voice:   voice || undefined,
      })
      // Tear down any previous test playback before starting a new one so
      // rapid clicks don't pile up overlapping audio.
      playRef.current?.stop()
      playRef.current = await playBlobWithGain(blob, gain)
      toast.success('Voice OK', { id: t })
      qc.invalidateQueries({ queryKey: ['tts-status'] })
      onVerified?.()
    } catch (e) {
      const msg = (e as { response?: { data?: { error?: string } }; message?: string })
        ?.response?.data?.error ?? (e as Error)?.message ?? 'Unknown error'
      toast.error(`Voice test failed: ${msg}`, { id: t })
    } finally {
      setBusy(false)
    }
  }

  return (
    <button
      onClick={onClick}
      disabled={busy}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        padding: '5px 12px',
        background: 'var(--bg-overlay)',
        border: '1px solid var(--border)',
        borderRadius: 'var(--radius-sm)',
        color: 'var(--text-primary)',
        fontSize: 12,
        cursor: busy ? 'wait' : 'pointer',
        opacity: busy ? 0.6 : 1,
      }}
    >
      <Volume2 size={13} />
      {busy ? 'Testing…' : 'Test voice'}
    </button>
  )
}

// ── Advanced tab ──────────────────────────────────────────────────────────────

function AdvancedTab({
  json, error, onChange, set, str, num,
}: {
  json: string; error: string; onChange: (v: string) => void
  set: (p: string, v: unknown) => void
  str: (p: string, fb?: string) => string
  num: (p: string, fb?: number) => number
}) {
  return (
    <div className={styles.tabBody}>
      <Section title="Logging">
        <p className={styles.sectionDesc}>
          Logs are written to a file so the terminal stays clean. Rotation is automatic — once a file passes the size cap a new one is started.
        </p>
        <Field label="Level" desc="Minimum log level recorded. 'info' for normal use, 'debug' or 'trace' for troubleshooting.">
          <SelectInput
            value={str('logging.level', 'info')}
            onChange={(v) => set('logging.level', v)}
            options={[
              { value: 'trace', label: 'trace' },
              { value: 'debug', label: 'debug' },
              { value: 'info',  label: 'info' },
              { value: 'warn',  label: 'warn' },
              { value: 'error', label: 'error' },
            ]}
          />
        </Field>
        <Field label="Format" desc="'compact' is single-line human-readable, 'pretty' is multi-line with colour, 'json' is for log aggregators (Loki, Datadog).">
          <SelectInput
            value={str('logging.format', 'compact')}
            onChange={(v) => set('logging.format', v)}
            options={[
              { value: 'compact', label: 'compact' },
              { value: 'pretty',  label: 'pretty' },
              { value: 'json',    label: 'json' },
            ]}
          />
        </Field>
        <Field label="Log file" desc="Path to the active log file. Supports ~ expansion. Parent directory is created on demand.">
          <TextInput value={str('logging.file', '~/.mira/mira.log')} onChange={(v) => set('logging.file', v)} placeholder="~/.mira/mira.log" mono />
        </Field>
        <Field label="Max file size (MB)" desc="Active file rotates when it grows past this size.">
          <NumberInput value={num('logging.max_file_size_mb', 50)} onChange={(v) => set('logging.max_file_size_mb', v)} min={1} max={10240} />
        </Field>
        <Field label="Retained rotations" desc="Number of rotated log files to keep, including the active one.">
          <NumberInput value={num('logging.max_files', 5)} onChange={(v) => set('logging.max_files', v)} min={1} max={100} />
        </Field>
      </Section>

      <Section title="Raw JSON editor">
        <p className={styles.sectionDesc}>
          Direct access to the full configuration as JSON. Changes here override the form fields above.
          Be careful — invalid JSON or wrong field types will be rejected by the server.
        </p>
        {error && <div className={styles.inlineError}>{error}</div>}
        <textarea
          className={styles.jsonEditor}
          value={json}
          onChange={(e) => onChange(e.target.value)}
          spellCheck={false}
        />
      </Section>
    </div>
  )
}
