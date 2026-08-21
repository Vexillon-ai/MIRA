// SPDX-License-Identifier: AGPL-3.0-or-later

// Per-user, per-channel voice reply preferences — the caller's own overrides
// (response policy + voice id) layered over the server-wide defaults. Extracted
// from ProfileDialog so every per-user preference lives on the "My Preferences"
// page. Saves via PUT /api/users/{id} with only `voice_prefs` — the handler
// preserves every other field (`req.field.or(existing.field)`), so this never
// touches the caller's account details.

import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Check } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import { useAuthStore } from '@/store/authStore'
import type { ChannelDescriptor, User, VoicePrefsMap, VoiceResponsePolicy } from '@/api/types'
import VoiceIdPicker from './VoiceIdPicker'
import { normaliseVoicePrefs, updatePref, voicePrefsEqual } from './voicePrefs'
import styles from './ProfileDialog.module.css'

export default function VoiceReplyPrefs() {
  const user    = useAuthStore((s) => s.user)
  const setUser = useAuthStore((s) => s.setUser)
  const qc      = useQueryClient()
  const [voicePrefs, setVoicePrefs] = useState<VoicePrefsMap>({})

  useEffect(() => { setVoicePrefs(user?.voice_prefs ?? {}) }, [user])

  const { data: channels } = useQuery({
    queryKey: ['channels'],
    queryFn:  async () => (await api.get<ChannelDescriptor[]>('/api/channels')).data,
    enabled:  !!user,
    staleTime: 5 * 60_000,
    refetchOnWindowFocus: false,
  })

  const save = useMutation({
    mutationFn: async () => {
      if (!user) throw new Error('No user')
      // voice_prefs ONLY — the server preserves every other user field.
      const r = await api.put<User>(`/api/users/${user.id}`, {
        voice_prefs: normaliseVoicePrefs(voicePrefs),
      })
      return r.data
    },
    onSuccess: (updated) => {
      setUser(updated)
      qc.invalidateQueries({ queryKey: ['users'] })
      toast.success('Voice preferences saved.')
    },
    onError: () => toast.error('Save failed'),
  })

  if (!user) return null
  const voiceChannels = (channels ?? []).filter((c) => c.supports_voice)
  const dirty = !voicePrefsEqual(voicePrefs, user.voice_prefs ?? {})

  return (
    <div>
      <p className={styles.onbHint}>
        Override how the assistant replies to <strong>you</strong> on each channel.
        Anything left as <em>Inherit</em> follows the server-wide default an admin set.
      </p>
      {voiceChannels.length === 0 ? (
        <p className={styles.onbHint}>No voice-capable channels are configured yet.</p>
      ) : (
        <div className={styles.groupList}>
          {voiceChannels.map((ch) => {
            const entry  = voicePrefs[ch.id] ?? {}
            const policy = entry.response_policy ?? ''
            const vid    = entry.voice_id ?? ''
            return (
              <div key={ch.id} className={styles.voiceRow}>
                <div className={styles.voiceLabel}>{ch.display_name}</div>
                <select
                  className={styles.input}
                  aria-label={`${ch.display_name} response policy`}
                  value={policy}
                  onChange={(e) => {
                    const v = e.target.value as VoiceResponsePolicy | ''
                    setVoicePrefs((m) => updatePref(m, ch.id, {
                      response_policy: v === '' ? null : v,
                    }))
                  }}
                >
                  <option value="">Inherit</option>
                  <option value="always">Always</option>
                  <option value="on_voice_input">On voice input</option>
                  <option value="never">Never</option>
                </select>
                <VoiceIdPicker
                  ariaLabel={`${ch.display_name} voice id`}
                  channel={ch.id}
                  value={vid}
                  onChange={(v) => {
                    setVoicePrefs((m) => updatePref(m, ch.id, {
                      voice_id: v === '' ? null : v,
                    }))
                  }}
                />
              </div>
            )
          })}
        </div>
      )}
      <div className={styles.actions}>
        <button
          className={styles.btn}
          onClick={() => save.mutate()}
          disabled={!dirty || save.isPending}
        >
          <Check size={14} />
          {save.isPending ? 'Saving…' : 'Save voice preferences'}
        </button>
      </div>
    </div>
  )
}
