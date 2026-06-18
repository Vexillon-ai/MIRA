// SPDX-License-Identifier: AGPL-3.0-or-later

import { create } from 'zustand'
import type { Conversation, Message, ThinkingEntry } from '@/api/types'

export interface SelectedModel {
  id: string
  provider: string
}

/// Per-turn token + provider/model summary set on every `done` SSE event.
/// `cost_usd` is computed in the renderer from the OpenRouter catalog cache;
/// for non-OpenRouter providers it stays null and only the token counts show.
export interface LastTurnCost {
  provider:         string
  model:            string
  prompt_tokens:    number
  completion_tokens: number
}

interface ChatState {
  conversations: Conversation[]
  activeConversationId: string | null
  messages: Message[]
  isStreaming: boolean
  streamingContent: string
  /// Live thinking trail captured from SSE tool_call / tool_result /
  /// reasoning / wiki_context events. Cleared on every new turn and
  /// when a message is committed (the panel switches to history mode
  /// reading from message.thinking).
  streamingThinking: ThinkingEntry[]
  selectedModel: SelectedModel | null
  lastTurnCost: LastTurnCost | null

  setConversations: (convs: Conversation[]) => void
  setActiveConversation: (id: string | null) => void
  setMessages: (msgs: Message[]) => void
  appendMessage: (msg: Message) => void
  commitMessage: (msg: Message) => void
  updateConversation: (conv: Conversation) => void
  removeConversation: (id: string) => void
  setStreaming: (streaming: boolean, content?: string) => void
  appendStreamChunk: (chunk: string) => void
  setStreamingThinking: (entries: ThinkingEntry[]) => void
  setSelectedModel: (model: SelectedModel | null) => void
  setLastTurnCost: (info: LastTurnCost | null) => void
}

export const useChatStore = create<ChatState>((set) => ({
  conversations: [],
  activeConversationId: null,
  messages: [],
  isStreaming: false,
  streamingContent: '',
  streamingThinking: [],
  selectedModel: null,
  lastTurnCost: null,

  setConversations: (convs) => set({ conversations: convs }),

  // Switching conversations clears the per-turn cost — it belonged to the
  // previous chat and would mislead if shown against a different model.
  setActiveConversation: (id) => set({ activeConversationId: id, messages: [], streamingContent: '', streamingThinking: [], lastTurnCost: null }),

  setMessages: (msgs) => set({ messages: msgs }),

  appendMessage: (msg) =>
    set((s) => ({ messages: [...s.messages, msg] })),

  // Atomically append the final AI message and clear streaming state in one
  // render — eliminates the intermediate state where isStreaming=true while
  // the message is already in messages[], which caused the unformatted flash.
  commitMessage: (msg) =>
    set((s) => ({
      messages: [...s.messages, msg],
      isStreaming: false,
      streamingContent: '',
      streamingThinking: [],
    })),

  updateConversation: (conv) =>
    set((s) => ({
      conversations: s.conversations.map((c) => c.id === conv.id ? conv : c),
    })),

  removeConversation: (id) =>
    set((s) => ({
      conversations: s.conversations.filter((c) => c.id !== id),
      activeConversationId: s.activeConversationId === id ? null : s.activeConversationId,
    })),

  setStreaming: (streaming, content = '') =>
    set({ isStreaming: streaming, streamingContent: content }),

  appendStreamChunk: (chunk) =>
    set((s) => ({ streamingContent: s.streamingContent + chunk })),

  setStreamingThinking: (entries) => set({ streamingThinking: entries }),

  setSelectedModel: (model) => set({ selectedModel: model }),

  setLastTurnCost: (info) => set({ lastTurnCost: info }),
}))
