// SPDX-License-Identifier: AGPL-3.0-or-later

import { getAccessToken } from './client'
import type { SpeakRequest } from './tts'

/**
 * One chunk delivered by `POST /api/tts/speak/stream`.
 *
 * Each chunk is an independently playable audio buffer (the internal Piper /
 * eSpeak backends emit one complete WAV per sentence). The web client queues
 * them sequentially via `<audio>` elements — see `SpeakBtn` in ChatPage.tsx.
 *
 * `is_final` is true on the last sentence only, so callers know when to stop
 * waiting for further chunks. The terminal SSE `done` event still fires after
 * that as a belt-and-braces signal.
 */
export interface TtsChunk {
  codec:    string  // "wav" | "mp3" | "ogg-opus" | "pcm"
  bytes:    Uint8Array
  is_final: boolean
}

export interface TtsStreamCallbacks {
  onChunk?: (chunk: TtsChunk) => void
  onError?: (message: string) => void
  onDone?:  () => void
}

/**
 * POST a SpeakRequest to `/api/tts/speak/stream` and parse the Server-Sent
 * Events response. Returns an `abort()` function the caller can invoke to
 * cancel mid-stream (e.g. if the user clicks Stop before the last sentence).
 *
 * Why hand-rolled instead of `EventSource`? `EventSource` is GET-only. POST +
 * SSE is straightforward to parse: each event is `event: <name>\ndata: <json>\n\n`.
 */
export function openTtsStream(
  req:        SpeakRequest,
  callbacks:  TtsStreamCallbacks,
): () => void {
  const controller = new AbortController()

  ;(async () => {
    let response: Response
    try {
      const headers: Record<string, string> = {
        'content-type': 'application/json',
        'accept':       'text/event-stream',
      }
      const token = getAccessToken()
      if (token) headers['Authorization'] = `Bearer ${token}`
      response = await fetch('/api/tts/speak/stream', {
        method:  'POST',
        headers,
        body:    JSON.stringify(req),
        signal:  controller.signal,
        credentials: 'include',
      })
    } catch (e) {
      if (controller.signal.aborted) return
      callbacks.onError?.(e instanceof Error ? e.message : String(e))
      return
    }
    if (!response.ok) {
      callbacks.onError?.(`HTTP ${response.status}`)
      return
    }
    if (!response.body) {
      callbacks.onError?.('streaming not supported by this response')
      return
    }

    const reader  = response.body.getReader()
    const decoder = new TextDecoder('utf-8')
    let buffer    = ''

    try {
      while (true) {
        const { value, done } = await reader.read()
        if (done) break
        buffer += decoder.decode(value, { stream: true })

        // SSE events are separated by a blank line.
        let sep
        while ((sep = buffer.indexOf('\n\n')) !== -1) {
          const raw = buffer.slice(0, sep)
          buffer    = buffer.slice(sep + 2)
          dispatch(raw, callbacks)
        }
      }
    } catch (e) {
      if (controller.signal.aborted) return
      callbacks.onError?.(e instanceof Error ? e.message : String(e))
    }
  })()

  return () => controller.abort()
}

function dispatch(raw: string, callbacks: TtsStreamCallbacks) {
  let event = 'message'
  const dataLines: string[] = []
  for (const line of raw.split('\n')) {
    if (!line || line.startsWith(':')) continue
    if (line.startsWith('event:')) {
      event = line.slice(6).trim()
    } else if (line.startsWith('data:')) {
      dataLines.push(line.slice(5).replace(/^ /, ''))
    }
  }
  const data = dataLines.join('\n')

  if (event === 'chunk') {
    try {
      const j = JSON.parse(data) as { codec: string; b64: string; is_final: boolean }
      callbacks.onChunk?.({
        codec:    j.codec,
        bytes:    base64ToBytes(j.b64),
        is_final: j.is_final,
      })
    } catch {
      callbacks.onError?.('failed to parse chunk event')
    }
  } else if (event === 'error') {
    try {
      const j = JSON.parse(data) as { message: string }
      callbacks.onError?.(j.message ?? 'unknown error')
    } catch {
      callbacks.onError?.(data || 'unknown error')
    }
  } else if (event === 'done') {
    callbacks.onDone?.()
  }
}

function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

/** Map a backend codec label to a browser-friendly MIME type. */
export function codecToMime(codec: string): string {
  switch (codec) {
    case 'wav':       return 'audio/wav'
    case 'mp3':       return 'audio/mpeg'
    case 'ogg-opus':  return 'audio/ogg'
    case 'pcm':       return 'audio/L16'
    default:          return 'application/octet-stream'
  }
}
