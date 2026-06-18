// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * Web Audio playback for synthesised TTS blobs.
 *
 * The chat 🔊 button and the Settings "Test voice" button both push
 * server-rendered audio through this helper so per-backend volume settings
 * (`tts.<backend>.volume`) can apply a gain >1.0 — HTMLAudioElement caps
 * `volume` at 1.0, so plain `<audio>` can only attenuate, never boost.
 *
 * One AudioContext is shared across the page; it is created lazily on first
 * play and stays alive for the session. Browser autoplay policy requires a
 * user gesture before audio can start, so the first play in a session must
 * happen inside a click/keydown handler. Auto-play of fresh assistant
 * messages relies on the user's prior Send-button click being a recent
 * enough gesture.
 */

let _ctx: AudioContext | null = null
function getCtx(): AudioContext {
  if (!_ctx) {
    const Ctor = window.AudioContext
      ?? (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext
    _ctx = new Ctor()
  }
  return _ctx
}

export interface PlayHandle {
  /** Resolves when playback finishes (either ended naturally or `stop()` ran). */
  done: Promise<void>
  /** Halt playback immediately. Idempotent. */
  stop: () => void
}

/**
 * Decode `blob` and play it once with the given linear gain. `gain` of 1.0
 * is unaltered; values above 1.0 amplify (clipping if the source is already
 * near full-scale).
 */
export async function playBlobWithGain(blob: Blob, gain: number): Promise<PlayHandle> {
  const ctx = getCtx()
  if (ctx.state === 'suspended') {
    try { await ctx.resume() } catch { /* autoplay block — caller can retry */ }
  }

  // decodeAudioData detaches the input ArrayBuffer in older Safari, so pass
  // a fresh slice. Cheap — TTS clips are small.
  const raw = await blob.arrayBuffer()
  const audioBuf = await ctx.decodeAudioData(raw.slice(0))

  const src  = ctx.createBufferSource()
  src.buffer = audioBuf

  const gainNode = ctx.createGain()
  gainNode.gain.value = Math.max(0, gain)

  src.connect(gainNode)
  gainNode.connect(ctx.destination)

  let resolveDone: () => void = () => {}
  const done = new Promise<void>((r) => { resolveDone = r })
  let stopped = false

  src.onended = () => { if (!stopped) { stopped = true; resolveDone() } }
  src.start(0)

  return {
    done,
    stop: () => {
      if (stopped) return
      stopped = true
      try { src.stop() } catch { /* already ended */ }
      try { src.disconnect() } catch { /* already disconnected */ }
      try { gainNode.disconnect() } catch { /* already disconnected */ }
      resolveDone()
    },
  }
}

// ── Volume resolution ───────────────────────────────────────────────────────
//
// The web client doesn't always know which backend the server picked for a
// given /api/tts/speak — for chat playback we omit `backend` and rely on
// `tts.routing.web` → `tts.default_backend`. This module replicates that
// resolution against the cached config so the playback layer can pick the
// right per-backend gain.

interface TtsCfgShape {
  default_backend?: string
  internal?:      { engine?: string; volume?: number }
  openai?:        { volume?: number }
  openai_compat?: { volume?: number }
  elevenlabs?:    { volume?: number }
  cartesia?:      { volume?: number }
  routing?: { web?: string }
}

interface ConfigShape { tts?: TtsCfgShape }

/**
 * Mirror of the server's backend resolution for `channel: 'web'` requests.
 * Walks `tts.routing.web` → `tts.default_backend`; expands the synthetic
 * `internal` label to the configured `tts.internal.engine`.
 */
export function resolveWebBackend(cfg: ConfigShape | undefined | null, override?: string): string {
  if (override && override !== '') {
    if (override === 'internal') return cfg?.tts?.internal?.engine ?? 'piper'
    return override
  }
  const route = (cfg?.tts?.routing?.web ?? '').trim()
  const picked = route !== '' ? route : (cfg?.tts?.default_backend ?? 'internal')
  if (picked === 'internal') return cfg?.tts?.internal?.engine ?? 'piper'
  return picked
}

/** Per-backend playback gain, falling back to 1.0 when unknown. */
export function volumeForBackend(cfg: ConfigShape | undefined | null, backend: string): number {
  const t = cfg?.tts
  if (!t) return 1.0
  switch (backend) {
    case 'piper':
    case 'espeak':
    case 'kokoro':
      return t.internal?.volume ?? 1.0
    case 'openai':         return t.openai?.volume         ?? 1.0
    case 'openai_compat':  return t.openai_compat?.volume  ?? 1.0
    case 'elevenlabs':     return t.elevenlabs?.volume     ?? 1.0
    case 'cartesia':       return t.cartesia?.volume       ?? 1.0
    default:               return 1.0
  }
}
