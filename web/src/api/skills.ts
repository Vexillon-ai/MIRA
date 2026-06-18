// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/api/skills.ts
//
// TypeScript client for the /api/skills surface (slice A4 from
// design-docs/skills-and-agents.md). DTO shapes mirror
// `src/server/handlers/skills.rs` exactly so axios returns ready-to-render
// objects without conversion.

import { api } from './client'

export type ToolKind = 'builtin' | 'prompt' | 'executable'

export interface ToolSummary {
  name: string
  kind: ToolKind
  /** For builtin: impl name. For prompt: template path. For executable: path. */
  binding: string
}

/** One declared secret as the manifest exposes it (slice 2). */
export interface SecretSchema {
  key:         string
  description: string | null
  required:    boolean
  sensitive:   boolean
  /** "system" | "user" | "either" — manifest's hint for which scope the value
   *  is best set under. The runtime always merges system + user with user
   *  shadowing on collision. */
  scope_hint:  string
  /** Optional manifest-supplied example shape (e.g. a fully-qualified
   *  model id with provider prefix). Surfaced below the input field
   *  and as the input placeholder so users see the expected format
   *  without having to dig into docs. */
  example:     string | null
}

export interface SkillPermissions {
  network_egress:                    string[]
  filesystem:                        string[]
  subprocess:                        boolean
  subprocess_allowlist:              string[]
  secrets:                           SecretSchema[]
  llm_providers:                     string[]
  max_llm_spend_per_invocation_usd?: number | null
}

export interface SkillSummary {
  id:           string
  version:      string
  display_name: string
  description:  string
  authors:      string[]
  license:      string | null
  /** Manifest had a [verification] block. Doesn't mean verified. */
  signed:       boolean
  /** Signature validated against a key in the trust store (slice A7). */
  verified:     boolean
  /** Trust-store label for the publisher key, when the signature pointed at a known key. */
  publisher_label?:    string | null
  /** Why the Skill is unverified (signed-but-failed, or unknown publisher). */
  verification_error?: string | null
  permissions:  SkillPermissions
  tools:        ToolSummary[]
  /** Filesystem path the Skill is installed at. */
  root_dir:     string
  /** Per-user enable/disable (slice A5). */
  enabled:      boolean
  /** Built-in capability — can be disabled but not uninstalled. */
  system:       boolean
}

export interface TrustEntry {
  fingerprint: string
  label:       string
  added_at:    number
}

export interface TrustStoreResponse {
  trust_store_path: string
  entries:          TrustEntry[]
}

export interface SkillLoadError {
  path:  string
  error: string
}

export interface SkillsResponse {
  skills_dir: string
  loaded:     SkillSummary[]
  errors:     SkillLoadError[]
}

export interface PreviewResponse {
  manifest:    SkillSummary
  conflicts:   boolean
  total_bytes: number
}

export interface InstallResponse {
  installed:        boolean
  skill_id:         string
  root_dir:         string
  overwritten:      boolean
  restart_required: boolean
}

export interface UninstallResponse {
  uninstalled:      boolean
  skill_id:         string
  restart_required: boolean
}

export const skillsApi = {
  async list(): Promise<SkillsResponse> {
    const { data } = await api.get<SkillsResponse>('/api/skills')
    return data
  },

  /** Toggle the calling user's enable/disable preference for one Skill (slice A5). */
  async setEnabled(skillId: string, enabled: boolean): Promise<void> {
    await api.put(`/api/skills/${encodeURIComponent(skillId)}/preferences`, { enabled })
  },

  /** Admin: validate an uploaded archive and return the manifest for review. */
  async preview(archive: File): Promise<PreviewResponse> {
    const fd = new FormData()
    fd.append('archive', archive)
    const { data } = await api.post<PreviewResponse>('/api/skills/preview', fd)
    return data
  },

  /** Admin: install an uploaded archive. Pass `force` to overwrite an existing id. */
  async install(archive: File, force = false): Promise<InstallResponse> {
    const fd = new FormData()
    fd.append('archive', archive)
    const { data } = await api.post<InstallResponse>(
      `/api/skills/install${force ? '?force=true' : ''}`,
      fd,
    )
    return data
  },

  /** Admin: remove an installed Skill by id. */
  async uninstall(skillId: string): Promise<UninstallResponse> {
    const { data } = await api.delete<UninstallResponse>(
      `/api/skills/${encodeURIComponent(skillId)}`,
    )
    return data
  },

  /** Admin: list all trusted publisher keys (slice A7). */
  async listTrust(): Promise<TrustStoreResponse> {
    const { data } = await api.get<TrustStoreResponse>('/api/skills/trust-store')
    return data
  },

  /** Admin: add a publisher's ed25519 public key (base64) under a label. */
  async addTrust(label: string, publicKey: string): Promise<TrustEntry> {
    const { data } = await api.post<TrustEntry>('/api/skills/trust-store', {
      label,
      public_key: publicKey,
    })
    return data
  },

  /** Admin: remove a publisher key by fingerprint. */
  async removeTrust(fingerprint: string): Promise<void> {
    await api.delete(`/api/skills/trust-store/${encodeURIComponent(fingerprint)}`)
  },

  // ── Skill secrets (slice 4) ───────────────────────────────────────────
  // Admin-only env-var management. Values never round-trip through the API:
  // GET returns metadata only; PUT/DELETE write directly to the encrypted
  // vault. Scope selector: "system" (default) or "user:<id>".

  /** Admin: list secret keys (NOT values) registered for one skill. */
  async listSecrets(skillId: string, scope = 'system'): Promise<SecretListEntry[]> {
    const { data } = await api.get<SecretListEntry[]>(
      `/api/admin/skills/${encodeURIComponent(skillId)}/secrets`,
      { params: { scope } },
    )
    return data
  },

  /** Admin: set or update one secret. Empty value rejected (use DELETE). */
  async setSecret(skillId: string, key: string, value: string, scope = 'system'): Promise<void> {
    await api.put(
      `/api/admin/skills/${encodeURIComponent(skillId)}/secrets/${encodeURIComponent(key)}`,
      { value },
      { params: { scope } },
    )
  },

  /** Admin: clear one secret. */
  async deleteSecret(skillId: string, key: string, scope = 'system'): Promise<void> {
    await api.delete(
      `/api/admin/skills/${encodeURIComponent(skillId)}/secrets/${encodeURIComponent(key)}`,
      { params: { scope } },
    )
  },

  /** Admin: list current `agent.llm_aliases` map. */
  async listLlmAliases(): Promise<LlmAliasDto[]> {
    const { data } = await api.get<LlmAliasDto[]>('/api/admin/llm-aliases')
    return data
  },

  /** Admin: replace the whole alias map. Persists to mira_config.json. */
  async setLlmAliases(aliases: LlmAliasDto[]): Promise<void> {
    await api.put('/api/admin/llm-aliases', { aliases })
  },

  /** Admin: "test connection" for a skill's configured env vars. */
  async probe(skillId: string, scope = 'system'): Promise<ProbeResult> {
    const { data } = await api.post<ProbeResult>(
      `/api/admin/skills/${encodeURIComponent(skillId)}/probe`,
      undefined,
      { params: { scope } },
    )
    return data
  },

  /** Admin: re-extract bundled skills onto disk. */
  async refreshBundled(opts: { force?: boolean; id?: string } = {}): Promise<RefreshBundledResponse> {
    const { data } = await api.post<RefreshBundledResponse>(
      '/api/admin/skills/refresh-bundled',
      opts,
    )
    return data
  },
}

/** Result of POST /api/admin/skills/{id}/probe. */
export interface ProbeResult {
  ok:         boolean
  message:    string
  latency_ms: number
}

export interface RefreshBundledRow {
  id:      string
  kind:    'extracted' | 'refreshed' | 'forced' | 'up_to_date' | 'skipped'
  from?:   string
  to?:     string
  reason?: string
}

export interface RefreshBundledResponse {
  report:           RefreshBundledRow[]
  restart_required: boolean
}

/** Returned by `listSecrets`: one row per registered secret. */
export interface SecretListEntry {
  key:        string
  scope:      string
  scope_id:   string
  updated_at: number
}

/** One row in the LLM aliases map. */
export interface LlmAliasDto {
  alias:    string
  provider: string
  model:    string | null
}
