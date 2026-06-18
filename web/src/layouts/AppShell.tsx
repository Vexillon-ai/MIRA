// SPDX-License-Identifier: AGPL-3.0-or-later

import { useState } from 'react'
import { Outlet } from 'react-router-dom'
import { ChevronLeft, ChevronRight } from 'lucide-react'
import Sidebar from './Sidebar'
import TopBar from './TopBar'
import UpdateBanner from '@/components/UpdateBanner'
import SetupChecklistBanner from '@/components/SetupChecklistBanner'
import { useUiStore } from '@/store/uiStore'
import styles from './AppShell.module.css'

export default function AppShell() {
  const [mobileOpen, setMobileOpen] = useState(false)
  const { sidebarCollapsed, toggleSidebar } = useUiStore()

  return (
    <div className={`${styles.shell} ${sidebarCollapsed ? styles.sidebarCollapsed : ''}`}>
      <div className={`${styles.sidebarWrapper} ${mobileOpen ? styles.mobileOpen : ''}`}>
        <Sidebar onNavigate={() => setMobileOpen(false)} />
        <button
          className={styles.collapseTab}
          onClick={toggleSidebar}
          title={sidebarCollapsed ? 'Expand sidebar' : 'Collapse sidebar'}
          aria-label={sidebarCollapsed ? 'Expand sidebar' : 'Collapse sidebar'}
        >
          {sidebarCollapsed ? <ChevronRight size={11} /> : <ChevronLeft size={11} />}
        </button>
      </div>
      {mobileOpen && (
        <div className={styles.backdrop} onClick={() => setMobileOpen(false)} />
      )}
      <div className={styles.content}>
        <SetupChecklistBanner />
        <UpdateBanner />
        <TopBar onMenuClick={() => setMobileOpen(v => !v)} />
        <main className={styles.main}>
          <Outlet />
        </main>
      </div>
    </div>
  )
}
