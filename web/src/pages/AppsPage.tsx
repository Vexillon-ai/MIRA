// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useMemo, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { LayoutGrid } from 'lucide-react'
import { packagesApi, type InstalledPackage } from '@/api/packages'
import { appsApi } from '@/api/apps'

// An "app" is an installed package whose manifest carries an `app` component.
function isApp(pkg: InstalledPackage): boolean {
  const comps = (pkg.manifest?.components ?? []) as Array<{ type?: string }>
  return Array.isArray(comps) && comps.some((c) => c.type === 'app')
}

// Runtime egress lockdown for the app iframe. The UI is rendered via
// `srcDoc`, so the `connect-src 'none'` HEADER the server sets on the fetch does
// NOT govern the running iframe (a srcdoc document doesn't inherit a fetched
// response's headers). Inject the policy as a `<meta>` CSP into the document
// itself — combined with `sandbox="allow-scripts"` (opaque origin) this blocks
// the app from making its own network calls (fetch/img/WebSocket) to phone home
// or exfiltrate. `postMessage` to the parent is unaffected (not a network fetch).
// CSP metas only tighten, so an app's own policy can't loosen this.
const APP_IFRAME_CSP =
  "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; " +
  "img-src data: blob:; font-src data:; media-src data: blob:; connect-src 'none'"

function withAppCsp(html: string): string {
  const meta = `<meta http-equiv="Content-Security-Policy" content="${APP_IFRAME_CSP}">`
  const head = html.match(/<head[^>]*>/i)
  if (head) return html.slice(0, head.index! + head[0].length) + meta + html.slice(head.index! + head[0].length)
  const htmlTag = html.match(/<html[^>]*>/i)
  if (htmlTag) {
    const i = htmlTag.index! + htmlTag[0].length
    return html.slice(0, i) + `<head>${meta}</head>` + html.slice(i)
  }
  return `<!doctype html><head>${meta}</head>${html}`
}

// Settings form for an app that declares a `config_schema`. Non-secret fields
// are pre-filled with current values; secret fields (API keys) show whether one
// is saved and are only sent when the admin types a new value. Renders nothing
// for apps with no config schema.
function AppConfigPanel({ appId }: { appId: string }) {
  const qc = useQueryClient()
  const cfg = useQuery({ queryKey: ['app-config', appId], queryFn: () => appsApi.getConfig(appId) })
  const [form, setForm] = useState<Record<string, string>>({})

  useEffect(() => {
    if (!cfg.data) return
    const init: Record<string, string> = {}
    for (const f of cfg.data.config_schema) {
      init[f.key] = !f.secret && cfg.data.values[f.key] != null ? String(cfg.data.values[f.key]) : ''
    }
    setForm(init)
  }, [cfg.data])

  const save = useMutation({
    mutationFn: () => {
      const payload: Record<string, unknown> = {}
      for (const f of cfg.data!.config_schema) {
        if (f.secret) { if (form[f.key]) payload[f.key] = form[f.key] } // only send a typed secret
        else payload[f.key] = form[f.key] ?? ''
      }
      return appsApi.putConfig(appId, payload)
    },
    onSuccess: () => {
      toast.success('Saved — the app’s tools reloaded with the new settings.')
      qc.invalidateQueries({ queryKey: ['app-config', appId] })
    },
    onError: (e: any) => toast.error(`Save failed: ${e?.response?.data?.error ?? e?.message ?? 'error'}`),
  })

  if (cfg.isLoading || !cfg.data || cfg.data.config_schema.length === 0) return null
  const secretsSet = new Set(cfg.data.secrets_set)

  return (
    <div style={{ border: '1px solid var(--border, #444)', borderRadius: 8, padding: 16, display: 'flex', flexDirection: 'column', gap: 12, maxWidth: 560 }}>
      <strong style={{ fontSize: 13, textTransform: 'uppercase', letterSpacing: '.04em', opacity: 0.7 }}>Settings</strong>
      {cfg.data.config_schema.map((f) => (
        <label key={f.key} style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
          <span style={{ fontSize: 13 }}>{f.label ?? f.key}{f.required ? ' *' : ''}</span>
          <input
            type={f.secret ? 'password' : 'text'}
            value={form[f.key] ?? ''}
            placeholder={f.secret && secretsSet.has(f.key) ? '•••••••• saved — leave blank to keep' : (f.help ?? '')}
            onChange={(e) => setForm((s) => ({ ...s, [f.key]: e.target.value }))}
            style={{ padding: '7px 10px', borderRadius: 6, border: '1px solid var(--border, #444)', background: 'var(--bg, #1a1a1a)', color: 'inherit' }}
          />
          {f.help && <small style={{ opacity: 0.6 }}>{f.help}</small>}
        </label>
      ))}
      <div>
        <button
          onClick={() => save.mutate()}
          disabled={save.isPending}
          style={{ padding: '7px 16px', borderRadius: 6, border: 'none', background: 'var(--accent, #3b82f6)', color: '#fff', cursor: 'pointer' }}
        >
          {save.isPending ? 'Saving…' : 'Save settings'}
        </button>
      </div>
    </div>
  )
}

export default function AppsPage() {
  const installed = useQuery<InstalledPackage[]>({
    queryKey: ['installed-packages'],
    queryFn: () => packagesApi.list(),
  })
  const apps = useMemo(() => (installed.data ?? []).filter(isApp), [installed.data])

  const [selected, setSelected] = useState<string | null>(null)
  const [html, setHtml] = useState<string>('')
  const iframeRef = useRef<HTMLIFrameElement | null>(null)

  // Auto-select the first app once the list loads.
  useEffect(() => {
    if (!selected && apps.length) setSelected(apps[0].id)
  }, [apps, selected])

  // Fetch the selected app's UI entry HTML (admin-authenticated via the api
  // client) and render it into a sandboxed iframe via `srcDoc`. Sandboxing
  // without `allow-same-origin` gives the frame an opaque origin — it can't read
  // MIRA's Bearer token, and its only path back to MIRA is a postMessage the
  // broker below turns into an authenticated emit.
  useEffect(() => {
    if (!selected) { setHtml(''); return }
    let cancelled = false
    appsApi.getUi(selected)
      .then((h) => { if (!cancelled) setHtml(withAppCsp(h)) })
      .catch(() => { if (!cancelled) setHtml('<p style="font-family:sans-serif">Failed to load app UI.</p>') })
    return () => { cancelled = true }
  }, [selected])

  // Broker app→MIRA event emits from the sandboxed iframe.
  useEffect(() => {
    function onMessage(e: MessageEvent) {
      // Only accept messages from our own app iframe (opaque origin, so we can't
      // check e.origin — verify the source window instead).
      if (!iframeRef.current || e.source !== iframeRef.current.contentWindow) return
      const data = e.data
      if (!data || data.type !== 'mira.emit' || typeof data.event !== 'string') return
      if (!selected) return
      appsApi.emit(selected, data.event, data.payload ?? {})
        .then((r) => toast.success(`emitted ${r.event} (${r.severity})`))
        .catch((err) => toast.error(`emit failed: ${err?.response?.data ?? err?.message ?? 'error'}`))
    }
    window.addEventListener('message', onMessage)
    return () => window.removeEventListener('message', onMessage)
  }, [selected])

  return (
    <div style={{ padding: 24, display: 'flex', flexDirection: 'column', gap: 16 }}>
      <h1 style={{ display: 'flex', alignItems: 'center', gap: 8, margin: 0 }}>
        <LayoutGrid size={20} /> Apps
      </h1>
      <p style={{ opacity: 0.7, margin: 0 }}>
        Installed apps render their own UI here, sandboxed. Install or remove apps on the Plugins page.
      </p>

      {installed.isLoading && <p>Loading…</p>}
      {!installed.isLoading && apps.length === 0 && (
        <p>No apps installed. Install an app package on the <strong>Plugins</strong> page.</p>
      )}

      {apps.length > 0 && (
        <>
          <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
            {apps.map((a) => (
              <button
                key={a.id}
                onClick={() => setSelected(a.id)}
                style={{
                  padding: '6px 12px',
                  borderRadius: 6,
                  border: '1px solid var(--border, #444)',
                  background: selected === a.id ? 'var(--accent, #3b82f6)' : 'transparent',
                  color: selected === a.id ? '#fff' : 'inherit',
                  cursor: 'pointer',
                }}
              >
                {a.name}
              </button>
            ))}
          </div>
          {selected && <AppConfigPanel appId={selected} />}
          <iframe
            ref={iframeRef}
            title="app-ui"
            sandbox="allow-scripts"
            srcDoc={html}
            style={{
              width: '100%',
              height: 480,
              border: '1px solid var(--border, #444)',
              borderRadius: 8,
              background: '#fff',
            }}
          />
        </>
      )}
    </div>
  )
}
