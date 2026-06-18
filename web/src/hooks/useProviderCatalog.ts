// SPDX-License-Identifier: AGPL-3.0-or-later

import { useQuery } from '@tanstack/react-query'
import { catalogApi, type ModelCatalog } from '@/api/catalog'

/**
 * Lazy-fetch a provider's model catalog. Enabled only when `enabled`
 * is true so an unopened Settings section doesn't trigger upstream
 * API calls. Cached for the React-Query staleTime (24h) on top of
 * the server's own 24h disk cache.
 *
 * Errors don't throw — the UI inspects `error` and falls back to
 * the free-text input when the catalog can't be loaded.
 */
export function useProviderCatalog(slug: string, enabled: boolean) {
  return useQuery<ModelCatalog>({
    queryKey: ['provider-catalog', slug],
    queryFn:  () => catalogApi.fetch(slug),
    enabled:  enabled && Boolean(slug),
    staleTime: 24 * 60 * 60 * 1000, // 24h — same as server cache
    retry:     false,                // upstream fail = show fallback, don't retry
  })
}
