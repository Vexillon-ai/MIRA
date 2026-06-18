// MIRA push-notification service worker (Q1.2).
//
// Loaded from the web SPA on first user opt-in via
// navigator.serviceWorker.register('/mira-sw.js'). The server's
// /api/notifications/push/* endpoints handle subscription persistence;
// this worker just has to decrypt the incoming push (the browser does
// http-ece for us) and call showNotification + handle the click.

self.addEventListener('install', (event) => {
  // Activate immediately rather than waiting for a refresh — first
  // install is paired with an explicit user opt-in so they expect it
  // to start working right away.
  event.waitUntil(self.skipWaiting())
})

self.addEventListener('activate', (event) => {
  // Take control of all open tabs in this scope so a Settings page
  // that just opted in starts seeing pushes without a reload.
  event.waitUntil(self.clients.claim())
})

self.addEventListener('push', (event) => {
  let payload = { title: 'MIRA', body: 'New activity', url: '/' }
  try {
    if (event.data) {
      const parsed = event.data.json()
      payload = { ...payload, ...parsed }
    }
  } catch (e) {
    // Server always sends JSON; if we ever see non-JSON it's a bug
    // in the wiring rather than a malformed remote — render the
    // default rather than dropping silently.
    console.warn('mira-sw: push payload not JSON', e)
  }
  event.waitUntil(
    self.registration.showNotification(payload.title, {
      body:    payload.body,
      icon:    '/favicon.svg',
      badge:   '/favicon.svg',
      data:    { url: payload.url || '/' },
      tag:     payload.channel ? `mira-${payload.channel}` : 'mira',
      // Replace any prior notification with the same tag — avoids a
      // pile of duplicate "Companion check-in" toasts when many fire
      // close together (rare but worth covering).
      renotify: false,
    }),
  )
})

self.addEventListener('notificationclick', (event) => {
  event.notification.close()
  const target = event.notification.data?.url || '/'
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true })
      .then((windowClients) => {
        // If a MIRA tab is already open, focus it and navigate.
        for (const client of windowClients) {
          if ('focus' in client) {
            try { client.navigate(target) } catch { /* cross-origin etc. */ }
            return client.focus()
          }
        }
        if (self.clients.openWindow) {
          return self.clients.openWindow(target)
        }
      }),
  )
})
