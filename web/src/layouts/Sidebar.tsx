// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect, useState } from 'react'
import { NavLink, useNavigate } from 'react-router-dom'
import miraLogo from '@/assets/mira-logo.svg'
import { useQuery } from '@tanstack/react-query'
import {
  MessageSquare, Settings, Users, Plus,
  MessagesSquare, Brain, BookOpen, Server, Wifi, Search,
  Activity, FileText, Radio, Users2, Calendar, Bot, Boxes, Network,
  ScrollText, ShieldCheck, HeartPulse, Plug, Mail, Package, BotMessageSquare, Workflow,
  Sparkles,
} from 'lucide-react'
import { conversationsApi } from '@/api/conversations'
import { useAuthStore } from '@/store/authStore'
import { useChatStore } from '@/store/chatStore'
import { useUiStore } from '@/store/uiStore'
import type { Conversation } from '@/api/types'
import { isToday, isYesterday, isThisWeek, isThisMonth } from 'date-fns'
import ChannelBadge from '@/components/ChannelBadge'
import styles from './Sidebar.module.css'

const CHANNEL_OPTIONS = [
  { value: 'all', label: 'All' },
  { value: 'web', label: 'Web' },
  { value: 'tui', label: 'TUI' },
  { value: 'telegram', label: 'TG' },
  { value: 'signal', label: 'Sig' },
]

function groupConversations(convs: Conversation[]) {
  const groups: Record<string, Conversation[]> = {
    Today: [], Yesterday: [], 'This week': [], 'This month': [], Older: [],
  }
  for (const c of convs) {
    const d = new Date((c.last_message_at ?? c.updated_at) * 1000)
    if (isToday(d))      groups['Today'].push(c)
    else if (isYesterday(d)) groups['Yesterday'].push(c)
    else if (isThisWeek(d))  groups['This week'].push(c)
    else if (isThisMonth(d)) groups['This month'].push(c)
    else                     groups['Older'].push(c)
  }
  return Object.entries(groups)
    .filter(([, items]) => items.length > 0)
    .map(([label, items]) => ({ label, items }))
}

// Unified nav item that renders correctly when collapsed (icon only + tooltip)
function NavItem({
  to, icon, label, collapsed,
}: { to: string; icon: React.ReactNode; label: string; collapsed: boolean }) {
  return (
    <NavLink
      to={to}
      className={({ isActive }) =>
        `${styles.navItem} ${isActive ? styles.navItemActive : ''} ${collapsed ? styles.navItemCollapsed : ''}`
      }
      title={collapsed ? label : undefined}
    >
      <span className={styles.navIcon}>{icon}</span>
      {!collapsed && <span className={styles.navLabel}>{label}</span>}
    </NavLink>
  )
}

export default function Sidebar({ onNavigate }: { onNavigate?: () => void }) {
  const { user } = useAuthStore()
  const { conversations, setConversations, setActiveConversation } = useChatStore()
  const { sidebarCollapsed } = useUiStore()
  const navigate = useNavigate()
  const [search, setSearch] = useState('')
  const [channelFilter, setChannelFilter] = useState('all')

  const { data } = useQuery({
    queryKey: ['conversations'],
    queryFn: conversationsApi.list,
    refetchInterval: 10_000,
  })

  useEffect(() => { if (data) setConversations(data) }, [data, setConversations])

  const handleNew = () => {
    setActiveConversation(null)
    navigate('/chat')
    onNavigate?.()
  }

  const filtered = conversations.filter((c) => {
    const matchSearch  = !search || (c.title ?? 'Untitled').toLowerCase().includes(search.toLowerCase())
    const matchChannel = channelFilter === 'all' || c.channel === channelFilter
    return matchSearch && matchChannel
  })

  const groups = groupConversations(filtered)

  return (
    <aside className={`${styles.sidebar} ${sidebarCollapsed ? styles.collapsed : ''}`}>
      {/* Logo + collapse toggle */}
      <div className={styles.logoRow}>
        <img src={miraLogo} alt="MIRA" className={styles.logoImg} />
        {!sidebarCollapsed && <span className={styles.logoText}>MIRA</span>}
      </div>

      {/* New chat */}
      <button
        className={`${styles.newChat} ${sidebarCollapsed ? styles.newChatCollapsed : ''}`}
        onClick={handleNew}
        title={sidebarCollapsed ? 'New chat' : undefined}
      >
        <Plus size={15} />
        {!sidebarCollapsed && 'New chat'}
      </button>

      {/* Search + channel filter — hidden when collapsed */}
      {!sidebarCollapsed && (
        <>
          <div className={styles.searchBox}>
            <Search size={12} className={styles.searchIcon} />
            <input
              className={styles.searchInput}
              placeholder="Search…"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
            />
          </div>
          <div className={styles.channelFilter}>
            {CHANNEL_OPTIONS.map((opt) => (
              <button
                key={opt.value}
                className={`${styles.channelChip} ${channelFilter === opt.value ? styles.channelChipActive : ''}`}
                onClick={() => setChannelFilter(opt.value)}
              >
                {opt.label}
              </button>
            ))}
          </div>
        </>
      )}

      {/* Conversation list */}
      <nav className={styles.convList}>
        {!sidebarCollapsed ? (
          <>
            {groups.length === 0 && <p className={styles.empty}>No conversations yet</p>}
            {groups.map((group) => (
              <div key={group.label}>
                <div className={styles.groupLabel}>{group.label}</div>
                {group.items.map((conv) => (
                  <NavLink
                    key={conv.id}
                    to={`/chat/${conv.id}`}
                    className={({ isActive }) =>
                      `${styles.convItem} ${isActive ? styles.convItemActive : ''}`
                    }
                    // ChatPage owns activeConversationId via the URL
                    // effect; setting it here too made a re-click of the
                    // already-active row wipe `messages` to [] (the
                    // store resets them on every set), briefly showing
                    // the empty "new chat" view until the React Query
                    // refetch repopulated the list.
                    onClick={() => { onNavigate?.() }}
                  >
                    <MessageSquare size={13} className={styles.convIcon} />
                    <span className={styles.convTitle}>{conv.title || 'Untitled'}</span>
                    <ChannelBadge channel={conv.channel} />
                  </NavLink>
                ))}
              </div>
            ))}
          </>
        ) : (
          /* collapsed: just a few recent icons */
          conversations.slice(0, 8).map((conv) => (
            <NavLink
              key={conv.id}
              to={`/chat/${conv.id}`}
              className={({ isActive }) =>
                `${styles.convItemCollapsed} ${isActive ? styles.convItemActive : ''}`
              }
              onClick={() => { onNavigate?.() }}
              title={conv.title || 'Untitled'}
            >
              <MessageSquare size={14} />
            </NavLink>
          ))
        )}
      </nav>

      {/* Bottom nav */}
      <div className={styles.bottomNav}>
        <NavItem to="/conversations" icon={<MessagesSquare size={15} />} label="History"   collapsed={sidebarCollapsed} />
        <NavItem to="/calendar"      icon={<Calendar size={15} />}        label="Calendar"  collapsed={sidebarCollapsed} />
        <NavItem to="/automations"   icon={<Bot size={15} />}             label="Automations" collapsed={sidebarCollapsed} />
        <NavItem to="/skills"        icon={<Boxes size={15} />}           label="Skills"    collapsed={sidebarCollapsed} />
        <NavItem to="/agents"        icon={<Network size={15} />}         label="Agents"    collapsed={sidebarCollapsed} />
        {/* Audit is per-user now (non-admins see only their own agents' events;
            admins see system-wide), so it lives in the public block. */}
        <NavItem to="/audit"         icon={<ScrollText size={15} />}      label="Audit"     collapsed={sidebarCollapsed} />
        <NavItem to="/memory"        icon={<Brain size={15} />}           label="Memory"    collapsed={sidebarCollapsed} />
        <NavItem to="/wiki"          icon={<BookOpen size={15} />}        label="Wiki"      collapsed={sidebarCollapsed} />
        <NavItem to="/providers"     icon={<Server size={15} />}          label="Providers" collapsed={sidebarCollapsed} />
        <NavItem to="/status"        icon={<Activity size={15} />}        label="Status"    collapsed={sidebarCollapsed} />
        <NavItem to="/channel-accounts" icon={<Radio size={15} />}        label="Channels"  collapsed={sidebarCollapsed} />
        <NavItem to="/mcp"           icon={<Plug size={15} />}            label="MCP"       collapsed={sidebarCollapsed} />
        <NavItem to="/email"         icon={<Mail size={15} />}            label="Email"     collapsed={sidebarCollapsed} />
        <NavItem to="/presence"      icon={<Sparkles size={15} />}        label="Presence"  collapsed={sidebarCollapsed} />

        {user?.role === 'admin' && (
          <>
            {/* Admin-only because each exposes cross-user / system data:
                Logs (every user's messages + internals), Sessions (all users'
                live sessions + evict). */}
            <NavItem to="/logs"     icon={<FileText size={15} />}     label="Logs"     collapsed={sidebarCollapsed} />
            <NavItem to="/sessions" icon={<Wifi size={15} />}         label="Sessions" collapsed={sidebarCollapsed} />
            <NavItem to="/users"    icon={<Users size={15} />}        label="Users"    collapsed={sidebarCollapsed} />
            <NavItem to="/groups"   icon={<Users2 size={15} />}       label="Groups"   collapsed={sidebarCollapsed} />
            <NavItem to="/plugins"  icon={<Package size={15} />}      label="Plugins"  collapsed={sidebarCollapsed} />
            <NavItem to="/agents/definitions" icon={<BotMessageSquare size={15} />} label="Named Agents" collapsed={sidebarCollapsed} />
            <NavItem to="/workflows" icon={<Workflow size={15} />} label="Workflows" collapsed={sidebarCollapsed} />
            <NavItem to="/policy"   icon={<ShieldCheck size={15} />}  label="Policy"   collapsed={sidebarCollapsed} />
            <NavItem to="/health"   icon={<HeartPulse size={15} />}   label="Health"   collapsed={sidebarCollapsed} />
            <NavItem to="/settings" icon={<Settings size={15} />}     label="Settings" collapsed={sidebarCollapsed} />
          </>
        )}

      </div>

      {/* Brand attribution */}
      {!sidebarCollapsed && (
        <a
          href="https://vexillon.ai"
          target="_blank"
          rel="noopener noreferrer"
          style={{
            display: 'block',
            padding: '8px 12px 10px',
            fontSize: '11px',
            opacity: 0.5,
            textDecoration: 'none',
            color: 'inherit',
          }}
          title="MIRA is a project of Vexillon"
        >
          by Vexillon
        </a>
      )}
    </aside>
  )
}
