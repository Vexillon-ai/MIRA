// SPDX-License-Identifier: AGPL-3.0-or-later

import { Navigate, Outlet } from 'react-router-dom'
import { useAuthStore } from '@/store/authStore'

export default function AdminGuard() {
  const user = useAuthStore((s) => s.user)
  return user?.role === 'admin' ? <Outlet /> : <Navigate to="/" replace />
}
