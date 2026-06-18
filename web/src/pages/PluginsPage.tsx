// SPDX-License-Identifier: AGPL-3.0-or-later

import { useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { Package, ShieldCheck, ShieldAlert, ShieldQuestion, Shield, Trash2, Power, PowerOff, ArrowUpCircle, Ban } from 'lucide-react'
import {
  packagesApi,
  type Capabilities,
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
  const [preview, setPreview] = useState<PreviewResponse | null>(null)
  const [config, setConfig] = useState('{}')
  const [ack, setAck] = useState(false)

  const installed = useQuery<InstalledPackage[]>({
    queryKey: ['installed-packages'],
    queryFn: () => packagesApi.list(),
  })

  const previewMut = useMutation({
    mutationFn: (f: File) => packagesApi.preview(f),
    onSuccess: (d) => {
      setPreview(d)
      setAck(false)
      // Pre-fill the config editor with the union of component spec.env keys.
      const env: Record<string, string> = {}
      for (const c of d.manifest.components) {
        const e = (c.spec?.env ?? {}) as Record<string, string>
        for (const k of Object.keys(e)) env[k] = e[k] ?? ''
      }
      setConfig(JSON.stringify(env, null, 2))
    },
    onError: (e: any) => toast.error(`Preview failed: ${e?.response?.data?.error ?? e?.message ?? e}`),
  })

  const installMut = useMutation({
    mutationFn: () => {
      let parsed: Record<string, string> = {}
      try { parsed = JSON.parse(config || '{}') } catch { throw new Error('config is not valid JSON') }
      return packagesApi.install(file!, parsed, ack)
    },
    onSuccess: () => {
      toast.success('Package installed — tools are loading now.')
      setPreview(null); setFile(null); setConfig('{}'); setAck(false)
      qc.invalidateQueries({ queryKey: ['installed-packages'] })
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

  const closeWizard = () => { setPreview(null); setFile(null); setConfig('{}'); setAck(false) }
  const finishWizard = () => { closeWizard(); qc.invalidateQueries({ queryKey: ['installed-packages'] }) }

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <h1><Package size={18} style={{ verticalAlign: 'text-bottom', marginRight: 8 }} />Plugin Packages</h1>
        <p>Upload a <code>.mirapkg</code> to verify its trust, then install. MCP-server packages install in one step; channel-provider packages run a short guided setup.</p>
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
                  disabled={installMut.isPending || !file || (preview.trust.level !== 'verified' && !ack)}
                  onClick={() => installMut.mutate()}
                >
                  {installMut.isPending ? 'Installing…' : 'Install'}
                </button>
              </div>
            )}
          </div>
        )}

        {preview && cppComponent && file && (
          <CppInstallWizard
            file={file}
            manifest={preview.manifest}
            component={cppComponent}
            trust={preview.trust}
            update={preview.update}
            onDone={finishWizard}
            onClose={closeWizard}
          />
        )}

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
