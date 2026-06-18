// SPDX-License-Identifier: AGPL-3.0-or-later

import { useMemo, useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { useNavigate } from 'react-router-dom'
import {
  Trash2, MessageSquare, ExternalLink, Download,
  ChevronDown, ChevronRight, User as UserIcon,
} from 'lucide-react'
import { conversationsApi } from '@/api/conversations'
import { useAuthStore } from '@/store/authStore'
import { format, formatDistanceToNow } from 'date-fns'
import ChannelBadge from '@/components/ChannelBadge'
import type { Conversation, HistoryStats, PerUserStats } from '@/api/types'
import styles from './ConversationsPage.module.css'

function formatNumber(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, '') + 'M'
  if (n >= 1_000)     return (n / 1_000).toFixed(1).replace(/\.0$/, '')     + 'K'
  return String(n)
}

function StatsCard({ stats, title }: { stats: HistoryStats; title?: string }) {
  const since = stats.first_message_at
    ? format(new Date(stats.first_message_at), 'MMM d, yyyy')
    : null

  return (
    <div className={styles.statsColumn}>
      {title && <div className={styles.statsColumnTitle}>{title}</div>}
      <div className={styles.stats}>
        <div className={styles.statTile}>
          <span className={styles.statLabel}>Conversations</span>
          <span className={styles.statValue}>{formatNumber(stats.total_conversations)}</span>
        </div>
        <div className={styles.statTile}>
          <span className={styles.statLabel}>Messages</span>
          <span className={styles.statValue}>{formatNumber(stats.total_messages)}</span>
          <span className={styles.statSub}>
            {formatNumber(stats.user_messages)} req · {formatNumber(stats.assistant_messages)} resp
            {stats.tool_messages > 0 && ` · ${formatNumber(stats.tool_messages)} tool`}
          </span>
        </div>
        <div className={styles.statTile}>
          <span className={styles.statLabel}>Tokens (approx)</span>
          <span className={styles.statValue}>{formatNumber(stats.estimated_tokens)}</span>
          <span className={styles.statSub}>
            ~{stats.total_messages > 0
              ? formatNumber(Math.round(stats.estimated_tokens / stats.total_messages))
              : '0'} / msg
          </span>
        </div>
        {stats.top_model && (
          <div className={styles.statTile}>
            <span className={styles.statLabel}>Top model</span>
            <span className={styles.statValueSm}>{stats.top_model}</span>
          </div>
        )}
        {since && (
          <div className={styles.statTile}>
            <span className={styles.statLabel}>Active since</span>
            <span className={styles.statValueSm}>{since}</span>
          </div>
        )}
        {stats.per_channel.length > 0 && (
          <div className={`${styles.statTile} ${styles.channelsTile}`}>
            <span className={styles.statLabel}>By channel</span>
            <div className={styles.channelList}>
              {stats.per_channel.map((c) => (
                <div key={c.channel} className={styles.channelRow}>
                  <ChannelBadge channel={c.channel} />
                  <span className={styles.channelCount}>
                    {formatNumber(c.conversations)}c · {formatNumber(c.messages)}m
                  </span>
                </div>
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  )
}

async function exportConversation(id: string, title: string, fmt: 'md' | 'json') {
  const messages = await conversationsApi.messages(id)
  let content: string
  let filename: string
  const safe = (title || 'untitled').replace(/[^a-z0-9]/gi, '-').toLowerCase()

  if (fmt === 'json') {
    content  = JSON.stringify({ title, messages }, null, 2)
    filename = `${safe}.json`
  } else {
    const lines = [`# ${title || 'Untitled conversation'}`, '']
    for (const m of messages) {
      lines.push(`## ${m.role === 'user' ? 'User' : 'Assistant'}`)
      lines.push(m.content)
      lines.push('')
    }
    content  = lines.join('\n')
    filename = `${safe}.md`
  }

  const blob = new Blob([content], { type: 'text/plain' })
  const url  = URL.createObjectURL(blob)
  const a    = document.createElement('a')
  a.href     = url
  a.download = filename
  a.click()
  URL.revokeObjectURL(url)
}

function ConversationRow({
  conv,
  onDelete,
}: {
  conv: Conversation
  onDelete: (id: string) => void
}) {
  const navigate = useNavigate()
  return (
    <div className={styles.item}>
      <div className={styles.itemInfo}>
        <span className={styles.itemTitle}>{conv.title || 'Untitled conversation'}</span>
        <span className={styles.itemMeta}>
          <ChannelBadge channel={conv.channel} />
          {conv.updated_at && (
            <span className={styles.itemTime}>
              {formatDistanceToNow(new Date(conv.updated_at), { addSuffix: true })}
            </span>
          )}
        </span>
      </div>
      <div className={styles.itemActions}>
        <button
          className={styles.iconBtn}
          onClick={() => navigate(`/chat/${conv.id}`)}
          title="Open"
        >
          <ExternalLink size={15} />
        </button>
        <button
          className={styles.iconBtn}
          onClick={() => exportConversation(conv.id, conv.title ?? 'Untitled', 'md')}
          title="Export as Markdown"
        >
          <Download size={15} />
        </button>
        <button
          className={`${styles.iconBtn} ${styles.danger}`}
          onClick={() => {
            if (confirm('Delete this conversation?')) onDelete(conv.id)
          }}
          title="Delete"
        >
          <Trash2 size={15} />
        </button>
      </div>
    </div>
  )
}

function UserGroupSection({
  ownerLabel,
  ownerId,
  conversations,
  defaultOpen,
  perUserStats,
  onDelete,
}: {
  ownerLabel: string
  ownerId:    string
  conversations: Conversation[]
  defaultOpen: boolean
  perUserStats?: PerUserStats
  onDelete:   (id: string) => void
}) {
  const [open, setOpen] = useState(defaultOpen)

  return (
    <div className={styles.group} data-owner={ownerId}>
      <button className={styles.groupHeader} onClick={() => setOpen((v) => !v)}>
        {open ? <ChevronDown size={16} /> : <ChevronRight size={16} />}
        <UserIcon size={14} />
        <span className={styles.groupLabel}>{ownerLabel}</span>
        <span className={styles.groupCount}>
          {conversations.length} conv
          {perUserStats && ` · ${formatNumber(perUserStats.stats.total_messages)} msg`}
        </span>
      </button>
      {open && (
        <div className={styles.groupBody}>
          {conversations.length === 0 ? (
            <div className={styles.empty}>
              <MessageSquare size={32} />
              <p>No conversations for this user.</p>
            </div>
          ) : (
            conversations.map((c) => (
              <ConversationRow key={c.id} conv={c} onDelete={onDelete} />
            ))
          )}
        </div>
      )}
    </div>
  )
}

export default function ConversationsPage() {
  const qc = useQueryClient()
  const user   = useAuthStore((s) => s.user)
  const isAdmin = user?.role === 'admin'

  // Everyone fetches their own list + own stats. Admins additionally fetch
  // grouped + admin-totals so the page can show the cross-user view.
  const { data: myConversations = [], isLoading: loadingList } = useQuery({
    queryKey: ['conversations'],
    queryFn: conversationsApi.list,
  })

  const { data: myStats } = useQuery({
    queryKey: ['conversations', 'stats'],
    queryFn: conversationsApi.stats,
  })

  const { data: groups, isLoading: loadingGroups } = useQuery({
    queryKey: ['conversations', 'admin', 'grouped'],
    queryFn: conversationsApi.adminGrouped,
    enabled: isAdmin,
  })

  const { data: adminStats } = useQuery({
    queryKey: ['conversations', 'admin', 'stats'],
    queryFn: conversationsApi.adminStats,
    enabled: isAdmin,
  })

  const deleteMut = useMutation({
    mutationFn: (id: string) => conversationsApi.delete(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['conversations'] })
    },
  })

  // Admin-only: separate "your" group from others, sort others by activity.
  const { ownGroup, otherGroups } = useMemo(() => {
    if (!isAdmin || !groups || !user) {
      return { ownGroup: null, otherGroups: [] as typeof groups }
    }
    const own   = groups.find((g) => g.owner.id === user.id) ?? null
    const rest  = groups.filter((g) => g.owner.id !== user.id)
    return { ownGroup: own, otherGroups: rest }
  }, [groups, isAdmin, user])

  const perUserByOwner = useMemo(() => {
    const map = new Map<string, PerUserStats>()
    adminStats?.per_user.forEach((p) => map.set(p.id, p))
    return map
  }, [adminStats])

  if (loadingList || (isAdmin && loadingGroups)) {
    return <div className={styles.loading}>Loading conversations…</div>
  }

  // ── Non-admin: flat view (unchanged behavior) ────────────────────────────────
  if (!isAdmin) {
    return (
      <div className={styles.page}>
        <div className={styles.header}>
          <h1>Your Conversations</h1>
          <p>{myConversations.length} conversation{myConversations.length !== 1 ? 's' : ''}</p>
        </div>

        {myStats && <StatsCard stats={myStats} />}

        <div className={styles.list}>
          {myConversations.length === 0 && (
            <div className={styles.empty}>
              <MessageSquare size={40} />
              <p>No conversations yet. Start chatting!</p>
            </div>
          )}
          {myConversations.map((conv) => (
            <ConversationRow key={conv.id} conv={conv} onDelete={deleteMut.mutate} />
          ))}
        </div>
      </div>
    )
  }

  // ── Admin view: "Your conversations" section + per-user sections ─────────────
  const totalConvs = groups?.reduce((s, g) => s + g.conversations.length, 0) ?? 0

  return (
    <div className={styles.page}>
      <div className={styles.header}>
        <h1>All Conversations</h1>
        <p>
          {totalConvs} conversation{totalConvs !== 1 ? 's' : ''} across{' '}
          {groups?.length ?? 0} user{(groups?.length ?? 0) !== 1 ? 's' : ''}
        </p>
      </div>

      {(myStats || adminStats) && (
        <div className={styles.statsDual}>
          {myStats     && <StatsCard stats={myStats}           title="Your totals" />}
          {adminStats  && <StatsCard stats={adminStats.totals} title="All users totals" />}
        </div>
      )}

      <div className={styles.list}>
        {ownGroup && (
          <UserGroupSection
            ownerLabel={`Your conversations (${ownGroup.owner.display_name || ownGroup.owner.username})`}
            ownerId={ownGroup.owner.id}
            conversations={ownGroup.conversations}
            defaultOpen
            perUserStats={perUserByOwner.get(ownGroup.owner.id)}
            onDelete={deleteMut.mutate}
          />
        )}

        {otherGroups && otherGroups.map((g) => (
          <UserGroupSection
            key={g.owner.id}
            ownerLabel={g.owner.display_name || g.owner.username}
            ownerId={g.owner.id}
            conversations={g.conversations}
            defaultOpen={false}
            perUserStats={perUserByOwner.get(g.owner.id)}
            onDelete={deleteMut.mutate}
          />
        ))}

        {(!ownGroup && (!otherGroups || otherGroups.length === 0)) && (
          <div className={styles.empty}>
            <MessageSquare size={40} />
            <p>No conversations yet.</p>
          </div>
        )}
      </div>
    </div>
  )
}
