// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export type TrustLevel =
  | { level: 'verified'; publisher: string }
  | { level: 'invalid'; reason: string }
  | { level: 'untrusted'; reason: string }
  | { level: 'unsigned' }

export interface Capabilities {
  network_egress: string[]
  filesystem: string[]
  secrets: string[]
  subprocess: boolean
  subprocess_allowlist: string[]
  listen_port?: number
}

/** One field of a cpp_provider component's install form. */
export interface ConfigField {
  key: string
  label?: string
  help?: string
  type: 'string' | 'secret' | 'url' | 'host' | 'int' | 'bool' | 'enum' | 'multiline'
  source: 'input' | 'generate' | 'derive' | 'step_output'
  secret?: boolean
  group?: string
  required?: boolean
  default?: any
  enum?: string[]
  visible_when?: string
}

export interface ComponentSummary {
  type: string
  runtime: string
  capabilities: Capabilities
  spec: Record<string, any>
  config_schema?: ConfigField[]
}

/** Per-step runtime state in the guided cpp_provider wizard. */
export interface WizardStep {
  id: string
  title: string
  actor: 'mira' | 'admin' | 'admin_external'
  verb: string
  status: 'pending' | 'done' | 'skipped' | 'awaiting_input' | 'failed'
  render?: string
  message?: string
  awaiting_outputs?: string[]
}

export interface WizardState {
  package_id: string
  name: string
  version: string
  trust: string
  status: 'in_progress' | 'awaiting_input' | 'complete' | 'failed'
  steps: WizardStep[]
  awaiting?: string | null
  warnings?: string[]
}

export interface PackageSummary {
  id: string
  name: string
  version: string
  description?: string
  publisher?: string
  components: ComponentSummary[]
}

/** The reviewable plan for updating an installed package (the three diffs). */
export interface UpdatePlan {
  id: string
  from_version: string
  to_version: string
  trust_changed: boolean
  new_trust: string
  capability: {
    added_egress: string[]
    added_filesystem: string[]
    added_secrets: string[]
    gained_subprocess: boolean
    added_subprocess: string[]
    gained_listen_port?: number
  }
  config: {
    new_required_inputs: string[]
    new_optional: string[]
    removed: string[]
    renamed: Record<string, string>
    rotated: string[]
  }
  needs_capability_reapproval: boolean
  needs_trust_reapproval: boolean
}

export interface PreviewResponse {
  manifest: PackageSummary
  trust: TrustLevel
  total_bytes: number
  /** Set when a package with this id is already installed. */
  installed_version?: string
  /** The update plan when this bundle is a valid newer version. */
  update?: UpdatePlan
  /** Why this can't be applied as an update (downgrade, needs newer MIRA, …). */
  update_blocked?: string
}

export interface InstalledPackage {
  id: string
  version: string
  name: string
  trust: string
  installed_by: string
  installed_at: number
  updated_at: number
  ledger: Array<Record<string, any>>
  manifest: Record<string, any>
  /** Lifecycle state: 'active' | 'disabled'. */
  state?: string
}

export const packagesApi = {
  /** Upload a .mirapkg for parse + trust verification (no install). */
  async preview(bundle: File): Promise<PreviewResponse> {
    const fd = new FormData()
    fd.append('bundle', bundle)
    const { data } = await api.post<PreviewResponse>('/api/admin/packages/preview', fd)
    return data
  },

  /** Install a .mirapkg with an optional config (values for secrets/env). */
  async install(bundle: File, config: Record<string, string>, allowUntrusted = false): Promise<any> {
    const fd = new FormData()
    fd.append('bundle', bundle)
    fd.append('config', JSON.stringify(config))
    if (allowUntrusted) fd.append('allow_untrusted', 'true')
    const { data } = await api.post('/api/admin/packages/install', fd)
    return data
  },

  async list(): Promise<InstalledPackage[]> {
    const { data } = await api.get<InstalledPackage[]>('/api/admin/packages')
    return data
  },

  async uninstall(id: string): Promise<void> {
    await api.delete(`/api/admin/packages/${encodeURIComponent(id)}`)
  },

  /** Begin a guided cpp_provider install. `config` holds the admin's answers to
   *  the component's `input` config fields. Returns the first wizard state. */
  async cppInstall(bundle: File, config: Record<string, any>, allowUntrusted = false): Promise<WizardState> {
    const fd = new FormData()
    fd.append('bundle', bundle)
    fd.append('config', JSON.stringify(config))
    if (allowUntrusted) fd.append('allow_untrusted', 'true')
    const { data } = await api.post<WizardState>('/api/admin/packages/cpp/install', fd)
    return data
  },

  /** Begin a guided update of an installed cpp_provider package. A 409 means a
   *  re-approval gate (capability/trust) is unmet — the error body carries the
   *  plan + which ack is needed. */
  async cppUpdate(
    bundle: File,
    config: Record<string, any>,
    opts: { capabilityAck?: boolean; trustAck?: boolean; allowUntrusted?: boolean } = {},
  ): Promise<WizardState> {
    const fd = new FormData()
    fd.append('bundle', bundle)
    fd.append('config', JSON.stringify(config))
    if (opts.allowUntrusted) fd.append('allow_untrusted', 'true')
    if (opts.capabilityAck) fd.append('capability_ack', 'true')
    if (opts.trustAck) fd.append('trust_ack', 'true')
    const { data } = await api.post<WizardState>('/api/admin/packages/cpp/update', fd)
    return data
  },

  /** Disable an installed package (account off + service stopped; record kept). */
  async disable(id: string): Promise<void> {
    await api.post(`/api/admin/packages/${encodeURIComponent(id)}/disable`)
  },

  /** Re-enable a disabled package. */
  async enable(id: string): Promise<void> {
    await api.post(`/api/admin/packages/${encodeURIComponent(id)}/enable`)
  },

  /** Re-fetch the in-flight wizard state (resume). */
  async cppSession(id: string): Promise<WizardState> {
    const { data } = await api.get<WizardState>(`/api/admin/packages/cpp/${encodeURIComponent(id)}/session`)
    return data
  },

  /** Submit a human step's result and advance the wizard. */
  async cppStep(id: string, stepId: string, outputs: Record<string, string>): Promise<WizardState> {
    const { data } = await api.post<WizardState>(
      `/api/admin/packages/cpp/${encodeURIComponent(id)}/step`,
      { step_id: stepId, outputs },
    )
    return data
  },

  /** Abandon an in-flight install, reversing whatever it provisioned. */
  async cppCancel(id: string): Promise<void> {
    await api.post(`/api/admin/packages/cpp/${encodeURIComponent(id)}/cancel`)
  },
}
