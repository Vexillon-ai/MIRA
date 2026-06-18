// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/pages/McpPage.tsx
//
// Per-user MCP server management (Q2 #7,).
//
// Lists every MCP server the caller owns, with the runtime connect
// status overlaid from `/api/mcp/status`. Add / edit / delete maps to
// the `/api/mcp/servers` CRUD endpoints. Changes take effect on the
// next MIRA restart; we surface a banner reminding the operator.
//
// The Settings → Tools → MCP servers section is intentionally still
// in place as a read-only summary for admins who want a single-page
// view — that section just enumerates the same data. This dedicated
// page is the per-user editor.

import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { Plug, Plus, RotateCcw, Store, Download, Trash2, Eye, EyeOff, Pencil } from 'lucide-react'
import { api } from '@/api/client'
import { useRestartServer } from '@/hooks/useRestartServer'
import { useAuthStore } from '@/store/authStore'

interface McpServerRow {
  id:         string
  user_id:    string
  name:       string
  transport:  string
  enabled:    boolean
  config_json: string
  created_at: number
  updated_at: number
}

interface McpServerStatus {
  id:         string
  owner_user_id: string
  name:       string
  transport:  string
  enabled:    boolean
  state:      string
  tool_count: number
  tools:      { name: string; description: string }[]
  supports_resources: boolean
  supports_prompts:   boolean
  renamed_for_collision: boolean
  sampling_enabled: boolean
  last_error: string | null
}

interface ServerForm {
  id?:       string  // present = edit, absent = create
  name:      string
  transport: 'stdio' | 'http'
  command:   string
  args:      string   // comma-separated in the form, split on save
  env:       string   // KEY=value lines
  url:       string
  enabled:   boolean
  sampling_enabled: boolean
}

function blankForm(): ServerForm {
  return {
    name: '', transport: 'stdio', command: '', args: '', env: '', url: '',
    enabled: true, sampling_enabled: false,
  }
}

function rowToForm(r: McpServerRow): ServerForm {
  // The config_json is the round-tripped McpServerConfig — parse to
  // extract command/args/env/url for the form fields.
  let cfg: any = {}
  try { cfg = JSON.parse(r.config_json) } catch {}
  return {
    id:        r.id,
    name:      r.name,
    transport: (r.transport === 'http' ? 'http' : 'stdio'),
    command:   cfg.command ?? '',
    args:      Array.isArray(cfg.args) ? cfg.args.join(', ') : '',
    env:       cfg.env && typeof cfg.env === 'object'
                 ? Object.entries(cfg.env).map(([k, v]) => `${k}=${v}`).join('\n')
                 : '',
    url:       cfg.url ?? '',
    enabled:   r.enabled,
    sampling_enabled: Boolean(cfg.sampling_enabled),
  }
}

interface McpCatalogEntry {
  id:             string
  name:           string
  title:          string
  description:    string
  transport:      string
  command?:       string | null
  args:           string[]
  env:            Record<string, string>
  url?:           string | null
  requires_setup: boolean
  homepage?:      string | null
  enabled:        boolean
  sort_order:     number
}

// Catalog entry → the add-server form (pre-fill, then the user reviews
// and saves). Mirrors rowToForm but sourced from the catalog shape.
function catalogToForm(e: McpCatalogEntry): ServerForm {
  return {
    name:      e.name,
    transport: e.transport === 'http' ? 'http' : 'stdio',
    command:   e.command ?? '',
    args:      Array.isArray(e.args) ? e.args.join(', ') : '',
    env:       e.env && typeof e.env === 'object'
                 ? Object.entries(e.env).map(([k, v]) => `${k}=${v}`).join('\n')
                 : '',
    url:       e.url ?? '',
    enabled:   true,
    sampling_enabled: false,
  }
}

interface CatalogForm {
  id?:            string
  name:           string
  title:          string
  description:    string
  transport:      'stdio' | 'http'
  command:        string
  args:           string  // comma-separated
  env:            string  // KEY=value lines
  url:            string
  requires_setup: boolean
  enabled:        boolean
}

function blankCatalogForm(): CatalogForm {
  return { name: '', title: '', description: '', transport: 'stdio', command: '', args: '', env: '', url: '', requires_setup: false, enabled: true }
}

function catalogToEditForm(e: McpCatalogEntry): CatalogForm {
  return {
    id: e.id, name: e.name, title: e.title, description: e.description,
    transport: e.transport === 'http' ? 'http' : 'stdio',
    command: e.command ?? '', args: (e.args ?? []).join(', '),
    env: e.env ? Object.entries(e.env).map(([k, v]) => `${k}=${v}`).join('\n') : '',
    url: e.url ?? '', requires_setup: e.requires_setup, enabled: e.enabled,
  }
}

function catalogFormToPayload(f: CatalogForm) {
  const env: Record<string, string> = {}
  for (const line of f.env.split('\n')) {
    const eq = line.indexOf('=')
    if (eq <= 0) continue
    env[line.slice(0, eq).trim()] = line.slice(eq + 1)
  }
  return {
    name: f.name.trim(), title: f.title.trim(), description: f.description.trim(),
    transport: f.transport,
    command: f.transport === 'stdio' ? (f.command.trim() || null) : null,
    args: f.transport === 'stdio' ? f.args.split(',').map((x) => x.trim()).filter(Boolean) : [],
    env: f.transport === 'stdio' ? env : {},
    url: f.transport === 'http' ? (f.url.trim() || null) : null,
    requires_setup: f.requires_setup, enabled: f.enabled,
  }
}

function formToPayload(f: ServerForm) {
  const env: Record<string, string> = {}
  for (const line of f.env.split('\n')) {
    const eq = line.indexOf('=')
    if (eq <= 0) continue
    const k = line.slice(0, eq).trim()
    const v = line.slice(eq + 1)
    if (k) env[k] = v
  }
  const base: any = {
    name:      f.name.trim(),
    transport: f.transport,
    enabled:   f.enabled,
    sampling_enabled: f.sampling_enabled,
  }
  if (f.transport === 'stdio') {
    base.command = f.command.trim() || null
    base.args    = f.args.split(',').map((x) => x.trim()).filter(Boolean)
    base.env     = env
  } else {
    base.url     = f.url.trim() || null
  }
  return base
}

export default function McpPage() {
  const qc = useQueryClient()
  const restartMut = useRestartServer({ supervised: true })
  const [editing, setEditing] = useState<ServerForm | null>(null)
  const [showCatalog, setShowCatalog] = useState(false)
  const [catalogEdit, setCatalogEdit] = useState<CatalogForm | null>(null)
  const isAdmin = useAuthStore((s) => s.user)?.role === 'admin'

  const { data: servers = [], isLoading } = useQuery<McpServerRow[]>({
    queryKey: ['mcp', 'servers'],
    queryFn:  () => api.get('/api/mcp/servers').then((r) => r.data),
  })

  const { data: statuses = [] } = useQuery<McpServerStatus[]>({
    queryKey: ['mcp', 'status'],
    queryFn:  () => api.get('/api/mcp/status').then((r) => r.data),
    refetchInterval: 15_000,
  })
  const statusByName = new Map(statuses.map((s) => [s.name, s]))

  // The server-side handlers hot-reload the registry before responding, so
  // by the time these resolve the tools are already live — refresh both the
  // row list and the runtime status, no restart needed.
  const invalidateMcp = () => {
    qc.invalidateQueries({ queryKey: ['mcp', 'servers'] })
    qc.invalidateQueries({ queryKey: ['mcp', 'status'] })
  }

  const createMut = useMutation({
    mutationFn: (body: any) => api.post('/api/mcp/servers', body).then((r) => r.data),
    onSuccess: () => {
      invalidateMcp()
      setEditing(null)
      toast.success('MCP server added — connecting and loading its tools now.')
    },
    onError: (e: any) => toast.error(`Create failed: ${e?.response?.data ?? e?.message ?? e}`),
  })

  const updateMut = useMutation({
    mutationFn: ({ id, body }: { id: string; body: any }) =>
      api.put(`/api/mcp/servers/${id}`, body).then((r) => r.data),
    onSuccess: () => {
      invalidateMcp()
      setEditing(null)
      toast.success('Updated and reloaded — changes are live.')
    },
    onError: (e: any) => toast.error(`Update failed: ${e?.response?.data ?? e?.message ?? e}`),
  })

  const deleteMut = useMutation({
    mutationFn: (id: string) => api.delete(`/api/mcp/servers/${id}`).then(() => id),
    onSuccess: () => {
      invalidateMcp()
      toast.success('Deleted — its tools were removed.')
    },
    onError: (e: any) => toast.error(`Delete failed: ${e?.response?.data ?? e?.message ?? e}`),
  })

  const onSave = () => {
    if (!editing) return
    if (!editing.name.trim()) { toast.error('Name is required'); return }
    const body = formToPayload(editing)
    if (editing.id) updateMut.mutate({ id: editing.id, body })
    else            createMut.mutate(body)
  }

  // ── Catalog of recommended servers ──────────────────────────────────────
  // Admins fetch the full list (incl. disabled) to manage it; everyone else
  // sees only enabled entries to pick from.
  const { data: catalog = [] } = useQuery<McpCatalogEntry[]>({
    queryKey: ['mcp', 'catalog', isAdmin],
    queryFn:  () => api.get(isAdmin ? '/api/admin/mcp/catalog' : '/api/mcp/catalog').then((r) => r.data),
    enabled:  showCatalog,
  })

  const catalogCreateMut = useMutation({
    mutationFn: (body: any) => api.post('/api/admin/mcp/catalog', body).then((r) => r.data),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ['mcp', 'catalog'] }); setCatalogEdit(null); toast.success('Catalog entry added.') },
    onError: (e: any) => toast.error(`Add failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const catalogUpdateMut = useMutation({
    mutationFn: ({ id, body }: { id: string; body: any }) => api.put(`/api/admin/mcp/catalog/${id}`, body).then((r) => r.data),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ['mcp', 'catalog'] }); setCatalogEdit(null); toast.success('Catalog entry updated.') },
    onError: (e: any) => toast.error(`Update failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const catalogDeleteMut = useMutation({
    mutationFn: (id: string) => api.delete(`/api/admin/mcp/catalog/${id}`).then(() => id),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ['mcp', 'catalog'] }); toast.success('Catalog entry removed.') },
    onError: (e: any) => toast.error(`Remove failed: ${e?.response?.data ?? e?.message ?? e}`),
  })

  // Pre-fill the add-server form from a catalog entry, then scroll the user
  // to it so they can review (e.g. fill a key / path) before saving.
  const useFromCatalog = (e: McpCatalogEntry) => {
    setEditing(catalogToForm(e))
    setShowCatalog(false)
    if (e.requires_setup) {
      toast('Review the highlighted fields (path / credential) before saving.', { icon: '✏️' })
    }
  }

  return (
    <div style={{ padding: '24px 32px', maxWidth: 960, margin: '0 auto', overflow: 'auto' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 8 }}>
        <Plug size={22} />
        <h1 style={{ fontSize: 22, margin: 0 }}>MCP Servers</h1>
      </div>
      <p style={{ color: 'var(--text-muted)', fontSize: 13, marginBottom: 20 }}>
        External Model Context Protocol servers connected to MIRA on
        your behalf. Each server's tools become available to the
        agent under <code>mcp__&lt;name&gt;__&lt;tool&gt;</code>.
        Changes take effect immediately — adding, editing, or removing a
        server hot-reloads its tools, no restart needed.
      </p>

      <div style={{ display: 'flex', gap: 8, marginBottom: 16 }}>
        <button
          onClick={() => setEditing(blankForm())}
          disabled={editing !== null}
          style={btnPrimary}
        >
          <Plus size={14} /> Add server
        </button>
        <button
          onClick={() => setShowCatalog((v) => !v)}
          style={btnSecondary}
          title="Pick from MIRA's catalog of recommended MCP servers."
        >
          <Store size={14} /> {showCatalog ? 'Hide catalog' : 'Browse catalog'}
        </button>
        <button
          onClick={() => restartMut.mutate()}
          disabled={restartMut.isPending}
          title="MCP changes apply live now; use this only to force a full reconnect if a server's connection gets wedged."
          style={btnSecondary}
        >
          <RotateCcw size={14} /> {restartMut.isPending ? 'Restarting…' : 'Force restart'}
        </button>
      </div>

      {showCatalog && (
        <CatalogPanel
          entries={catalog}
          isAdmin={isAdmin}
          onUse={useFromCatalog}
          onAddEntry={() => setCatalogEdit(blankCatalogForm())}
          onEditEntry={(e) => setCatalogEdit(catalogToEditForm(e))}
          onToggle={(e) => catalogUpdateMut.mutate({ id: e.id, body: { ...catalogFormToPayload(catalogToEditForm(e)), enabled: !e.enabled } })}
          onDelete={(e) => { if (confirm(`Remove "${e.title}" from the catalog?`)) catalogDeleteMut.mutate(e.id) }}
        />
      )}

      {catalogEdit && (
        <CatalogEditor
          form={catalogEdit}
          onChange={setCatalogEdit}
          onCancel={() => setCatalogEdit(null)}
          onSave={() => {
            if (!catalogEdit.name.trim() || !catalogEdit.title.trim()) { toast.error('Name and title are required'); return }
            const body = catalogFormToPayload(catalogEdit)
            if (catalogEdit.id) catalogUpdateMut.mutate({ id: catalogEdit.id, body })
            else                catalogCreateMut.mutate(body)
          }}
          busy={catalogCreateMut.isPending || catalogUpdateMut.isPending}
        />
      )}

      {editing && (
        <ServerEditor
          form={editing}
          onChange={setEditing}
          onCancel={() => setEditing(null)}
          onSave={onSave}
          busy={createMut.isPending || updateMut.isPending}
        />
      )}

      {isLoading && <p style={{ color: 'var(--text-muted)' }}>Loading…</p>}

      {!isLoading && servers.length === 0 && !editing && (
        <div style={emptyCard}>
          <p style={{ marginBottom: 8 }}>No MCP servers configured yet.</p>
          <p style={{ fontSize: 13, color: 'var(--text-muted)' }}>
            Try the filesystem server: name <code>fs</code>, command{' '}
            <code>npx</code>, args{' '}
            <code>-y, @modelcontextprotocol/server-filesystem, /home/me/notes</code>.
          </p>
        </div>
      )}

      {servers.map((s) => (
        <ServerCard
          key={s.id}
          row={s}
          status={statusByName.get(s.name) ?? null}
          onEdit={() => setEditing(rowToForm(s))}
          onDelete={() => {
            if (confirm(`Delete MCP server "${s.name}"?`)) deleteMut.mutate(s.id)
          }}
        />
      ))}
    </div>
  )
}

function ServerCard({
  row, status, onEdit, onDelete,
}: {
  row: McpServerRow; status: McpServerStatus | null
  onEdit: () => void; onDelete: () => void
}) {
  const [open, setOpen] = useState(false)
  const dotColor = status?.state === 'connected' ? '#22c55e'
                : status?.state === 'disabled'  ? '#9ca3af'
                : status?.state === 'error'     ? '#ef4444'
                :                                  '#fbbf24'
  const hasTools = (status?.tools?.length ?? 0) > 0
  return (
    <div style={card}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 6 }}>
        <span title={status?.last_error ?? status?.state ?? 'unknown'} style={{
          width: 10, height: 10, borderRadius: '50%', background: dotColor, flexShrink: 0,
        }} />
        <strong style={{ flex: 1 }}>
          {row.name}
          <span style={badge}>{row.transport}</span>
          {status?.supports_resources && (
            <span style={{ ...badge, background: 'rgba(34, 197, 94, 0.15)', color: '#22c55e' }}>
              resources
            </span>
          )}
          {status?.supports_prompts && (
            <span style={{ ...badge, background: 'rgba(168, 85, 247, 0.15)', color: '#a855f7' }}>
              prompts
            </span>
          )}
          {status?.sampling_enabled && (
            <span title="This server can ask MIRA to make LLM calls on its behalf, routed through your primary provider."
                  style={{ ...badge, background: 'rgba(239, 68, 68, 0.15)', color: '#ef4444' }}>
              sampling
            </span>
          )}
          {status?.renamed_for_collision && (
            <span title="Another user owns a server with this name. Your tools are suffixed __u<short-id> to keep them unique. See the expandable tool list for the actual names."
                  style={{ ...badge, background: 'rgba(251, 191, 36, 0.15)', color: '#fbbf24' }}>
              renamed
            </span>
          )}
          {!row.enabled && (
            <span style={{ ...badge, background: 'rgba(156, 163, 175, 0.2)', color: '#9ca3af' }}>
              disabled
            </span>
          )}
          {status && status.state === 'connected' && (
            <button onClick={() => setOpen(!open)}
                    style={{
                      marginLeft: 8, background: 'transparent', border: 'none',
                      color: 'inherit', opacity: 0.7, cursor: hasTools ? 'pointer' : 'default',
                      fontWeight: 'normal',
                      textDecoration: hasTools ? 'underline dotted' : 'none',
                    }}>
              {status.tool_count} tool{status.tool_count === 1 ? '' : 's'}
              {hasTools && <span style={{ marginLeft: 4 }}>{open ? '▾' : '▸'}</span>}
            </button>
          )}
          {status?.last_error && (
            <span style={{ marginLeft: 8, color: '#ef4444', fontWeight: 'normal', fontSize: 13 }}>
              · {status.last_error.slice(0, 80)}{status.last_error.length > 80 ? '…' : ''}
            </span>
          )}
        </strong>
        <button onClick={onEdit} style={btnGhost}>Edit</button>
        <button onClick={onDelete} style={btnGhost}>Delete</button>
      </div>
      {open && hasTools && (
        <div style={toolsBox}>
          <div style={{ opacity: 0.7, marginBottom: 6, fontSize: 12 }}>
            Discovered tools
          </div>
          {status!.tools.map((t) => (
            <div key={t.name} style={{ display: 'flex', gap: 10, padding: '3px 0' }}>
              <code style={{ fontWeight: 600, color: 'var(--accent-light)' }}>{t.name}</code>
              {t.description && <span style={{ opacity: 0.7, flex: 1 }}>{t.description}</span>}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

function ServerEditor({
  form, onChange, onCancel, onSave, busy,
}: {
  form: ServerForm
  onChange: (f: ServerForm) => void
  onCancel: () => void
  onSave: () => void
  busy: boolean
}) {
  const update = (patch: Partial<ServerForm>) => onChange({ ...form, ...patch })
  return (
    <div style={{ ...card, background: 'var(--bg-elevated)', marginBottom: 16 }}>
      <h3 style={{ marginTop: 0, fontSize: 15 }}>{form.id ? 'Edit MCP server' : 'New MCP server'}</h3>
      <FormRow label="Name" hint="Short unique label. Doubles as the tool-namespace prefix.">
        <input type="text" value={form.name} onChange={(e) => update({ name: e.target.value })} placeholder="filesystem" style={input} />
      </FormRow>
      <FormRow label="Transport" hint="stdio: local child process. http: remote Streamable-HTTP endpoint.">
        <select value={form.transport} onChange={(e) => update({ transport: e.target.value as 'stdio' | 'http' })} style={input}>
          <option value="stdio">stdio (local child process)</option>
          <option value="http">http (remote endpoint)</option>
        </select>
      </FormRow>
      {form.transport === 'stdio' ? (
        <>
          <FormRow label="Command" hint="Executable to spawn (npx, uvx, or an absolute path).">
            <input type="text" value={form.command} onChange={(e) => update({ command: e.target.value })} placeholder="npx" style={input} />
          </FormRow>
          <FormRow label="Args" hint="Comma-separated.">
            <input type="text" value={form.args} onChange={(e) => update({ args: e.target.value })}
                   placeholder="-y, @modelcontextprotocol/server-filesystem, /home/me/notes"
                   style={input} />
          </FormRow>
          <FormRow label="Env" hint="KEY=value, one per line. Merged over MIRA's env.">
            <textarea value={form.env} onChange={(e) => update({ env: e.target.value })} placeholder="BRAVE_API_KEY=…" rows={3} style={input} />
          </FormRow>
        </>
      ) : (
        <FormRow label="URL" hint="Full Streamable-HTTP MCP endpoint.">
          <input type="text" value={form.url} onChange={(e) => update({ url: e.target.value })}
                 placeholder="https://mcp.example.com/v1" style={input} />
        </FormRow>
      )}
      <FormRow label="Enabled" hint="Disabled = skip connect on startup; tools disappear until re-enabled and MIRA restarts.">
        <input type="checkbox" checked={form.enabled} onChange={(e) => update({ enabled: e.target.checked })} />
      </FormRow>
      <FormRow label="Allow sampling" hint="When on, this server can ask MIRA to make LLM calls on its behalf — billed to your configured provider. Only enable for servers you trust. Off by default.">
        <input type="checkbox" checked={form.sampling_enabled} onChange={(e) => update({ sampling_enabled: e.target.checked })} />
      </FormRow>
      <div style={{ display: 'flex', gap: 8, marginTop: 12 }}>
        <button onClick={onSave} disabled={busy} style={btnPrimary}>{busy ? 'Saving…' : 'Save'}</button>
        <button onClick={onCancel} disabled={busy} style={btnSecondary}>Cancel</button>
      </div>
    </div>
  )
}

function CatalogPanel({
  entries, isAdmin, onUse, onAddEntry, onEditEntry, onToggle, onDelete,
}: {
  entries:    McpCatalogEntry[]
  isAdmin:    boolean
  onUse:      (e: McpCatalogEntry) => void
  onAddEntry: () => void
  onEditEntry:(e: McpCatalogEntry) => void
  onToggle:   (e: McpCatalogEntry) => void
  onDelete:   (e: McpCatalogEntry) => void
}) {
  return (
    <div style={{ ...card, background: 'var(--bg-elevated)', marginBottom: 16 }}>
      <div style={{ display: 'flex', alignItems: 'center', marginBottom: 8 }}>
        <h3 style={{ margin: 0, fontSize: 15, flex: 1 }}>Recommended servers</h3>
        {isAdmin && (
          <button onClick={onAddEntry} style={btnSecondary}>
            <Plus size={13} /> Add to catalog
          </button>
        )}
      </div>
      <p style={{ color: 'var(--text-muted)', fontSize: 12, margin: '0 0 12px' }}>
        Pick one to pre-fill the add-server form — review (fill any path or
        key) and save. {isAdmin && 'As an admin you can also add, edit, hide, or remove catalog entries.'}
      </p>
      {entries.length === 0 && (
        <p style={{ color: 'var(--text-muted)', fontSize: 13 }}>No catalog entries.</p>
      )}
      {entries.map((e) => (
        <div key={e.id} style={{
          display: 'flex', alignItems: 'flex-start', gap: 10, padding: '8px 0',
          borderTop: '1px solid var(--border)', opacity: e.enabled ? 1 : 0.5,
        }}>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontWeight: 600, fontSize: 13 }}>
              {e.title}
              <span style={badge}>{e.transport}</span>
              {e.requires_setup && (
                <span style={{ ...badge, background: 'rgba(251,191,36,0.15)', color: '#fbbf24' }}>needs setup</span>
              )}
              {isAdmin && !e.enabled && (
                <span style={{ ...badge, background: 'rgba(156,163,175,0.2)', color: '#9ca3af' }}>hidden</span>
              )}
            </div>
            <div style={{ color: 'var(--text-muted)', fontSize: 12, marginTop: 2 }}>{e.description}</div>
            <code style={{ fontSize: 11, color: 'var(--text-muted)', opacity: 0.8 }}>
              {e.transport === 'http' ? (e.url ?? '') : `${e.command ?? ''} ${(e.args ?? []).join(' ')}`}
            </code>
          </div>
          <button onClick={() => onUse(e)} style={btnPrimary} title="Pre-fill the add-server form with this entry.">
            <Download size={13} /> Use
          </button>
          {isAdmin && (
            <>
              <button onClick={() => onToggle(e)} style={btnGhost} title={e.enabled ? 'Hide from non-admins' : 'Show to everyone'}>
                {e.enabled ? <EyeOff size={13} /> : <Eye size={13} />}
              </button>
              <button onClick={() => onEditEntry(e)} style={btnGhost} title="Edit entry"><Pencil size={13} /></button>
              <button onClick={() => onDelete(e)} style={btnGhost} title="Remove entry"><Trash2 size={13} /></button>
            </>
          )}
        </div>
      ))}
    </div>
  )
}

function CatalogEditor({
  form, onChange, onCancel, onSave, busy,
}: {
  form: CatalogForm
  onChange: (f: CatalogForm) => void
  onCancel: () => void
  onSave: () => void
  busy: boolean
}) {
  const update = (patch: Partial<CatalogForm>) => onChange({ ...form, ...patch })
  return (
    <div style={{ ...card, background: 'var(--bg-elevated)', marginBottom: 16 }}>
      <h3 style={{ marginTop: 0, fontSize: 15 }}>{form.id ? 'Edit catalog entry' : 'New catalog entry'}</h3>
      <FormRow label="Title" hint="Display name in the catalog (e.g. “GitHub”).">
        <input type="text" value={form.title} onChange={(e) => update({ title: e.target.value })} placeholder="GitHub" style={input} />
      </FormRow>
      <FormRow label="Suggested name" hint="Default server name when added (tool-namespace prefix).">
        <input type="text" value={form.name} onChange={(e) => update({ name: e.target.value })} placeholder="github" style={input} />
      </FormRow>
      <FormRow label="Description" hint="One line shown under the title.">
        <input type="text" value={form.description} onChange={(e) => update({ description: e.target.value })} style={input} />
      </FormRow>
      <FormRow label="Transport">
        <select value={form.transport} onChange={(e) => update({ transport: e.target.value as 'stdio' | 'http' })} style={input}>
          <option value="stdio">stdio (local child process)</option>
          <option value="http">http (remote endpoint)</option>
        </select>
      </FormRow>
      {form.transport === 'stdio' ? (
        <>
          <FormRow label="Command" hint="npx, uvx, or an absolute path.">
            <input type="text" value={form.command} onChange={(e) => update({ command: e.target.value })} placeholder="npx" style={input} />
          </FormRow>
          <FormRow label="Args" hint="Comma-separated. Use a placeholder path/connection-string for entries that need editing.">
            <input type="text" value={form.args} onChange={(e) => update({ args: e.target.value })} style={input} />
          </FormRow>
          <FormRow label="Env" hint="KEY=value per line. Leave a value empty to signal a credential the user must fill.">
            <textarea value={form.env} onChange={(e) => update({ env: e.target.value })} rows={3} style={input} />
          </FormRow>
        </>
      ) : (
        <FormRow label="URL" hint="Full Streamable-HTTP MCP endpoint.">
          <input type="text" value={form.url} onChange={(e) => update({ url: e.target.value })} placeholder="https://mcp.example.com/mcp" style={input} />
        </FormRow>
      )}
      <FormRow label="Needs setup" hint="Flags the entry with a badge + reminds the user to fill a path/key before saving.">
        <input type="checkbox" checked={form.requires_setup} onChange={(e) => update({ requires_setup: e.target.checked })} />
      </FormRow>
      <FormRow label="Enabled" hint="Off = hidden from non-admins (still listed here for admins).">
        <input type="checkbox" checked={form.enabled} onChange={(e) => update({ enabled: e.target.checked })} />
      </FormRow>
      <div style={{ display: 'flex', gap: 8, marginTop: 12 }}>
        <button onClick={onSave} disabled={busy} style={btnPrimary}>{busy ? 'Saving…' : 'Save'}</button>
        <button onClick={onCancel} disabled={busy} style={btnSecondary}>Cancel</button>
      </div>
    </div>
  )
}

function FormRow({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div style={{ marginBottom: 10 }}>
      <label style={{ display: 'block', fontWeight: 600, fontSize: 13, marginBottom: 2 }}>{label}</label>
      {hint && <div style={{ color: 'var(--text-muted)', fontSize: 12, marginBottom: 4 }}>{hint}</div>}
      {children}
    </div>
  )
}

// ── Inline styles ────────────────────────────────────────────────────────────
const card: React.CSSProperties = {
  border: '1px solid var(--border)', borderRadius: 8, padding: 14, marginBottom: 12,
}
const emptyCard: React.CSSProperties = {
  ...card, textAlign: 'center', padding: 32, background: 'var(--bg-elevated)',
}
const toolsBox: React.CSSProperties = {
  marginTop: 10, padding: '8px 10px', background: 'var(--bg-input)',
  borderRadius: 6, border: '1px solid var(--border)', fontSize: 13,
}
const badge: React.CSSProperties = {
  marginLeft: 8, padding: '1px 6px', fontSize: 11, fontWeight: 'normal',
  background: 'var(--accent-dim)', color: 'var(--accent-light)', borderRadius: 4,
  textTransform: 'uppercase', letterSpacing: 0.5,
}
const btnPrimary: React.CSSProperties = {
  background: 'var(--accent)', border: '1px solid var(--accent-border)',
  color: 'var(--accent-fg, white)', borderRadius: 6, padding: '6px 14px',
  cursor: 'pointer', display: 'inline-flex', alignItems: 'center', gap: 6,
}
const btnSecondary: React.CSSProperties = {
  background: 'transparent', border: '1px solid var(--border)',
  color: 'var(--text-secondary)', borderRadius: 6, padding: '6px 14px',
  cursor: 'pointer', display: 'inline-flex', alignItems: 'center', gap: 6,
}
const btnGhost: React.CSSProperties = {
  background: 'transparent', border: '1px solid var(--border)',
  color: 'var(--text-secondary)', borderRadius: 6, padding: '4px 10px',
  cursor: 'pointer',
}
const input: React.CSSProperties = {
  width: '100%', boxSizing: 'border-box', padding: 6,
  background: 'var(--bg-input)', border: '1px solid var(--border)',
  borderRadius: 6, color: 'var(--text-primary)',
  fontFamily: 'var(--font-mono)', fontSize: 13,
}
