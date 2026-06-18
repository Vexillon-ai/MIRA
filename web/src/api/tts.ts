// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export interface SpeakRequest {
  text: string
  voice?: string
  speed?: number
  format?: 'wav' | 'mp3' | 'ogg-opus'
  backend?: string
  channel?: string
}

export interface VoiceDto {
  id:             string
  name:           string
  language:       string
  gender:         string | null
  sample_rate:    number | null
  is_downloaded:  boolean
  backend:        string
}

export interface TtsStatus {
  enabled:         boolean
  backend:         string
  backends:        string[]
  /** `{channel_id → resolved backend}` for each known channel. Used to
   *  scope the voice-id picker to the routed backend per channel. */
  routing:         Record<string, string>
  healthy:         boolean
  last_latency_ms: number | null
  note:            string | null
  cache:           { entries: number; total_bytes: number }
}

export const ttsApi = {
  /**
   * POST /api/tts/speak — synthesise text and return audio bytes as a Blob
   * tagged with the server-reported MIME type.
   */
  async speak(req: SpeakRequest): Promise<Blob> {
    const { data, headers } = await api.post('/api/tts/speak', req, {
      responseType: 'blob',
    })
    const blob = data as Blob
    const ct =
      (headers['content-type'] as string | undefined) ?? blob.type ?? 'audio/wav'
    return blob.type === ct ? blob : new Blob([blob], { type: ct })
  },

  async voices(backend?: string): Promise<VoiceDto[]> {
    const { data } = await api.get<VoiceDto[]>('/api/tts/voices', {
      params: backend ? { backend } : undefined,
    })
    return data
  },

  async status(backend?: string): Promise<TtsStatus> {
    const { data } = await api.get<TtsStatus>('/api/tts/status', {
      params: backend ? { backend } : undefined,
    })
    return data
  },

  /**
   * Pre-fetch a voice's assets. For Piper this downloads the `.onnx` model
   * pair from huggingface so the first speak doesn't pay the network cost.
   * Cloud backends accept this call and return ok with no work.
   */
  async downloadVoice(backend: string | undefined, voice_id: string): Promise<void> {
    await api.post('/api/tts/voices/download', { backend, voice_id })
  },
}
