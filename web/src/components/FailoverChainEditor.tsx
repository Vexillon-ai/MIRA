// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/FailoverChainEditor.tsx
//
// Settings → Providers → "Automatic failover". Edits `failover_providers`: the
// ordered list of providers MIRA falls back to when the primary fails. MIRA is
// fail-closed by default — only LOCAL providers are auto-fallbacks, so a local
// "heart" can't silently ship conversations to a cloud provider on a crash.
// Enabling a cloud provider here is an explicit, warned choice.
//
// `value` semantics mirror the server: `null`/undefined = the local-only
// default (computed here), an array = the explicit ordered chain. Any edit
// materialises the explicit array via onChange. Cloud classification MUST match
// src/gateway/builder.rs::provider_is_local / host_is_local.

import { useState } from 'react'

interface ProviderMeta { slug: string; label: string }

const PROVIDER_META: ProviderMeta[] = [
  { slug: 'lmstudio',      label: 'LM Studio' },
  { slug: 'ollama',        label: 'Ollama' },
  { slug: 'openai_compat', label: 'OpenAI-compatible (custom)' },
  { slug: 'openrouter',    label: 'OpenRouter' },
  { slug: 'openai',        label: 'OpenAI' },
  { slug: 'deepseek',      label: 'DeepSeek' },
  { slug: 'moonshot',      label: 'Moonshot (Kimi)' },
  { slug: 'groq',          label: 'Groq' },
  { slug: 'xai',           label: 'xAI (Grok)' },
  { slug: 'anthropic',     label: 'Anthropic (Claude)' },
  { slug: 'gemini',        label: 'Google Gemini' },
]

// Keep in lockstep with the Rust `host_is_local`.
function hostIsLocal(url: string): boolean {
  const after     = url.split('://')[1] ?? url
  const authority = (after.split(/[/?#]/)[0] ?? '').split('@').pop() ?? ''
  let host = authority.startsWith('[')
    ? authority.slice(1).split(']')[0]
    : (authority.split(':')[0] ?? '')
  host = host.trim().toLowerCase()
  if (!host) return false
  if (host === 'localhost' || host.endsWith('.localhost') || host.endsWith('.local')) return true
  const m = host.match(/^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$/)
  if (m) {
    const a = Number(m[1]), b = Number(m[2])
    return a === 127 || a === 10 || (a === 192 && b === 168) ||
           (a === 172 && b >= 16 && b <= 31) || (a === 169 && b === 254)
  }
  if (host.includes(':')) return host === '::1' || /^f[cd]/.test(host) || /^fe[89ab]/.test(host)
  return false
}

function isLocal(slug: string, openaiCompatUrl: string): boolean {
  if (slug === 'lmstudio' || slug === 'ollama') return true
  if (slug === 'openai_compat') return hostIsLocal(openaiCompatUrl)
  return false
}

function localDefault(primary: string, openaiCompatUrl: string): string[] {
  return PROVIDER_META
    .filter((p) => p.slug !== primary && isLocal(p.slug, openaiCompatUrl))
    .map((p) => p.slug)
}

const label = (slug: string) => PROVIDER_META.find((p) => p.slug === slug)?.label ?? slug

export default function FailoverChainEditor({
  value, primary, openaiCompatUrl, onChange,
}: {
  value:           string[] | null | undefined
  primary:         string
  openaiCompatUrl: string
  onChange:        (list: string[]) => void
}) {
  const [dragIdx, setDragIdx] = useState<number | null>(null)

  // Effective enabled fallback list (explicit value, or the local-only default).
  const enabled: string[] = Array.isArray(value)
    ? value.filter((s) => s !== primary)
    : localDefault(primary, openaiCompatUrl)
  const enabledSet = new Set(enabled)
  const available  = PROVIDER_META.filter((p) => p.slug !== primary && !enabledSet.has(p.slug))
  const cloud      = (slug: string) => !isLocal(slug, openaiCompatUrl)

  // Every edit writes an explicit array (materialising the default on first change).
  const commit = (list: string[]) => onChange(list.filter((s) => s !== primary))
  const add    = (slug: string) => commit([...enabled, slug])
  const remove = (slug: string) => commit(enabled.filter((s) => s !== slug))
  const move   = (from: number, to: number) => {
    if (to < 0 || to >= enabled.length || from === to) return
    const next = [...enabled]
    const [x] = next.splice(from, 1)
    next.splice(to, 0, x)
    commit(next)
  }

  const s = STYLES
  return (
    <div style={s.wrap}>
      <p style={s.intro}>
        MIRA stays local by default. If the primary model fails, it tries these in order.
        Only local providers are enabled by default — enabling a cloud provider here means your
        conversations can be sent to that provider when the local model is unavailable.
      </p>

      <div style={s.primaryRow}>
        <span style={s.pin}>PRIMARY</span>
        <span style={s.name}>{label(primary)}</span>
        {cloud(primary) && <span style={s.cloudTag}>cloud</span>}
      </div>

      {enabled.map((slug, i) => (
        <div
          key={slug}
          style={{ ...s.row, ...(dragIdx === i ? s.rowDragging : {}) }}
          draggable
          onDragStart={() => setDragIdx(i)}
          onDragOver={(e) => e.preventDefault()}
          onDrop={() => { if (dragIdx !== null) move(dragIdx, i); setDragIdx(null) }}
          onDragEnd={() => setDragIdx(null)}
        >
          <span style={s.handle} title="Drag to reorder">⠿</span>
          <span style={s.ord}>{i + 1}</span>
          <span style={s.name}>{label(slug)}</span>
          {cloud(slug) && (
            <span style={s.warn}>⚠️ conversations sent to {label(slug)} (cloud)</span>
          )}
          <span style={s.spacer} />
          <button style={s.iconBtn} onClick={() => move(i, i - 1)} disabled={i === 0} title="Move up">↑</button>
          <button style={s.iconBtn} onClick={() => move(i, i + 1)} disabled={i === enabled.length - 1} title="Move down">↓</button>
          <button style={s.removeBtn} onClick={() => remove(slug)} title="Remove from fallback">✕</button>
        </div>
      ))}

      {enabled.length === 0 && (
        <div style={s.empty}>
          No automatic fallback. If the primary fails, MIRA returns an error rather than reaching
          for another provider (fully fail-closed).
        </div>
      )}

      {available.length > 0 && (
        <div style={s.addWrap}>
          <span style={s.addLabel}>Add a fallback:</span>
          {available.map((p) => (
            <button key={p.slug} style={s.addChip} onClick={() => add(p.slug)}>
              + {p.label}{cloud(p.slug) ? ' ⚠️' : ''}
            </button>
          ))}
        </div>
      )}
      <p style={s.foot}>Providers you haven't configured are ignored until they're set up.</p>
    </div>
  )
}

const STYLES: Record<string, React.CSSProperties> = {
  wrap:       { display: 'flex', flexDirection: 'column', gap: 8, fontSize: 13 },
  intro:      { margin: 0, fontSize: 12, color: 'var(--text-muted)', lineHeight: 1.5 },
  primaryRow: { display: 'flex', alignItems: 'center', gap: 8, padding: '6px 10px', borderRadius: 6, background: 'var(--surface-2, #1b1b1b)', border: '1px solid var(--border, #2a2a2a)' },
  pin:        { fontSize: 10, fontWeight: 700, letterSpacing: 0.5, color: 'var(--accent)' },
  row:        { display: 'flex', alignItems: 'center', gap: 8, padding: '6px 10px', borderRadius: 6, background: 'var(--surface-1, #131313)', border: '1px solid var(--border, #2a2a2a)', cursor: 'grab' },
  rowDragging:{ opacity: 0.5 },
  handle:     { cursor: 'grab', color: 'var(--text-muted)', userSelect: 'none' },
  ord:        { minWidth: 14, color: 'var(--text-muted)' },
  name:       { fontWeight: 500 },
  cloudTag:   { fontSize: 10, padding: '1px 6px', borderRadius: 10, background: '#5a4300', color: '#ffd479' },
  warn:       { fontSize: 11, color: '#e0a33a' },
  spacer:     { flex: 1 },
  iconBtn:    { border: '1px solid var(--border, #2a2a2a)', background: 'transparent', color: 'var(--text)', borderRadius: 4, cursor: 'pointer', width: 22, height: 22 },
  removeBtn:  { border: '1px solid var(--border, #2a2a2a)', background: 'transparent', color: '#d9534f', borderRadius: 4, cursor: 'pointer', width: 22, height: 22 },
  empty:      { fontSize: 12, color: 'var(--text-muted)', padding: '6px 10px', border: '1px dashed var(--border, #2a2a2a)', borderRadius: 6 },
  addWrap:    { display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 6, marginTop: 2 },
  addLabel:   { fontSize: 12, color: 'var(--text-muted)' },
  addChip:    { border: '1px solid var(--border, #2a2a2a)', background: 'transparent', color: 'var(--text)', borderRadius: 14, padding: '3px 10px', cursor: 'pointer', fontSize: 12 },
  foot:       { margin: 0, fontSize: 11, color: 'var(--text-muted)' },
}
