// SPDX-License-Identifier: AGPL-3.0-or-later

import { useRef, useCallback } from 'react'
import { useNavigate } from 'react-router-dom'
import { useQueryClient } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { getAccessToken } from '@/api/client'
import { useChatStore } from '@/store/chatStore'
import type { Attachment, Message, ThinkingEntry } from '@/api/types'

// Helper: best-effort JSON.parse, returns the raw string if not
// parseable (some servers send tool-call args as a JSON string, others
// as an inline object — accept both).
function safeParse(s: string): unknown {
  try { return JSON.parse(s) } catch { return s }
}

export type ChatStatus = 'idle' | 'streaming' | 'error'

// ── SSE parser ────────────────────────────────────────────────────────────────
// Processes lines according to the SSE spec: accumulate event/data fields,
// dispatch on blank line. Avoids the double-loop / indexOf bug.

function parseSseLines(
  lines: string[],
  state: { event: string; data: string },
  onEvent: (event: string, data: string) => void,
): void {
  for (const line of lines) {
    if (line === '') {
      // Blank line = event boundary.
      if (state.event || state.data) {
        onEvent(state.event, state.data)
      }
      state.event = ''
      state.data  = ''
    } else if (line.startsWith('event:')) {
      state.event = line.slice(6).trim()
    } else if (line.startsWith('data:')) {
      // SSE spec: strip exactly one leading space after the colon.
      const val = line.slice(5)
      const chunk = val.startsWith(' ') ? val.slice(1) : val
      state.data += state.data.length > 0 ? '\n' + chunk : chunk
    }
    // Ignore id:, retry:, and comment lines.
  }
}

// ── Hook ──────────────────────────────────────────────────────────────────────

export function useChat() {
  const navigate    = useNavigate()
  const qc          = useQueryClient()
  const abortRef    = useRef<AbortController | null>(null)
  const {
    appendMessage, setStreaming, appendStreamChunk, commitMessage, setLastTurnCost,
    setStreamingThinking,
  } = useChatStore()

  const stop = useCallback(() => {
    abortRef.current?.abort()
    setStreaming(false)
  }, [setStreaming])

  const sendMessage = useCallback(async (
    text: string,
    conversationId: string | null,
    modelOverride?: string,
    providerOverride?: string,
    attachments?: Attachment[],
    disableReasoning?: boolean,
  ) => {
    // Allow attachment-only sends (image with no caption) — the
    // vision-capable providers handle this gracefully.
    const hasText = text.trim().length > 0
    const hasAttachments = (attachments?.length ?? 0) > 0
    if (!hasText && !hasAttachments) return

    const userMsg: Message = {
      id:              `tmp-${Date.now()}`,
      conversation_id: conversationId ?? '',
      role:            'user',
      content:         text,
      content_type:    'text',
      channel:         'web',
      created_at:      Date.now(),
      token_count:     null,
      tool_calls:      null,
      attachments:     hasAttachments ? attachments : undefined,
    }
    appendMessage(userMsg)
    setStreaming(true, '')
    setStreamingThinking([])

    const controller = new AbortController()
    abortRef.current = controller

    try {
      const token = getAccessToken()
      const resp = await fetch('/api/chat', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          ...(token ? { Authorization: `Bearer ${token}` } : {}),
        },
        body: JSON.stringify({
          message:           text,
          conversation_id:   conversationId ?? undefined,
          model_override:    modelOverride ?? undefined,
          provider_override: providerOverride ?? undefined,
          attachments:       hasAttachments ? attachments : undefined,
          disable_reasoning: disableReasoning,
        }),
        signal: controller.signal,
      })

      if (!resp.ok) {
        const errText = await resp.text()
        throw new Error(errText || `HTTP ${resp.status}`)
      }

      const reader = resp.body?.getReader()
      if (!reader) throw new Error('No response body')

      const decoder = new TextDecoder()
      let buf  = ''
      let newConvId: string | null = null
      let onboardingFlipped = false
      // Slice H — wiki pages that fed this turn. Arrives in a single
      // `wiki_context` SSE event before the first token; gets attached
      // to the assistant message on `done`.
      let wikiPages: string[] = []
      // private chain-of-thought from reasoning models.
      // Arrives as one or more `reasoning` SSE events (one per tool-
      // loop round). Attached to the assistant message on `done`.
      let reasoningContent: string = ''
      // Non-fatal warnings (e.g. provider failover) surfaced this turn.
      // Each is toasted immediately and attached to the message on `done`.
      const warnings: string[] = []
      // Unified thinking trail — collects tool_call / tool_result /
      // reasoning / wiki_context events in arrival order so the
      // ThinkingPanel on the assistant message renders them as a
      // single rollup. Server also writes the same shape to the
      // message's metadata blob; this client-side accumulator is
      // for the LIVE stream before the message is persisted.
      const thinkingEntries: ThinkingEntry[] = []
      // Push live entries into the streaming state so the panel
      // updates as events arrive (not only when the stream ends).
      const pushThinking = (e: ThinkingEntry) => {
        thinkingEntries.push(e)
        setStreamingThinking([...thinkingEntries])
      }
      // SSE parser state carries over between network chunks.
      const sseState = { event: '', data: '' }

      while (true) {
        const { done, value } = await reader.read()
        if (done) break

        buf += decoder.decode(value, { stream: true })
        const lines = buf.split('\n')
        // Keep the last (potentially incomplete) line in the buffer.
        buf = lines.pop() ?? ''

        parseSseLines(lines, sseState, (evName, data) => {
          if (evName === 'token') {
            appendStreamChunk(data)
          } else if (evName === 'tool_call') {
            try {
              const ev = JSON.parse(data)
              pushThinking({
                type: 'tool_call',
                name: ev.tool ?? ev.name ?? '?',
                args: typeof ev.args === 'string' ? safeParse(ev.args) : ev.args,
                call_id: ev.call_id,
              })
            } catch { /* ignore */ }
          } else if (evName === 'tool_result') {
            try {
              const ev = JSON.parse(data)
              pushThinking({
                type:    'tool_result',
                name:    ev.tool ?? ev.name ?? '?',
                output:  typeof ev.output === 'string' ? ev.output : JSON.stringify(ev.output ?? ''),
                success: Boolean(ev.success ?? true),
                call_id: ev.call_id,
              })
            } catch { /* ignore */ }
          } else if (evName === 'wiki_context') {
            try {
              const payload = JSON.parse(data)
              if (Array.isArray(payload?.pages)) {
                wikiPages = payload.pages
                pushThinking({ type: 'wiki_context', pages: payload.pages })
              }
            } catch { /* ignore */ }
          } else if (evName === 'reasoning') {
            // Reasoning models stream a separate chain-of-thought.
            // Multi-round tool loops may emit several `reasoning`
            // events; concatenate them with a blank line for the
            // legacy `reasoning` field while ALSO pushing each as
            // its own thinking entry.
            reasoningContent = reasoningContent
              ? `${reasoningContent}\n\n${data}`
              : data
            pushThinking({ type: 'reasoning', text: data })
          } else if (evName === 'warning') {
            // Non-fatal notice (e.g. provider failover). Toast it now for an
            // immediate heads-up and keep it for the inline message callout.
            warnings.push(data)
            toast(data, { icon: '⚠️', duration: 8000 })
          } else if (evName === 'done') {
            try {
              const payload = JSON.parse(data)
              if (payload.conversation_id) newConvId = payload.conversation_id
              // The done payload also carries the model/provider/usage from
              // the server (added in the OpenRouter pricing work). Push it
              // into the store so the chat footer can render the per-turn
              // cost without an extra round-trip.
              if (payload.model && payload.provider && payload.usage) {
                setLastTurnCost({
                  provider:          payload.provider,
                  model:             payload.model,
                  prompt_tokens:     payload.usage.prompt_tokens     ?? 0,
                  completion_tokens: payload.usage.completion_tokens ?? 0,
                })
              }
            } catch { /* ok */ }
            // Commit the streamed content and unlock input the moment the
            // server tells us this turn is done. Any trailing events (e.g.
            // `onboarding_complete` from the post-turn extractor) still
            // arrive on this same stream — we keep reading but the user is
            // free to type again.
            const finalContent = useChatStore.getState().streamingContent
            if (finalContent) {
              const aiMsg: Message = {
                id:              `ai-${Date.now()}`,
                conversation_id: newConvId ?? conversationId ?? '',
                role:            'assistant',
                content:         finalContent,
                content_type:    'text',
                channel:         'web',
                created_at:      Date.now(),
                token_count:     null,
                tool_calls:      null,
                wiki_pages:      wikiPages.length > 0 ? wikiPages : undefined,
                reasoning:       reasoningContent || undefined,
                thinking:        thinkingEntries.length > 0 ? [...thinkingEntries] : undefined,
                warnings:        warnings.length > 0 ? [...warnings] : undefined,
              }
              commitMessage(aiMsg)
            }
            // Clear the streaming-thinking buffer now that the
            // committed message has its own copy; the panel switches
            // to history mode at this point.
            setStreamingThinking([])
            setStreaming(false)
          } else if (evName === 'error') {
            throw new Error(data)
          } else if (evName === 'onboarding_complete') {
            // Server-confirmed `onboarded_at` flip for this turn. Refetch
            // the state query immediately so the completion modal sees the
            // transition without waiting on the post-stream invalidation.
            onboardingFlipped = true
            qc.invalidateQueries({ queryKey: ['onboarding-state'] })
          }
          // tool_call / tool_result / warning handled by rendering layer
        })
      }

      // Assistant message + streaming flag were already committed on the
      // `done` SSE event (see the parser callback above). We still reach
      // here after the stream closes — that's when we do the non-urgent
      // cache work (conversation list, onboarding state) that doesn't
      // need to happen mid-turn. The stream stays open after `done` so
      // the server can emit a trailing `onboarding_complete` event.
      //
      // Fallback: if the stream ended without a `done` (e.g. server crash
      // mid-stream) the commit above didn't fire — commit whatever we
      // streamed so the user doesn't lose the partial response.
      if (useChatStore.getState().isStreaming) {
        const finalContent = useChatStore.getState().streamingContent
        if (finalContent) {
          const aiMsg: Message = {
            id:              `ai-${Date.now()}`,
            conversation_id: newConvId ?? conversationId ?? '',
            role:            'assistant',
            content:         finalContent,
            content_type:    'text',
            channel:         'web',
            created_at:      Date.now(),
            token_count:     null,
            tool_calls:      null,
          }
          commitMessage(aiMsg)
        }
      }

      // Navigate to newly created conversation.
      if (newConvId && newConvId !== conversationId) {
        navigate(`/chat/${newConvId}`, { replace: true })
        qc.invalidateQueries({ queryKey: ['conversations'] })
      } else if (conversationId) {
        qc.invalidateQueries({ queryKey: ['conversations'] })
      }
      // Progress strip and Settings revisit list both read this; a turn may
      // have advanced the user through a group or completed onboarding.
      qc.invalidateQueries({ queryKey: ['onboarding-state'] })
      if (onboardingFlipped) {
        // Second invalidation after stream finalizes so the modal's query,
        // which was mid-refetch when the event arrived, definitely picks up
        // the stamped `onboarded_at`.
        qc.invalidateQueries({ queryKey: ['onboarding-state'] })
      }

    } catch (err: unknown) {
      if ((err as Error).name !== 'AbortError') {
        appendMessage({
          id:              `err-${Date.now()}`,
          conversation_id: conversationId ?? '',
          role:            'assistant',
          content:         `_Error: ${(err as Error).message || 'Failed to get response.'}_`,
          content_type:    'text',
          channel:         'web',
          created_at:      Date.now(),
          token_count:     null,
          tool_calls:      null,
        })
      }
    } finally {
      // Only clear the streaming flag if WE are still the active stream.
      // Otherwise we'd squash the indicator of a newer sendMessage that
      // started between `done` (which releases our input) and the stream
      // actually closing (which can be later on onboarding turns — the
      // post-turn extractor keeps the SSE connection open for the
      // trailing `onboarding_complete` event).
      if (abortRef.current === controller) {
        setStreaming(false)
      }
    }
  }, [appendMessage, commitMessage, setStreaming, appendStreamChunk, setLastTurnCost, navigate, qc])

  return { sendMessage, stop }
}
