// SPDX-License-Identifier: AGPL-3.0-or-later

import { useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { Package, ShieldCheck, ShieldAlert, ShieldQuestion, Shield, Trash2, Power, PowerOff, ArrowUpCircle, Ban, RefreshCw, Download, Globe } from 'lucide-react'
import {
  packagesApi,
  type Capabilities,
  type CatalogEntry,
  type InstalledPackage,
  type PreviewResponse,
  type TrustLevel,
} from '@/api/packages'
import CppInstallWizard from './CppInstallWizard'
import styles from './PluginsPage.module.css'

function TrustBadge({ trust }: { trust: TrustLevel | string }) {
  const level = typeof trust === 'string' ? trust : trust.level
  switch (level) {
    case 'verified':
      return <span className={`${styles.badge} ${styles.verified}`}><ShieldCheck size={14} /> Verified{typeof trust !== 'string' && trust.level === 'verified' ? ` — ${trust.publisher || 'trusted'}` : ''}</span>
    case 'invalid':
      return <span className={`${styles.badge} ${styles.invalid}`}><ShieldAlert size={14} /> Invalid signature</span>
    case 'untrusted':
      return <span className={`${styles.badge} ${styles.untrusted}`}><ShieldQuestion size={14} /> Untrusted publisher</span>
    default:
      return <span className={`${styles.badge} ${styles.unsigned}`}><Shield size={14} /> Unsigned</span>
  }
}

function CapList({ caps }: { caps: Capabilities }) {
  const items: string[] = []
  if (caps.network_egress?.length) items.push(`net → ${caps.network_egress.join(', ')}`)
  if (caps.secrets?.length) items.push(`secrets: ${caps.secrets.join(', ')}`)
  if (caps.subprocess) items.push(`subprocess${caps.subprocess_allowlist?.length ? `: ${caps.subprocess_allowlist.join(', ')}` : ''}`)
  if (caps.filesystem?.length) items.push(`fs: ${caps.filesystem.join(', ')}`)
  if (caps.listen_port) items.push(`listen :${caps.listen_port}`)
  return <span className={styles.caps}>{items.length ? items.join('  ·  ') : 'no special capabilities'}</span>
}

export default function PluginsPage() {
  const qc = useQueryClient()
  const [file, setFile] = useState<File | null>(null)
  // D1 — when the package came from a URL, remember it so Install re-fetches by
  // URL instead of needing the file bytes.
  const [sourceUrl, setSourceUrl] = useState<string | null>(null)
  const [urlInput, setUrlInput] = useState('')
  const [preview, setPreview] = useState<PreviewResponse | null>(null)
  const [config, setConfig] = useState('{}')
  const [ack, setAck] = useState(false)

  const installed = useQuery<InstalledPackage[]>({
    queryKey: ['installed-packages'],
    queryFn: () => packagesApi.list(),
  })

  // Shared: show a fetched/parsed preview and pre-fill the config editor.
  const applyPreview = (d: PreviewResponse) => {
    setPreview(d)
    setAck(false)
    const env: Record<string, string> = {}
    for (const c of d.manifest.components) {
      const e = (c.spec?.env ?? {}) as Record<string, string>
      for (const k of Object.keys(e)) env[k] = e[k] ?? ''
    }
    setConfig(JSON.stringify(env, null, 2))
  }

  const previewMut = useMutation({
    mutationFn: (f: File) => packagesApi.preview(f),
    onSuccess: (d) => { setSourceUrl(null); applyPreview(d) },
    onError: (e: any) => toast.error(`Preview failed: ${e?.response?.data?.error ?? e?.message ?? e}`),
  })

  // D1 — preview a package fetched by URL.
  const previewUrlMut = useMutation({
    mutationFn: (url: string) => packagesApi.previewUrl(url),
    onSuccess: (d, url) => { setFile(null); setSourceUrl(url); applyPreview(d) },
    onError: (e: any) => toast.error(`Fetch failed: ${e?.response?.data?.error ?? e?.message ?? e}`),
  })

  const resetInstaller = () => {
    setPreview(null); setFile(null); setSourceUrl(null); setUrlInput(''); setConfig('{}'); setAck(false)
  }

  const installMut = useMutation({
    mutationFn: () => {
      let parsed: Record<string, string> = {}
      try { parsed = JSON.parse(config || '{}') } catch { throw new Error('config is not valid JSON') }
      return sourceUrl
        ? packagesApi.installUrl(sourceUrl, parsed, ack)
        : packagesApi.install(file!, parsed, ack)
    },
    onSuccess: () => {
      toast.success('Package installed — tools are loading now.')
      resetInstaller()
      qc.invalidateQueries({ queryKey: ['installed-packages'] })
      qc.invalidateQueries({ queryKey: ['package-updates'] })
    },
    onError: (e: any) => toast.error(`Install failed: ${e?.response?.data?.error ?? e?.message ?? e}`),
  })

  const uninstallMut = useMutation({
    mutationFn: (id: string) => packagesApi.uninstall(id),
    onSuccess: () => {
      toast.success('Package uninstalled.')
      qc.invalidateQueries({ queryKey: ['installed-packages'] })
    },
    onError: (e: any) => toast.error(`Uninstall failed: ${e?.response?.data?.error ?? e?.message ?? e}`),
  })

  const toggleMut = useMutation({
    mutationFn: ({ id, disabled }: { id: string; disabled: boolean }) =>
      disabled ? packagesApi.enable(id) : packagesApi.disable(id),
    onSuccess: (_d, v) => {
      toast.success(v.disabled ? 'Package enabled.' : 'Package disabled.')
      qc.invalidateQueries({ queryKey: ['installed-packages'] })
    },
    onError: (e: any) => toast.error(`Failed: ${e?.response?.data?.error ?? e?.message ?? e}`),
  })

  const list = useMemo(() => installed.data ?? [], [installed.data])

  // A cpp_provider package installs via the guided wizard, not the one-shot
  // config-textarea flow.
  const cppComponent = useMemo(
    () => preview?.manifest.components.find((c) => c.type === 'cpp_provider') ?? null,
    [preview],
  )

  const closeWizard = () => { resetInstaller() }
  const finishWizard = () => { closeWizard(); qc.invalidateQueries({ queryKey: ['installed-packages'] }) }

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1><Package size={18} style={{ verticalAlign: 'text-bottom', marginRight: 8 }} />Plugin Packages</h1>
        <p>Upload a <code>.mirapkg</code> — or paste its URL — to verify its trust, then install. MCP-server packages install in one step; channel-provider packages run a short guided setup.</p>
      </header>

      <div className={styles.body}>
        <label className={styles.upload}>
          <input
            type="file"
            accept=".mirapkg,.tar.gz,application/gzip"
            onChange={(e) => {
              const f = e.target.files?.[0]
              if (f) { setFile(f); previewMut.mutate(f) }
            }}
          />
          {previewMut.isPending ? 'Verifying…' : 'Choose a .mirapkg to preview'}
        </label>

        {/* D1 — install from URL. Admin-initiated, SSRF-guarded fetch; the
            trust decision is still per-artifact (signature + trust store). */}
        <form
          className={styles.urlRow}
          onSubmit={(e) => { e.preventDefault(); const u = urlInput.trim(); if (u) previewUrlMut.mutate(u) }}
        >
          <input
            type="url"
            className={styles.urlInput}
            placeholder="…or paste a .mirapkg URL (https://…)"
            value={urlInput}
            onChange={(e) => setUrlInput(e.target.value)}
          />
          <button type="submit" className={styles.urlBtn} disabled={previewUrlMut.isPending || !urlInput.trim()}>
            {previewUrlMut.isPending ? 'Fetching…' : 'Fetch & preview'}
          </button>
        </form>

        {preview && (
          <div className={styles.card}>
            <div className={styles.cardHead}>
              <div>
                <h3 className={styles.title}>
                  {preview.manifest.name} <span className={styles.ver}>v{preview.manifest.version}</span>
                </h3>
                <p className={styles.id}>
                  {preview.manifest.id}{preview.manifest.publisher ? `  ·  ${preview.manifest.publisher}` : ''}
                </p>
                {preview.manifest.description && <p className={styles.desc}>{preview.manifest.description}</p>}
              </div>
              <TrustBadge trust={preview.trust} />
            </div>

            {preview.installed_version && (
              <p className={styles.updateNote}>
                {preview.update
                  ? <><ArrowUpCircle size={14} /> Update available — v{preview.installed_version} → v{preview.manifest.version}</>
                  : preview.update_blocked
                    ? <><Ban size={14} /> Already installed (v{preview.installed_version}). {preview.update_blocked}</>
                    : <>Already installed at v{preview.installed_version}.</>}
              </p>
            )}

            <div className={styles.components}>
              {preview.manifest.components.map((c, i) => (
                <div key={i} className={styles.component}>
                  <span className={styles.kind}>{c.type}</span>
                  <span className={styles.runtime}>{c.runtime}</span>
                  <CapList caps={c.capabilities} />
                </div>
              ))}
            </div>

            {!cppComponent && (
              <div className={styles.installRow}>
                <label className={styles.configLabel}>Config (JSON — fill in secrets / values)</label>
                <textarea
                  className={styles.textarea}
                  rows={Math.min(10, Math.max(3, config.split('\n').length))}
                  value={config}
                  onChange={(e) => setConfig(e.target.value)}
                  spellCheck={false}
                />
                {preview.trust.level !== 'verified' && (
                  <label className={styles.ackRow}>
                    <input type="checkbox" checked={ack} onChange={(e) => setAck(e.target.checked)} />
                    <span>This package is <strong>{preview.trust.level}</strong> — install anyway (I trust its publisher).</span>
                  </label>
                )}
                <button
                  className={styles.installBtn}
                  disabled={installMut.isPending || (!file && !sourceUrl) || (preview.trust.level !== 'verified' && !ack)}
                  onClick={() => installMut.mutate()}
                >
                  {installMut.isPending ? 'Installing…' : 'Install'}
                </button>
              </div>
            )}
          </div>
        )}

        {preview && cppComponent && (file || sourceUrl) && (
          <CppInstallWizard
            file={file ?? undefined}
            sourceUrl={sourceUrl ?? undefined}
            manifest={preview.manifest}
            component={cppComponent}
            trust={preview.trust}
            update={preview.update}
            onDone={finishWizard}
            onClose={closeWizard}
          />
        )}

        <UpdatesSection
          onUpdate={(url) => previewUrlMut.mutate(url)}
          picking={previewUrlMut.isPending}
        />

        <BrowseCatalog
          onPick={(url) => previewUrlMut.mutate(url)}
          picking={previewUrlMut.isPending}
        />

        <section className={styles.installedSection}>
          <h2 className={styles.sectionTitle}>Installed</h2>
          {installed.isLoading && <p className={styles.muted}>Loading…</p>}
          {!installed.isLoading && list.length === 0 && (
            <p className={styles.muted}>No packages installed yet.</p>
          )}
          <div className={styles.list}>
            {list.map((p) => {
              const disabled = p.state === 'disabled'
              return (
              <div key={p.id} className={styles.installedCard}>
                <div>
                  <h3 className={styles.title}>
                    {p.name} <span className={styles.ver}>v{p.version}</span>
                    {disabled && <span className={styles.disabledTag}>disabled</span>}
                  </h3>
                  <p className={styles.id}>{p.id}</p>
                </div>
                <div className={styles.installedActions}>
                  <TrustBadge trust={p.trust} />
                  <button
                    className={styles.iconBtn}
                    title={disabled ? 'Enable' : 'Disable'}
                    disabled={toggleMut.isPending}
                    onClick={() => toggleMut.mutate({ id: p.id, disabled })}
                  >
                    {disabled ? <Power size={14} /> : <PowerOff size={14} />}
                  </button>
                  <button
                    className={styles.iconBtn}
                    title="Uninstall"
                    disabled={uninstallMut.isPending}
                    onClick={() => { if (confirm(`Uninstall "${p.name}"?`)) uninstallMut.mutate(p.id) }}
                  >
                    <Trash2 size={14} />
                  </button>
                </div>
              </div>
            )})}
          </div>
        </section>
      </div>
    </div>
  )
}

// D4 — "updates available" surface. Auto-loads (returns instantly with no
// network when every source is disabled, the default). Hidden when none.
// Updating = the normal install-from-URL path: picking runs the preview (which
// shows the authoritative update plan + capability-reapproval gate) then install.
function UpdatesSection({ onUpdate, picking }: { onUpdate: (url: string) => void; picking: boolean }) {
  const updates = useQuery({
    queryKey: ['package-updates'],
    queryFn: () => packagesApi.updates(),
  })
  const list = updates.data ?? []
  if (list.length === 0) return null

  return (
    <section className={styles.installedSection}>
      <h2 className={styles.sectionTitle}>
        <ArrowUpCircle size={16} style={{ verticalAlign: 'text-bottom', marginRight: 6 }} />
        Updates available ({list.length})
      </h2>
      <div className={styles.list}>
        {list.map((u) => (
          <div key={u.id} className={styles.card}>
            <div className={styles.cardHead}>
              <div>
                <span className={styles.pkgName}>{u.name}</span>
                <span className={styles.pkgId}> {u.id} · v{u.installed_version} → v{u.latest_version}</span>
              </div>
              <span className={styles.muted}>{u.source}</span>
            </div>
            <div className={styles.cardActions}>
              <button
                type="button"
                className={styles.urlBtn}
                disabled={picking}
                onClick={() => onUpdate(u.download_url)}
              >
                <ArrowUpCircle size={13} style={{ verticalAlign: 'text-bottom', marginRight: 4 }} />
                {picking ? 'Fetching…' : `Update to v${u.latest_version}`}
              </button>
            </div>
          </div>
        ))}
      </div>
    </section>
  )
}

// D3 — Browse installable packages from the configured sources. Loads on demand
// (it hits the network + verifies signatures). Picking a package runs the D1
// install-from-URL preview, so the admin sees the per-artifact trust + caps in
// the card above before confirming the install.
function BrowseCatalog({ onPick, picking }: { onPick: (url: string) => void; picking: boolean }) {
  const [open, setOpen] = useState(false)
  const catalog = useQuery({
    queryKey: ['package-catalog'],
    queryFn: () => packagesApi.catalog(),
    enabled: open,
  })

  const entries: CatalogEntry[] = catalog.data?.packages ?? []
  const sources = catalog.data?.sources ?? []

  return (
    <section className={styles.installedSection}>
      <div className={styles.browseHead}>
        <h2 className={styles.sectionTitle}><Globe size={16} style={{ verticalAlign: 'text-bottom', marginRight: 6 }} />Browse</h2>
        <button
          type="button"
          className={styles.urlBtn}
          onClick={() => { setOpen(true); catalog.refetch() }}
          disabled={catalog.isFetching}
        >
          <RefreshCw size={13} style={{ verticalAlign: 'text-bottom', marginRight: 4 }} />
          {catalog.isFetching ? 'Refreshing…' : open ? 'Refresh' : 'Load catalog'}
        </button>
      </div>

      {!open && <p className={styles.muted}>Load the catalog to see packages available from your configured sources.</p>}

      {open && sources.length > 0 && (
        <div className={styles.sourceLine}>
          {sources.map((s) => (
            <span key={s.name} className={styles.muted} title={s.error ?? ''}>
              {s.name}: {!s.enabled ? 'disabled'
                : s.error ? `error — ${s.error}`
                : `${s.package_count} package${s.package_count === 1 ? '' : 's'}${s.verified ? ' ✓ verified' : ' (unverified index)'}${s.from_cache ? ', cached' : ''}`}
            </span>
          ))}
        </div>
      )}

      {open && !catalog.isFetching && entries.length === 0 && (
        <p className={styles.muted}>No packages available. (The first-party source ships disabled until its repository is live — an admin enables it in <code>packages.sources</code>.)</p>
      )}

      <div className={styles.list}>
        {entries.map((e) => {
          const v = e.versions.find((x) => x.version === e.latest) ?? e.versions[0]
          return (
            <div key={`${e.source}:${e.id}`} className={styles.card}>
              <div className={styles.cardHead}>
                <div>
                  <span className={styles.pkgName}>{e.name}</span>
                  <span className={styles.pkgId}> {e.id} · v{e.latest} · {e.kind}</span>
                </div>
                <span className={styles.muted}>
                  {e.source}{e.source_verified ? ' ✓' : ''}
                </span>
              </div>
              {v?.summary && <p className={styles.muted}>{v.summary}</p>}
              <div className={styles.cardActions}>
                <button
                  type="button"
                  className={styles.urlBtn}
                  disabled={picking || !v?.download_url}
                  onClick={() => v?.download_url && onPick(v.download_url)}
                >
                  <Download size={13} style={{ verticalAlign: 'text-bottom', marginRight: 4 }} />
                  {picking ? 'Fetching…' : 'Install'}
                </button>
              </div>
            </div>
          )
        })}
      </div>
    </section>
  )
}
