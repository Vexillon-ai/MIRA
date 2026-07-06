// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useMemo, useRef, useState, useCallback, type KeyboardEvent } from 'react'
import { useParams } from 'react-router-dom'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import rehypeHighlight from 'rehype-highlight'
import {
  Send, Square, User, ChevronDown, Paperclip, X as XIcon,
  Copy, Check, Wrench, RotateCcw, Pencil, Settings, Volume2, VolumeX, Loader2,
  BookOpen, BookmarkPlus, Download, Link as LinkIcon, Brain,
} from 'lucide-react'
import toast from 'react-hot-toast'
import { wikiApi } from '@/api/wiki'
import { ttsApi } from '@/api/tts'
import { openTtsStream, codecToMime, type TtsChunk } from '@/api/ttsStream'
import { playBlobWithGain, resolveWebBackend, volumeForBackend, type PlayHandle } from '@/api/ttsPlayback'
import { api } from '@/api/client'
import { useVoiceStore } from '@/store/voiceStore'
import miraLogo from '@/assets/mira-logo.svg'
import AgentAvatar from '@/components/AgentAvatar'
import Avatar from '@/components/Avatar'
import { useAuthStore } from '@/store/authStore'
import OnboardingProgressStrip from '@/components/OnboardingProgressStrip'
import OnboardingCompleteModal from '@/components/OnboardingCompleteModal'
import RecordButton from '@/components/RecordButton'
import { conversationsApi } from '@/api/conversations'
import {
  providersApi, type ModelInfo, type OpenRouterCatalog,
  costForTurn, formatUsd,
} from '@/api/providers'
import { capabilitiesApi, capsAllowModel, type CapabilityProfile } from '@/api/capabilities'
import { useChatStore } from '@/store/chatStore'
import { useChat } from '@/hooks/useChat'
import type { Attachment, Message, ThinkingEntry } from '@/api/types'
import { parseMessageAttachments, parseMessageMetadata, parseMessageWarnings } from '@/api/types'
import ThinkingPanel from '@/components/ThinkingPanel'
import styles from './ChatPage.module.css'

/// Render a compact "in $X / out $Y per 1K" pricing line for a model row.
/// Returns null for free / unpriced models so they show as plain rows.
function pricingBadge(catalog: OpenRouterCatalog | undefined, modelId: string): string | null {
  if (!catalog) return null
  const m = catalog.models.find(x => x.id === modelId)
  if (!m) return null
  const { prompt, completion, request } = m.pricing
  if (prompt === 0 && completion === 0 && request === 0) return 'free'
  const per1k = (per_token: number) => formatUsd(per_token * 1000)
  return `in ${per1k(prompt)} · out ${per1k(completion)} / 1K`
}

export default function ChatPage() {
  const { conversationId } = useParams<{ conversationId?: string }>()
  const convIdStr = conversationId ?? null

  const { messages, setMessages, isStreaming, streamingContent, setActiveConversation } = useChatStore()
  const selectedModel    = useChatStore((s) => s.selectedModel)
  const setSelectedModel = useChatStore((s) => s.setSelectedModel)
  const lastTurnCost     = useChatStore((s) => s.lastTurnCost)
  const { sendMessage, stop } = useChat()

  const [input, setInput]           = useState('')
  // Q1.3 — image attachments pending send. Each entry is the same shape
  // we'll POST to /api/chat (base64-encoded in the browser via FileReader).
  // Cleared after a successful send; X chip removes individuals.
  const [pendingAttachments, setPendingAttachments] = useState<Attachment[]>([])
  // Drop-target visual feedback. Tracked separately from
  // pendingAttachments so the overlay shows during the drag even
  // before any file is actually dropped.
  const [isDragOver, setIsDragOver] = useState(false)
  // Per-conversation reasoning suppression. false = inherit the global
  // `agent.disable_reasoning` (send nothing); true = force `/no_think` for
  // this chat. Lets you silence a reasoning model's chain-of-thought ad hoc.
  const [noThink, setNoThink] = useState(false)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const [showModels, setShowModels] = useState(false)

  const textareaRef   = useRef<HTMLTextAreaElement>(null)
  const parentRef     = useRef<HTMLDivElement>(null)
  const prevConvIdRef = useRef<string | null>(null)

  // Auto-play gate: an assistant message auto-speaks only when its
  // `created_at` is *later* than:
  //   - `mountTime`           — prevents replay on hard-reload
  //   - `convActivatedAt[id]` — prevents the "walked into a chat with
  //                             N unread messages → all N TTS clips
  //                             play at once" cacophony
  //
  // We need both. `mountTime` alone misses the cross-conversation
  // case: ChatPage stays mounted across nav, so messages that arrived
  // in another conversation while the user was elsewhere still count
  // as "after mount" and would all blast on visit.
  //
  // GRACE_MS preserves the existing send-creates-new-conversation
  // behaviour the original comment called out: when the user sends
  // the first message in a brand-new chat, navigate to /chat/<id>
  // happens *after* the assistant reply commits — by the time
  // convActivatedAt is recorded, the reply's created_at is a few
  // hundred ms in the past. The grace window lets that reply through
  // while still gating older history.
  const AUTOPLAY_GRACE_MS = 5_000
  const voiceEnabled = useVoiceStore((s) => s.enabled)
  const [mountTime] = useState(() => Date.now())
  const convActivatedAtRef = useRef<Map<string, number>>(new Map())
  const [, forceRerender] = useState(0)

  const { data: fetchedMessages, isLoading: messagesLoading } = useQuery({
    queryKey: ['messages', convIdStr],
    queryFn:  () => conversationsApi.messages(convIdStr!),
    enabled:  convIdStr !== null,
    staleTime: 0,
  })

  // Fetch the conversation itself to know its `mode`. Cached longer than
  // messages because mode is stamped at creation and never changes.
  const { data: conv } = useQuery({
    queryKey: ['conversation', convIdStr],
    queryFn:  () => conversationsApi.get(convIdStr!),
    enabled:  convIdStr !== null,
    staleTime: 60_000,
  })
  const isOnboarding = conv?.mode === 'onboarding'

  const { data: models = [] } = useQuery<ModelInfo[]>({
    queryKey: ['models'],
    queryFn:  providersApi.models,
    staleTime: 60_000,
  })

  // Catalog drives pricing badges in the dropdown and the per-turn cost
  // footer. Cached for 10 minutes — refreshing happens via ProvidersPage.
  const { data: catalog } = useQuery<OpenRouterCatalog>({
    queryKey: ['openrouter/catalog'],
    queryFn:  () => providersApi.openRouterCatalog(false),
    staleTime: 10 * 60_000,
    retry:     false,
  })

  // Capability RBAC — the caller's effective profile. Used to hide models /
  // providers their account isn't permitted to select (the server also
  // enforces with a 403, so this is UX defense-in-depth).
  const { data: myCaps } = useQuery<CapabilityProfile>({
    queryKey: ['me/capabilities'],
    queryFn:  capabilitiesApi.mine,
    staleTime: 5 * 60_000,
    retry:     false,
  })

  // The chat dropdown is fed directly by /api/providers/models,
  // which already returns one entry per model in each enabled
  // provider's `available_models`. The OpenRouter-specific pinned-ids
  // mechanism (zustand-store, client-side) is dead code now —
  // anything pinned via the legacy Providers page UI was migrated
  // by the user actively adding models in the new per-provider
  // catalog rollups. Filtered by the caller's capability profile.
  const dropdownModels: ModelInfo[] = models.filter(m =>
    capsAllowModel(myCaps, m.provider, m.id))

  // Pre-select the effective default (the primary provider's default model,
  // flagged `is_default` by the server) whenever nothing is explicitly picked,
  // so the header shows — and the send uses — the real default rather than just
  // the first model in the combined list. Picking from the dropdown overrides
  // it for the session; existing conversations keep the model they started with.
  useEffect(() => {
    if (selectedModel || dropdownModels.length === 0) return
    const def = dropdownModels.find(m => m.is_default) ?? dropdownModels[0]
    setSelectedModel({ id: def.id, provider: def.provider })
  }, [selectedModel, dropdownModels, setSelectedModel])

  // The order of these two effects matters: the "clear on conv change" effect
  // must run BEFORE the "populate from fetched data" effect. Otherwise, when
  // React Query returns a cached same-reference result for the new conv (e.g.
  // you visited it recently, or the query resolves synchronously), Effect 1
  // populates `messages`, then this effect fires and wipes it via
  // setActiveConversation — leaving the chat area stuck on the Welcome screen
  // while the header shows the correct title.
  useEffect(() => {
    if (convIdStr !== prevConvIdRef.current) {
      prevConvIdRef.current = convIdStr
      setActiveConversation(convIdStr ?? null)
      // Stamp the moment this conversation became visible. The
      // autoplay gate uses this so unread messages already on disk
      // don't all blast their TTS at once when the user navigates
      // in. Re-arm on every entry, not just the first — otherwise
      // the same backlog-blast happens when the user leaves a chat,
      // four new replies queue up, and they return: with a one-shot
      // stamp those four would all post-date the original
      // activation and autoplay together.
      if (convIdStr) {
        convActivatedAtRef.current.set(convIdStr, Date.now())
        // Force a render so the autoplay computation below picks
        // up the new map entry — the ref alone wouldn't trigger.
        forceRerender((n) => n + 1)
      }
    }
  }, [convIdStr, setActiveConversation])

  // Sync fetched messages into the store. convIdStr is included so this effect
  // re-fires even when React Query returns a cached same-reference result
  // (which happens when navigating back to a recently-visited conversation).
  useEffect(() => {
    if (fetchedMessages !== undefined) setMessages(fetchedMessages)
  }, [fetchedMessages, convIdStr, setMessages])

  const allItems: Array<Message | 'streaming'> = [
    ...messages,
    ...(isStreaming ? ['streaming' as const] : []),
  ]

  // Scroll to bottom whenever the item count changes (new message or conversation load).
  // Direct DOM scroll avoids the estimation errors that caused virtualizer jitter.
  useEffect(() => {
    if (allItems.length > 0 && parentRef.current) {
      parentRef.current.scrollTop = parentRef.current.scrollHeight
    }
  }, [allItems.length])

  // Keep pinned to bottom on every streaming token.
  useEffect(() => {
    if (isStreaming && parentRef.current) {
      parentRef.current.scrollTop = parentRef.current.scrollHeight
    }
  }, [streamingContent, isStreaming])

  const handleInputChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    setInput(e.target.value)
    e.target.style.height = 'auto'
    e.target.style.height = Math.min(e.target.scrollHeight, 200) + 'px'
  }

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); handleSend() }
  }

  const handleSend = useCallback((overrideText?: string) => {
    const text = (overrideText ?? input).trim()
    // Allow attachment-only sends. An image with no caption is a
    // legitimate "look at this" prompt that the vision-capable
    // providers handle on their own.
    const hasAttachments = !overrideText && pendingAttachments.length > 0
    if ((!text && !hasAttachments) || isStreaming) return
    const atts = hasAttachments ? pendingAttachments : undefined
    if (!overrideText) {
      setInput('')
      setPendingAttachments([])
      if (textareaRef.current) textareaRef.current.style.height = 'auto'
    }
    sendMessage(text, convIdStr, selectedModel?.id, selectedModel?.provider, atts,
                noThink ? true : undefined)
  }, [input, isStreaming, sendMessage, convIdStr, selectedModel, pendingAttachments, noThink])

  // Q1.3 — convert a File into the on-wire Attachment shape. Hard caps
  // at 10 MiB after base64 because the major vision endpoints all reject
  // larger payloads (Anthropic ~5 MiB, Gemini ~20 MiB; 10 MiB is the
  // safe middle).
  const readFileAsAttachment = useCallback(async (file: File): Promise<Attachment | null> => {
    if (!file.type.startsWith('image/')) {
      toast.error(`${file.name}: only image files are supported (today).`)
      return null
    }
    // Soft pre-check on the raw byte size; the base64 form is ~33%
    // larger, so a 7.5 MiB raw file lands at the 10 MiB ceiling.
    if (file.size > 7_500_000) {
      toast.error(`${file.name}: too large (${(file.size / 1024 / 1024).toFixed(1)} MiB). Max ~7.5 MiB.`)
      return null
    }
    return new Promise((resolve) => {
      const reader = new FileReader()
      reader.onload = () => {
        const result = reader.result as string
        // FileReader returns "data:<mime>;base64,<b64>" — strip the
        // prefix so we store the raw b64 only (matches the server
        // shape and what each provider's wire layer expects).
        const comma = result.indexOf(',')
        const dataB64 = comma >= 0 ? result.slice(comma + 1) : ''
        resolve({ kind: 'image', mime_type: file.type, data_b64: dataB64 })
      }
      reader.onerror = () => {
        toast.error(`${file.name}: failed to read`)
        resolve(null)
      }
      reader.readAsDataURL(file)
    })
  }, [])

  const addFiles = useCallback(async (files: FileList | File[]) => {
    const arr = Array.from(files)
    const results = await Promise.all(arr.map(readFileAsAttachment))
    const ok = results.filter((a): a is Attachment => a !== null)
    if (ok.length > 0) {
      setPendingAttachments((prev) => [...prev, ...ok])
    }
  }, [readFileAsAttachment])

  // Paste-from-clipboard. Most browsers expose copied screenshots as
  // image/png on the textarea's paste event. We intercept on the
  // textarea so plain-text paste still works as expected.
  const handlePaste = useCallback((e: React.ClipboardEvent<HTMLTextAreaElement>) => {
    const items = e.clipboardData?.items
    if (!items) return
    const images: File[] = []
    for (let i = 0; i < items.length; i++) {
      const it = items[i]
      if (it.kind === 'file' && it.type.startsWith('image/')) {
        const f = it.getAsFile()
        if (f) images.push(f)
      }
    }
    if (images.length > 0) {
      e.preventDefault()
      addFiles(images)
    }
  }, [addFiles])

  const handleDrop = useCallback((e: React.DragEvent<HTMLDivElement>) => {
    e.preventDefault()
    setIsDragOver(false)
    const files = e.dataTransfer?.files
    if (files && files.length > 0) addFiles(files)
  }, [addFiles])

  const handleEdit = useCallback((content: string) => {
    setInput(content)
    textareaRef.current?.focus()
    requestAnimationFrame(() => {
      if (textareaRef.current) {
        textareaRef.current.style.height = 'auto'
        textareaRef.current.style.height = Math.min(textareaRef.current.scrollHeight, 200) + 'px'
      }
    })
  }, [])

  // Append a transcript from the mic button to the existing input. Keeping
  // the typed text + voice paste in the same string lets the user combine
  // dictation with edits before sending.
  const handleTranscript = useCallback((text: string) => {
    setInput(prev => {
      const trimmed = prev.trimEnd()
      return trimmed.length === 0 ? text : `${trimmed} ${text}`
    })
    textareaRef.current?.focus()
    requestAnimationFrame(() => {
      if (textareaRef.current) {
        textareaRef.current.style.height = 'auto'
        textareaRef.current.style.height = Math.min(textareaRef.current.scrollHeight, 200) + 'px'
      }
    })
  }, [])

  const handleRegenerate = useCallback((msgIndex: number) => {
    for (let i = msgIndex - 1; i >= 0; i--) {
      if (messages[i]?.role === 'user') { handleSend(messages[i].content); return }
    }
  }, [messages, handleSend])

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && isStreaming) stop()
      if (e.key === '/' && document.activeElement !== textareaRef.current) {
        e.preventDefault(); textareaRef.current?.focus()
      }
    }
    window.addEventListener('keydown', handler as unknown as EventListener)
    return () => window.removeEventListener('keydown', handler as unknown as EventListener)
  }, [isStreaming, stop])

  const isLoadingConv = convIdStr !== null && messagesLoading && messages.length === 0
  const isEmpty = messages.length === 0 && !isStreaming && !isLoadingConv

  // Cumulative output (sum of completion_tokens stored per assistant message)
  // recovers from history on reload. The "in" side uses the latest turn's
  // prompt_tokens when known — that represents the current conversation
  // context size, the number that drives the cost of the *next* send. We
  // don't have per-turn prompt sizes for older messages (history doesn't
  // store them), so summing here would be a lie — better to show the
  // current-context number than a misleading "total".
  const totalOutputTokens = useMemo(
    () => messages.reduce(
      (s, m) => s + (m.role === 'assistant' ? (m.token_count ?? 0) : 0),
      0,
    ),
    [messages],
  )
  const contextTokens = lastTurnCost?.prompt_tokens ?? 0

  return (
    <div className={styles.page}>
      {isOnboarding && (
        <OnboardingProgressStrip onSendSkipMessage={(text) => handleSend(text)} />
      )}
      {isOnboarding && <OnboardingCompleteModal active={isOnboarding} />}
      {!isOnboarding && convIdStr && <ChatToolbar conversationId={convIdStr} />}
      <div className={styles.messages} ref={parentRef} role="log" aria-live="polite" aria-label="Conversation">
        {isLoadingConv ? (
          <MessagesLoading />
        ) : isEmpty ? (
          <Welcome />
        ) : (
          <div className={styles.messageList}>
            {allItems.map((item, index) => (
              <div
                key={item === 'streaming' ? 'streaming' : (item as Message).id}
                className={styles.msgTrack}
              >
                {item === 'streaming' ? (
                  <StreamingCard content={streamingContent} />
                ) : (
                  <MessageBubble
                    message={item as Message}
                    msgIndex={index}
                    onEdit={handleEdit}
                    onRegenerate={handleRegenerate}
                    autoSpeak={(() => {
                      if (!voiceEnabled) return false
                      const createdAt = (item as Message).created_at
                      if (createdAt <= mountTime) return false
                      const activatedAt = convIdStr
                        ? convActivatedAtRef.current.get(convIdStr) ?? mountTime
                        : mountTime
                      return createdAt > activatedAt - AUTOPLAY_GRACE_MS
                    })()}
                  />
                )}
              </div>
            ))}
          </div>
        )}
      </div>

      <div className={styles.inputArea}>
        {dropdownModels.length > 0 && (
          <div className={styles.modelBar}>
            <div className={styles.modelDropdown}>
              <button className={styles.modelBtn} onClick={() => setShowModels(v => !v)}>
                <span>{selectedModel?.id ?? dropdownModels[0]?.id ?? 'default model'}</span>
                <ChevronDown size={11} />
              </button>
              {showModels && (
                <div className={styles.modelMenu}>
                  {dropdownModels.map(m => {
                    const badge = m.provider === 'openrouter'
                      ? pricingBadge(catalog, m.id) : null
                    return (
                      <button
                        key={`${m.provider}:${m.id}`}
                        className={`${styles.modelItem} ${selectedModel?.id === m.id && selectedModel?.provider === m.provider ? styles.modelItemActive : ''}`}
                        onClick={() => { setSelectedModel({ id: m.id, provider: m.provider }); setShowModels(false) }}
                      >
                        <span className={styles.modelProvider}>{m.provider}</span>
                        <span className={styles.modelId}>{m.id}</span>
                        {badge && <span className={styles.modelPrice}>{badge}</span>}
                      </button>
                    )
                  })}
                </div>
              )}
            </div>
            <button
              type="button"
              className={styles.modelBtn}
              onClick={() => setNoThink((v) => !v)}
              title={noThink
                ? 'Reasoning suppressed for this chat (/no_think). Click to allow thinking.'
                : 'Model may show its thinking. Click to suppress reasoning (/no_think) for this chat.'}
              style={noThink ? { color: 'var(--accent, #4f8cff)' } : undefined}
            >
              <Brain size={12} />
              <span>{noThink ? 'No-think' : 'Thinking'}</span>
            </button>
            <div className={styles.modelBarRight}>
              {(contextTokens > 0 || totalOutputTokens > 0) && (() => {
                const cost = lastTurnCost && lastTurnCost.provider === 'openrouter'
                  ? costForTurn(catalog, lastTurnCost.model,
                                lastTurnCost.prompt_tokens, lastTurnCost.completion_tokens)
                  : null
                return (
                  <span
                    className={styles.tokenTotals}
                    title="Context tokens (in) · cumulative output tokens"
                  >
                    {contextTokens}↑ {totalOutputTokens}↓ tok
                    {cost !== null && <> · {formatUsd(cost)}</>}
                  </span>
                )
              })()}
              {input.length > 100 && (
                <span className={styles.charCount}>{input.length}</span>
              )}
              <VoiceToggle />
            </div>
          </div>
        )}

        {/* Pending attachment preview chips. Render above the input
            so they're visible alongside the typed caption. */}
        {pendingAttachments.length > 0 && (
          <div className={styles.attachmentRow}>
            {pendingAttachments.map((a, i) => (
              <div key={i} className={styles.attachmentChip}>
                <img
                  src={`data:${a.mime_type};base64,${a.data_b64}`}
                  alt={`attachment ${i + 1}`}
                  className={styles.attachmentThumb}
                />
                <button
                  className={styles.attachmentRemove}
                  onClick={() => setPendingAttachments((prev) => prev.filter((_, j) => j !== i))}
                  title="Remove"
                  type="button"
                >
                  <XIcon size={10} />
                </button>
              </div>
            ))}
          </div>
        )}

        <div
          className={`${styles.inputBox} ${isDragOver ? styles.inputBoxDragOver : ''}`}
          onDragOver={(e) => {
            // Only react to file drags — ignore drags of selected text etc.
            if (e.dataTransfer?.types?.includes('Files')) {
              e.preventDefault()
              setIsDragOver(true)
            }
          }}
          onDragLeave={() => setIsDragOver(false)}
          onDrop={handleDrop}
        >
          {/* Hidden file picker triggered by the paperclip button. */}
          <input
            ref={fileInputRef}
            type="file"
            accept="image/*"
            multiple
            style={{ display: 'none' }}
            onChange={(e) => {
              if (e.target.files) addFiles(e.target.files)
              // Reset so picking the same file twice still fires onChange.
              e.target.value = ''
            }}
          />
          <button
            className={styles.attachBtn}
            onClick={() => fileInputRef.current?.click()}
            disabled={isStreaming}
            title="Attach images"
            type="button"
          >
            <Paperclip size={15} />
          </button>
          <textarea
            ref={textareaRef}
            className={styles.textarea}
            placeholder="Message MIRA…"
            value={input}
            onChange={handleInputChange}
            onKeyDown={handleKeyDown}
            onPaste={handlePaste}
            disabled={isStreaming}
            rows={1}
            aria-label="Message input"
          />
          <RecordButton
            onTranscript={handleTranscript}
            disabled={isStreaming}
          />
          {isStreaming ? (
            <button className={styles.stopBtn} onClick={stop} title="Stop (Esc)">
              <Square size={15} />
            </button>
          ) : (
            <button
              className={styles.sendBtn}
              onClick={() => handleSend()}
              disabled={!input.trim() && pendingAttachments.length === 0}
              title="Send (Enter)"
            >
              <Send size={15} />
            </button>
          )}
        </div>
        <p className={styles.hint}>
          <kbd>Enter</kbd> send · <kbd>Shift+Enter</kbd> new line · <kbd>Esc</kbd> stop
        </p>
      </div>
    </div>
  )
}

// ── Welcome screen ────────────────────────────────────────────────────────────

function Welcome() {
  return (
    <div className={styles.welcome}>
      <div className={styles.welcomeGlow} />
      <div className={styles.welcomeIcon}>
        <img src={miraLogo} alt="MIRA" className={styles.welcomeLogo} />
      </div>
      <h2>How can I help you today?</h2>
      <p>Start a conversation · Press <kbd>/</kbd> to focus the input</p>
    </div>
  )
}

// ── Loading skeleton ──────────────────────────────────────────────────────────

function MessagesLoading() {
  return (
    <div className={styles.loadingList}>
      {[80, 55, 90, 40, 70].map((w, i) => (
        <div key={i} className={`${styles.skeletonRow} ${i % 2 === 0 ? styles.skeletonRowAi : styles.skeletonRowUser}`}>
          <div className={styles.skeletonBubble} style={{ width: `${w}%` }} />
        </div>
      ))}
    </div>
  )
}

// ── Message dispatcher ────────────────────────────────────────────────────────

function MessageBubble({
  message, msgIndex, onEdit, onRegenerate, autoSpeak,
}: {
  message: Message
  msgIndex: number
  onEdit: (content: string) => void
  onRegenerate: (index: number) => void
  autoSpeak: boolean
}) {
  if (message.role === 'user')      return <UserCard message={message} onEdit={onEdit} />
  if (message.role === 'system')    return <SystemCard message={message} variant="system" />
  if (message.role === 'tool')      return <SystemCard message={message} variant="tool" />
  return <AssistantCard message={message} msgIndex={msgIndex} onRegenerate={onRegenerate} autoSpeak={autoSpeak} />
}

// ── Message timestamp ─────────────────────────────────────────────────────────

/// Compact, relative-aware timestamp shown under each message. Today →
/// just the time; yesterday → "Yesterday HH:MM"; this year → "Mon D, HH:MM";
/// older → "Mon D YYYY, HH:MM". `created_at` is epoch ms.
function formatMsgTime(ms: number): string {
  if (!ms) return ''
  const d = new Date(ms)
  if (Number.isNaN(d.getTime())) return ''
  const now = new Date()
  const sameDay = (a: Date, b: Date) =>
    a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate()
  const time = d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })
  if (sameDay(d, now)) return time
  const yesterday = new Date(now)
  yesterday.setDate(now.getDate() - 1)
  if (sameDay(d, yesterday)) return `Yesterday ${time}`
  const opts: Intl.DateTimeFormatOptions = d.getFullYear() === now.getFullYear()
    ? { month: 'short', day: 'numeric' }
    : { month: 'short', day: 'numeric', year: 'numeric' }
  return `${d.toLocaleDateString([], opts)}, ${time}`
}

/// Always-visible muted timestamp line. `title` carries the full
/// locale string for a precise value on hover.
function MsgTime({ ms }: { ms: number }) {
  const label = formatMsgTime(ms)
  if (!label) return null
  return (
    <div className={styles.msgTime} title={ms ? new Date(ms).toLocaleString() : undefined}>
      {label}
    </div>
  )
}

// ── User bubble ───────────────────────────────────────────────────────────────

function UserCard({ message, onEdit }: { message: Message; onEdit: (c: string) => void }) {
  const [copied, setCopied] = useState(false)
  const copy = () => { navigator.clipboard.writeText(message.content); setCopied(true); setTimeout(() => setCopied(false), 2000) }
  // Q1.3 — render any image attachments alongside the bubble. Reads
  // from message.attachments first (live, just-sent), falls back to
  // metadata.attachments (persisted, reload).
  const attachments = useMemo(() => parseMessageAttachments(message), [message])
  const user = useAuthStore((s) => s.user)

  return (
    <div className={styles.userRow}>
      <div className={styles.userGroup}>
        {attachments.length > 0 && (
          <div className={styles.userAttachments}>
            {attachments.map((a, i) => (
              <img
                key={i}
                src={`data:${a.mime_type};base64,${a.data_b64}`}
                alt={`attachment ${i + 1}`}
                className={styles.userAttachmentImg}
              />
            ))}
          </div>
        )}
        {message.content && (
          <div className={styles.userBubble}>{message.content}</div>
        )}
        <div className={styles.userActions}>
          <ActionBtn title="Edit (fork)" onClick={() => onEdit(message.content)}><Pencil size={11} /></ActionBtn>
          <ActionBtn title="Copy" onClick={copy}>{copied ? <Check size={11} /> : <Copy size={11} />}</ActionBtn>
        </div>
        <MsgTime ms={message.created_at} />
      </div>
      {user ? (
        <div className={styles.userAvatarReal} aria-hidden="true">
          <Avatar user={user} size={28} />
        </div>
      ) : (
        <div className={styles.userAvatar} aria-hidden="true"><User size={13} /></div>
      )}
    </div>
  )
}

// ── Assistant card ────────────────────────────────────────────────────────────

function AssistantCard({
  message, msgIndex, onRegenerate, autoSpeak,
}: {
  message: Message; msgIndex: number; onRegenerate: (i: number) => void; autoSpeak: boolean
}) {
  const [copied, setCopied] = useState(false)
  const copy = () => { navigator.clipboard.writeText(message.content); setCopied(true); setTimeout(() => setCopied(false), 2000) }
  // Cached config — useQuery dedupes by key so calling it per message
  // is free. We render the rollup only when the user has opted in
  // (default true) and there's something to show.
  const { data: cfg } = useQuery({
    queryKey: ['config'],
    queryFn:  () => api.get('/api/config').then((r) => r.data),
    staleTime: 30_000,
  })
  const showThinking = cfg?.agent?.show_thinking !== false
  const thinkingEntries = useMemo(
    () => parseMessageMetadata(message),
    [message],
  )
  // Media a tool produced (screenshots, synthesised speech, …) belongs in
  // the message body, not buried in the collapsible trace. Pull artifact
  // links out of the tool results — but skip any the model already inlined
  // into its own reply (MarkdownContent renders those) to avoid duplicates.
  const toolMedia = useMemo(
    () => collectToolMedia(thinkingEntries, message.tool_calls, message.content),
    [thinkingEntries, message.tool_calls, message.content],
  )

  return (
    <div className={styles.aiRow} role="article" aria-label={`MIRA: ${message.content.slice(0, 80)}`}>
      <div className={styles.aiAvatar} aria-hidden="true">
        <AgentAvatar size={34} className={styles.aiAvatarImg} />
      </div>
      <div className={styles.aiGroup}>
        <div className={styles.aiCard}>
          <div className={styles.aiCardLabel}>MIRA</div>
          {parseMessageWarnings(message).map((w, j) => (
            <div key={j} className={styles.aiWarning} role="alert">
              <span aria-hidden="true">⚠️</span>
              <span>{w}</span>
            </div>
          ))}
          {showThinking && thinkingEntries.length > 0 && (
            <ThinkingPanel entries={thinkingEntries} />
          )}
          <div className={styles.aiCardBody}>
            <MarkdownContent content={message.content} />
            {toolMedia.map((m, j) => <ArtifactMedia key={j} url={m.url} kind={m.kind} />)}
            {message.tool_calls && <ToolCallsDisplay raw={message.tool_calls} />}
          </div>
          <div className={styles.aiCardActions}>
            <ActionBtn title="Copy" onClick={copy}>{copied ? <Check size={11} /> : <Copy size={11} />}</ActionBtn>
            <SpeakBtn text={message.content} autoStart={autoSpeak} />
            <ActionBtn title="Regenerate" onClick={() => onRegenerate(msgIndex)}><RotateCcw size={11} /></ActionBtn>
          </div>
        </div>
        <MsgTime ms={message.created_at} />
      </div>
    </div>
  )
}

// ── Streaming card ────────────────────────────────────────────────────────────

function StreamingMarkdownContent({ content }: { content: string }) {
  const segments = splitThinking(content)
  return (
    <div className={styles.markdown}>
      {segments.map((seg, i) =>
        seg.type === 'thinking'
          ? <ThinkingBlock key={i} text={seg.text} streaming={!seg.complete} />
          : <ReactMarkdown key={i} remarkPlugins={[remarkGfm]}>{seg.text}</ReactMarkdown>
      )}
    </div>
  )
}

/// Split a message body into alternating prose and `<thinking>...</thinking>`
/// segments. Handles a still-streaming, unclosed `<thinking>` at the end —
/// returned with `complete: false` so the renderer can show "thinking..."
/// instead of a closed/expandable details element.
type Segment = { type: 'text' | 'thinking'; text: string; complete: boolean }
function splitThinking(content: string): Segment[] {
  const out: Segment[] = []
  let cursor = 0
  while (cursor < content.length) {
    const open = content.indexOf('<thinking>', cursor)
    if (open === -1) {
      const tail = content.slice(cursor)
      if (tail.length > 0) out.push({ type: 'text', text: tail, complete: true })
      break
    }
    if (open > cursor) {
      out.push({ type: 'text', text: content.slice(cursor, open), complete: true })
    }
    const inner = open + '<thinking>'.length
    const close = content.indexOf('</thinking>', inner)
    if (close === -1) {
      out.push({ type: 'thinking', text: content.slice(inner), complete: false })
      break
    }
    out.push({ type: 'thinking', text: content.slice(inner, close), complete: true })
    cursor = close + '</thinking>'.length
  }
  return out
}

function ThinkingBlock({ text, streaming }: { text: string; streaming: boolean }) {
  const [open, setOpen] = useState(false)
  return (
    <details
      className={styles.thinkingBlock}
      open={open}
      onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}
    >
      <summary className={styles.thinkingSummary}>
        <ChevronDown
          size={11}
          style={{ transform: open ? 'rotate(180deg)' : '', transition: 'transform 0.15s' }}
        />
        <span>{streaming ? 'Thinking…' : 'Thought process'}</span>
      </summary>
      <pre className={styles.thinkingBody}>{text}</pre>
    </details>
  )
}

function StreamingCard({ content }: { content: string }) {
  // Live thinking trail mirrors what the SSE stream has pushed so far.
  // Read from the store (not props) so the panel grows in real time
  // without re-rendering the whole ChatPage on every event.
  const streamingThinking = useChatStore((s) => s.streamingThinking)
  const { data: cfg } = useQuery({
    queryKey: ['config'],
    queryFn:  () => api.get('/api/config').then((r) => r.data),
    staleTime: 30_000,
  })
  const showThinking = cfg?.agent?.show_thinking !== false
  return (
    <div className={styles.aiRow}>
      <div className={styles.aiAvatar} aria-hidden="true">
        <AgentAvatar size={34} className={styles.aiAvatarImg} />
      </div>
      <div className={`${styles.aiCard} ${styles.aiCardStreaming}`}>
        <div className={styles.aiCardLabel}>MIRA</div>
        {showThinking && (
          <ThinkingPanel entries={streamingThinking} isStreaming />
        )}
        <div className={styles.aiCardBody}>
          {content ? (
            <>
              <StreamingMarkdownContent content={content} />
              <span className={styles.cursor} aria-hidden="true" />
            </>
          ) : (
            <div className={styles.typing} aria-label="Thinking">
              <span /><span /><span />
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

// ── System / tool message ─────────────────────────────────────────────────────

function SystemCard({ message, variant }: { message: Message; variant: 'system' | 'tool' }) {
  return (
    <div className={styles.systemRow}>
      <div className={styles.systemGroup}>
        <div className={`${styles.systemBubble} ${styles[`system_${variant}`]}`}>
          {variant === 'tool' ? <Wrench size={11} /> : <Settings size={11} />}
          <span>{message.content}</span>
        </div>
        <MsgTime ms={message.created_at} />
      </div>
    </div>
  )
}

// ── Tool calls display ────────────────────────────────────────────────────────

// Pull artifact media links (screenshots, synthesised audio, …) out of a
// tool's output string so the message body can render them as real players.
function artifactMediaFromOutput(output?: string): { url: string; kind: 'img' | 'audio' | 'video' }[] {
  if (!output) return []
  const re = /\/api\/artifacts\/[0-9a-f]{64}\.(png|jpe?g|gif|svg|webp|mp3|wav|ogg|opus|m4a|flac|mp4|webm|mov)/gi
  const seen = new Set<string>()
  const out: { url: string; kind: 'img' | 'audio' | 'video' }[] = []
  for (const m of output.matchAll(re)) {
    const url = m[0]
    if (seen.has(url)) continue
    seen.add(url)
    const ext = m[1].toLowerCase()
    const kind = ['mp3','wav','ogg','opus','m4a','flac'].includes(ext) ? 'audio'
               : ['mp4','webm','mov'].includes(ext) ? 'video' : 'img'
    out.push({ url, kind })
  }
  return out
}

/// Gather every artifact a turn's tools produced (from the parsed thinking
/// trace + the persisted tool_calls), deduped and excluding any URL the
/// model already inlined into its reply text (MarkdownContent renders those).
function collectToolMedia(
  entries: ThinkingEntry[],
  toolCallsRaw: string | null,
  content: string,
): { url: string; kind: 'img' | 'audio' | 'video' }[] {
  const outputs: string[] = []
  for (const e of entries) if (e.type === 'tool_result') outputs.push(e.output)
  if (toolCallsRaw) {
    try {
      const calls = JSON.parse(toolCallsRaw) as Array<{ output?: string }>
      for (const c of calls) if (c.output) outputs.push(c.output)
    } catch { /* not JSON — ignore */ }
  }
  const seen = new Set<string>()
  const out: { url: string; kind: 'img' | 'audio' | 'video' }[] = []
  for (const o of outputs) {
    for (const m of artifactMediaFromOutput(o)) {
      if (seen.has(m.url) || content.includes(m.url)) continue
      seen.add(m.url)
      out.push(m)
    }
  }
  return out
}

/// A tool-produced media artifact (screenshot / audio clip / video) rendered
/// in the message body, with download + copy-link mini buttons. The
/// `/api/artifacts/<sha>.<ext>` URL is a public capability URL — no auth —
/// so download/copy work directly and a copied link is shareable to anyone.
function ArtifactMedia({ url, kind }: { url: string; kind: 'img' | 'audio' | 'video' }) {
  const [copied, setCopied] = useState(false)
  const ext = url.split('.').pop() ?? 'bin'
  const copyLink = () => {
    const abs = `${window.location.origin}${url}`
    navigator.clipboard.writeText(abs)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
    toast.success('Link copied — anyone with it can open it (no login needed).')
  }
  return (
    <div className={styles.artifactMedia}>
      {kind === 'audio'
        ? <audio src={url} controls preload="metadata" className={styles.markdownMedia} />
        : kind === 'video'
        ? <video src={url} controls preload="metadata" className={styles.markdownMedia} />
        : <img src={url} alt="tool artifact" loading="lazy" className={styles.markdownImage} />}
      <div className={styles.artifactActions}>
        <a className={styles.artifactBtn} href={url} download={`mira-artifact.${ext}`} title="Download this file">
          <Download size={11} /> Download
        </a>
        <button className={styles.artifactBtn} onClick={copyLink} title="Copy a shareable link">
          {copied ? <Check size={11} /> : <LinkIcon size={11} />} {copied ? 'Copied' : 'Copy link'}
        </button>
      </div>
    </div>
  )
}

function ToolCallsDisplay({ raw }: { raw: string }) {
  const [open, setOpen] = useState(false)
  let calls: Array<{ type: string; tool: string; args?: string; success?: boolean; output?: string }> = []
  try { calls = JSON.parse(raw) } catch { return null }
  if (!calls.length) return null

  return (
    <div className={styles.toolCalls}>
      {calls.map((c, i) => (
        <div key={i} className={styles.toolCard}>
          <button className={styles.toolHeader} onClick={() => setOpen(v => !v)}>
            <Wrench size={11} />
            <span className={styles.toolName}>{c.tool}</span>
            {c.success !== undefined && (
              <span className={c.success ? styles.toolOk : styles.toolErr}>
                {c.success ? '✓' : '✗'}
              </span>
            )}
            <ChevronDown size={10} style={{ marginLeft: 'auto', transform: open ? 'rotate(180deg)' : '', transition: 'transform 0.15s' }} />
          </button>
          {open && (
            <div className={styles.toolBody}>
              {c.args   && <pre className={styles.toolPre}>{c.args}</pre>}
              {c.output && <pre className={styles.toolPre}>{c.output.slice(0, 2000)}</pre>}
            </div>
          )}
        </div>
      ))}
    </div>
  )
}

// ── Voice auto-play toggle ────────────────────────────────────────────────────
//
// Sits in the model bar above the composer. Toggles `voiceStore.enabled`,
// which drives auto-play on freshly-arrived assistant messages. The per-
// message 🔊 button stays available regardless.

function VoiceToggle() {
  const enabled = useVoiceStore((s) => s.enabled)
  const toggle  = useVoiceStore((s) => s.toggle)
  const title   = enabled ? 'Voice auto-play: on (click to disable)'
                          : 'Voice auto-play: off (click to enable)'
  return (
    <button
      className={`${styles.voiceToggle} ${enabled ? styles.voiceToggleOn : ''}`}
      onClick={toggle}
      title={title}
      aria-label={title}
      aria-pressed={enabled}
    >
      {enabled ? <Volume2 size={11} /> : <VolumeX size={11} />}
      <span>voice {enabled ? 'on' : 'off'}</span>
    </button>
  )
}

// ── Action button ─────────────────────────────────────────────────────────────

function ActionBtn({
  children, title, onClick, disabled,
}: {
  children: React.ReactNode
  title: string
  onClick: () => void
  disabled?: boolean
}) {
  return (
    <button
      className={styles.actionBtn}
      title={title}
      onClick={onClick}
      aria-label={title}
      disabled={disabled}
    >
      {children}
    </button>
  )
}

// ── Speak (TTS) button ────────────────────────────────────────────────────────
//
// Per-message 🔊: streams the assistant text through `/api/tts/speak/stream`.
// Each SSE `chunk` event is one sentence's WAV bytes; we queue them as
// independent <Audio> elements and play them back-to-back. Playback starts as
// soon as the first sentence arrives — perceived latency is dominated by the
// first synthesise call, not the full response. Falls back to the full-buffer
// `/api/tts/speak` endpoint if the stream errors out.

/**
 * Drives streaming TTS playback for one piece of text. Used by both the
 * manual 🔊 button and the auto-play path on fresh assistant messages.
 *
 * Returns a stable imperative handle so callers don't need to re-render to
 * trigger play/stop, and the current state for icon swapping.
 */
function useTtsPlayback(text: string) {
  const [state, setState] = useState<'idle' | 'loading' | 'playing'>('idle')
  const voiceId           = useVoiceStore((s) => s.voiceId)
  const speed             = useVoiceStore((s) => s.speed)
  // Cached config powers per-backend playback gain. Stale-while-revalidate is
  // fine: a freshly-saved volume slider takes effect on the *next* play, and
  // the SettingsPage save invalidates ['config'] so the refresh is immediate.
  const { data: cfg } = useQuery({
    queryKey: ['config'],
    queryFn:  () => api.get('/api/config').then((r) => r.data),
    staleTime: 30_000,
  })
  const queueRef          = useRef<Blob[]>([])
  const playingRef        = useRef<PlayHandle | null>(null)
  // Set synchronously the moment a chunk is shifted off the queue. Without
  // this, the `else if (!playingRef.current)` guard in onChunk fires twice:
  // playingRef.current is only assigned *after* `playBlobWithGain` resolves
  // (decodeAudioData is async), so a second chunk arriving during the decode
  // window would kick off a parallel playback — sentence 1 and sentence 2
  // overlapping. The flag closes that race.
  const busyRef           = useRef<boolean>(false)
  const abortRef          = useRef<(() => void) | null>(null)
  const doneRef           = useRef<boolean>(false)
  const stoppedRef        = useRef<boolean>(false)

  const teardown = () => {
    abortRef.current?.()
    abortRef.current = null
    playingRef.current?.stop()
    playingRef.current = null
    busyRef.current  = false
    queueRef.current = []
    doneRef.current  = false
  }

  useEffect(() => () => { stoppedRef.current = true; teardown() }, [])

  // Resolve the gain at start time so a slider change between turns picks up
  // on the next play. The web channel inherits routing.web → default_backend.
  const gainForThisStart = (): number => {
    const backend = resolveWebBackend(cfg)
    return volumeForBackend(cfg, backend)
  }

  const playNext = (gain: number) => {
    if (busyRef.current) return
    const blob = queueRef.current.shift()
    if (!blob) {
      if (doneRef.current) {
        playingRef.current = null
        setState('idle')
      }
      return
    }
    busyRef.current = true
    void (async () => {
      try {
        const handle = await playBlobWithGain(blob, gain)
        if (stoppedRef.current) { handle.stop(); busyRef.current = false; return }
        playingRef.current = handle
        await handle.done
        playingRef.current = null
        busyRef.current = false
        if (stoppedRef.current) return
        playNext(gain)
      } catch {
        // Decoding or autoplay failure — drop the chunk and try the next.
        playingRef.current = null
        busyRef.current = false
        playNext(gain)
      }
    })()
  }

  const stop = () => {
    stoppedRef.current = true
    teardown()
    setState('idle')
  }

  const start = () => {
    stoppedRef.current = false
    setState('loading')
    const gain = gainForThisStart()
    let firstChunkSeen = false
    let streamErrored  = false

    abortRef.current = openTtsStream(
      { text, channel: 'web', voice: voiceId ?? undefined, speed },
      {
        onChunk: (chunk: TtsChunk) => {
          if (stoppedRef.current) return
          const blob = new Blob([chunk.bytes as BlobPart], { type: codecToMime(chunk.codec) })
          queueRef.current.push(blob)

          if (!firstChunkSeen) {
            firstChunkSeen = true
            setState('playing')
            playNext(gain)
          } else if (!busyRef.current) {
            playNext(gain)
          }
        },
        onError: async (msg: string) => {
          if (stoppedRef.current) return
          streamErrored = true
          try {
            const blob = await ttsApi.speak({ text, channel: 'web', voice: voiceId ?? undefined, speed })
            if (stoppedRef.current) return
            queueRef.current = [blob]
            doneRef.current  = true
            setState('playing')
            playNext(gain)
          } catch {
            console.warn('TTS stream failed and fallback errored:', msg)
            teardown()
            setState('idle')
          }
        },
        onDone: () => {
          if (streamErrored) return
          doneRef.current = true
          if (!busyRef.current && queueRef.current.length === 0) {
            setState('idle')
          }
        },
      },
    )
  }

  return { state, start, stop }
}

// ── Speak (TTS) button ────────────────────────────────────────────────────────
//
// Per-message 🔊: streams the assistant text through `/api/tts/speak/stream`.
// Each SSE `chunk` event is one sentence's WAV bytes; we queue them as
// independent <Audio> elements and play them back-to-back. Playback starts as
// soon as the first sentence arrives — perceived latency is dominated by the
// first synthesise call, not the full response. Falls back to the full-buffer
// `/api/tts/speak` endpoint if the stream errors out.
//
// `autoStart` triggers play once on mount — used by AssistantCard when the
// user has voice auto-play enabled and the message is freshly arrived.

function SpeakBtn({ text, autoStart = false }: { text: string; autoStart?: boolean }) {
  const { state, start, stop } = useTtsPlayback(text)
  const startedRef = useRef(false)

  useEffect(() => {
    if (!autoStart || startedRef.current) return
    // Defer to a macrotask so we run *after* React StrictMode's dev
    // setup-cleanup-setup dance. Otherwise useTtsPlayback's teardown cleanup
    // would abort the stream we just started, leaving state stuck on 'loading'.
    const id = setTimeout(() => {
      if (startedRef.current) return
      startedRef.current = true
      start()
    }, 0)
    return () => clearTimeout(id)
    // start is captured at first render — intentionally don't re-run on changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoStart])

  const onClick = () => {
    if (state === 'playing' || state === 'loading') stop()
    else start()
  }

  const title = state === 'loading' ? 'Synthesising…'
              : state === 'playing' ? 'Stop'
              : 'Speak'
  return (
    <ActionBtn title={title} onClick={onClick} disabled={false}>
      {state === 'loading'
        ? <Loader2 size={11} className={styles.spin ?? ''} />
        : state === 'playing'
        ? <Square size={11} />
        : <Volume2 size={11} />}
    </ActionBtn>
  )
}

// ── Markdown content ──────────────────────────────────────────────────────────

/// `<sha256-hex>.<ext>` — what `ArtifactStore::filename()` produces server-side.
const ARTIFACT_FILENAME_RE = /^[0-9a-f]{64}\.(png|jpg|jpeg|gif|svg|webp)$/i

/// react-markdown's default `urlTransform` strips anything that isn't
/// http(s)/mailto/tel — that includes our same-origin `/api/artifacts/...`
/// paths, which would otherwise be silently dropped from `<img>` srcs. We
/// allow them explicitly here. Anything else we don't recognise falls back
/// to react-markdown's default sanitiser by returning the original string;
/// react-markdown then runs its own checks. We intentionally do NOT allow
/// `data:` URIs (those would let an attacker inline arbitrary content).
///
/// Models occasionally hallucinate a hostname when echoing the relative
/// `/api/artifacts/<sha>.<ext>` URL we hand them (e.g. they emit
/// `https://example.com/api/artifacts/<sha>.<ext>`). Since the artifact id
/// is itself a content-addressed capability, we defensively detect that
/// shape and rewrite to a same-origin path so the image still renders.
function safeUrlTransform(url: string): string {
  if (url.startsWith('/api/artifacts/')) return url
  if (url.startsWith('http://') || url.startsWith('https://')) {
    try {
      const parsed = new URL(url)
      const m = parsed.pathname.match(/\/api\/artifacts\/([^/]+)$/)
      if (m && ARTIFACT_FILENAME_RE.test(m[1])) {
        return `/api/artifacts/${m[1]}`
      }
    } catch { /* fall through */ }
    return url
  }
  if (url.startsWith('mailto:') || url.startsWith('tel:')) return url
  // Same-origin SPA routes the chat may legitimately reference
  // (watchdog incident pages, future analyzer / agent task / etc).
  // Each new SPA path that ChannelMessage templates emit needs an
  // entry here, otherwise react-markdown will null-href the link
  // and clicking does nothing. UUID-shaped suffixes are checked
  // loosely; the SPA route will 404 if the id isn't real.
  if (url.startsWith('/incidents/'))      return url
  if (url.startsWith('/chat/'))           return url
  if (url.startsWith('/agents'))          return url
  if (url.startsWith('/automations'))     return url
  return ''
}

function MarkdownContent({ content }: { content: string }) {
  const segments = splitThinking(content)
  const renderMarkdown = (text: string, key: number) => (
    <ReactMarkdown
      key={key}
      remarkPlugins={[remarkGfm]}
      rehypePlugins={[rehypeHighlight]}
      urlTransform={safeUrlTransform}
      components={{
        pre({ children }) {
          return <CodeBlock>{children}</CodeBlock>
        },
        code({ className, children, ...props }) {
          return (
            <code className={`${styles.inlineCode} ${className ?? ''}`} {...(props as object)}>
              {children}
            </code>
          )
        },
        img({ src, alt, ...props }) {
          const url = typeof src === 'string' ? src : undefined
          // Tools (MCP etc.) hand back audio/video as artifacts using image
          // markdown syntax (`![](…)`). Switch on the extension so they
          // render as real players, not a broken <img>.
          const ext = url?.split('?')[0].split('#')[0].split('.').pop()?.toLowerCase()
          if (ext && ['mp3','wav','ogg','opus','m4a','flac'].includes(ext)) {
            return <audio src={url} controls preload="metadata" className={styles.markdownMedia} />
          }
          if (ext && ['mp4','webm','mov'].includes(ext)) {
            return <video src={url} controls preload="metadata" className={styles.markdownMedia} />
          }
          return (
            <img
              src={url}
              alt={alt ?? ''}
              loading="lazy"
              className={styles.markdownImage}
              {...(props as object)}
            />
          )
        },
      }}
    >
      {text}
    </ReactMarkdown>
  )
  return (
    <div className={styles.markdown}>
      {segments.map((seg, i) =>
        seg.type === 'thinking'
          ? <ThinkingBlock key={i} text={seg.text} streaming={!seg.complete} />
          : renderMarkdown(seg.text, i)
      )}
    </div>
  )
}

// ── Code block with copy ──────────────────────────────────────────────────────

function CodeBlock({ children }: { children: React.ReactNode }) {
  const [copied, setCopied] = useState(false)
  const preRef = useRef<HTMLPreElement>(null)

  const copy = () => {
    const text = preRef.current?.textContent ?? ''
    navigator.clipboard.writeText(text)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  // Extract language from the code element's className
  const codeEl = (children as React.ReactElement<{ className?: string }>)
  const lang = /language-(\w+)/.exec(codeEl?.props?.className ?? '')?.[1]

  return (
    <div className={styles.codeBlock}>
      <div className={styles.codeBlockHeader}>
        <span className={styles.codeBlockLang}>{lang ?? 'code'}</span>
        <button className={styles.codeBlockCopy} onClick={copy} title="Copy code">
          {copied ? <Check size={11} /> : <Copy size={11} />}
          {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
      <pre ref={preRef} className={styles.codeBlockPre}>{children}</pre>
    </div>
  )
}

// ── Slice H — chat-level wiki affordances ─────────────────────────────────────

/**
 * Sticky toolbar that lives just below the optional onboarding strip
 * and above the messages. Two affordances live here:
 *
 * - **Wiki toggle** — flips `conversation.skip_wiki`. When on, the
 *   server skips the wiki context-injection hook for every turn in
 *   this thread. Useful for one-off chats the user doesn't want
 *   biased by their wiki.
 * - **Save thread** — turns the current conversation into a wiki
 *   page under `pages/conversations/<slug>.md`.
 */
function ChatToolbar({ conversationId }: { conversationId: string }) {
  const qc = useQueryClient()
  const { data: conv } = useQuery({
    queryKey: ['conversation', conversationId],
    queryFn: () => conversationsApi.get(conversationId),
  })
  const skipWiki = !!conv?.skip_wiki

  const toggleMut = useMutation({
    mutationFn: () => conversationsApi.update(conversationId, { skip_wiki: !skipWiki }),
    onSuccess: () => {
      toast.success(skipWiki ? 'Wiki context re-enabled' : 'Wiki context muted for this chat')
      qc.invalidateQueries({ queryKey: ['conversation', conversationId] })
    },
    onError: () => toast.error('Toggle failed'),
  })

  const saveMut = useMutation({
    mutationFn: () => wikiApi.saveThread(conversationId),
    onSuccess: (r) => {
      const path = (r.op as { path?: string } | undefined)?.path ?? '(saved)'
      toast.success(`Saved to wiki: ${path}`)
      qc.invalidateQueries({ queryKey: ['wiki'] })
    },
    onError: (err: unknown) => {
      const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error
      toast.error(msg ?? 'Save failed')
    },
  })

  return (
    <div className={styles.chatToolbar}>
      <button
        className={`${styles.toolbarBtn} ${skipWiki ? styles.toolbarBtnMuted : ''}`}
        onClick={() => toggleMut.mutate()}
        disabled={toggleMut.isPending}
        title={skipWiki
          ? 'Wiki context is muted for this chat — click to re-enable'
          : 'Wiki context is on for this chat — click to mute'}
      >
        <BookOpen size={12} />
        {skipWiki ? 'Wiki: off' : 'Wiki: on'}
      </button>
      <button
        className={styles.toolbarBtn}
        onClick={() => saveMut.mutate()}
        disabled={saveMut.isPending}
        title="Save this thread as a wiki page"
      >
        <BookmarkPlus size={12} />
        Save to wiki
      </button>
    </div>
  )
}

