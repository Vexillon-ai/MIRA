// SPDX-License-Identifier: AGPL-3.0-or-later
//
// First-run setup wizard — the prominent, guided form of the setup
// walkthrough. On a fresh install an admin is stepped through the three
// optional things `mira setup` defers to the web UI: enable voice, connect a
// channel, and turn on proactive check-ins. Every step is skippable; nothing
// here is mandatory. Closing or skipping the wizard sets the shared `skipped`
// flag — that releases the user-onboarding gate and leaves the slim
// SetupChecklistBanner as a "finish later" reminder (which can reopen this).
//
// Voice and channels are configured inline against the same validated APIs the
// Settings pages use (GET→mutate→PUT /api/config round-trips redacted secrets
// via the `***` sentinel, so it's safe). Check-ins enable companion mode for
// the caller via POST /api/me/companion/enable.

import { useState, useEffect } from 'react'
import { useNavigate } from 'react-router-dom'
import { useQuery, useQueryClient, useMutation } from '@tanstack/react-query'
import axios from 'axios'
import toast from 'react-hot-toast'
import {
  Sparkles, Check, X, Volume2, MessageSquare, BellRing,
  ChevronRight, Loader2, ExternalLink,
} from 'lucide-react'
import { api } from '@/api/client'
import { ttsApi } from '@/api/tts'
import { channelAccountsApi, type ChannelKind } from '@/api/channelAccounts'
import { companionApi } from '@/api/companion'
import type { User } from '@/api/types'
import { useAuthStore } from '@/store/authStore'
import { useUiStore } from '@/store/uiStore'
import { useSetupChecklist } from '@/hooks/useSetupChecklist'
import styles from './SetupWizard.module.css'

type StepId = 'welcome' | 'voice' | 'channel' | 'checkins' | 'done'
const STEP_ORDER: StepId[] = ['welcome', 'voice', 'channel', 'checkins', 'done']

// The three configurable steps, for the progress rail. `welcome`/`done` are
// framing, not checklist items.
const RAIL: { id: StepId; label: string; icon: typeof Volume2; key?: 'voice' | 'channel' | 'companion' }[] = [
  { id: 'voice',    label: 'Voice',     icon: Volume2,       key: 'voice' },
  { id: 'channel',  label: 'Channel',   icon: MessageSquare, key: 'channel' },
  { id: 'checkins', label: 'Check-ins', icon: BellRing,      key: 'companion' },
]

// Channels we can connect with a single inline form (one or two secrets).
// Everything else deep-links to the full Channels page.
const INLINE_CHANNELS: { kind: ChannelKind; label: string; help: string }[] = [
  { kind: 'telegram', label: 'Telegram', help: 'Paste the bot token from @BotFather.' },
  { kind: 'discord',  label: 'Discord',  help: 'Paste the bot token from the Discord Developer Portal.' },
]

function errMessage(e: unknown, fallback: string): string {
  if (axios.isAxiosError(e)) {
    const d = e.response?.data as { error?: string; message?: string } | undefined
    return d?.error || d?.message || e.message || fallback
  }
  return fallback
}

export default function SetupWizard() {
  const navigate = useNavigate()
  const qc = useQueryClient()
  const isAuthed = useAuthStore((s) => s.isAuthenticated)
  const me = useAuthStore((s) => s.user)
  const setSkipped = useUiStore((s) => s.setSetupChecklistSkippedAt)

  const { status, allDone, wizardActive } = useSetupChecklist()

  const [idx, setIdx] = useState(0)
  // Once the admin engages (advances past welcome / completes a step), keep the
  // wizard open even if all steps become done — so they reach the "done" card
  // instead of the modal vanishing mid-flow.
  const [engaged, setEngaged] = useState(false)

  const step = STEP_ORDER[idx]

  // Show for a fresh-install admin who hasn't skipped. An already-configured
  // admin (allDone) who never engaged is skipped past.
  const show = isAuthed && wizardActive && (!allDone || engaged)

  // Voice step state
  const [voiceBackend, setVoiceBackend] = useState<string>('')
  // Channel step state
  const [chanKind, setChanKind] = useState<ChannelKind>('telegram')
  const [chanToken, setChanToken] = useState('')
  const [chanSecret, setChanSecret] = useState('') // slack signing secret (unused for tg/discord)
  // Check-ins step state
  const [safetyContact, setSafetyContact] = useState('')
  const [maxPerDay, setMaxPerDay] = useState(3)
  const [briefHour, setBriefHour] = useState(7)
  const [ackCheckins, setAckCheckins] = useState(false)

  // Voice backends (for the picker); only fetched while the wizard is shown.
  const { data: ttsStatus } = useQuery({
    queryKey: ['tts', 'status'],
    queryFn: () => ttsApi.status(),
    enabled: show,
    staleTime: 60_000,
  })
  useEffect(() => {
    if (ttsStatus && !voiceBackend) setVoiceBackend(ttsStatus.backend || ttsStatus.backends?.[0] || '')
  }, [ttsStatus, voiceBackend])

  // Other users → safety-contact candidates (a user can't be their own).
  const { data: users = [] } = useQuery<User[]>({
    queryKey: ['users'],
    queryFn: () => api.get('/api/users').then((r) => r.data),
    enabled: show && step === 'checkins',
    staleTime: 60_000,
  })
  const contacts = users.filter((u) => u.id !== me?.id)

  const advance = () => { setEngaged(true); setIdx((i) => Math.min(i + 1, STEP_ORDER.length - 1)) }
  const back = () => setIdx((i) => Math.max(i - 1, 0))
  const refreshChecklist = () => qc.invalidateQueries({ queryKey: ['setup-checklist'] })

  const closeAsSkipped = () => setSkipped(Date.now()) // releases onboarding; banner takes over

  const voiceMut = useMutation({
    mutationFn: async () => {
      const cfg = await api.get('/api/config').then((r) => r.data)
      cfg.tts = cfg.tts || {}
      cfg.tts.enabled = true
      if (voiceBackend) cfg.tts.default_backend = voiceBackend
      await api.put('/api/config', cfg)
    },
    onSuccess: () => { refreshChecklist(); qc.invalidateQueries({ queryKey: ['tts', 'status'] }); toast.success('Voice enabled'); advance() },
    onError: (e) => {
      if (axios.isAxiosError(e) && e.response?.status === 422) {
        toast.error('That voice needs a one-time model download — finish it in Settings → Voice.')
      } else {
        toast.error(errMessage(e, 'Could not enable voice'))
      }
    },
  })

  const channelMut = useMutation({
    mutationFn: () => {
      const label = INLINE_CHANNELS.find((c) => c.kind === chanKind)?.label ?? 'Account'
      const config =
        chanKind === 'telegram' ? { bot_token: chanToken.trim(), mode: 'webhook', secret_token: null }
        : chanKind === 'discord' ? { bot_token: chanToken.trim(), application_id: '', mention_only: false }
        : { bot_token: chanToken.trim(), signing_secret: chanSecret.trim(), mention_only: false }
      return channelAccountsApi.create({ channel: chanKind, account_label: label, config, enabled: true })
    },
    onSuccess: () => { refreshChecklist(); qc.invalidateQueries({ queryKey: ['channel-accounts'] }); toast.success('Channel connected'); advance() },
    onError: (e) => toast.error(errMessage(e, 'Could not connect the channel')),
  })

  const checkinsMut = useMutation({
    mutationFn: () => companionApi.enable({
      safety_contact_user_id: safetyContact || null,
      max_per_day: maxPerDay,
      briefing_enabled: true,
      briefing_hour: briefHour,
    }),
    onSuccess: () => { refreshChecklist(); qc.invalidateQueries({ queryKey: ['briefing'] }); toast.success('Check-ins enabled'); advance() },
    onError: (e) => toast.error(errMessage(e, 'Could not enable check-ins')),
  })

  // Esc skips the whole wizard (counts as "I'll finish later").
  useEffect(() => {
    if (!show) return
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') closeAsSkipped() }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [show])

  if (!show) return null

  const busy = voiceMut.isPending || channelMut.isPending || checkinsMut.isPending
  const stepDone = (key?: 'voice' | 'channel' | 'companion') => !!(key && status?.[key])

  return (
    <div className={styles.backdrop} role="dialog" aria-modal="true" aria-labelledby="wiz-title">
      <div className={styles.dialog}>
        <button className={styles.closeBtn} onClick={closeAsSkipped} aria-label="Skip setup (Esc)" title="Skip setup (Esc)">
          <X size={15} />
        </button>

        {step !== 'welcome' && step !== 'done' && (
          <div className={styles.rail}>
            {RAIL.map((r) => {
              const isCur = r.id === step
              const done = stepDone(r.key)
              const Icon = r.icon
              return (
                <div key={r.id} className={`${styles.railItem} ${isCur ? styles.railCur : ''} ${done ? styles.railDone : ''}`}>
                  <span className={styles.railDot}>{done ? <Check size={12} /> : <Icon size={12} />}</span>
                  <span>{r.label}</span>
                </div>
              )
            })}
          </div>
        )}

        <div className={styles.body}>
          {step === 'welcome' && (
            <>
              <div className={styles.icon}><Sparkles size={26} /></div>
              <h2 id="wiz-title" className={styles.title}>Let's set up MIRA</h2>
              <p className={styles.lead}>
                A few quick, optional steps to get the most out of MIRA — spoken
                replies, a messaging channel so it can reach you outside the
                browser, and proactive check-ins. You can skip any of them and
                come back later.
              </p>
              <div className={styles.actions}>
                <button className={styles.primary} onClick={advance}>Get started <ChevronRight size={15} /></button>
                <button className={styles.ghost} onClick={closeAsSkipped}>Skip setup</button>
              </div>
            </>
          )}

          {step === 'voice' && (
            <>
              <h2 id="wiz-title" className={styles.title}>Enable voice</h2>
              <p className={styles.lead}>Let MIRA speak its replies and check-ins aloud (text-to-speech).</p>
              {stepDone('voice') ? (
                <p className={styles.doneNote}><Check size={15} /> Voice is already on.</p>
              ) : (
                <label className={styles.field}>
                  <span className={styles.fieldLabel}>Voice engine</span>
                  <select className={styles.select} value={voiceBackend} onChange={(e) => setVoiceBackend(e.target.value)}>
                    {(ttsStatus?.backends ?? []).map((b) => <option key={b} value={b}>{b}</option>)}
                  </select>
                </label>
              )}
              <div className={styles.actions}>
                <button className={styles.ghost} onClick={back}>Back</button>
                {stepDone('voice')
                  ? <button className={styles.primary} onClick={advance}>Next <ChevronRight size={15} /></button>
                  : <>
                      <button className={styles.skip} onClick={advance}>Skip</button>
                      <button className={styles.primary} disabled={busy} onClick={() => voiceMut.mutate()}>
                        {voiceMut.isPending ? <><Loader2 size={15} className={styles.spin} /> Enabling…</> : 'Enable voice'}
                      </button>
                    </>}
              </div>
            </>
          )}

          {step === 'channel' && (
            <>
              <h2 id="wiz-title" className={styles.title}>Connect a channel</h2>
              <p className={styles.lead}>Reach MIRA outside the browser — message it and get proactive check-ins.</p>
              {stepDone('channel') ? (
                <p className={styles.doneNote}><Check size={15} /> A channel is already connected.</p>
              ) : (
                <>
                  <div className={styles.chips}>
                    {INLINE_CHANNELS.map((c) => (
                      <button key={c.kind} className={`${styles.chip} ${chanKind === c.kind ? styles.chipOn : ''}`}
                        onClick={() => { setChanKind(c.kind); setChanToken(''); setChanSecret('') }}>
                        {c.label}
                      </button>
                    ))}
                  </div>
                  <p className={styles.hint}>{INLINE_CHANNELS.find((c) => c.kind === chanKind)?.help}</p>
                  <label className={styles.field}>
                    <span className={styles.fieldLabel}>Bot token</span>
                    <input className={styles.input} type="password" autoComplete="off" value={chanToken}
                      onChange={(e) => setChanToken(e.target.value)} placeholder="Paste token…" />
                  </label>
                  <button className={styles.linkBtn} onClick={() => { closeAsSkipped(); navigate('/channel-accounts') }}>
                    Set up a different channel (Signal, email, …) <ExternalLink size={12} />
                  </button>
                </>
              )}
              <div className={styles.actions}>
                <button className={styles.ghost} onClick={back}>Back</button>
                {stepDone('channel')
                  ? <button className={styles.primary} onClick={advance}>Next <ChevronRight size={15} /></button>
                  : <>
                      <button className={styles.skip} onClick={advance}>Skip</button>
                      <button className={styles.primary} disabled={busy || !chanToken.trim()} onClick={() => channelMut.mutate()}>
                        {channelMut.isPending ? <><Loader2 size={15} className={styles.spin} /> Connecting…</> : 'Connect'}
                      </button>
                    </>}
              </div>
            </>
          )}

          {step === 'checkins' && (
            <>
              <h2 id="wiz-title" className={styles.title}>Enable proactive check-ins</h2>
              <p className={styles.lead}>
                MIRA reaches out on its own — a warm hello when it's been a while,
                and an optional daily briefing. It will not exceed your cap or
                message during quiet hours.
              </p>
              {stepDone('companion') ? (
                <p className={styles.doneNote}><Check size={15} /> Check-ins are already on.</p>
              ) : (
                <>
                  <label className={styles.field}>
                    <span className={styles.fieldLabel}>Safety contact <span className={styles.opt}>(optional)</span></span>
                    <select className={styles.select} value={safetyContact} onChange={(e) => setSafetyContact(e.target.value)}>
                      <option value="">None — no one to notify</option>
                      {contacts.map((u) => <option key={u.id} value={u.id}>{u.display_name || u.username}</option>)}
                    </select>
                    <span className={styles.hint}>Another MIRA user notified only if the safety floor triggers.</span>
                  </label>
                  <div className={styles.row}>
                    <label className={styles.field}>
                      <span className={styles.fieldLabel}>Max check-ins / day</span>
                      <input className={styles.input} type="number" min={0} max={24} value={maxPerDay}
                        onChange={(e) => setMaxPerDay(Math.max(0, Math.min(24, Number(e.target.value) || 0)))} />
                    </label>
                    <label className={styles.field}>
                      <span className={styles.fieldLabel}>Daily briefing</span>
                      <select className={styles.select} value={briefHour} onChange={(e) => setBriefHour(Number(e.target.value))}>
                        {Array.from({ length: 24 }, (_, h) => (
                          <option key={h} value={h}>{String(h).padStart(2, '0')}:00</option>
                        ))}
                      </select>
                    </label>
                  </div>
                  <label className={styles.ack}>
                    <input type="checkbox" checked={ackCheckins} onChange={(e) => setAckCheckins(e.target.checked)} />
                    <span>I understand MIRA will start conversations with me on its own.</span>
                  </label>
                </>
              )}
              <div className={styles.actions}>
                <button className={styles.ghost} onClick={back}>Back</button>
                {stepDone('companion')
                  ? <button className={styles.primary} onClick={advance}>Next <ChevronRight size={15} /></button>
                  : <>
                      <button className={styles.skip} onClick={advance}>Skip</button>
                      <button className={styles.primary} disabled={busy || !ackCheckins} onClick={() => checkinsMut.mutate()}>
                        {checkinsMut.isPending ? <><Loader2 size={15} className={styles.spin} /> Enabling…</> : 'Enable check-ins'}
                      </button>
                    </>}
              </div>
            </>
          )}

          {step === 'done' && (
            <>
              <div className={styles.icon}><Check size={26} /></div>
              <h2 id="wiz-title" className={styles.title}>You're set up</h2>
              <p className={styles.lead}>
                Nice. You can change any of this anytime from Settings and the
                Channels page. Next, MIRA will get to know you a little.
              </p>
              <div className={styles.actions}>
                <button className={styles.primary} onClick={closeAsSkipped}>Finish</button>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  )
}
