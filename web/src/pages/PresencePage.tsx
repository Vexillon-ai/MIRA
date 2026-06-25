// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useMemo, useState } from 'react'
import { useQuery, useMutation } from '@tanstack/react-query'
import { Link } from 'react-router-dom'
import { Sparkles, Plus, Trash2, Loader2, Save } from 'lucide-react'
import toast from 'react-hot-toast'
import { queryClient } from '@/api/queryClient'
import {
  getPresence,
  updatePresence,
  type PresenceSettings,
  type PresenceUpdate,
  type PresenceTone,
  type PresenceMessageMix,
} from '@/api/companion'
import styles from './PresencePage.module.css'

// Defaults applied when the server sends a null (the band/gap fields are
// nullable on the wire but want a concrete value in the form).
const DEFAULT_MAX_PER_DAY = 6
const DEFAULT_MIN_GAP_MINUTES = 90

const HHMM = /^([01]\d|2[0-3]):([0-5]\d)$/

// Tone presets — each sets all three sliders at once.
const TONE_PRESETS: { label: string; tone: PresenceTone }[] = [
  { label: 'Warm & chatty',  tone: { warmth: 80, playfulness: 60, verbosity: 70 } },
  { label: 'Calm & concise', tone: { warmth: 60, playfulness: 25, verbosity: 25 } },
  { label: 'Playful',        tone: { warmth: 70, playfulness: 90, verbosity: 55 } },
  { label: 'Professional',   tone: { warmth: 45, playfulness: 15, verbosity: 45 } },
]

// message_mix keys → friendly labels, in display order.
const MESSAGE_MIX_LABELS: { key: keyof PresenceMessageMix; label: string }[] = [
  { key: 'check_in',      label: 'Check-ins' },
  { key: 'joke',          label: 'Jokes' },
  { key: 'status_update', label: "What I've been up to" },
  { key: 'follow_up',     label: 'Follow-ups' },
  { key: 'share',         label: 'Shares' },
  { key: 'encouragement', label: 'Encouragement' },
]

// Local, editable copy of just the fields this page tunes.
interface FormState {
  frequency_mode: 'fuzzy' | 'scheduled'
  min_per_day: number
  max_per_day: number
  min_gap_minutes: number
  scheduled_times: string[]
  tone: PresenceTone
  message_mix: PresenceMessageMix
  share_agent_activity: boolean
  daily_briefing_enabled: boolean
  daily_briefing_hour: number
}

function toForm(s: PresenceSettings): FormState {
  return {
    frequency_mode: s.frequency_mode,
    min_per_day: s.min_per_day,
    max_per_day: s.max_per_day ?? DEFAULT_MAX_PER_DAY,
    min_gap_minutes: s.min_gap_minutes ?? DEFAULT_MIN_GAP_MINUTES,
    scheduled_times: [...s.scheduled_times],
    tone: { ...s.tone },
    message_mix: { ...s.message_mix },
    share_agent_activity: s.share_agent_activity,
    daily_briefing_enabled: s.daily_briefing_enabled,
    daily_briefing_hour: s.daily_briefing_hour,
  }
}

// Relative-time formatter for "Last reached out".
function relativeTime(ms: number): string {
  const diff = Date.now() - ms
  if (diff < 0) return 'just now'
  const mins = Math.floor(diff / 60_000)
  if (mins < 1) return 'just now'
  if (mins < 60) return `${mins} min ago`
  const hrs = Math.floor(mins / 60)
  if (hrs < 24) return `${hrs}h ago`
  const days = Math.floor(hrs / 24)
  if (days < 30) return `${days}d ago`
  return new Date(ms).toLocaleDateString()
}

export default function PresencePage() {
  const { data, isLoading } = useQuery({
    queryKey: ['me-companion'],
    queryFn:  getPresence,
  })

  const [form, setForm] = useState<FormState | null>(null)
  const [timesError, setTimesError] = useState('')

  // Seed / re-seed local form whenever the query data changes (initial load
  // and after a save refetch).
  useEffect(() => {
    if (data) setForm(toForm(data))
  }, [data])

  const saveMut = useMutation({
    mutationFn: (body: PresenceUpdate) => updatePresence(body),
    onSuccess: (updated) => {
      // Push the fresh server copy into the cache; the effect above re-seeds
      // the form from it.
      queryClient.setQueryData(['me-companion'], updated)
      queryClient.invalidateQueries({ queryKey: ['me-companion'] })
      toast.success('Presence updated.')
    },
    onError: () => toast.error('Could not save presence settings.'),
  })

  const pausedUntil = useMemo(() => {
    if (!data?.paused_until_ms) return null
    return data.paused_until_ms > Date.now() ? data.paused_until_ms : null
  }, [data])

  if (isLoading || !data || !form) {
    return <div className={styles.loading}>Loading…</div>
  }

  const set = <K extends keyof FormState>(key: K, value: FormState[K]) =>
    setForm((f) => (f ? { ...f, [key]: value } : f))

  const setTone = (key: keyof PresenceTone, value: number) =>
    setForm((f) => (f ? { ...f, tone: { ...f.tone, [key]: value } } : f))

  const setMix = (key: keyof PresenceMessageMix, value: boolean) =>
    setForm((f) => (f ? { ...f, message_mix: { ...f.message_mix, [key]: value } } : f))

  const onSave = () => {
    // Validate scheduled times before sending.
    if (form.frequency_mode === 'scheduled') {
      const bad = form.scheduled_times.some((t) => !HHMM.test(t))
      if (form.scheduled_times.length === 0 || bad) {
        setTimesError('Enter at least one valid time as HH:MM (24-hour).')
        return
      }
    }
    setTimesError('')

    const body: PresenceUpdate = {
      frequency_mode:         form.frequency_mode,
      min_per_day:            form.min_per_day,
      max_per_day:            form.max_per_day,
      min_gap_minutes:        form.min_gap_minutes,
      scheduled_times:        form.scheduled_times,
      tone:                   form.tone,
      message_mix:            form.message_mix,
      share_agent_activity:   form.share_agent_activity,
      daily_briefing_enabled: form.daily_briefing_enabled,
      daily_briefing_hour:    form.daily_briefing_hour,
    }
    saveMut.mutate(body)
  }

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <div>
          <h1><Sparkles size={18} /> Presence</h1>
          <p>How MIRA reaches out to you — its rhythm and personality.</p>
          <div className={styles.stateRow}>
            {data.enabled ? (
              <>
                <span className={`${styles.statePill} ${styles.statePillOn}`}>On</span>
                {pausedUntil && (
                  <span className={styles.stateMeta}>
                    Paused until {new Date(pausedUntil).toLocaleString()}
                  </span>
                )}
              </>
            ) : (
              <span className={styles.statePill}>Off</span>
            )}
            {data.last_checkin_at_ms != null && (
              <span className={styles.stateMeta}>
                Last reached out: {relativeTime(data.last_checkin_at_ms)}
              </span>
            )}
          </div>
        </div>
        <div className={styles.headerActions}>
          <button className={styles.btn} onClick={onSave} disabled={saveMut.isPending}>
            {saveMut.isPending ? <Loader2 size={14} className={styles.spin} /> : <Save size={14} />}
            {saveMut.isPending ? 'Saving…' : 'Save changes'}
          </button>
        </div>
      </div>

      {!data.enabled && (
        <div className={styles.notEnabledNote}>
          Companion check-ins aren't enabled yet. Turn them on in the Setup wizard,
          or just ask MIRA in chat — then tune them here.
        </div>
      )}

      <div className={styles.body}>
        {/* ── 2. Rhythm ──────────────────────────────────────────────────── */}
        <div className={styles.section}>
          <div className={styles.sectionTitle}>Rhythm</div>
          <div className={styles.segmentedRow}>
            <button
              type="button"
              className={`${styles.segmented} ${form.frequency_mode === 'fuzzy' ? styles.segmentedActive : ''}`}
              onClick={() => set('frequency_mode', 'fuzzy')}
            >
              Fuzzy
            </button>
            <button
              type="button"
              className={`${styles.segmented} ${form.frequency_mode === 'scheduled' ? styles.segmentedActive : ''}`}
              onClick={() => set('frequency_mode', 'scheduled')}
            >
              Scheduled
            </button>
          </div>

          {form.frequency_mode === 'fuzzy' ? (
            <>
              <div className={styles.fieldRow}>
                <label className={styles.field}>
                  <span>Min / day</span>
                  <input
                    className={`${styles.input} ${styles.numInput}`}
                    type="number"
                    min={0}
                    max={10}
                    value={form.min_per_day}
                    onChange={(e) => set('min_per_day', clamp(Number(e.target.value), 0, 10))}
                  />
                </label>
                <label className={styles.field}>
                  <span>Max / day</span>
                  <input
                    className={`${styles.input} ${styles.numInput}`}
                    type="number"
                    min={1}
                    max={12}
                    value={form.max_per_day}
                    onChange={(e) => set('max_per_day', clamp(Number(e.target.value), 1, 12))}
                  />
                </label>
                <label className={styles.field}>
                  <span>Min gap (min)</span>
                  <input
                    className={`${styles.input} ${styles.numInput}`}
                    type="number"
                    min={0}
                    value={form.min_gap_minutes}
                    onChange={(e) => set('min_gap_minutes', Math.max(0, Number(e.target.value)))}
                  />
                </label>
              </div>
              <p className={styles.help}>
                MIRA reaches out {form.min_per_day}–{form.max_per_day} times a day at
                varied times within your contactable hours.
              </p>
            </>
          ) : (
            <>
              <div className={styles.timeList}>
                {form.scheduled_times.map((t, i) => (
                  <div className={styles.timeRow} key={i}>
                    <input
                      className={`${styles.input} ${styles.numInput}`}
                      type="time"
                      value={HHMM.test(t) ? t : ''}
                      onChange={(e) => {
                        const next = [...form.scheduled_times]
                        next[i] = e.target.value
                        set('scheduled_times', next)
                      }}
                    />
                    <button
                      type="button"
                      className={styles.preset}
                      title="Remove time"
                      onClick={() =>
                        set('scheduled_times', form.scheduled_times.filter((_, j) => j !== i))
                      }
                    >
                      <Trash2 size={13} />
                    </button>
                  </div>
                ))}
                <button
                  type="button"
                  className={styles.preset}
                  style={{ alignSelf: 'flex-start' }}
                  onClick={() => set('scheduled_times', [...form.scheduled_times, '09:00'])}
                >
                  <Plus size={13} /> Add time
                </button>
              </div>
              {timesError && <p className={styles.error}>{timesError}</p>}
              <p className={styles.help}>
                MIRA reaches out at exactly these times (24-hour, HH:MM).
              </p>
            </>
          )}
        </div>

        {/* ── 3. Tone & personality ──────────────────────────────────────── */}
        <div className={styles.section}>
          <div className={styles.sectionTitle}>Tone & personality</div>
          <ToneSlider label="Warmth"      value={form.tone.warmth}      onChange={(v) => setTone('warmth', v)} />
          <ToneSlider label="Playfulness" value={form.tone.playfulness} onChange={(v) => setTone('playfulness', v)} />
          <ToneSlider label="Verbosity"   value={form.tone.verbosity}   onChange={(v) => setTone('verbosity', v)} />
          <div className={styles.presetRow}>
            {TONE_PRESETS.map((p) => (
              <button
                key={p.label}
                type="button"
                className={styles.preset}
                onClick={() => set('tone', { ...p.tone })}
              >
                {p.label}
              </button>
            ))}
          </div>
          <Link to="/wiki" className={styles.personaLink}>
            Edit MIRA's persona →
          </Link>
        </div>

        {/* ── 4. What MIRA sends ─────────────────────────────────────────── */}
        <div className={styles.section}>
          <div className={styles.sectionTitle}>What MIRA sends</div>
          <div className={styles.toggleGrid}>
            {MESSAGE_MIX_LABELS.map(({ key, label }) => (
              <label className={styles.toggleLine} key={key}>
                {label}
                <span className={styles.toggleWrap}>
                  <input
                    type="checkbox"
                    checked={form.message_mix[key]}
                    onChange={(e) => setMix(key, e.target.checked)}
                  />
                  <span className={styles.toggleTrack} />
                </span>
              </label>
            ))}
          </div>

          <div className={styles.divider} />

          <label className={styles.toggleLine}>
            Let MIRA mention what its agents did for you
            <span className={styles.toggleWrap}>
              <input
                type="checkbox"
                checked={form.share_agent_activity}
                onChange={(e) => set('share_agent_activity', e.target.checked)}
              />
              <span className={styles.toggleTrack} />
            </span>
          </label>

          <div className={styles.divider} />

          <label className={styles.toggleLine}>
            Daily briefing
            <span className={styles.toggleWrap}>
              <input
                type="checkbox"
                checked={form.daily_briefing_enabled}
                onChange={(e) => set('daily_briefing_enabled', e.target.checked)}
              />
              <span className={styles.toggleTrack} />
            </span>
          </label>
          {form.daily_briefing_enabled && (
            <label className={styles.field}>
              <span>Briefing hour</span>
              <select
                className={styles.select}
                style={{ width: 100 }}
                value={form.daily_briefing_hour}
                onChange={(e) => set('daily_briefing_hour', Number(e.target.value))}
              >
                {Array.from({ length: 24 }, (_, h) => (
                  <option key={h} value={h}>{String(h).padStart(2, '0')}:00</option>
                ))}
              </select>
            </label>
          )}
        </div>

        {/* ── 5. Footer note ─────────────────────────────────────────────── */}
        <p className={styles.footerNote}>
          You can also just tell MIRA in chat — "message me less", "be funnier",
          "pause till Monday" — and it updates these settings.
        </p>
      </div>
    </div>
  )
}

function clamp(n: number, lo: number, hi: number): number {
  if (Number.isNaN(n)) return lo
  return Math.min(hi, Math.max(lo, n))
}

function ToneSlider({
  label, value, onChange,
}: { label: string; value: number; onChange: (v: number) => void }) {
  return (
    <div className={styles.sliderRow}>
      <span className={styles.sliderLabel}>{label}</span>
      <input
        type="range"
        className={styles.range}
        min={0}
        max={100}
        step={1}
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
      />
      <span className={styles.rangeValue}>{value}</span>
    </div>
  )
}
