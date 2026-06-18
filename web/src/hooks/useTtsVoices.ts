// SPDX-License-Identifier: AGPL-3.0-or-later

import { useQuery } from '@tanstack/react-query'
import { ttsApi, type VoiceDto } from '@/api/tts'

export type VoicesByBackend = Record<string, VoiceDto[]>

export interface VoicesAndRouting {
  voices:          VoicesByBackend
  routing:         Record<string, string>
  default_backend: string
}

/**
 * Loads every voice for every configured backend so the UI can render a
 * single grouped dropdown instead of asking the user to type a voice id by
 * hand. The result is keyed by backend id and stays cached for the page —
 * the underlying data only changes when the admin reconfigures backends.
 *
 * Also returns the per-channel routing map so callers can scope the picker
 * to the backend that will actually run for that channel.
 *
 * Failures for a single backend (e.g. the cloud key is wrong) don't fail the
 * whole hook; that backend simply contributes no voices.
 */
export function useTtsVoices() {
  return useQuery<VoicesAndRouting>({
    queryKey: ['tts', 'voices', 'all-backends'],
    queryFn: async () => {
      const status = await ttsApi.status()
      const backends = status.backends ?? []
      const settled = await Promise.all(
        backends.map(async (b) => {
          try {
            const list = await ttsApi.voices(b)
            return [b, list] as const
          } catch {
            return [b, [] as VoiceDto[]] as const
          }
        }),
      )
      const voices: VoicesByBackend = {}
      for (const [b, list] of settled) voices[b] = list
      return {
        voices,
        routing:         status.routing ?? {},
        default_backend: status.backend,
      }
    },
    staleTime: 5 * 60_000,
    refetchOnWindowFocus: false,
  })
}
