// SPDX-License-Identifier: AGPL-3.0-or-later

// Per-user, per-channel voice-preference helpers, shared by the "My Preferences"
// page (VoiceReplyPrefs) — extracted from ProfileDialog so per-user preferences
// have a single home.

import type { ChannelVoicePrefs, VoicePrefsMap } from '@/api/types'

/** Apply a partial change to one channel's prefs, dropping the entry entirely
 *  when it ends up fully inheriting (no policy + no voice id). This keeps the
 *  stored map canonical so the dirty check doesn't flap on no-op edits. */
export function updatePref(
  map: VoicePrefsMap,
  channel: string,
  patch: Partial<ChannelVoicePrefs>,
): VoicePrefsMap {
  const next: ChannelVoicePrefs = { ...(map[channel] ?? {}), ...patch }
  const empty =
    (next.response_policy === null || next.response_policy === undefined) &&
    (next.voice_id === null || next.voice_id === undefined ||
     (typeof next.voice_id === 'string' && next.voice_id.trim() === ''))
  const out = { ...map }
  if (empty) delete out[channel]
  else        out[channel] = next
  return out
}

export function normaliseVoicePrefs(map: VoicePrefsMap): VoicePrefsMap {
  const out: VoicePrefsMap = {}
  for (const [k, v] of Object.entries(map)) {
    const trimmedVoice =
      typeof v.voice_id === 'string' ? v.voice_id.trim() : v.voice_id
    const entry: ChannelVoicePrefs = {}
    if (v.response_policy) entry.response_policy = v.response_policy
    if (typeof trimmedVoice === 'string' && trimmedVoice !== '') {
      entry.voice_id = trimmedVoice
    }
    if (entry.response_policy || entry.voice_id) out[k] = entry
  }
  return out
}

export function voicePrefsEqual(a: VoicePrefsMap, b: VoicePrefsMap): boolean {
  const na = normaliseVoicePrefs(a)
  const nb = normaliseVoicePrefs(b)
  const ka = Object.keys(na).sort()
  const kb = Object.keys(nb).sort()
  if (ka.length !== kb.length) return false
  for (let i = 0; i < ka.length; i++) {
    if (ka[i] !== kb[i]) return false
    const ea = na[ka[i]]
    const eb = nb[kb[i]]
    if ((ea.response_policy ?? null) !== (eb.response_policy ?? null)) return false
    if ((ea.voice_id        ?? null) !== (eb.voice_id        ?? null)) return false
  }
  return true
}
