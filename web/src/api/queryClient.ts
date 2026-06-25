// SPDX-License-Identifier: AGPL-3.0-or-later

import { QueryClient } from '@tanstack/react-query'

// Shared singleton so non-React code (e.g. authStore) can clear the cache on a
// user switch. React Query keys are NOT scoped by user, so without clearing on
// login/logout the previous account's cached data (conversations, config, …)
// can briefly surface to the next user in a shared browser.
export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      staleTime: 30_000,
      refetchOnWindowFocus: false,
    },
  },
})
