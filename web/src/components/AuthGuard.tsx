// SPDX-License-Identifier: AGPL-3.0-or-later

import { Navigate, Outlet } from 'react-router-dom'
import { useAuthStore } from '@/store/authStore'

export default function AuthGuard() {
  const isAuthenticated = useAuthStore((s) => s.isAuthenticated)
  return isAuthenticated ? <Outlet /> : <Navigate to="/login" replace />
}
