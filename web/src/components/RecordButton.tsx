// SPDX-License-Identifier: AGPL-3.0-or-later

import { useCallback, useEffect, useRef, useState } from 'react'
import { Mic, MicOff, Loader2 } from 'lucide-react'
import { sttApi } from '@/api/stt'
import styles from './RecordButton.module.css'

type Status = 'idle' | 'recording' | 'uploading'

interface RecordButtonProps {
  /** Called with the transcribed text once the recording is processed. */
  onTranscript: (text: string) => void
  /** Disable the button (e.g. while the chat is streaming). */
  disabled?:    boolean
  /** Optional language hint passed through to the STT backend. */
  language?:    string
}

/**
 * Mic-button affordance for the chat composer. One click starts capture
 * via `MediaRecorder`; the next click stops, uploads to `/api/stt/transcribe`,
 * and hands the transcript to the parent. Errors surface as inline tooltips
 * — we deliberately don't auto-send so the user always reviews before
 * dispatching to the agent.
 */
export default function RecordButton({ onTranscript, disabled, language }: RecordButtonProps) {
  const [status, setStatus] = useState<Status>('idle')
  const [error,  setError]  = useState<string | null>(null)
  const recorderRef = useRef<MediaRecorder | null>(null)
  const chunksRef   = useRef<Blob[]>([])
  const streamRef   = useRef<MediaStream | null>(null)

  const cleanup = useCallback(() => {
    if (streamRef.current) {
      streamRef.current.getTracks().forEach(t => t.stop())
      streamRef.current = null
    }
    recorderRef.current = null
    chunksRef.current = []
  }, [])

  useEffect(() => () => cleanup(), [cleanup])

  const start = useCallback(async () => {
    setError(null)
    if (typeof navigator === 'undefined' || !navigator.mediaDevices?.getUserMedia) {
      setError('Microphone API not available in this browser')
      return
    }
    if (typeof MediaRecorder === 'undefined') {
      setError('MediaRecorder not supported in this browser')
      return
    }
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true })
      streamRef.current = stream

      // Try the codecs that Symphonia/whisper.cpp decode reliably; fall
      // back to the browser default if neither is supported.
      const mime =
        MediaRecorder.isTypeSupported('audio/webm;codecs=opus')  ? 'audio/webm;codecs=opus'  :
        MediaRecorder.isTypeSupported('audio/ogg;codecs=opus')   ? 'audio/ogg;codecs=opus'   :
        ''
      const recorder = mime
        ? new MediaRecorder(stream, { mimeType: mime })
        : new MediaRecorder(stream)
      recorderRef.current = recorder
      chunksRef.current = []

      recorder.ondataavailable = (e) => {
        if (e.data.size > 0) chunksRef.current.push(e.data)
      }
      recorder.onstop = async () => {
        const type = recorder.mimeType || 'audio/webm'
        const blob = new Blob(chunksRef.current, { type })
        cleanup()
        if (blob.size === 0) {
          setStatus('idle')
          setError('No audio captured')
          return
        }
        setStatus('uploading')
        try {
          const res = await sttApi.transcribe(blob, language ? { language } : undefined)
          const text = res.text.trim()
          if (text) onTranscript(text)
          else setError('Empty transcript')
        } catch (e) {
          setError(messageOf(e))
        } finally {
          setStatus('idle')
        }
      }

      recorder.start()
      setStatus('recording')
    } catch (e) {
      cleanup()
      setStatus('idle')
      setError(messageOf(e))
    }
  }, [cleanup, language, onTranscript])

  const stop = useCallback(() => {
    const r = recorderRef.current
    if (r && r.state !== 'inactive') r.stop()
  }, [])

  const onClick = useCallback(() => {
    if (status === 'recording') stop()
    else if (status === 'idle') void start()
  }, [status, start, stop])

  const title =
    error               ? `Mic error: ${error}` :
    status === 'recording' ? 'Stop recording' :
    status === 'uploading' ? 'Transcribing…' :
                              'Record voice (uses STT)'

  const className =
    status === 'recording' ? `${styles.btn} ${styles.recording}` :
    status === 'uploading' ? `${styles.btn} ${styles.uploading}` :
                              styles.btn

  return (
    <button
      type="button"
      className={className}
      onClick={onClick}
      disabled={disabled || status === 'uploading'}
      title={title}
      aria-label={title}
    >
      {status === 'uploading' ? <Loader2 size={15} className={styles.spin} /> :
       status === 'recording' ? <MicOff size={15} /> :
                                 <Mic size={15} />}
    </button>
  )
}

function messageOf(e: unknown): string {
  if (e instanceof Error) return e.message
  if (typeof e === 'string') return e
  return 'unknown error'
}
