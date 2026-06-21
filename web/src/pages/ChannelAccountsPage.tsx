// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Plus, Trash2, Phone, Send, MessageSquare, MessageCircle, Hash, AtSign, Plug, RotateCcw, AlertTriangle, Pencil, Check, X, Play, Square, RotateCw, Loader2, Settings, Link2, Copy } from 'lucide-react'
import toast from 'react-hot-toast'
import {
  channelAccountsApi,
  type AnyChannelConfig,
  type ChannelAccount,
  type ChannelKind,
  type CreateChannelAccountRequest,
  type DiscordAccountConfig,
  type MatrixAccountConfig,
  type WhatsAppAccountConfig,
  type SlackAccountConfig,
  type ExternalAccountConfig,
  type SignalAccountConfig,
  type TelegramAccountConfig,
  type AccountHealth,
} from '@/api/channelAccounts'
import { channelLinksApi, type ChannelLink } from '@/api/channelLinks'
import { useAuthStore } from '@/store/authStore'
import { useRestartServer } from '@/hooks/useRestartServer'
import { providersApi, type StatusInfo } from '@/api/providers'
import { capabilitiesApi, capsAllowChannel } from '@/api/capabilities'
import styles from './ChannelAccountsPage.module.css'

// ── Helpers ─────────────────────────────────────────────────────────────────

const CHANNEL_OPTIONS: { value: ChannelKind; label: string }[] = [
  { value: 'signal',   label: 'Signal' },
  { value: 'telegram', label: 'Telegram' },
  { value: 'discord',  label: 'Discord' },
  { value: 'matrix',   label: 'Matrix' },
  { value: 'whatsapp', label: 'WhatsApp' },
  { value: 'slack',    label: 'Slack' },
  { value: 'external', label: 'External (plugin)' },
]

function blankConfig(channel: ChannelKind): AnyChannelConfig {
  if (channel === 'signal') {
    return { phone_number: '' }
  }
  if (channel === 'discord') {
    return { bot_token: '', application_id: '', mention_only: false }
  }
  if (channel === 'matrix') {
    return { homeserver: 'https://matrix.org', access_token: '', mention_only: false }
  }
  if (channel === 'whatsapp') {
    return { phone_number_id: '', access_token: '', app_secret: '', verify_token: '', mention_only: false }
  }
  if (channel === 'slack') {
    return { bot_token: '', signing_secret: '', mention_only: false }
  }
  if (channel === 'external') {
    return { provider_kind: '', send_url: '', mention_only: false }
  }
  // Polling works behind NAT/localhost with no public URL — the right default
  // for a self-hosted install. Webhook is for public deployments behind a proxy.
  return { bot_token: '', mode: 'polling', secret_token: null }
}

function summarise(acct: ChannelAccount): string {
  if (acct.channel === 'signal') {
    const c = acct.config as SignalAccountConfig
    const port = c.rest_port ? ` · port ${c.rest_port}` : ''
    return `${c.phone_number}${port}`
  }
  if (acct.channel === 'discord') {
    const c = acct.config as DiscordAccountConfig
    return `token ${c.bot_token}${c.mention_only ? ' · mention-only' : ''}`
  }
  if (acct.channel === 'matrix') {
    const c = acct.config as MatrixAccountConfig
    return `${c.homeserver} · token ${c.access_token}${c.mention_only ? ' · mention-only' : ''}`
  }
  if (acct.channel === 'whatsapp') {
    const c = acct.config as WhatsAppAccountConfig
    return `pnid ${c.phone_number_id} · token ${c.access_token}${c.mention_only ? ' · mention-only' : ''}`
  }
  if (acct.channel === 'slack') {
    const c = acct.config as SlackAccountConfig
    return `token ${c.bot_token}${c.mention_only ? ' · mention-only' : ''}`
  }
  if (acct.channel === 'external') {
    const c = acct.config as ExternalAccountConfig
    return `${c.provider_kind} → ${c.send_url}${c.mention_only ? ' · mention-only' : ''}`
  }
  const c = acct.config as TelegramAccountConfig
  return `token ${c.bot_token}${c.secret_token ? ' · secret set' : ''}`
}

// ── Page ────────────────────────────────────────────────────────────────────

export default function ChannelAccountsPage() {
  const qc = useQueryClient()
  const { user } = useAuthStore()
  const isAdmin = user?.role === 'admin'

  const [showCreate, setShowCreate] = useState(false)
  const [channel, setChannel]       = useState<ChannelKind>('signal')
  const [label, setLabel]           = useState('')
  const [enabled, setEnabled]       = useState(true)
  // R1+R2 routing mode for the new account. 'personal' (default) runs
  // every inbound as the bot owner; 'shared'/'guest_ok' resolve the
  // sender to a MIRA user via the identity table (admin-managed bot).
  const [routingMode, setRoutingMode] = useState<'personal' | 'shared' | 'guest_ok'>('personal')
  // Captures a just-created CPP account's one-time secrets + webhook URL.
  const [cppSecrets, setCppSecrets] = useState<{ accountId: string; inboundSecret: string; outboundSecret: string } | null>(null)
  const [cfg, setCfg]               = useState<AnyChannelConfig>(
    blankConfig('signal')
  )
  const [createError, setCreateError] = useState('')
  const [dirty, setDirty]             = useState(false)
  const [editingId, setEditingId]     = useState<string | null>(null)
  const [editLabel, setEditLabel]     = useState('')
  // Inline config edit (separate from inline rename above). When set,
  // the row expands into a SignalForm/TelegramForm prefilled with the
  // existing config; Save calls update({ config }). Avoids the
  // delete-and-recreate dance when a token changes or a mode is flipped.
  const [editConfigId, setEditConfigId]       = useState<string | null>(null)
  const [editConfigDraft, setEditConfigDraft] = useState<AnyChannelConfig | null>(null)
  const [editConfigError, setEditConfigError] = useState('')

  const { data: accounts = [], isLoading } = useQuery<ChannelAccount[]>({
    queryKey: ['channel-accounts'],
    queryFn: channelAccountsApi.list,
  })

  // Capability RBAC — hide channels the caller isn't permitted to add. The
  // backend also enforces (403), so this is UX defense-in-depth.
  const { data: myCaps } = useQuery({
    queryKey: ['me/capabilities'],
    queryFn:  capabilitiesApi.mine,
    retry:    false,
    staleTime: 5 * 60_000,
  })

  const createMut = useMutation({
    mutationFn: (body: CreateChannelAccountRequest) => channelAccountsApi.create(body),
    onSuccess: (created) => {
      qc.invalidateQueries({ queryKey: ['channel-accounts'] })
      // External (CPP) accounts return their generated secrets ONCE. Capture
      // them + the webhook URL so the operator can copy them into the
      // provider before they're redacted on the next read.
      if (created.channel === 'external') {
        const c = created.config as ExternalAccountConfig
        setCppSecrets({
          accountId:      created.id,
          inboundSecret:  c.inbound_secret ?? '',
          outboundSecret: c.outbound_secret ?? '',
        })
      }
      setShowCreate(false)
      setLabel('')
      setCfg(blankConfig(channel))
      setEnabled(true)
      setCreateError('')
      setDirty(true)
      toast.success('Account added. Restart the server to apply.')
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: string } })?.response?.data ?? 'Create failed'
      setCreateError(typeof msg === 'string' ? msg : 'Create failed')
    },
  })

  const deleteMut = useMutation({
    mutationFn: (id: string) => channelAccountsApi.remove(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['channel-accounts'] })
      setDirty(true)
      toast.success('Account removed. Restart the server to apply.')
    },
  })

  const toggleMut = useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      channelAccountsApi.update(id, { enabled }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['channel-accounts'] })
      setDirty(true)
      toast.success('Updated. Restart the server to apply.')
    },
  })

  const renameMut = useMutation({
    mutationFn: ({ id, account_label }: { id: string; account_label: string }) =>
      channelAccountsApi.update(id, { account_label }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['channel-accounts'] })
      setEditingId(null)
      setEditLabel('')
      toast.success('Label updated.')
    },
    onError: () => toast.error('Rename failed'),
  })

  // Edit-config mutation. Same PUT endpoint as rename, but ships the
  // full `config` blob (bot_token, mode, poll_timeout_secs, …). On
  // success: close the inline form, mark dirty so the Restart-server
  // button highlights — the daemons / pollers only pick up the new
  // config on the next start_all.
  const updateConfigMut = useMutation({
    mutationFn: ({ id, config }: { id: string; config: AnyChannelConfig }) =>
      channelAccountsApi.update(id, { config }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['channel-accounts'] })
      setEditConfigId(null)
      setEditConfigDraft(null)
      setEditConfigError('')
      setDirty(true)
      toast.success('Settings updated. Restart the server to apply.')
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: string } })?.response?.data ?? 'Update failed'
      setEditConfigError(typeof msg === 'string' ? msg : 'Update failed')
    },
  })

  // Per-account daemon lifecycle. Each mutation refreshes the health
  // poll on success so the badge flips colour without waiting for the
  // 5-second tick.
  const startMut = useMutation({
    mutationFn: (id: string) => channelAccountsApi.startDaemon(id),
    onSuccess:  (r) => { toast.success(r.message); qc.invalidateQueries({ queryKey: ['channel-accounts-health'] }) },
    onError:    (e: unknown) => {
      const m = (e as { response?: { data?: { message?: string } } }).response?.data?.message
              ?? (e as Error).message
      toast.error(m ?? 'Start failed')
    },
  })
  const stopMut = useMutation({
    mutationFn: (id: string) => channelAccountsApi.stopDaemon(id),
    onSuccess:  (r) => { toast.success(r.message); qc.invalidateQueries({ queryKey: ['channel-accounts-health'] }) },
    onError:    (e: unknown) => toast.error((e as Error).message ?? 'Stop failed'),
  })
  const restartDaemonMut = useMutation({
    mutationFn: (id: string) => channelAccountsApi.restartDaemon(id),
    onSuccess:  (r) => { toast.success(r.message); qc.invalidateQueries({ queryKey: ['channel-accounts-health'] }) },
    onError:    (e: unknown) => {
      const m = (e as { response?: { data?: { message?: string } } }).response?.data?.message
              ?? (e as Error).message
      toast.error(m ?? 'Restart failed')
    },
  })

  const { data: status } = useQuery<StatusInfo>({
    queryKey: ['status'],
    queryFn:  providersApi.status,
    staleTime: 30_000,
  })

  // Per-account daemon liveness. Polled every 5s while the page is
  // open. Cheap (parallel localhost probes); pauses when the tab is
  // hidden so a backgrounded admin UI doesn't keep firing forever.
  const { data: healthList } = useQuery({
    queryKey:    ['channel-accounts-health'],
    queryFn:     channelAccountsApi.health,
    refetchInterval: 5_000,
    refetchIntervalInBackground: false,
  })
  const healthByAccount = new Map((healthList ?? []).map(h => [h.account_id, h]))
  const supervised = status?.supervised ?? false
  const restartMut = useRestartServer({
    supervised,
    onSuccess: () => setDirty(false),
  })

  const onChannelChange = (v: ChannelKind) => {
    setChannel(v)
    setCfg(blankConfig(v))
  }

  const submit = () => {
    if (!label.trim()) {
      setCreateError('Label is required')
      return
    }
    createMut.mutate({ channel, account_label: label.trim(), enabled, routing_mode: routingMode, config: cfg })
  }

  if (isLoading) return <div className={styles.loading}>Loading accounts…</div>

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1>Channel Accounts</h1>
          <p>
            {accounts.length} account{accounts.length !== 1 ? 's' : ''}
            {!isAdmin && ' · showing only your accounts'}
          </p>
        </div>
        <div className={styles.headerActions}>
          {isAdmin && (
            <button
              className={`${styles.btnSecondary} ${dirty ? styles.btnAttention : ''}`}
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
                ? 'Required after adding, removing, or toggling accounts'
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
          <button className={styles.btn} onClick={() => setShowCreate(true)}>
            <Plus size={15} />
            Add account
          </button>
        </div>
      </div>

      {dirty && (
        <div className={styles.dirtyBanner}>
          <AlertTriangle size={14} />
          Changes don't take effect until the server restarts.
        </div>
      )}

      {cppSecrets && (
        <div style={{
          border: '1px solid var(--warning, #b58900)', borderRadius: 8,
          padding: '12px 16px', marginBottom: 16, fontSize: 13,
        }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 8 }}>
            <AlertTriangle size={15} />
            <strong>Copy these into your provider now — they won't be shown again.</strong>
          </div>
          <div style={{ display: 'grid', gridTemplateColumns: 'max-content 1fr', gap: '4px 10px', alignItems: 'center' }}>
            <span style={{ opacity: 0.7 }}>Webhook URL</span>
            <code style={{ wordBreak: 'break-all' }}>{`${window.location.origin}/webhook/external/${cppSecrets.accountId}`}</code>
            <span style={{ opacity: 0.7 }}>Inbound secret</span>
            <code style={{ wordBreak: 'break-all' }}>{cppSecrets.inboundSecret}</code>
            <span style={{ opacity: 0.7 }}>Outbound secret</span>
            <code style={{ wordBreak: 'break-all' }}>{cppSecrets.outboundSecret}</code>
          </div>
          <p style={{ fontSize: 11, opacity: 0.7, margin: '8px 0 0' }}>
            Your provider signs inbound webhooks with the inbound secret and verifies
            MIRA's outbound calls with the outbound secret. See the{' '}
            <a href="https://vexillon.ai/docs/concepts/channels" target="_blank" rel="noopener noreferrer">Channels documentation</a>.
          </p>
          <button className={styles.btnSecondary} style={{ marginTop: 8 }} onClick={() => setCppSecrets(null)}>
            Done — I've copied them
          </button>
        </div>
      )}

      <MyChannelLinks />

      {showCreate && (
        <div className={styles.createForm}>
          <h3>New channel account</h3>
          <div className={styles.formRow}>
            <select
              className={styles.select}
              value={channel}
              onChange={(e) => onChannelChange(e.target.value as ChannelKind)}
            >
              {CHANNEL_OPTIONS
                .filter((o) => capsAllowChannel(myCaps, o.value))
                .map((o) => <option key={o.value} value={o.value}>{o.label}</option>)}
            </select>
            <input
              className={styles.input}
              placeholder="Label (e.g. 'Personal', 'Support bot')"
              value={label}
              onChange={(e) => setLabel(e.target.value)}
            />
            <label className={styles.checkbox}>
              <input
                type="checkbox"
                checked={enabled}
                onChange={(e) => setEnabled(e.target.checked)}
              />
              Enabled
            </label>
          </div>

          {/* R1+R2 — routing mode. Hidden for Signal (per-number daemons
              are inherently personal); shown for Telegram + Discord where
              one admin-managed bot can fan out to many users. */}
          {channel !== 'signal' && (
            <div className={styles.formRow}>
              <select
                className={styles.select}
                value={routingMode}
                onChange={(e) => setRoutingMode(e.target.value as 'personal' | 'shared' | 'guest_ok')}
                title="How inbound messages pick the MIRA user the agent runs as"
              >
                <option value="personal">Personal — every message runs as me (the owner)</option>
                <option value="shared">Shared — route to the linked MIRA user (others must link first)</option>
                <option value="guest_ok">Guest OK — like Shared, but unlinked senders get a guest session</option>
              </select>
            </div>
          )}

          {channel === 'signal' && (
            <SignalForm
              cfg={cfg as SignalAccountConfig}
              onChange={(c) => setCfg(c)}
            />
          )}
          {channel === 'telegram' && (
            <TelegramForm
              cfg={cfg as TelegramAccountConfig}
              onChange={(c) => setCfg(c)}
            />
          )}
          {channel === 'discord' && (
            <DiscordForm
              cfg={cfg as DiscordAccountConfig}
              onChange={(c) => setCfg(c)}
            />
          )}
          {channel === 'matrix' && (
            <MatrixForm
              cfg={cfg as MatrixAccountConfig}
              onChange={(c) => setCfg(c)}
            />
          )}
          {channel === 'whatsapp' && (
            <WhatsAppForm
              cfg={cfg as WhatsAppAccountConfig}
              onChange={(c) => setCfg(c)}
            />
          )}
          {channel === 'slack' && (
            <SlackForm
              cfg={cfg as SlackAccountConfig}
              onChange={(c) => setCfg(c)}
            />
          )}
          {channel === 'external' && (
            <ExternalForm
              cfg={cfg as ExternalAccountConfig}
              onChange={(c) => setCfg(c)}
            />
          )}

          {createError && <p className={styles.error}>{createError}</p>}

          <div className={styles.formActions}>
            <button className={styles.btnSecondary} onClick={() => {
              setShowCreate(false)
              setCreateError('')
            }}>
              Cancel
            </button>
            <button
              className={styles.btn}
              disabled={createMut.isPending}
              onClick={submit}
            >
              {createMut.isPending ? 'Saving…' : 'Save'}
            </button>
          </div>
        </div>
      )}

      <div className={styles.list}>
        {accounts.length === 0 && (
          <p className={styles.empty}>No accounts yet — add one above.</p>
        )}
        {accounts.map((a) => {
          const isMine    = a.user_id === user?.id
          const ownerText = isMine ? 'you' : a.user_id.slice(0, 8)
          const isEditing = editingId === a.id
          const submitRename = () => {
            const trimmed = editLabel.trim()
            if (!trimmed || trimmed === a.account_label) {
              setEditingId(null)
              return
            }
            renameMut.mutate({ id: a.id, account_label: trimmed })
          }
          return (
            <div key={a.id} className={styles.item} data-disabled={!a.enabled || undefined}>
              <div className={styles.iconBubble} data-channel={a.channel}>
                {a.channel === 'signal'
                  ? <Phone size={16} />
                  : a.channel === 'discord'
                    ? <MessageSquare size={16} />
                    : a.channel === 'matrix'
                      ? <Hash size={16} />
                      : a.channel === 'whatsapp'
                        ? <MessageCircle size={16} />
                        : a.channel === 'slack'
                          ? <AtSign size={16} />
                          : a.channel === 'external'
                            ? <Plug size={16} />
                            : <Send size={16} />}
              </div>
              <div className={styles.info}>
                <span className={styles.name}>
                  {isEditing ? (
                    <input
                      className={styles.inlineInput}
                      autoFocus
                      value={editLabel}
                      onChange={(e) => setEditLabel(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') submitRename()
                        if (e.key === 'Escape') { setEditingId(null); setEditLabel('') }
                      }}
                    />
                  ) : (
                    a.account_label
                  )}
                  <span className={styles.channelLabel}>· {a.channel}</span>
                  <HealthBadge h={healthByAccount.get(a.id)} />
                </span>
                <span className={styles.meta}>
                  {summarise(a)}
                  {' · owner '}
                  {isMine ? ownerText : <code>{ownerText}</code>}
                </span>
              </div>
              {isEditing ? (
                <>
                  <button
                    className={styles.iconBtn}
                    onClick={submitRename}
                    title="Save"
                    disabled={renameMut.isPending}
                  >
                    <Check size={15} />
                  </button>
                  <button
                    className={styles.iconBtn}
                    onClick={() => { setEditingId(null); setEditLabel('') }}
                    title="Cancel"
                  >
                    <X size={15} />
                  </button>
                </>
              ) : (
                <>
                  <label className={styles.toggleWrap} title={a.enabled ? 'Disable' : 'Enable'}>
                    <input
                      type="checkbox"
                      checked={a.enabled}
                      onChange={(e) => toggleMut.mutate({ id: a.id, enabled: e.target.checked })}
                    />
                    <span className={styles.toggleTrack} />
                  </label>
                  {(a.channel === 'signal' || a.channel === 'discord' || a.channel === 'matrix') && (
                    <>
                      <button
                        className={styles.iconBtn}
                        onClick={() => startMut.mutate(a.id)}
                        disabled={startMut.isPending || (healthByAccount.get(a.id)?.alive ?? false)}
                        title={a.channel === 'signal' ? 'Start daemon' : 'Connect'}
                      >
                        <Play size={14} />
                      </button>
                      <button
                        className={styles.iconBtn}
                        onClick={() => stopMut.mutate(a.id)}
                        disabled={stopMut.isPending || !(healthByAccount.get(a.id)?.alive ?? false)}
                        title={a.channel === 'signal' ? 'Stop daemon' : 'Disconnect'}
                      >
                        <Square size={14} />
                      </button>
                      <button
                        className={styles.iconBtn}
                        onClick={() => restartDaemonMut.mutate(a.id)}
                        disabled={restartDaemonMut.isPending}
                        title={a.channel === 'signal' ? 'Restart daemon' : 'Reconnect'}
                      >
                        <RotateCw size={14} />
                      </button>
                    </>
                  )}
                  <button
                    className={styles.iconBtn}
                    onClick={() => { setEditingId(a.id); setEditLabel(a.account_label) }}
                    title="Rename"
                  >
                    <Pencil size={14} />
                  </button>
                  <button
                    className={styles.iconBtn}
                    onClick={() => {
                      setEditConfigId(a.id)
                      // Clone so edits don't mutate the React Query
                      // cache row in place (would otherwise show stale
                      // values everywhere else until refetch).
                      setEditConfigDraft({ ...(a.config as AnyChannelConfig) })
                      setEditConfigError('')
                    }}
                    title="Edit settings"
                  >
                    <Settings size={14} />
                  </button>
                  <button
                    className={`${styles.iconBtn} ${styles.danger}`}
                    onClick={() => {
                      if (confirm(`Delete account "${a.account_label}"?`)) deleteMut.mutate(a.id)
                    }}
                    title="Delete"
                  >
                    <Trash2 size={15} />
                  </button>
                </>
              )}
            </div>
          )
        })}

        {/* Inline edit-config panel. Rendered below the row, prefilled
            with the current config. Mirrors the create form but calls
            the update mutation. */}
        {accounts.map((a) => editConfigId === a.id && editConfigDraft && (
          <div key={`${a.id}-edit`} className={styles.create}>
            <h3>Edit settings — {a.account_label}</h3>
            <p className={styles.help}>
              Change any field below and Save. The server needs to restart for
              channel changes (bot tokens, modes) to take effect.
            </p>
            {a.channel === 'signal' && (
              <SignalForm
                cfg={editConfigDraft as SignalAccountConfig}
                onChange={(c) => setEditConfigDraft(c)}
              />
            )}
            {a.channel === 'telegram' && (
              <TelegramForm
                cfg={editConfigDraft as TelegramAccountConfig}
                onChange={(c) => setEditConfigDraft(c)}
              />
            )}
            {a.channel === 'discord' && (
              <DiscordForm
                cfg={editConfigDraft as DiscordAccountConfig}
                onChange={(c) => setEditConfigDraft(c)}
              />
            )}
            {a.channel === 'matrix' && (
              <MatrixForm
                cfg={editConfigDraft as MatrixAccountConfig}
                onChange={(c) => setEditConfigDraft(c)}
              />
            )}
            {a.channel === 'whatsapp' && (
              <WhatsAppForm
                cfg={editConfigDraft as WhatsAppAccountConfig}
                onChange={(c) => setEditConfigDraft(c)}
              />
            )}
            {a.channel === 'slack' && (
              <SlackForm
                cfg={editConfigDraft as SlackAccountConfig}
                onChange={(c) => setEditConfigDraft(c)}
              />
            )}
            {a.channel === 'external' && (
              <ExternalForm
                cfg={editConfigDraft as ExternalAccountConfig}
                onChange={(c) => setEditConfigDraft(c)}
                editing
              />
            )}
            {editConfigError && <p className={styles.error}>{editConfigError}</p>}
            <div className={styles.formActions}>
              <button className={styles.btnSecondary} onClick={() => {
                setEditConfigId(null)
                setEditConfigDraft(null)
                setEditConfigError('')
              }}>
                Cancel
              </button>
              <button
                className={styles.btn}
                disabled={updateConfigMut.isPending}
                onClick={() => updateConfigMut.mutate({ id: a.id, config: editConfigDraft })}
              >
                {updateConfigMut.isPending ? 'Saving…' : 'Save'}
              </button>
            </div>
          </div>
        ))}
      </div>
    </div>
  )
}

// ── Channel-specific forms ──────────────────────────────────────────────────

function SignalForm({
  cfg, onChange,
}: { cfg: SignalAccountConfig; onChange: (c: SignalAccountConfig) => void }) {
  return (
    <div className={styles.formRow}>
      <input
        className={styles.input}
        placeholder="Phone number (E.164, e.g. +14155552671)"
        value={cfg.phone_number}
        onChange={(e) => onChange({ ...cfg, phone_number: e.target.value })}
      />
      <input
        className={styles.input}
        placeholder="signal-cli binary (optional)"
        value={cfg.cli_binary ?? ''}
        onChange={(e) => onChange({ ...cfg, cli_binary: e.target.value || undefined })}
      />
      <input
        className={styles.input}
        placeholder="Data dir (optional)"
        value={cfg.data_dir ?? ''}
        onChange={(e) => onChange({ ...cfg, data_dir: e.target.value || undefined })}
      />
    </div>
  )
}

function TelegramForm({
  cfg, onChange,
}: { cfg: TelegramAccountConfig; onChange: (c: TelegramAccountConfig) => void }) {
  return (
    <div className={styles.formRow}>
      <input
        className={styles.input}
        placeholder="Bot token (from @BotFather)"
        value={cfg.bot_token}
        onChange={(e) => onChange({ ...cfg, bot_token: e.target.value })}
      />
      <input
        className={styles.input}
        placeholder="Webhook secret (optional)"
        value={cfg.secret_token ?? ''}
        onChange={(e) => onChange({ ...cfg, secret_token: e.target.value || null })}
      />
      <select
        className={styles.select}
        value={cfg.mode ?? 'webhook'}
        onChange={(e) => onChange({ ...cfg, mode: e.target.value })}
      >
        <option value="webhook">Webhook</option>
        <option value="polling">Polling</option>
      </select>
      {cfg.mode === 'polling' && (
        <input
          className={styles.input}
          type="number"
          min={5}
          max={50}
          placeholder="Poll timeout (s, default 30)"
          value={cfg.poll_timeout_secs ?? ''}
          onChange={(e) => onChange({
            ...cfg,
            poll_timeout_secs: e.target.value ? Number(e.target.value) : undefined,
          })}
          title="Long-poll hold time in seconds. Telegram caps at 50. Default 30."
        />
      )}
    </div>
  )
}

function DiscordForm({
  cfg, onChange,
}: { cfg: DiscordAccountConfig; onChange: (c: DiscordAccountConfig) => void }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <div className={styles.formRow}>
        <input
          className={styles.input}
          type="password"
          placeholder="Bot token (Developer Portal → Bot → Reset Token)"
          value={cfg.bot_token}
          onChange={(e) => onChange({ ...cfg, bot_token: e.target.value })}
          autoComplete="new-password"
        />
        <input
          className={styles.input}
          placeholder="Application ID (optional)"
          value={cfg.application_id ?? ''}
          onChange={(e) => onChange({ ...cfg, application_id: e.target.value || null })}
          title="Discord Application snowflake (numeric string from General Information). Optional — used to skip our own echoed MESSAGE_CREATE events; we also cache it from the READY event."
        />
        <label className={styles.checkbox} title="When on, MIRA only responds to messages that @-mention the bot. Recommended for shared servers.">
          <input
            type="checkbox"
            checked={cfg.mention_only ?? false}
            onChange={(e) => onChange({ ...cfg, mention_only: e.target.checked })}
          />
          Mention-only
        </label>
      </div>
      <p style={{ fontSize: 11, color: 'var(--text-muted)', margin: 0 }}>
        Setup: <a href="https://discord.com/developers/applications" target="_blank" rel="noopener noreferrer">discord.com/developers/applications</a>
        {' → '}New Application → Bot → Reset Token. In <strong>Privileged Gateway Intents</strong>{' '}
        enable <strong>MESSAGE CONTENT</strong> (required — without it Discord strips message
        text). Invite the bot to your server with OAuth2 → URL Generator: scopes <code>bot</code>,
        permissions <code>Send Messages</code> + <code>Read Message History</code>.
      </p>
    </div>
  )
}

function MatrixForm({
  cfg, onChange,
}: { cfg: MatrixAccountConfig; onChange: (c: MatrixAccountConfig) => void }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <div className={styles.formRow}>
        <input
          className={styles.input}
          placeholder="Homeserver (e.g. https://matrix.org)"
          value={cfg.homeserver}
          onChange={(e) => onChange({ ...cfg, homeserver: e.target.value })}
          title="Base URL of the Matrix homeserver the bot account lives on."
        />
        <input
          className={styles.input}
          type="password"
          placeholder="Access token"
          value={cfg.access_token}
          onChange={(e) => onChange({ ...cfg, access_token: e.target.value })}
          autoComplete="new-password"
          title="Long-lived access token for the bot account."
        />
        <label className={styles.checkbox} title="When on, MIRA only responds to messages that mention the bot. Recommended for shared/group rooms.">
          <input
            type="checkbox"
            checked={cfg.mention_only ?? false}
            onChange={(e) => onChange({ ...cfg, mention_only: e.target.checked })}
          />
          Mention-only
        </label>
      </div>
      <p style={{ fontSize: 11, color: 'var(--text-muted)', margin: 0 }}>
        Setup: create a Matrix account for the bot (on <a href="https://matrix.org" target="_blank" rel="noopener noreferrer">matrix.org</a> or
        your own homeserver), then grab its <strong>access token</strong> from Element →
        Settings → Help &amp; About → Advanced → Access Token. To talk to the bot, invite
        it to a room (it auto-joins) or DM it. The bot replies as text.
      </p>
    </div>
  )
}

function WhatsAppForm({
  cfg, onChange,
}: { cfg: WhatsAppAccountConfig; onChange: (c: WhatsAppAccountConfig) => void }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <div className={styles.formRow}>
        <input
          className={styles.input}
          placeholder="Phone number ID"
          value={cfg.phone_number_id}
          onChange={(e) => onChange({ ...cfg, phone_number_id: e.target.value })}
          title="Cloud API phone-number id from Meta app → WhatsApp → API Setup."
        />
        <input
          className={styles.input}
          type="password"
          placeholder="Access token (permanent)"
          value={cfg.access_token}
          onChange={(e) => onChange({ ...cfg, access_token: e.target.value })}
          autoComplete="new-password"
        />
      </div>
      <div className={styles.formRow}>
        <input
          className={styles.input}
          type="password"
          placeholder="App secret (recommended)"
          value={cfg.app_secret ?? ''}
          onChange={(e) => onChange({ ...cfg, app_secret: e.target.value || null })}
          autoComplete="new-password"
          title="Used to verify inbound webhook signatures. Strongly recommended — without it, anyone who learns your webhook URL can post fake messages."
        />
        <input
          className={styles.input}
          placeholder="Verify token"
          value={cfg.verify_token}
          onChange={(e) => onChange({ ...cfg, verify_token: e.target.value })}
          title="A string you choose; enter the same value in Meta's webhook config. MIRA echoes it back during the subscription handshake."
        />
        <label className={styles.checkbox} title="When on, MIRA only responds to messages containing 'mira' (useful in group chats).">
          <input
            type="checkbox"
            checked={cfg.mention_only ?? false}
            onChange={(e) => onChange({ ...cfg, mention_only: e.target.checked })}
          />
          Mention-only
        </label>
      </div>
      <p style={{ fontSize: 11, color: 'var(--text-muted)', margin: 0 }}>
        Setup needs a Meta <a href="https://developers.facebook.com/docs/whatsapp/cloud-api/get-started" target="_blank" rel="noopener noreferrer">WhatsApp Business Cloud API</a> app:
        a Business account, a registered number, and a permanent access token. Point the
        app's webhook at <code>https://YOUR-HOST/webhook/whatsapp/&lt;this-account-id&gt;</code>{' '}
        (saved after you create the account) with the verify token above. <strong>Note:</strong>{' '}
        proactive messages (check-ins) only work within 24h of the user's last message —
        outside that window Meta requires pre-approved templates (not supported).
        See the <a href="https://vexillon.ai/docs/guides/connect-a-channel" target="_blank" rel="noopener noreferrer">channel setup guide</a>.
      </p>
    </div>
  )
}

function SlackForm({
  cfg, onChange,
}: { cfg: SlackAccountConfig; onChange: (c: SlackAccountConfig) => void }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <div className={styles.formRow}>
        <input
          className={styles.input}
          type="password"
          placeholder="Bot token (xoxb-…)"
          value={cfg.bot_token}
          onChange={(e) => onChange({ ...cfg, bot_token: e.target.value })}
          autoComplete="new-password"
          title="Bot User OAuth token from OAuth & Permissions. Needs chat:write."
        />
        <input
          className={styles.input}
          type="password"
          placeholder="Signing secret"
          value={cfg.signing_secret}
          onChange={(e) => onChange({ ...cfg, signing_secret: e.target.value })}
          autoComplete="new-password"
          title="App signing secret from Basic Information → App Credentials. Verifies inbound event signatures."
        />
        <label className={styles.checkbox} title="When on, MIRA only responds to messages containing 'mira' (useful in busy channels).">
          <input
            type="checkbox"
            checked={cfg.mention_only ?? false}
            onChange={(e) => onChange({ ...cfg, mention_only: e.target.checked })}
          />
          Mention-only
        </label>
      </div>
      <p style={{ fontSize: 11, color: 'var(--text-muted)', margin: 0 }}>
        Setup: create a Slack app at <a href="https://api.slack.com/apps" target="_blank" rel="noopener noreferrer">api.slack.com/apps</a>,
        add the <code>chat:write</code> bot scope + the message events you want
        (e.g. <code>im:history</code>, <code>message.channels</code>), and install it to your
        workspace for the bot token. Under <strong>Event Subscriptions</strong>, set the
        Request URL to <code>https://YOUR-HOST/webhook/slack/&lt;this-account-id&gt;</code>{' '}
        (saved after you create the account) — Slack verifies it with a challenge MIRA
        echoes back automatically. See the{' '}
        <a href="https://vexillon.ai/docs/guides/connect-a-channel" target="_blank" rel="noopener noreferrer">channel setup guide</a>.
      </p>
    </div>
  )
}

function ExternalForm({
  cfg, onChange, editing,
}: { cfg: ExternalAccountConfig; onChange: (c: ExternalAccountConfig) => void; editing?: boolean }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <div className={styles.formRow}>
        <input
          className={styles.input}
          placeholder="Provider kind (e.g. nctalk)"
          value={cfg.provider_kind}
          onChange={(e) => onChange({ ...cfg, provider_kind: e.target.value })}
          title="Short slug identifying the provider. Namespaces conversations + identity links as external:<kind>."
        />
        <input
          className={styles.input}
          placeholder="Send URL (https://your-provider/cpp/send)"
          value={cfg.send_url}
          onChange={(e) => onChange({ ...cfg, send_url: e.target.value })}
          title="The provider endpoint MIRA POSTs outbound replies to."
        />
        <label className={styles.checkbox} title="When on, MIRA only responds to messages containing 'mira'.">
          <input
            type="checkbox"
            checked={cfg.mention_only ?? false}
            onChange={(e) => onChange({ ...cfg, mention_only: e.target.checked })}
          />
          Mention-only
        </label>
        <label className={styles.checkbox} title="Check only if your provider can play audio (e.g. Nextcloud Talk voice messages). Enables voice replies for this channel, gated by the user's per-channel voice policy in Settings.">
          <input
            type="checkbox"
            checked={cfg.supports_voice ?? false}
            onChange={(e) => onChange({ ...cfg, supports_voice: e.target.checked })}
          />
          Provider supports voice
        </label>
      </div>
      <p style={{ fontSize: 11, color: 'var(--text-muted)', margin: 0 }}>
        A <strong>Channel Provider Protocol (CPP)</strong> plugin channel — an external
        process bridges some messaging system (e.g. Nextcloud Talk) to MIRA over signed
        HTTP. {editing
          ? 'The two HMAC secrets are set; they are not shown again here.'
          : 'On save, MIRA generates two HMAC secrets and shows them once (copy them into your provider).'}{' '}
        See the <a href="https://vexillon.ai/docs/concepts/channels" target="_blank" rel="noopener noreferrer">Channels documentation</a> to write a provider.
      </p>
    </div>
  )
}

/// Per-account daemon liveness badge. Renders as a colored dot with a
/// hover tooltip carrying the latency / error reason. Inline styles
/// only (avoids touching the page's CSS module).
function HealthBadge({ h }: { h?: AccountHealth }) {
  if (!h) {
    return (
      <span title="Health probe pending…" style={dotStyle('#888')}>
        <span style={dotInner('#888')} />
        <span style={dotLabel}>?</span>
      </span>
    )
  }
  if (h.alive) {
    const lat = h.latency_ms != null ? `${h.latency_ms}ms` : 'webhook'
    return (
      <span title={`Daemon alive (${lat})`} style={dotStyle('var(--success, #22c55e)')}>
        <span style={dotInner('var(--success, #22c55e)')} />
        <span style={dotLabel}>live</span>
      </span>
    )
  }
  return (
    <span
      title={h.error ?? 'Daemon down'}
      style={dotStyle('var(--error, #ef4444)')}
    >
      <span style={dotInner('var(--error, #ef4444)')} />
      <span style={dotLabel}>down</span>
    </span>
  )
}

const dotStyle = (color: string): React.CSSProperties => ({
  display: 'inline-flex',
  alignItems: 'center',
  gap: 4,
  marginLeft: 8,
  padding: '0 6px 0 4px',
  borderRadius: 999,
  border: `1px solid ${color}`,
  fontSize: 10,
  lineHeight: '14px',
  height: 14,
  color,
  textTransform: 'uppercase',
  letterSpacing: 0.4,
})
const dotInner = (color: string): React.CSSProperties => ({
  width: 6, height: 6, borderRadius: '50%', background: color,
})
const dotLabel: React.CSSProperties = { fontWeight: 600 }

/// "My Channels" — self-serve identity linking (R1+R2). Lets any user
/// (admin or not) link their own Telegram/Discord/Signal identity to a
/// shared, admin-managed bot: generate a one-time LINK-XXXX-XXXX code,
/// send it to the bot, and the bot claims the mapping. Existing links
/// are listed with revoke buttons. Inline-styled to stay self-contained.
function MyChannelLinks() {
  const qc = useQueryClient()
  // Channel value the code is generated for. Fixed-slug channels use their
  // bare name; CPP providers use the full `external:<provider_kind>` string
  // (that's what link codes are keyed on). Hence a plain string, not a union.
  const [linkChannel, setLinkChannel] = useState<string>('telegram')
  const [issuedCode, setIssuedCode]   = useState<{ code: string; channel: string; ttl: number } | null>(null)

  const { data: links = [] } = useQuery<ChannelLink[]>({
    queryKey: ['my-channel-links'],
    queryFn:  channelLinksApi.list,
  })

  // Reuse the page's cached channel-accounts list to discover any
  // configured CPP (external) providers, so the dropdown can offer a
  // per-provider `external:<kind>` entry instead of a useless generic one.
  const { data: accounts = [] } = useQuery<ChannelAccount[]>({
    queryKey: ['channel-accounts'],
    queryFn:  channelAccountsApi.list,
  })
  const externalKinds = Array.from(new Set(
    accounts
      .filter((a) => a.channel === 'external')
      .map((a) => (a.config as ExternalAccountConfig).provider_kind)
      .filter((k): k is string => !!k && k.trim().length > 0),
  )).sort()

  const issueMut = useMutation({
    mutationFn: () => channelLinksApi.issueCode(linkChannel),
    onSuccess:  (r) => {
      setIssuedCode({ code: r.code, channel: r.channel, ttl: r.ttl_seconds })
    },
    onError: () => toast.error('Could not generate a link code.'),
  })

  const removeMut = useMutation({
    mutationFn: (id: string) => channelLinksApi.remove(id),
    onSuccess:  () => {
      qc.invalidateQueries({ queryKey: ['my-channel-links'] })
      toast.success('Link removed.')
    },
    onError: () => toast.error('Could not remove link.'),
  })

  const box: React.CSSProperties = {
    border: '1px solid var(--border, #333)', borderRadius: 8,
    padding: '14px 16px', marginBottom: 20,
  }

  return (
    <div style={box}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 6 }}>
        <Link2 size={16} />
        <strong>My channels</strong>
      </div>
      <p style={{ fontSize: 13, opacity: 0.75, marginTop: 0, marginBottom: 12 }}>
        Talking to a shared MIRA bot someone else set up? Link your account so the
        bot knows it's you: generate a code, then send it to the bot in a direct
        message. Codes expire after 10 minutes and can be used once.
      </p>

      <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
        <select
          className={styles.select}
          value={linkChannel}
          onChange={(e) => { setLinkChannel(e.target.value); setIssuedCode(null) }}
        >
          <option value="telegram">Telegram</option>
          <option value="discord">Discord</option>
          <option value="matrix">Matrix</option>
          <option value="whatsapp">WhatsApp</option>
          <option value="slack">Slack</option>
          <option value="signal">Signal</option>
          {externalKinds.map((kind) => (
            <option key={kind} value={`external:${kind}`}>External — {kind}</option>
          ))}
        </select>
        <button
          className={styles.btn}
          disabled={issueMut.isPending}
          onClick={() => issueMut.mutate()}
        >
          {issueMut.isPending ? <Loader2 size={14} /> : <Plus size={14} />}
          Generate link code
        </button>
      </div>

      {issuedCode && (
        <div style={{
          marginTop: 12, padding: '10px 12px', borderRadius: 6,
          background: 'var(--bg-subtle, rgba(255,255,255,0.04))',
          display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap',
        }}>
          <code style={{ fontSize: 16, letterSpacing: 1, fontWeight: 600 }}>{issuedCode.code}</code>
          <button
            className={styles.iconBtn}
            title="Copy code"
            onClick={() => {
              navigator.clipboard?.writeText(issuedCode.code)
                .then(() => toast.success('Code copied.'))
                .catch(() => toast.error('Copy failed — select it manually.'))
            }}
          >
            <Copy size={14} />
          </button>
          <span style={{ fontSize: 12, opacity: 0.7 }}>
            Send this to your {issuedCode.channel} bot within {Math.round(issuedCode.ttl / 60)} min.
          </span>
        </div>
      )}

      {links.length > 0 && (
        <div style={{ marginTop: 14 }}>
          <div style={{ fontSize: 12, textTransform: 'uppercase', opacity: 0.6, marginBottom: 6 }}>
            Linked accounts
          </div>
          {links.map((l) => (
            <div key={l.id} style={{
              display: 'flex', alignItems: 'center', gap: 8,
              padding: '6px 0', borderTop: '1px solid var(--border, #2a2a2a)',
            }}>
              <span style={{ fontSize: 13, minWidth: 80, textTransform: 'capitalize' }}>{l.channel}</span>
              <code style={{ fontSize: 12, opacity: 0.8, flex: 1 }}>{l.external_id}</code>
              <button
                className={`${styles.iconBtn} ${styles.danger}`}
                title="Remove link"
                disabled={removeMut.isPending}
                onClick={() => {
                  if (confirm(`Unlink your ${l.channel} account (${l.external_id})?`)) {
                    removeMut.mutate(l.id)
                  }
                }}
              >
                <Trash2 size={14} />
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
