// SPDX-License-Identifier: AGPL-3.0-or-later

import { useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import {
  Boxes, ShieldCheck, ShieldAlert, FileSignature,
  Upload, Trash2, AlertTriangle, X, KeyRound, RefreshCw, Plug,
} from 'lucide-react'
import { skillsApi, type SkillSummary, type SkillPermissions, type PreviewResponse } from '@/api/skills'
import { useAuthStore } from '@/store/authStore'
import styles from './SkillsPage.module.css'

export default function SkillsPage() {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const [installOpen, setInstallOpen] = useState(false)
  const [trustOpen, setTrustOpen]     = useState(false)

  const { data, isLoading, error } = useQuery({
    queryKey: ['skills'],
    queryFn:  () => skillsApi.list(),
    refetchOnWindowFocus: false,
  })

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <div className={styles.headerRow}>
          <div>
            <h1><Boxes size={18} style={{ verticalAlign: 'text-bottom', marginRight: 8 }} />Skills</h1>
            <p>Bundles of prompts and tools the agent can use. Admins install <code>.miraskill</code> archives; users toggle individual Skills on or off.</p>
            {data?.skills_dir && (
              <div className={styles.skillsDir}>
                Loaded from <code>{data.skills_dir}</code>
              </div>
            )}
          </div>
          {isAdmin && (
            <div style={{ display: 'flex', gap: 8 }}>
              <RefreshBundledButton />
              <button
                type="button"
                className={styles.installBtnSecondary}
                onClick={() => setTrustOpen(true)}
              >
                <KeyRound size={14} /> Trust Store
              </button>
              <button
                type="button"
                className={styles.installBtn}
                onClick={() => setInstallOpen(true)}
              >
                <Upload size={14} /> Install Skill
              </button>
            </div>
          )}
        </div>
      </header>

      {installOpen && (
        <InstallModal onClose={() => setInstallOpen(false)} />
      )}
      {trustOpen && (
        <TrustStoreModal onClose={() => setTrustOpen(false)} />
      )}

      <div className={styles.body}>
        {isLoading && <div className={styles.empty}>Loading…</div>}
        {error && <div className={styles.empty}>Failed to load Skills.</div>}

        {data && data.errors.length > 0 && (
          <div className={styles.errorsBlock}>
            <h2>Skipped Skills ({data.errors.length})</h2>
            <ul>
              {data.errors.map((e) => (
                <li key={e.path}>
                  <span className={styles.errPath}>{e.path}</span>
                  {' — '}
                  <span className={styles.errMsg}>{e.error}</span>
                </li>
              ))}
            </ul>
          </div>
        )}

        {data && data.loaded.length === 0 && data.errors.length === 0 && (
          <div className={styles.empty}>
            <strong>No Skills installed yet.</strong>
            Drop a Skill directory into <code>{data.skills_dir}</code> and restart MIRA to load it.
            See the{' '}
            <a href="https://vexillon.ai/docs/concepts/agents-and-orchestration" target="_blank" rel="noopener noreferrer">agents &amp; skills documentation</a>.
          </div>
        )}

        {data && data.loaded.length > 0 && (
          <div className={styles.list}>
            {data.loaded.map((skill) => (
              <SkillCard key={skill.id} skill={skill} />
            ))}
          </div>
        )}
      </div>
    </div>
  )
}

function SkillCard({ skill }: { skill: SkillSummary }) {
  const qc = useQueryClient()
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')

  const toggle = useMutation({
    mutationFn: (enabled: boolean) => skillsApi.setEnabled(skill.id, enabled),
    onSuccess: (_, enabled) => {
      qc.invalidateQueries({ queryKey: ['skills'] })
      toast.success(`${skill.display_name} ${enabled ? 'enabled' : 'disabled'}`)
    },
    onError: (e: Error) => toast.error(`Couldn't update preference: ${e.message}`),
  })

  const uninstall = useMutation({
    mutationFn: () => skillsApi.uninstall(skill.id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['skills'] })
      toast.success(`${skill.display_name} uninstalled — restart MIRA so the agent forgets it`)
    },
    onError: (e: Error) => toast.error(`Uninstall failed: ${e.message}`),
  })

  function confirmUninstall() {
    if (window.confirm(`Uninstall "${skill.display_name}" (${skill.id})?\n\nThis removes the Skill from disk. Restart MIRA after to clear it from the running agent.`)) {
      uninstall.mutate()
    }
  }

  return (
    <div className={styles.card} data-disabled={!skill.enabled}>
      <div className={styles.cardHead}>
        <div className={styles.titleRow}>
          <span className={styles.title}>{skill.display_name}</span>
          <span className={styles.id}>{skill.id}</span>
          <span className={styles.version}>v{skill.version}</span>
        </div>
        <div className={styles.badges}>
          <VerificationBadge skill={skill} />
          {skill.system && (
            <span
              className={styles.badge}
              title="Built-in capability — can be disabled but not uninstalled"
            >
              System
            </span>
          )}
          {!skill.enabled && <span className={`${styles.badge} ${styles.badgeDisabled}`}>Disabled</span>}
          <ToggleButton
            enabled={skill.enabled}
            disabled={toggle.isPending}
            onClick={() => toggle.mutate(!skill.enabled)}
          />
          {/* System skills ship in the binary; they can be disabled but not
              removed, so no uninstall control. */}
          {isAdmin && !skill.system && (
            <button
              type="button"
              className={styles.iconBtn}
              title="Uninstall (admin only)"
              aria-label="Uninstall skill"
              disabled={uninstall.isPending}
              onClick={confirmUninstall}
            >
              <Trash2 size={13} />
            </button>
          )}
        </div>
      </div>

      <div className={styles.description}>{skill.description}</div>

      <div className={styles.meta}>
        {skill.authors.length > 0 && <span>By: {skill.authors.join(', ')}</span>}
        {skill.license && <span>License: {skill.license}</span>}
      </div>

      <div className={styles.section}>
        <div className={styles.sectionLabel}>Permissions</div>
        <PermissionList perms={skill.permissions} />
      </div>

      <div className={styles.section}>
        <div className={styles.sectionLabel}>Tools ({skill.tools.length})</div>
        {skill.tools.length === 0 ? (
          <div className={styles.permValueEmpty}>(no tools declared)</div>
        ) : (
          <table className={styles.toolsTable}>
            <thead>
              <tr><th>Name</th><th>Kind</th><th>Binding</th></tr>
            </thead>
            <tbody>
              {skill.tools.map((t) => (
                <tr key={t.name}>
                  <td>{t.name}</td>
                  <td>{t.kind}</td>
                  <td>{t.binding}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {/* Per-skill admin tools — encrypted secrets + LLM routing.
          Hidden for non-admins because the endpoints would 401 anyway. */}
      <SkillAdminPanel skill={skill} />

      <div className={styles.section}>
        <div className={styles.sectionLabel}>Installed at</div>
        <div className={styles.path}>{skill.root_dir}</div>
      </div>
    </div>
  )
}

function ToggleButton({ enabled, disabled, onClick }: { enabled: boolean; disabled: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      className={styles.toggle}
      data-on={enabled}
      disabled={disabled}
      onClick={onClick}
      title={enabled ? "Disable for this user" : "Enable for this user"}
      aria-label={enabled ? "Disable skill" : "Enable skill"}
    >
      <span className={styles.toggleThumb} />
      <span className={styles.toggleLabel}>{enabled ? 'On' : 'Off'}</span>
    </button>
  )
}

function VerificationBadge({ skill }: { skill: { verified: boolean; signed: boolean; publisher_label?: string | null; verification_error?: string | null } }) {
  if (skill.verified) {
    const label = skill.publisher_label ?? 'a trusted publisher'
    return (
      <span className={`${styles.badge} ${styles.badgeVerified}`} title={`Signed by ${label} and verified against the trust store.`}>
        <ShieldCheck size={12} /> Verified{skill.publisher_label ? ` · ${skill.publisher_label}` : ''}
      </span>
    )
  }
  if (skill.signed) {
    const why = skill.verification_error ?? 'signature did not match the trust store'
    return (
      <span className={`${styles.badge} ${styles.badgeUnverified}`} title={`Manifest is signed but ${why}.`}>
        <FileSignature size={12} /> Unverified (signed)
      </span>
    )
  }
  return (
    <span className={`${styles.badge} ${styles.badgeUnverified}`} title="No signature in manifest.">
      <ShieldAlert size={12} /> Unverified
    </span>
  )
}

function PermissionList({ perms }: { perms: SkillPermissions }) {
  return (
    <div className={styles.permList}>
      <PermLine label="Network">
        {perms.network_egress.length === 0
          ? <span className={styles.permValueEmpty}>none</span>
          : <span className={styles.permValue}>{perms.network_egress.join(', ')}</span>}
      </PermLine>
      <PermLine label="Filesystem">
        {perms.filesystem.length === 0
          ? <span className={styles.permValueEmpty}>none</span>
          : <span className={styles.permValue}>{perms.filesystem.join(', ')}</span>}
      </PermLine>
      <PermLine label="Subprocess">
        {perms.subprocess
          ? <span className={styles.permValue}>
              allowed{perms.subprocess_allowlist.length > 0 ? ` (${perms.subprocess_allowlist.join(', ')})` : ''}
            </span>
          : <span className={styles.permValueEmpty}>denied</span>}
      </PermLine>
      <PermLine label="Secrets">
        {perms.secrets.length === 0
          ? <span className={styles.permValueEmpty}>none</span>
          : <span className={styles.permValue}>{perms.secrets.map(s => s.key).join(', ')}</span>}
      </PermLine>
      <PermLine label="LLM providers">
        {perms.llm_providers.length === 0
          ? <span className={styles.permValueEmpty}>none</span>
          : <span className={styles.permValue}>{perms.llm_providers.join(', ')}</span>}
      </PermLine>
      <PermLine label="Max LLM spend / call">
        {perms.max_llm_spend_per_invocation_usd != null
          ? <span className={styles.permValue}>${perms.max_llm_spend_per_invocation_usd.toFixed(2)}</span>
          : <span className={styles.permValueEmpty}>unlimited</span>}
      </PermLine>
    </div>
  )
}

function PermLine({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <>
      <span className={styles.permKey}>{label}:</span>
      <span>{children}</span>
    </>
  )
}

// ── Install modal ────────────────────────────────────────────────────────

function InstallModal({ onClose }: { onClose: () => void }) {
  const qc = useQueryClient()
  const fileInputRef = useRef<HTMLInputElement>(null)
  const [file, setFile] = useState<File | null>(null)
  const [preview, setPreview] = useState<PreviewResponse | null>(null)
  const [previewError, setPreviewError] = useState<string | null>(null)

  const previewMut = useMutation({
    mutationFn: (f: File) => skillsApi.preview(f),
    onSuccess: (data) => { setPreview(data); setPreviewError(null) },
    onError:   (e: { response?: { data?: { error?: string } } } & Error) => {
      setPreview(null)
      setPreviewError(e.response?.data?.error ?? e.message)
    },
  })

  const installMut = useMutation({
    mutationFn: ({ file, force }: { file: File; force: boolean }) => skillsApi.install(file, force),
    onSuccess: (data) => {
      qc.invalidateQueries({ queryKey: ['skills'] })
      toast.success(
        `Installed ${data.skill_id}${data.overwritten ? ' (overwrote existing)' : ''}. ` +
        `Restart MIRA so the agent picks it up.`,
      )
      onClose()
    },
    onError: (e: { response?: { data?: { error?: string } } } & Error) => {
      toast.error(`Install failed: ${e.response?.data?.error ?? e.message}`)
    },
  })

  function pickFile() {
    fileInputRef.current?.click()
  }

  function onFileChosen(e: React.ChangeEvent<HTMLInputElement>) {
    const f = e.target.files?.[0] ?? null
    setFile(f)
    setPreview(null)
    setPreviewError(null)
    if (f) previewMut.mutate(f)
  }

  return (
    <div className={styles.modalBackdrop} onClick={onClose}>
      <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
        <div className={styles.modalHead}>
          <h2>Install a Skill</h2>
          <button type="button" className={styles.iconBtn} onClick={onClose} aria-label="Close">
            <X size={14} />
          </button>
        </div>

        <div className={styles.modalBody}>
          <p style={{ fontSize: 13, color: 'var(--text-muted)' }}>
            Upload a <code>.miraskill</code> archive (gzipped tar). The contents are validated
            against the manifest format before anything writes to disk. After install you'll
            need to restart MIRA so the agent picks up the new tools.
          </p>

          <input
            ref={fileInputRef}
            type="file"
            accept=".miraskill,.tar.gz,.tgz,application/gzip,application/x-gzip"
            onChange={onFileChosen}
            style={{ display: 'none' }}
          />

          <div className={styles.dropZone} onClick={pickFile}>
            {file
              ? <span><strong>{file.name}</strong> &middot; {Math.round(file.size / 1024)} kB &middot; <em>click to choose another</em></span>
              : <span>Click to choose a <code>.miraskill</code> archive…</span>}
          </div>

          {previewMut.isPending && <div className={styles.empty}>Validating archive…</div>}

          {previewError && (
            <div className={styles.errorsBlock}>
              <h2><AlertTriangle size={14} /> Archive rejected</h2>
              <ul><li><span className={styles.errMsg}>{previewError}</span></li></ul>
            </div>
          )}

          {preview && (
            <>
              {preview.conflicts && (
                <div className={styles.errorsBlock} style={{ borderColor: 'var(--warning)', background: 'color-mix(in srgb, var(--warning) 10%, var(--bg-base))' }}>
                  <h2 style={{ color: 'var(--warning)' }}><AlertTriangle size={14} /> Already installed</h2>
                  <ul><li>
                    <span className={styles.errMsg}>
                      A Skill with id <code>{preview.manifest.id}</code> is already installed.
                      Installing will overwrite it; the existing per-user enable/disable
                      preferences are kept.
                    </span>
                  </li></ul>
                </div>
              )}

              <SkillCardPreview manifest={preview.manifest} totalBytes={preview.total_bytes} />
            </>
          )}
        </div>

        <div className={styles.modalFoot}>
          <button type="button" className={styles.installBtnSecondary} onClick={onClose}>
            Cancel
          </button>
          <button
            type="button"
            className={styles.installBtn}
            disabled={!file || !preview || installMut.isPending}
            onClick={() => file && installMut.mutate({ file, force: !!preview?.conflicts })}
          >
            {installMut.isPending ? 'Installing…' : preview?.conflicts ? 'Overwrite + Install' : 'Install'}
          </button>
        </div>
      </div>
    </div>
  )
}

// Same SkillCard layout but rendered without enable/uninstall controls
// used in the preview modal so admins see exactly what they're about
// to install (manifest, permissions, tools).
function SkillCardPreview({ manifest, totalBytes }: { manifest: SkillSummary; totalBytes: number }) {
  return (
    <div className={styles.card}>
      <div className={styles.cardHead}>
        <div className={styles.titleRow}>
          <span className={styles.title}>{manifest.display_name}</span>
          <span className={styles.id}>{manifest.id}</span>
          <span className={styles.version}>v{manifest.version}</span>
        </div>
        <div className={styles.badges}>
          <VerificationBadge skill={manifest} />
          <span className={styles.badge}>{Math.round(totalBytes / 1024)} kB on disk</span>
        </div>
      </div>
      <div className={styles.description}>{manifest.description}</div>
      <div className={styles.meta}>
        {manifest.authors.length > 0 && <span>By: {manifest.authors.join(', ')}</span>}
        {manifest.license && <span>License: {manifest.license}</span>}
      </div>
      <div className={styles.section}>
        <div className={styles.sectionLabel}>Permissions you'd grant</div>
        <PermissionList perms={manifest.permissions} />
      </div>
      <div className={styles.section}>
        <div className={styles.sectionLabel}>Tools</div>
        {manifest.tools.length === 0
          ? <div className={styles.permValueEmpty}>(none)</div>
          : <table className={styles.toolsTable}>
              <thead><tr><th>Name</th><th>Kind</th><th>Binding</th></tr></thead>
              <tbody>{manifest.tools.map((t) => (
                <tr key={t.name}><td>{t.name}</td><td>{t.kind}</td><td>{t.binding}</td></tr>
              ))}</tbody>
            </table>}
      </div>
    </div>
  )
}

// ── Trust store modal (slice A7) ─────────────────────────────────────────

function TrustStoreModal({ onClose }: { onClose: () => void }) {
  const qc = useQueryClient()
  const [label, setLabel]         = useState('')
  const [pubKey, setPubKey]       = useState('')

  const { data, isLoading } = useQuery({
    queryKey: ['skills', 'trust-store'],
    queryFn:  () => skillsApi.listTrust(),
    refetchOnWindowFocus: false,
  })

  const addMut = useMutation({
    mutationFn: () => skillsApi.addTrust(label.trim(), pubKey.trim()),
    onSuccess: (entry) => {
      qc.invalidateQueries({ queryKey: ['skills', 'trust-store'] })
      qc.invalidateQueries({ queryKey: ['skills'] })
      toast.success(`Added ${entry.label} (${entry.fingerprint.slice(0, 16)}…)`)
      setLabel(''); setPubKey('')
    },
    onError: (e: { response?: { data?: { error?: string } } } & Error) =>
      toast.error(`Add failed: ${e.response?.data?.error ?? e.message}`),
  })

  const removeMut = useMutation({
    mutationFn: (fp: string) => skillsApi.removeTrust(fp),
    onSuccess: (_, fp) => {
      qc.invalidateQueries({ queryKey: ['skills', 'trust-store'] })
      qc.invalidateQueries({ queryKey: ['skills'] })
      toast.success(`Removed key ${fp.slice(0, 16)}…`)
    },
    onError: (e: Error) => toast.error(`Remove failed: ${e.message}`),
  })

  function confirmRemove(fp: string, label: string) {
    if (window.confirm(
      `Remove "${label}" (${fp.slice(0, 16)}…) from the trust store?\n\n` +
      `Skills signed by this publisher will go unverified on the next scan.`,
    )) {
      removeMut.mutate(fp)
    }
  }

  return (
    <div className={styles.modalBackdrop} onClick={onClose}>
      <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
        <div className={styles.modalHead}>
          <h2><KeyRound size={14} style={{ verticalAlign: 'text-bottom', marginRight: 6 }} />Trust Store</h2>
          <button type="button" className={styles.iconBtn} onClick={onClose} aria-label="Close">
            <X size={14} />
          </button>
        </div>

        <div className={styles.modalBody}>
          <p style={{ fontSize: 13, color: 'var(--text-muted)' }}>
            Skills are <strong>verified</strong> only when their signature checks against
            an ed25519 public key listed here. Add a publisher's key to mark every Skill
            they sign as Verified; remove a key to revoke trust.
          </p>
          {data?.trust_store_path && (
            <div className={styles.skillsDir}>
              Stored at <code>{data.trust_store_path}</code>
            </div>
          )}

          <div className={styles.section}>
            <div className={styles.sectionLabel}>Add a publisher key</div>
            <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', marginTop: 6 }}>
              <input
                type="text"
                placeholder="Label (e.g. MIRA Team)"
                value={label}
                onChange={(e) => setLabel(e.target.value)}
                style={inputStyle}
              />
              <input
                type="text"
                placeholder="ed25519 public key (base64, 32 bytes)"
                value={pubKey}
                onChange={(e) => setPubKey(e.target.value)}
                style={{ ...inputStyle, flex: 1, minWidth: 240, fontFamily: 'var(--font-mono, monospace)', fontSize: 11 }}
              />
              <button
                type="button"
                className={styles.installBtn}
                disabled={!label.trim() || !pubKey.trim() || addMut.isPending}
                onClick={() => addMut.mutate()}
              >
                {addMut.isPending ? 'Adding…' : 'Add'}
              </button>
            </div>
          </div>

          <div className={styles.section}>
            <div className={styles.sectionLabel}>Trusted publishers ({data?.entries.length ?? 0})</div>
            {isLoading && <div className={styles.empty}>Loading…</div>}
            {data && data.entries.length === 0 && (
              <div className={styles.empty}>
                No trusted publishers yet. Without keys here, every Skill shows as <em>Unverified</em>.
              </div>
            )}
            {data && data.entries.length > 0 && (
              <table className={styles.toolsTable}>
                <thead><tr><th>Label</th><th>Fingerprint (sha256)</th><th>Added</th><th></th></tr></thead>
                <tbody>{data.entries.map((e) => (
                  <tr key={e.fingerprint}>
                    <td>{e.label}</td>
                    <td style={{ fontFamily: 'var(--font-mono, monospace)', fontSize: 10 }}>{e.fingerprint}</td>
                    <td>{new Date(e.added_at).toLocaleDateString()}</td>
                    <td>
                      <button
                        type="button"
                        className={styles.iconBtn}
                        title="Remove"
                        onClick={() => confirmRemove(e.fingerprint, e.label)}
                      >
                        <Trash2 size={13} />
                      </button>
                    </td>
                  </tr>
                ))}</tbody>
              </table>
            )}
          </div>
        </div>

        <div className={styles.modalFoot}>
          <button type="button" className={styles.installBtnSecondary} onClick={onClose}>
            Done
          </button>
        </div>
      </div>
    </div>
  )
}

const inputStyle: React.CSSProperties = {
  padding: '6px 10px',
  border: '1px solid var(--border)',
  borderRadius: 'var(--radius-md)',
  background: 'var(--bg-base)',
  color: 'var(--text-primary)',
  fontSize: 13,
}

// ─── SkillAdminPanel — per-skill secrets + LLM routing (slice 4) ─────────────

function SkillAdminPanel({ skill }: { skill: SkillSummary }) {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  if (!isAdmin) return null
  const declared = skill.permissions.secrets
  if (declared.length === 0 && skill.permissions.llm_providers.length === 0) {
    return null
  }
  return (
    <>
      {declared.length > 0 && <SecretsSection skill={skill} />}
      {skill.permissions.llm_providers.length > 0 && <LlmRoutingSection skill={skill} />}
    </>
  )
}

function SecretsSection({ skill }: { skill: SkillSummary }) {
  const declared = skill.permissions.secrets
  const me       = useAuthStore(s => s.user)
  const qc       = useQueryClient()

  // Two scopes are reachable from this UI: host-wide ("system") and
  // the current admin's own user-scope. Cross-user admin management
  // stays on the CLI for v1.0 — keeps the page from drifting into a
  // user picker. The `system` tab shadows on collision (per
  // `env_vars_for` semantics): if a user has their own key set and
  // a system key exists, the user value wins for that user's tasks.
  const [scope, setScope] = useState<'system' | 'user'>('system')
  const scopeParam = scope === 'system' ? 'system' : `user:${me?.id ?? ''}`

  const listQ = useQuery({
    queryKey: ['skillSecrets', skill.id, scopeParam],
    queryFn:  () => skillsApi.listSecrets(skill.id, scopeParam),
  })
  const setKeys = new Set((listQ.data ?? []).map(e => e.key))

  const [editingKey, setEditingKey] = useState<string | null>(null)
  const [draft, setDraft] = useState('')

  const save = useMutation({
    mutationFn: ({ key, value }: { key: string; value: string }) =>
      skillsApi.setSecret(skill.id, key, value, scopeParam),
    onSuccess: (_, { key }) => {
      toast.success(`Saved ${key}`)
      qc.invalidateQueries({ queryKey: ['skillSecrets', skill.id, scopeParam] })
      setEditingKey(null); setDraft('')
    },
    onError: (e: unknown) => toast.error(`Save failed: ${(e as Error).message}`),
  })
  const del = useMutation({
    mutationFn: (key: string) => skillsApi.deleteSecret(skill.id, key, scopeParam),
    onSuccess: (_, key) => {
      toast.success(`Cleared ${key}`)
      qc.invalidateQueries({ queryKey: ['skillSecrets', skill.id, scopeParam] })
    },
    onError: (e: unknown) => toast.error(`Delete failed: ${(e as Error).message}`),
  })

  const probe = useMutation({
    mutationFn: () => skillsApi.probe(skill.id, scopeParam),
    onSuccess: (r) => {
      if (r.ok) toast.success(`✓ Connection OK (${r.latency_ms}ms)`)
      else      toast.error(`✗ ${r.message}`, { duration: 8000 })
    },
    onError: (e: unknown) => {
      // 422 = "no probe defined" — treat as info, not error.
      const msg = (e as { response?: { data?: { error?: string } } }).response?.data?.error
        ?? (e as Error).message
      toast(msg ?? 'Probe failed', { icon: '⚠️' })
    },
  })

  // Probe button only shown for skills that have a probe wired
  // server-side. com.mira.claudecode runs `claude --print ping`;
  // com.mira.opencode runs `opencode run --format json "ping"`.
  // Keep this list in sync with the match in
  // src/server/handlers/skills.rs::probe_skill.
  const hasProbe = skill.id === 'com.mira.claudecode'
                 || skill.id === 'com.mira.opencode'

  return (
    <div className={styles.section}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 6 }}>
        <div className={styles.sectionLabel} style={{ marginBottom: 0 }}>
          Secrets <span style={{ opacity: 0.6, fontWeight: 400 }}>(encrypted at rest)</span>
        </div>
        <ScopeTabs scope={scope} onChange={setScope} hasUser={!!me?.id} />
        <div style={{ marginLeft: 'auto' }}>
          {hasProbe && (
            <button
              type="button"
              className={styles.installBtnSecondary}
              disabled={probe.isPending}
              onClick={() => probe.mutate()}
              style={{ padding: '4px 10px', fontSize: 12 }}
              title="Run a connection check with the configured env vars"
            >
              <Plug size={13} /> {probe.isPending ? 'Testing…' : 'Test connection'}
            </button>
          )}
        </div>
      </div>
      <table className={styles.toolsTable}>
        <thead>
          <tr><th style={{ width: '30%' }}>Key</th><th>Description</th><th style={{ width: 110 }}>Status</th><th style={{ width: 200 }}></th></tr>
        </thead>
        <tbody>
          {declared.map((d) => {
            const isSet = setKeys.has(d.key)
            const isEditing = editingKey === d.key
            return (
              <tr key={d.key}>
                <td style={{ fontFamily: 'var(--font-mono, monospace)' }}>
                  {d.key}{d.required && <span style={{ color: 'var(--error)', marginLeft: 4 }}>*</span>}
                </td>
                <td>
                  <div>{d.description ?? '(no description)'}</div>
                  <div style={{ fontSize: 11, opacity: 0.55, marginTop: 2 }}>scope hint: {d.scope_hint}</div>
                </td>
                <td>
                  {isSet
                    ? <span className={styles.permValue}>set <span style={{ fontFamily: 'var(--font-mono, monospace)', opacity: 0.6 }}>(••••)</span></span>
                    : <span className={styles.permValueEmpty}>{d.required ? 'missing' : 'not set'}</span>}
                </td>
                <td>
                  {isEditing ? (
                    <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
                      <div style={{ display: 'flex', gap: 4 }}>
                        <input
                          type={d.sensitive ? 'password' : 'text'}
                          autoFocus
                          value={draft}
                          onChange={(e) => setDraft(e.target.value)}
                          onKeyDown={(e) => {
                            if (e.key === 'Enter' && draft) save.mutate({ key: d.key, value: draft })
                            if (e.key === 'Escape') { setEditingKey(null); setDraft('') }
                          }}
                          placeholder={d.example ?? (d.sensitive ? 'paste value' : 'value')}
                          style={{ ...inputStyle, flex: 1, fontSize: 12 }}
                        />
                        <button
                          type="button"
                          className={styles.installBtn}
                          disabled={!draft || save.isPending}
                          onClick={() => save.mutate({ key: d.key, value: draft })}
                          style={{ padding: '4px 10px', fontSize: 12 }}
                        >Save</button>
                        <button
                          type="button"
                          className={styles.installBtnSecondary}
                          onClick={() => { setEditingKey(null); setDraft('') }}
                          style={{ padding: '4px 10px', fontSize: 12 }}
                        >Cancel</button>
                      </div>
                      {d.example && (
                        <div style={{
                          fontSize: 11, opacity: 0.6,
                          fontFamily: 'var(--font-mono, monospace)',
                          paddingLeft: 2,
                        }}>
                          example: {d.example}
                        </div>
                      )}
                    </div>
                  ) : (
                    <div style={{ display: 'flex', gap: 4 }}>
                      <button
                        type="button"
                        className={styles.installBtnSecondary}
                        onClick={() => { setEditingKey(d.key); setDraft('') }}
                        style={{ padding: '4px 10px', fontSize: 12 }}
                      >{isSet ? 'Update' : 'Set'}</button>
                      {isSet && (
                        <button
                          type="button"
                          className={styles.iconBtn}
                          title="Clear value"
                          onClick={() => {
                            if (confirm(`Clear ${d.key}? Tasks using this skill will fall back to whatever's in the process env.`)) {
                              del.mutate(d.key)
                            }
                          }}
                        >
                          <Trash2 size={13} />
                        </button>
                      )}
                    </div>
                  )}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

/** Two-tab toggle for system vs current-user scope on the secrets panel. */
function ScopeTabs({
  scope,
  onChange,
  hasUser,
}: {
  scope: 'system' | 'user'
  onChange: (s: 'system' | 'user') => void
  hasUser: boolean
}) {
  const tab = (val: 'system' | 'user', label: string, hint: string) => (
    <button
      key={val}
      type="button"
      onClick={() => onChange(val)}
      disabled={val === 'user' && !hasUser}
      style={{
        padding: '3px 10px',
        fontSize: 11,
        border: '1px solid var(--border)',
        background: scope === val ? 'var(--accent-subtle, rgba(255,255,255,0.07))' : 'transparent',
        color: scope === val ? 'var(--text-primary)' : 'var(--text-secondary)',
        borderRadius: 'var(--radius-sm, 4px)',
        cursor: 'pointer',
        opacity: (val === 'user' && !hasUser) ? 0.5 : 1,
      }}
      title={hint}
    >
      {label}
    </button>
  )
  return (
    <div style={{ display: 'inline-flex', gap: 4 }}>
      {tab('system', 'System',  'Host-wide values that apply to every user.')}
      {tab('user',   'My user', 'Override the system value for your own tasks only.')}
    </div>
  )
}

/** Header button that triggers POST /api/admin/skills/refresh-bundled
 *  and surfaces the per-skill report as a toast. */
function RefreshBundledButton() {
  const qc = useQueryClient()
  const m = useMutation({
    mutationFn: () => skillsApi.refreshBundled({}),
    onSuccess: (r) => {
      const changed = r.report.filter(row => row.kind !== 'up_to_date' && row.kind !== 'skipped')
      if (changed.length === 0) {
        toast.success('Bundled skills are up-to-date')
        return
      }
      const lines = changed.map(row => {
        if (row.kind === 'extracted') return `+ ${row.id}`
        return `↻ ${row.id} (${row.from ?? '?'} → ${row.to ?? '?'})`
      }).join('\n')
      toast.success(
        `${changed.length} bundled skill${changed.length === 1 ? '' : 's'} refreshed.\n${lines}\nRestart MIRA to load.`,
        { duration: 8000 },
      )
      qc.invalidateQueries({ queryKey: ['skills'] })
    },
    onError: (e: unknown) => toast.error(`Refresh failed: ${(e as Error).message}`),
  })
  return (
    <button
      type="button"
      className={styles.installBtnSecondary}
      disabled={m.isPending}
      onClick={() => m.mutate()}
      title="Re-extract bundled skills whose manifest version is newer than what's installed"
    >
      <RefreshCw size={14} /> {m.isPending ? 'Refreshing…' : 'Refresh bundled'}
    </button>
  )
}

function LlmRoutingSection({ skill }: { skill: SkillSummary }) {
  const qc = useQueryClient()
  const aliasesQ = useQuery({
    queryKey: ['llmAliases'],
    queryFn:  () => skillsApi.listLlmAliases(),
  })
  const providersQ = useQuery({
    queryKey: ['providers'],
    queryFn:  async () => {
      const { providersApi } = await import('@/api/providers')
      return providersApi.health()
    },
  })
  const aliases = aliasesQ.data ?? []
  const providers = providersQ.data ?? []

  const set = useMutation({
    mutationFn: async ({ alias, provider, model }: { alias: string; provider: string; model: string | null }) => {
      // PUT replaces the whole map; preserve every other alias.
      const next = aliases.filter(a => a.alias !== alias)
      next.push({ alias, provider, model })
      await skillsApi.setLlmAliases(next)
    },
    onSuccess: () => {
      toast.success('Routing updated')
      qc.invalidateQueries({ queryKey: ['llmAliases'] })
    },
    onError: (e: unknown) => toast.error(`Update failed: ${(e as Error).message}`),
  })

  return (
    <div className={styles.section}>
      <div className={styles.sectionLabel}>
        LLM routing <span style={{ opacity: 0.6, fontWeight: 400 }}>(per-alias provider + model)</span>
      </div>
      <table className={styles.toolsTable}>
        <thead><tr><th style={{ width: '20%' }}>Alias</th><th>Provider</th><th>Model</th></tr></thead>
        <tbody>
          {skill.permissions.llm_providers.map((alias) => {
            const current = aliases.find(a => a.alias === alias)
            return (
              <tr key={alias}>
                <td style={{ fontFamily: 'var(--font-mono, monospace)' }}>{alias}</td>
                <td>
                  <select
                    value={current?.provider ?? ''}
                    onChange={(e) => {
                      const provider = e.target.value
                      if (!provider) return
                      set.mutate({ alias, provider, model: current?.model ?? null })
                    }}
                    style={{ ...inputStyle, fontSize: 12 }}
                  >
                    <option value="" disabled>(unset — falls back to primary_provider)</option>
                    {providers.map(p => (
                      <option key={p.name} value={p.name}>
                        {p.name}{p.healthy ? '' : ' (unhealthy)'}
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  <input
                    type="text"
                    placeholder="(use provider default)"
                    defaultValue={current?.model ?? ''}
                    onBlur={(e) => {
                      const model = e.target.value.trim()
                      if (!current) return
                      if (model === (current.model ?? '')) return
                      set.mutate({ alias, provider: current.provider, model: model || null })
                    }}
                    style={{ ...inputStyle, fontSize: 12, width: '100%' }}
                  />
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}
