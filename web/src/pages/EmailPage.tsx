// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/pages/EmailPage.tsx
//
// Per-user email account management (Q2 #8 E1+E3, chunks 1-2).
//
// Lists every email account the caller owns, with runtime poll
// status overlaid from /api/email/status. Add/edit/delete maps to
// the /api/email/accounts CRUD endpoints. Changes take effect on
// next gateway restart — the poller registry doesn't hot-reload.
//
// Future chunks land additional UI here:
//   - chunk 3: per-account security-knob editor (allowlist, denylist,
//     HTML/attachments toggles)
//   - chunk 5: quarantine queue panel
//   - chunk 6: rate-limit usage indicators

import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { Mail, Plus, RotateCcw, ShieldAlert, ScrollText, Check, X } from 'lucide-react'
import { api } from '@/api/client'
import { useRestartServer } from '@/hooks/useRestartServer'

// ── Types mirroring the backend ──────────────────────────────────────────────
// (EmailSecurity is exposed by the API today but not edited here yet —
// chunk 3 adds the security-knob editor and starts using the shape.)

interface EmailAccountRow {
  id:        string
  user_id:   string
  label:     string
  address:   string
  auth_mode: string
  imap_host?:    string | null
  imap_port?:    number | null
  imap_use_tls:  boolean
  imap_username?: string | null
  imap_password?: string | null
  smtp_host?:    string | null
  smtp_port?:    number | null
  smtp_use_tls:  boolean
  smtp_username?: string | null
  smtp_password?: string | null
  webhook_provider?: string | null
  webhook_secret?:   string | null
  security_json: string
  enabled:       boolean
  last_uid_seen: number
  created_at:    number
  updated_at:    number
}

interface QuarantineEntry {
  id:          string
  account_id:  string
  sender:      string
  subject:     string
  preview:     string
  message_id:  string
  reason:      string
  received_at: number
}

interface AuditEntry {
  id:             string
  account_id:     string
  direction:      string
  sender:         string
  recipient:      string
  subject:        string
  action:         string
  reason:         string | null
  body_sha256:    string
  attached_count: number
  at:             number
}

interface EmailPollerStatus {
  account_id:       string
  owner_user_id:    string
  address:          string
  state:            'idle' | 'polling' | 'ok' | 'error' | string
  last_error:       string | null
  last_polled_at:   number | null
  last_received_at: number | null
  total_received:   number
}

type AuthMode = 'password' | 'oauth_google' | 'oauth_microsoft' | 'webhook'
type WebhookProvider = 'postmark' | 'resend' | 'mailgun'

interface AccountForm {
  id?:       string
  label:     string
  address:   string
  auth_mode: AuthMode
  webhook_provider: WebhookProvider
  imap_host: string
  imap_port: number
  imap_use_tls:  boolean
  imap_username: string
  imap_password: string
  smtp_host: string
  smtp_port: number
  smtp_use_tls:  boolean
  smtp_username: string
  smtp_password: string
  enabled:   boolean
  // Security knobs are still managed via the per-account row;
  // dedicated editor will land in a follow-up.
}

function blankForm(): AccountForm {
  return {
    label: '', address: '',
    auth_mode: 'password',
    webhook_provider: 'postmark',
    imap_host: '', imap_port: 993, imap_use_tls: true,
    imap_username: '', imap_password: '',
    smtp_host: '', smtp_port: 465, smtp_use_tls: true,
    smtp_username: '', smtp_password: '',
    enabled: true,
  }
}

function rowToForm(r: EmailAccountRow): AccountForm {
  return {
    id:           r.id,
    label:        r.label,
    address:      r.address,
    auth_mode:    (r.auth_mode as AuthMode) ?? 'password',
    webhook_provider: ((r.webhook_provider as WebhookProvider) ?? 'postmark'),
    imap_host:    r.imap_host ?? '',
    imap_port:    r.imap_port ?? 993,
    imap_use_tls: r.imap_use_tls,
    imap_username: r.imap_username ?? '',
    imap_password: '',                // never echo password back
    smtp_host:    r.smtp_host ?? '',
    smtp_port:    r.smtp_port ?? 465,
    smtp_use_tls: r.smtp_use_tls,
    smtp_username: r.smtp_username ?? '',
    smtp_password: '',
    enabled:      r.enabled,
  }
}

function formToCreatePayload(f: AccountForm) {
  const base: Record<string, unknown> = {
    label:    f.label.trim(),
    address:  f.address.trim(),
    auth_mode: f.auth_mode,
    enabled:   f.enabled,
  }
  if (f.auth_mode === 'webhook') {
    base.webhook_provider = f.webhook_provider
  }
  if (f.auth_mode === 'password') {
    base.imap_host = f.imap_host.trim() || null
    base.imap_port = f.imap_port || null
    base.imap_use_tls = f.imap_use_tls
    base.imap_username = f.imap_username.trim() || null
    base.imap_password = f.imap_password || null
    base.smtp_host = f.smtp_host.trim() || null
    base.smtp_port = f.smtp_port || null
    base.smtp_use_tls = f.smtp_use_tls
    base.smtp_username = f.smtp_username.trim() || null
    base.smtp_password = f.smtp_password || null
  }
  // OAuth accounts: backend defaults IMAP/SMTP host from the
  // provider when blank, and tokens land via the Connect button +
  // OAuth callback. The form only needs label + address.
  return base
}

function formToUpdatePayload(f: AccountForm) {
  // PUT body — only send password fields when the user actually
  // typed a new value. The store keeps the existing password
  // otherwise (because Option<Option<String>> outer-None means
  // "leave alone").
  const body: Record<string, unknown> = {
    label:    f.label,
    address:  f.address,
    enabled:  f.enabled,
  }
  if (f.auth_mode === 'password') {
    body.imap_host = f.imap_host || null
    body.imap_port = f.imap_port || null
    body.imap_use_tls = f.imap_use_tls
    body.imap_username = f.imap_username || null
    body.smtp_host = f.smtp_host || null
    body.smtp_port = f.smtp_port || null
    body.smtp_use_tls = f.smtp_use_tls
    body.smtp_username = f.smtp_username || null
    if (f.imap_password) body.imap_password = f.imap_password
    if (f.smtp_password) body.smtp_password = f.smtp_password
  }
  return body
}

// ── Page ─────────────────────────────────────────────────────────────────────

export default function EmailPage() {
  const qc = useQueryClient()
  const restartMut = useRestartServer({ supervised: true })
  const [editing, setEditing] = useState<AccountForm | null>(null)

  const { data: accounts = [], isLoading } = useQuery<EmailAccountRow[]>({
    queryKey: ['email', 'accounts'],
    queryFn:  () => api.get('/api/email/accounts').then((r) => r.data),
  })
  const { data: statuses = [] } = useQuery<EmailPollerStatus[]>({
    queryKey: ['email', 'status'],
    queryFn:  () => api.get('/api/email/status').then((r) => r.data),
    refetchInterval: 15_000,
  })
  const statusByAccount = new Map(statuses.map((s) => [s.account_id, s]))

  const createMut = useMutation({
    mutationFn: (body: unknown) => api.post('/api/email/accounts', body).then((r) => r.data),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['email', 'accounts'] })
      setEditing(null)
      toast.success('Email account created. Restart MIRA to start polling.')
    },
    onError: (e: any) => toast.error(`Create failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const updateMut = useMutation({
    mutationFn: ({ id, body }: { id: string; body: unknown }) =>
      api.put(`/api/email/accounts/${id}`, body).then((r) => r.data),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['email', 'accounts'] })
      setEditing(null)
      toast.success('Updated. Restart MIRA to apply.')
    },
    onError: (e: any) => toast.error(`Update failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const deleteMut = useMutation({
    mutationFn: (id: string) => api.delete(`/api/email/accounts/${id}`).then(() => id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['email', 'accounts'] })
      toast.success('Deleted. Restart MIRA to fully stop the poller.')
    },
    onError: (e: any) => toast.error(`Delete failed: ${e?.response?.data ?? e?.message ?? e}`),
  })

  const onSave = () => {
    if (!editing) return
    if (!editing.label.trim()) { toast.error('Label is required'); return }
    if (!editing.address.trim()) { toast.error('Address is required'); return }
    if (editing.id) updateMut.mutate({ id: editing.id, body: formToUpdatePayload(editing) })
    else            createMut.mutate(formToCreatePayload(editing))
  }

  return (
    <div style={{ padding: '24px 32px', maxWidth: 960, margin: '0 auto', overflow: 'auto' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 8 }}>
        <Mail size={22} />
        <h1 style={{ fontSize: 22, margin: 0 }}>Email accounts</h1>
      </div>
      <p style={{ color: 'var(--text-muted)', fontSize: 13, marginBottom: 20 }}>
        Per-user mailboxes MIRA polls for inbound mail and (later) sends
        from. Chunk 2 lands the poller; chunks 3-6 add the security
        pipeline, conversation routing, quarantine queue, and rate
        limits. Changes take effect after restart.
      </p>

      <div style={{ display: 'flex', gap: 8, marginBottom: 16 }}>
        <button onClick={() => setEditing(blankForm())} disabled={editing !== null} style={btnPrimary}>
          <Plus size={14} /> Add account
        </button>
        <button onClick={() => restartMut.mutate()} disabled={restartMut.isPending}
                title="Apply pending email-account changes by restarting the gateway."
                style={btnSecondary}>
          <RotateCcw size={14} /> {restartMut.isPending ? 'Restarting…' : 'Restart to apply'}
        </button>
      </div>

      {editing && (
        <AccountEditor
          form={editing}
          onChange={setEditing}
          onCancel={() => setEditing(null)}
          onSave={onSave}
          busy={createMut.isPending || updateMut.isPending}
        />
      )}

      {isLoading && <p style={{ color: 'var(--text-muted)' }}>Loading…</p>}
      {!isLoading && accounts.length === 0 && !editing && (
        <div style={emptyCard}>
          <p style={{ marginBottom: 8 }}>No email accounts configured yet.</p>
          <p style={{ fontSize: 13, color: 'var(--text-muted)' }}>
            Self-hosted Dovecot, Fastmail, iCloud (with app passwords) and
            Gmail (with an App Password) work today. Outlook/365 needs
            OAuth — that ships in a later slice.
          </p>
        </div>
      )}

      {accounts.map((a) => (
        <AccountCard
          key={a.id}
          row={a}
          status={statusByAccount.get(a.id) ?? null}
          onEdit={() => setEditing(rowToForm(a))}
          onDelete={() => {
            if (confirm(`Delete email account "${a.label}"?`)) deleteMut.mutate(a.id)
          }}
        />
      ))}

      <QuarantinePanel />
      <AuditPanel />
    </div>
  )
}

// ── Quarantine panel ────────────────────────────────────────────────────────

function QuarantinePanel() {
  const qc = useQueryClient()
  const { data: entries = [] } = useQuery<QuarantineEntry[]>({
    queryKey: ['email', 'quarantine'],
    queryFn:  () => api.get('/api/email/quarantine').then((r) => r.data),
    refetchInterval: 30_000,
  })

  const approveMut = useMutation({
    mutationFn: ({ id, addToAllowlist }: { id: string; addToAllowlist: boolean }) =>
      api.post(`/api/email/quarantine/${id}/approve`,
               { add_to_allowlist: addToAllowlist }).then(() => id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['email', 'quarantine'] })
      qc.invalidateQueries({ queryKey: ['email', 'accounts'] })
      qc.invalidateQueries({ queryKey: ['email', 'audit'] })
      toast.success('Approved + re-dispatched.')
    },
    onError: (e: any) => toast.error(`Approve failed: ${e?.response?.data ?? e?.message ?? e}`),
  })
  const rejectMut = useMutation({
    mutationFn: ({ id, addToDenylist }: { id: string; addToDenylist: boolean }) =>
      api.post(`/api/email/quarantine/${id}/reject`,
               { add_to_denylist: addToDenylist }).then(() => id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['email', 'quarantine'] })
      qc.invalidateQueries({ queryKey: ['email', 'accounts'] })
      qc.invalidateQueries({ queryKey: ['email', 'audit'] })
      toast.success('Rejected.')
    },
    onError: (e: any) => toast.error(`Reject failed: ${e?.response?.data ?? e?.message ?? e}`),
  })

  return (
    <div style={{ marginTop: 32 }}>
      <h2 style={panelHeading}>
        <ShieldAlert size={18} />
        Quarantine queue
        <span style={{ marginLeft: 'auto', fontSize: 13, fontWeight: 'normal', color: 'var(--text-muted)' }}>
          {entries.length} held
        </span>
      </h2>
      {entries.length === 0 ? (
        <div style={{ ...card, color: 'var(--text-muted)', fontSize: 13 }}>
          Nothing held. Unknown-sender mail and SPF/DKIM-fail messages
          land here for review before MIRA engages.
        </div>
      ) : (
        entries.map((e) => (
          <QuarantineCard
            key={e.id}
            entry={e}
            onApprove={(addToAllowlist) =>
              approveMut.mutate({ id: e.id, addToAllowlist })}
            onReject={(addToDenylist) =>
              rejectMut.mutate({ id: e.id, addToDenylist })}
            busy={approveMut.isPending || rejectMut.isPending}
          />
        ))
      )}
    </div>
  )
}

function QuarantineCard({
  entry, onApprove, onReject, busy,
}: {
  entry: QuarantineEntry
  onApprove: (addToAllowlist: boolean) => void
  onReject:  (addToDenylist:  boolean) => void
  busy: boolean
}) {
  const [addAllow, setAddAllow]   = useState(true)
  const [addDeny,  setAddDeny]    = useState(false)
  const [open,     setOpen]       = useState(false)
  return (
    <div style={card}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 6 }}>
        <strong style={{ flex: 1 }}>
          {entry.sender}
          <span style={{ marginLeft: 8, opacity: 0.7, fontWeight: 'normal', fontSize: 13 }}>
            {entry.subject}
          </span>
          <span style={{ ...badge, background: 'rgba(251, 191, 36, 0.15)', color: '#fbbf24' }}>
            {entry.reason}
          </span>
        </strong>
        <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>
          {new Date(entry.received_at).toLocaleString()}
        </span>
        <button onClick={() => setOpen(!open)} style={btnGhost}>
          {open ? 'Hide' : 'Preview'}
        </button>
      </div>
      {open && (
        <pre style={{
          background: 'var(--bg-input)', padding: 8, borderRadius: 6,
          fontSize: 12, overflow: 'auto', maxHeight: 200,
          whiteSpace: 'pre-wrap', wordBreak: 'break-word',
        }}>{entry.preview || '(no preview)'}</pre>
      )}
      <div style={{ display: 'flex', gap: 12, alignItems: 'center', marginTop: 8, flexWrap: 'wrap' }}>
        <label style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: 12, color: 'var(--text-muted)' }}>
          <input type="checkbox" checked={addAllow} onChange={(e) => setAddAllow(e.target.checked)} />
          Add sender to allowlist
        </label>
        <button onClick={() => onApprove(addAllow)} disabled={busy} style={{ ...btnPrimary, padding: '4px 12px' }}>
          <Check size={13} /> Approve
        </button>
        <span style={{ width: 12 }} />
        <label style={{ display: 'flex', alignItems: 'center', gap: 4, fontSize: 12, color: 'var(--text-muted)' }}>
          <input type="checkbox" checked={addDeny} onChange={(e) => setAddDeny(e.target.checked)} />
          Add sender to denylist
        </label>
        <button onClick={() => onReject(addDeny)} disabled={busy} style={{ ...btnGhost, padding: '4px 12px' }}>
          <X size={13} /> Reject
        </button>
      </div>
    </div>
  )
}

// ── Audit panel ─────────────────────────────────────────────────────────────

function AuditPanel() {
  const { data: rows = [] } = useQuery<AuditEntry[]>({
    queryKey: ['email', 'audit'],
    queryFn:  () => api.get('/api/email/audit').then((r) => r.data),
    refetchInterval: 60_000,
  })
  const tagColor = (action: string) => action === 'accepted' || action === 'approved' ? '#22c55e'
                                     : action === 'quarantined' ? '#fbbf24'
                                     : action === 'rejected'    ? '#9ca3af'
                                     :                            '#ef4444'
  return (
    <div style={{ marginTop: 32 }}>
      <h2 style={panelHeading}>
        <ScrollText size={18} />
        Audit log
        <span style={{ marginLeft: 'auto', fontSize: 13, fontWeight: 'normal', color: 'var(--text-muted)' }}>
          last {rows.length} events
        </span>
      </h2>
      {rows.length === 0 ? (
        <div style={{ ...card, color: 'var(--text-muted)', fontSize: 13 }}>
          No email activity yet.
        </div>
      ) : (
        <div style={card}>
          <table style={{ width: '100%', fontSize: 13, borderCollapse: 'collapse' }}>
            <thead>
              <tr style={{ textAlign: 'left', borderBottom: '1px solid var(--border)', color: 'var(--text-muted)' }}>
                <th style={{ padding: '4px 6px' }}>When</th>
                <th style={{ padding: '4px 6px' }}>Action</th>
                <th style={{ padding: '4px 6px' }}>Sender</th>
                <th style={{ padding: '4px 6px' }}>Subject</th>
                <th style={{ padding: '4px 6px' }}>Reason</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.id} style={{ borderBottom: '1px solid var(--border-subtle)' }}>
                  <td style={{ padding: '4px 6px', whiteSpace: 'nowrap', color: 'var(--text-muted)' }}>
                    {new Date(r.at).toLocaleString()}
                  </td>
                  <td style={{ padding: '4px 6px' }}>
                    <span style={{
                      padding: '1px 6px', borderRadius: 4, fontSize: 11,
                      background: `${tagColor(r.action)}26`, color: tagColor(r.action),
                      textTransform: 'uppercase', letterSpacing: 0.5,
                    }}>{r.action}</span>
                  </td>
                  <td style={{ padding: '4px 6px' }}>{r.sender}</td>
                  <td style={{ padding: '4px 6px', maxWidth: 280, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {r.subject}
                  </td>
                  <td style={{ padding: '4px 6px', color: 'var(--text-muted)', fontSize: 12 }}>
                    {r.reason ?? ''}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  )
}

const panelHeading: React.CSSProperties = {
  display: 'flex', alignItems: 'center', gap: 8,
  fontSize: 16, marginBottom: 10,
}

function AccountCard({
  row, status, onEdit, onDelete,
}: {
  row: EmailAccountRow; status: EmailPollerStatus | null
  onEdit: () => void; onDelete: () => void
}) {
  const dotColor = status?.state === 'ok'      ? '#22c55e'
                : status?.state === 'polling' ? '#3b82f6'
                : status?.state === 'error'   ? '#ef4444'
                : status?.state === 'idle'    ? '#9ca3af'
                :                                '#fbbf24'
  const since = (ms: number | null) => ms ? new Date(ms).toLocaleString() : 'never'
  return (
    <div style={card}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 6 }}>
        <span title={status?.last_error ?? status?.state ?? 'unknown'} style={{
          width: 10, height: 10, borderRadius: '50%', background: dotColor, flexShrink: 0,
        }} />
        <strong style={{ flex: 1 }}>
          {row.label}
          <span style={{ marginLeft: 8, opacity: 0.7, fontWeight: 'normal', fontSize: 13 }}>
            {row.address}
          </span>
          {!row.enabled && (
            <span style={{ ...badge, background: 'rgba(156, 163, 175, 0.2)', color: '#9ca3af' }}>
              disabled
            </span>
          )}
        </strong>
        <button onClick={onEdit} style={btnGhost}>Edit</button>
        <button onClick={onDelete} style={btnGhost}>Delete</button>
      </div>
      <div style={{ fontSize: 12, color: 'var(--text-muted)', display: 'flex', gap: 16, flexWrap: 'wrap' }}>
        <span><b>IMAP:</b> {row.imap_host}:{row.imap_port}</span>
        <span><b>SMTP:</b> {row.smtp_host}:{row.smtp_port} (unused until E2)</span>
        <span><b>Last poll:</b> {since(status?.last_polled_at ?? null)}</span>
        <span><b>Last received:</b> {since(status?.last_received_at ?? null)}</span>
        <span><b>Total received:</b> {status?.total_received ?? 0}</span>
      </div>
      {status?.last_error && (
        <div style={{ marginTop: 6, color: '#ef4444', fontSize: 13 }}>
          {status.last_error}
        </div>
      )}
    </div>
  )
}

function AccountEditor({
  form, onChange, onCancel, onSave, busy,
}: {
  form: AccountForm
  onChange: (f: AccountForm) => void
  onCancel: () => void
  onSave: () => void
  busy: boolean
}) {
  const update = (patch: Partial<AccountForm>) => onChange({ ...form, ...patch })
  return (
    <div style={{ ...card, background: 'var(--bg-elevated)', marginBottom: 16 }}>
      <h3 style={{ marginTop: 0, fontSize: 15 }}>{form.id ? 'Edit email account' : 'New email account'}</h3>

      <FormRow label="Label" hint="A short name shown in the UI.">
        <input type="text" value={form.label} onChange={(e) => update({ label: e.target.value })}
               placeholder="Personal Fastmail" style={input} />
      </FormRow>
      <FormRow label="Email address" hint="The mailbox MIRA reads from and (later) sends as.">
        <input type="email" value={form.address} onChange={(e) => update({ address: e.target.value })}
               placeholder="me@example.com" style={input} />
      </FormRow>

      <FormRow
        label="Authentication"
        hint="Password = IMAP/SMTP creds (self-hosted, Fastmail, iCloud, Gmail App Password). OAuth = Gmail / Outlook via provider-managed token. Webhook = hosted-mail provider POSTs inbound at MIRA (Postmark/Resend/Mailgun) instead of IMAP polling."
      >
        <select
          value={form.auth_mode}
          onChange={(e) => update({ auth_mode: e.target.value as AuthMode })}
          style={input}
        >
          <option value="password">Password</option>
          <option value="oauth_google">OAuth · Gmail</option>
          <option value="oauth_microsoft">OAuth · Outlook / 365</option>
          <option value="webhook">Webhook (Postmark / Resend / Mailgun)</option>
        </select>
      </FormRow>

      {(form.auth_mode === 'oauth_google' || form.auth_mode === 'oauth_microsoft') && form.id && (
        <ConnectButton
          accountId={form.id}
          provider={form.auth_mode === 'oauth_google' ? 'google' : 'microsoft'}
        />
      )}
      {(form.auth_mode === 'oauth_google' || form.auth_mode === 'oauth_microsoft') && !form.id && (
        <p style={{ margin: '8px 0 12px', fontSize: 12, color: 'var(--text-muted)' }}>
          Save the account first, then click <b>Connect</b> on the saved row to
          run through the OAuth flow.
        </p>
      )}

      {form.auth_mode === 'webhook' && (
        <WebhookFields form={form} update={update} />
      )}

      {form.auth_mode === 'password' && <>
      <h4 style={subhead}>IMAP (inbound)</h4>
      <FormRow label="Host" hint="e.g. imap.fastmail.com">
        <input type="text" value={form.imap_host} onChange={(e) => update({ imap_host: e.target.value })}
               placeholder="imap.example.com" style={input} />
      </FormRow>
      <div style={twoCol}>
        <FormRow label="Port" hint="993 for TLS (default); STARTTLS on 143 isn't supported.">
          <input type="number" value={form.imap_port} onChange={(e) => update({ imap_port: parseInt(e.target.value || '0', 10) })} style={input} />
        </FormRow>
        <FormRow label="Use TLS" hint="Required in chunk 2.">
          <input type="checkbox" checked={form.imap_use_tls} onChange={(e) => update({ imap_use_tls: e.target.checked })} />
        </FormRow>
      </div>
      <FormRow label="Username" hint="Usually your email address.">
        <input type="text" value={form.imap_username} onChange={(e) => update({ imap_username: e.target.value })} style={input} />
      </FormRow>
      <FormRow label="Password" hint={form.id ? "Leave blank to keep the current password." : "For Gmail, use an App Password (2FA required)."}>
        <input type="password" value={form.imap_password} onChange={(e) => update({ imap_password: e.target.value })} style={input} autoComplete="new-password" />
      </FormRow>

      <h4 style={subhead}>SMTP (outbound — wired in slice E2)</h4>
      <FormRow label="Host" hint="e.g. smtp.fastmail.com">
        <input type="text" value={form.smtp_host} onChange={(e) => update({ smtp_host: e.target.value })}
               placeholder="smtp.example.com" style={input} />
      </FormRow>
      <div style={twoCol}>
        <FormRow label="Port" hint="465 for TLS (default), 587 for STARTTLS.">
          <input type="number" value={form.smtp_port} onChange={(e) => update({ smtp_port: parseInt(e.target.value || '0', 10) })} style={input} />
        </FormRow>
        <FormRow label="Use TLS" hint="">
          <input type="checkbox" checked={form.smtp_use_tls} onChange={(e) => update({ smtp_use_tls: e.target.checked })} />
        </FormRow>
      </div>
      <FormRow label="Username" hint="">
        <input type="text" value={form.smtp_username} onChange={(e) => update({ smtp_username: e.target.value })} style={input} />
      </FormRow>
      <FormRow label="Password" hint={form.id ? "Leave blank to keep the current password." : ""}>
        <input type="password" value={form.smtp_password} onChange={(e) => update({ smtp_password: e.target.value })} style={input} autoComplete="new-password" />
      </FormRow>
      </>}

      <FormRow label="Enabled" hint="Disabled accounts aren't polled until re-enabled and MIRA restarts.">
        <input type="checkbox" checked={form.enabled} onChange={(e) => update({ enabled: e.target.checked })} />
      </FormRow>

      <div style={{ display: 'flex', gap: 8, marginTop: 12 }}>
        <button onClick={onSave} disabled={busy} style={btnPrimary}>{busy ? 'Saving…' : 'Save'}</button>
        <button onClick={onCancel} disabled={busy} style={btnSecondary}>Cancel</button>
      </div>

      <p style={{ marginTop: 10, fontSize: 12, color: 'var(--text-muted)' }}>
        Security knobs (sender allowlist/denylist, HTML toggles, attachment
        policy, rate limits) will move to their own editor in chunk 3.
        Today every new account inherits the secure-by-default posture:
        unknown senders are quarantined, attachments are dropped, HTML
        is stripped to text.
      </p>
    </div>
  )
}

function WebhookFields({
  form, update,
}: {
  form:   AccountForm
  update: (patch: Partial<AccountForm>) => void
}) {
  const qc = useQueryClient()
  // Re-fetch the row after save to surface the auto-generated secret
  // (only the backend can mint it; the form starts empty).
  const { data: accounts = [] } = useQuery<EmailAccountRow[]>({
    queryKey: ['email', 'accounts'],
    queryFn:  () => api.get('/api/email/accounts').then((r) => r.data),
    enabled:  !!form.id,
  })
  const saved = accounts.find((a) => a.id === form.id)
  const secret = saved?.webhook_secret ?? null

  // Compose the URL the provider should POST to. The host portion
  // comes from the browser — same origin as the running MIRA — so
  // the operator can just copy/paste.
  const webhookUrl = (form.id && secret)
    ? `${window.location.origin}/webhook/email/${form.id}/${secret}`
    : null

  const copy = async () => {
    if (!webhookUrl) return
    try {
      await navigator.clipboard.writeText(webhookUrl)
      toast.success('Webhook URL copied.')
    } catch {
      toast.error('Clipboard write failed — copy from the box.')
    }
  }

  return (
    <div style={{
      marginBottom: 12, padding: 10,
      background: 'var(--bg-input)', borderRadius: 6,
      border: '1px solid var(--border)',
    }}>
      <FormRow
        label="Webhook provider"
        hint="What format MIRA's endpoint expects to receive. Set this to match where you'll configure the inbound forward."
      >
        <select
          value={form.webhook_provider}
          onChange={(e) => update({ webhook_provider: e.target.value as WebhookProvider })}
          style={input}
        >
          <option value="postmark">Postmark</option>
          <option value="resend">Resend</option>
          <option value="mailgun">Mailgun</option>
        </select>
      </FormRow>

      {!form.id && (
        <p style={{ fontSize: 12, color: 'var(--text-muted)' }}>
          Save the account first; MIRA mints the per-account secret on
          creation and the webhook URL appears here.
        </p>
      )}

      {form.id && webhookUrl && (
        <div style={{ marginTop: 6 }}>
          <label style={{ display: 'block', fontWeight: 600, fontSize: 13, marginBottom: 2 }}>
            Webhook URL
          </label>
          <div style={{ display: 'flex', gap: 6 }}>
            <input
              type="text"
              readOnly
              value={webhookUrl}
              style={{ ...input, flex: 1 }}
              onFocus={(e) => e.target.select()}
            />
            <button onClick={copy} style={btnSecondary}>Copy</button>
          </div>
          <p style={{ marginTop: 4, fontSize: 12, color: 'var(--text-muted)' }}>
            Paste this into your provider's inbound configuration
            (Postmark Server → Settings → Inbound; Resend Webhooks;
            Mailgun Routes → Forward to URL). The secret in the URL
            authenticates the call.
          </p>
        </div>
      )}

      {form.id && !webhookUrl && (
        <p style={{ marginTop: 6, fontSize: 12, color: 'var(--text-muted)' }}>
          <button onClick={() => qc.invalidateQueries({ queryKey: ['email', 'accounts'] })} style={btnGhost}>
            Refresh
          </button>{' '}
          to pull the secret from the server.
        </p>
      )}
    </div>
  )
}

function ConnectButton({
  accountId, provider,
}: {
  accountId: string
  provider:  'google' | 'microsoft'
}) {
  const [busy, setBusy] = useState(false)
  const label = provider === 'google' ? 'Gmail' : 'Outlook / 365'

  const onClick = async () => {
    setBusy(true)
    try {
      const r = await api.post(
        `/api/email/accounts/${accountId}/oauth/${provider}/start`,
        {},
      )
      const url = r.data?.authorize_url as string | undefined
      if (!url) {
        toast.error('OAuth start returned no authorize_url')
        return
      }
      // Pop a new tab; the callback page closes itself on success.
      window.open(url, '_blank', 'noopener,noreferrer')
      toast.success(`Opened ${label} OAuth in a new tab — approve there, then come back.`)
    } catch (e: any) {
      const msg = e?.response?.data ?? e?.message ?? String(e)
      toast.error(`Connect failed: ${msg}`)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div style={{
      marginBottom: 12, padding: 10,
      background: 'var(--bg-input)', borderRadius: 6,
      border: '1px solid var(--border)',
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        <button onClick={onClick} disabled={busy} style={btnPrimary}>
          {busy ? 'Opening…' : `Connect ${label}`}
        </button>
        <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>
          MIRA opens the provider's approval page in a new tab. After
          you allow access, the tab auto-closes and the account is
          ready to poll.
        </span>
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
  color: 'var(--text-secondary)', borderRadius: 6, padding: '4px 10px', cursor: 'pointer',
}
const input: React.CSSProperties = {
  width: '100%', boxSizing: 'border-box', padding: 6,
  background: 'var(--bg-input)', border: '1px solid var(--border)',
  borderRadius: 6, color: 'var(--text-primary)',
  fontFamily: 'var(--font-mono)', fontSize: 13,
}
const subhead: React.CSSProperties = {
  marginTop: 16, marginBottom: 6, fontSize: 13, color: 'var(--text-muted)',
  textTransform: 'uppercase', letterSpacing: 0.5,
}
const twoCol: React.CSSProperties = {
  display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12,
}
