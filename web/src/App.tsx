// SPDX-License-Identifier: AGPL-3.0-or-later

import { useEffect } from 'react'
import { Routes, Route, Navigate } from 'react-router-dom'
import { Toaster } from 'react-hot-toast'
import { useAuthStore } from '@/store/authStore'
import { useUiStore } from '@/store/uiStore'
import { useNotifications } from '@/hooks/useNotifications'
import AppShell from '@/layouts/AppShell'
import LoginPage from '@/pages/LoginPage'
import SignupPage from '@/pages/SignupPage'
import ChatPage from '@/pages/ChatPage'
import AutomationsPage from '@/pages/AutomationsPage'
import CalendarPage from '@/pages/CalendarPage'
import ChannelAccountsPage from '@/pages/ChannelAccountsPage'
import McpPage from '@/pages/McpPage'
import EmailPage from '@/pages/EmailPage'
import ConversationsPage from '@/pages/ConversationsPage'
import LogsPage from '@/pages/LogsPage'
import MemoryPage from '@/pages/MemoryPage'
import WikiPage from '@/pages/WikiPage'
import ProvidersPage from '@/pages/ProvidersPage'
import SessionsPage from '@/pages/SessionsPage'
import SkillsPage from '@/pages/SkillsPage'
import PluginsPage from '@/pages/PluginsPage'
import AgentsPage from '@/pages/AgentsPage'
import AgentDefinitionsPage from '@/pages/AgentDefinitionsPage'
import WorkflowsPage from '@/pages/WorkflowsPage'
import AgentDetailPage from '@/pages/AgentDetailPage'
import AuditPage from '@/pages/AuditPage'
import PolicyPage from '@/pages/PolicyPage'
import SettingsPage from '@/pages/SettingsPage'
import StatusPage from '@/pages/StatusPage'
import SystemHealthPage from '@/pages/SystemHealthPage'
import UsersPage from '@/pages/UsersPage'
import GroupsPage from '@/pages/GroupsPage'
import IncidentPage from '@/pages/IncidentPage'
import AuthGuard from '@/components/AuthGuard'
import AdminGuard from '@/components/AdminGuard'
import LoadingScreen from '@/components/LoadingScreen'
import OnboardingWelcomeModal from '@/components/OnboardingWelcomeModal'
import SetupWizard from '@/components/SetupWizard'

function NotificationWatcher() {
  useNotifications()
  return null
}

export default function App() {
  const { isLoading, isAuthenticated, refresh, logout, user } = useAuthStore()
  const syncOwner = useUiStore((s) => s.syncOwner)

  useEffect(() => { refresh() }, [refresh])

  // Scope per-user UI flags (wizard skip, banner dismiss, onboarding cooldown)
  // to the current user id, so a reinstall (fresh admin) doesn't inherit stale
  // localStorage and the first-run prompts reappear.
  useEffect(() => { if (user?.id) syncOwner(user.id) }, [user?.id, syncOwner])

  useEffect(() => {
    const handler = () => logout()
    window.addEventListener('mira:auth:logout', handler)
    return () => window.removeEventListener('mira:auth:logout', handler)
  }, [logout])

  if (isLoading) return <LoadingScreen />

  return (
    <>
      <Toaster
        position="bottom-right"
        toastOptions={{
          style: {
            background: 'var(--bg-elevated)',
            color: 'var(--text-primary)',
            border: '1px solid var(--border)',
            borderRadius: 'var(--radius-md)',
            fontSize: '13px',
          },
        }}
      />
      <NotificationWatcher />
      {/* The setup wizard renders above the onboarding modal (higher z-index)
          and gates it via the shared setup-checklist state, so the two never
          visibly overlap on a fresh install. */}
      {isAuthenticated && <SetupWizard />}
      {isAuthenticated && <OnboardingWelcomeModal />}

      <Routes>
        <Route path="/login" element={
          isAuthenticated ? <Navigate to="/" replace /> : <LoginPage />
        } />
        <Route path="/signup" element={
          isAuthenticated ? <Navigate to="/" replace /> : <SignupPage />
        } />

        <Route element={<AuthGuard />}>
          <Route element={<AppShell />}>
            <Route index element={<ChatPage />} />
            <Route path="/chat/:conversationId?" element={<ChatPage />} />
            <Route path="/conversations" element={<ConversationsPage />} />
            <Route path="/calendar" element={<CalendarPage />} />
            <Route path="/automations" element={<AutomationsPage />} />
            <Route path="/skills" element={<SkillsPage />} />
            <Route path="/agents" element={<AgentsPage />} />
            <Route path="/agents/definitions" element={<AgentDefinitionsPage />} />
            <Route path="/workflows" element={<WorkflowsPage />} />
            <Route path="/agents/:id" element={<AgentDetailPage />} />
            <Route path="/audit"  element={<AuditPage  />} />
            <Route path="/memory" element={<MemoryPage />} />
            <Route path="/wiki"   element={<WikiPage />} />
            <Route path="/providers" element={<ProvidersPage />} />
            <Route path="/sessions" element={<SessionsPage />} />
            <Route path="/status" element={<StatusPage />} />
            <Route path="/logs" element={<LogsPage />} />
            <Route path="/channel-accounts" element={<ChannelAccountsPage />} />
            <Route path="/mcp" element={<McpPage />} />
            <Route path="/email" element={<EmailPage />} />
            <Route path="/incidents/:id" element={<IncidentPage />} />
            <Route element={<AdminGuard />}>
              <Route path="/plugins"  element={<PluginsPage />} />
              <Route path="/users"    element={<UsersPage />} />
              <Route path="/groups"   element={<GroupsPage />} />
              <Route path="/policy"   element={<PolicyPage />} />
              <Route path="/health"   element={<SystemHealthPage />} />
              <Route path="/settings" element={<SettingsPage />} />
            </Route>
          </Route>
        </Route>

        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </>
  )
}
