// SPDX-License-Identifier: AGPL-3.0-or-later

import { api } from './client'

export interface TranscribeResponse {
  text:        string
  language:    string | null
  duration_ms: number | null
  latency_ms:  number
  backend:     string
}

export interface SttStatus {
  enabled:    boolean
  backend:    string
  backends:   string[]
  healthy:    boolean
  latency_ms: number | null
  note:       string | null
}

export const sttApi = {
  /**
   * POST /api/stt/transcribe — upload an audio Blob (typically a
   * MediaRecorder webm/opus chunk) and receive the transcript JSON.
   * Optional `language` / `backend` overrides match the server-side
   * multipart fields.
   */
  async transcribe(
    audio:    Blob,
    opts?: { language?: string; backend?: string },
  ): Promise<TranscribeResponse> {
    const form = new FormData()
    const ext = audio.type.includes('ogg')   ? 'ogg'
              : audio.type.includes('webm')  ? 'webm'
              : audio.type.includes('mp4')   ? 'm4a'
              : audio.type.includes('mpeg')  ? 'mp3'
              : 'wav'
    form.append('file', audio, `recording.${ext}`)
    if (opts?.language) form.append('language', opts.language)
    if (opts?.backend)  form.append('backend',  opts.backend)
    form.append('channel', 'web')
    const { data } = await api.post<TranscribeResponse>(
      '/api/stt/transcribe',
      form,
    )
    return data
  },

  async status(backend?: string): Promise<SttStatus> {
    const { data } = await api.get<SttStatus>('/api/stt/status', {
      params: backend ? { backend } : undefined,
    })
    return data
  },
}
