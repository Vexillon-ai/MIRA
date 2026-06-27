// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/NotificationSettings.tsx
//
// Browser/phone push notifications panel for the Settings page.
// Implements the W3C Push API subscribe handshake:
//   1. fetch VAPID public key from /api/notifications/push/public-key
//   2. register /mira-sw.js as a service worker
//   3. ask the browser to subscribe (triggers the OS permission prompt)
//   4. POST the subscription to /api/notifications/push/subscribe
//
// Also surfaces the user's existing subscriptions ("Registered devices")
// so they can revoke any individual device, plus a "Send test push"
// button that pings the server's /push/test endpoint.

import { useEffect, useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Bell, BellOff, Trash2, Send } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import btn from './actionButton.module.css'
import PairDeviceCard from './PairDeviceCard'

const SW_URL = '/mira-sw.js'

interface PushSubscriptionView {
  id:          string
  kind?:       string
  platform?:   string | null
  device_name?: string | null
  user_agent:  string | null
  created_at:  number
  updated_at:  number
}

/// Convert a base64url-no-pad string into a Uint8Array. The Push API
/// wants raw bytes for `applicationServerKey`; the server returns a
/// b64url-encoded uncompressed SEC1 P-256 public key.
function urlBase64ToUint8Array(b64url: string): Uint8Array {
  const padding = '='.repeat((4 - (b64url.length % 4)) % 4)
  const base64  = (b64url + padding).replace(/-/g, '+').replace(/_/g, '/')
  const raw     = atob(base64)
  const out     = new Uint8Array(raw.length)
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i)
  return out
}

/// Turn a PushManager.subscribe failure into something the user can
/// actually act on. The raw DOMException messages are tersely
/// browser-specific ("Registration failed - push service failed") and
/// almost never tell the user what to do next. We pattern-match on
/// well-known shapes + the runtime UA and append a recovery hint.
function diagnoseSubscribeError(e: unknown): string {
  const err = e as Error & { name?: string }
  const msg  = err?.message ?? String(e)
  const name = err?.name    ?? ''
  const ua   = navigator.userAgent
  const isBrave   = (navigator as { brave?: { isBrave?: () => Promise<boolean> } }).brave !== undefined
                  || /Brave/i.test(ua)
  const isFirefox = /Firefox\//i.test(ua) && !/Seamonkey/i.test(ua)

  // Chromium-family "Registration failed - push service failed". Means
  // the browser couldn't reach (or wasn't allowed to reach) its FCM
  // backend. Brave is the most common culprit because Shields blocks
  // Google services by default — and there's a dedicated Privacy
  // toggle the user has to flip first.
  if (/push service failed/i.test(msg) || /AbortError/i.test(name)) {
    if (isBrave) {
      return "Couldn't enable notifications: Brave blocks Google's push "
           + "service by default. Open `brave://settings/privacy` and turn on "
           + "'Use Google services for push messaging', then try again. "
           + "Also set the lion-shield to Standard (not Aggressive) for this site."
    }
    return "Couldn't enable notifications: the browser couldn't reach its "
         + "push service (FCM / Mozilla). This is usually a Chromium-without-"
         + "Google-API-keys build, a corporate proxy blocking the push backend, "
         + "or an aggressive privacy extension. Try a different browser to "
         + "confirm: Chrome, Firefox, or Edge all work out of the box."
  }
  if (/NotAllowedError/i.test(name) || /permission denied/i.test(msg)) {
    return "Couldn't enable notifications: the browser blocked the "
         + "permission prompt. Reset the site's notification permission in "
         + "browser settings and try again."
  }
  if (/NotSupportedError/i.test(name) || /secure context/i.test(msg)) {
    return "Couldn't enable notifications: the browser doesn't expose the "
         + "Push API on this page. Push needs HTTPS (or localhost). "
         + "If you're hitting MIRA from another machine, put it behind "
         + "HTTPS first."
  }
  if (/InvalidAccessError/i.test(name)) {
    return "Couldn't enable notifications: the VAPID public key was "
         + "rejected by the browser. Restart MIRA to regenerate it "
         + "(rare; usually a corrupt key file)."
  }
  if (isFirefox && /push service unavailable/i.test(msg)) {
    return "Couldn't enable notifications: Firefox's push service "
         + "(autopush.services.mozilla.com) was unreachable. Check your "
         + "network / firewall."
  }
  return `Couldn't enable notifications: ${msg}`
}

function arrayBufferToBase64Url(buf: ArrayBuffer | null): string {
  if (!buf) return ''
  const bytes = new Uint8Array(buf)
  let bin = ''
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i])
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

export default function NotificationSettings() {
  const qc = useQueryClient()
  const [supported, setSupported] = useState<boolean>(false)
  const [permission, setPermission] = useState<NotificationPermission>('default')
  const [subscribed, setSubscribed] = useState<boolean>(false)
  const [busy, setBusy] = useState<boolean>(false)

  useEffect(() => {
    const ok = 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window
    setSupported(ok)
    if (ok) setPermission(Notification.permission)
    if (ok) {
      navigator.serviceWorker.getRegistration(SW_URL).then(async (reg) => {
        const sub = await reg?.pushManager.getSubscription()
        setSubscribed(!!sub)
      })
    }
  }, [])

  const subsQuery = useQuery<{ subscriptions: PushSubscriptionView[] }>({
    queryKey: ['push-subscriptions'],
    queryFn:  () => api.get<{ subscriptions: PushSubscriptionView[] }>('/api/notifications/push/subscriptions').then((r) => r.data),
    enabled:  supported,
    staleTime: 30_000,
  })

  const enable = async () => {
    setBusy(true)
    try {
      // Service worker. `register` is idempotent — calling it twice
      // returns the same registration.
      const reg = await navigator.serviceWorker.register(SW_URL)
      await navigator.serviceWorker.ready

      // Permission. If the user already said "deny" we can't recover
      // without them resetting it in browser settings, so surface that
      // explicitly instead of silently failing.
      const perm = await Notification.requestPermission()
      setPermission(perm)
      if (perm !== 'granted') {
        toast.error('Browser blocked notifications. Allow them in browser settings and try again.')
        return
      }

      // VAPID key + subscribe.
      const keyResp = await api.get<{ vapid_public_key: string }>('/api/notifications/push/public-key')
      const applicationServerKey = urlBase64ToUint8Array(keyResp.data.vapid_public_key)
      const sub = await reg.pushManager.subscribe({
        userVisibleOnly: true,
        // Cast to BufferSource — TS's lib.dom shape uses ArrayBuffer
        // (not ArrayBufferLike) but Uint8Array is fine at runtime.
        applicationServerKey: applicationServerKey.buffer as ArrayBuffer,
      })
      const json = sub.toJSON() as { endpoint?: string; keys?: { p256dh?: string; auth?: string } }
      const p256dh = json.keys?.p256dh ?? arrayBufferToBase64Url(sub.getKey('p256dh'))
      const auth   = json.keys?.auth   ?? arrayBufferToBase64Url(sub.getKey('auth'))
      await api.post('/api/notifications/push/subscribe', {
        endpoint: json.endpoint ?? sub.endpoint,
        keys:     { p256dh, auth },
        user_agent: navigator.userAgent,
      })
      setSubscribed(true)
      qc.invalidateQueries({ queryKey: ['push-subscriptions'] })
      toast.success('Browser notifications enabled.')
    } catch (e) {
      toast.error(diagnoseSubscribeError(e), { duration: 12000 })
    } finally {
      setBusy(false)
    }
  }

  const disableLocal = async () => {
    setBusy(true)
    try {
      const reg = await navigator.serviceWorker.getRegistration(SW_URL)
      const sub = await reg?.pushManager.getSubscription()
      if (sub) await sub.unsubscribe()
      // Server-side cleanup: the matching row will be evicted by the
      // gateway returning 404/410 on the next push, OR explicitly by
      // walking the list and deleting any with this endpoint — for
      // simplicity we let server-side prune happen lazily.
      setSubscribed(false)
      toast.success('This browser will no longer receive pushes.')
    } catch (e) {
      toast.error(`Couldn't unsubscribe: ${(e as Error).message}`)
    } finally {
      setBusy(false)
    }
  }

  const revokeMut = useMutation({
    mutationFn: (id: string) => api.delete(`/api/notifications/push/subscriptions/${id}`),
    onSuccess:  () => {
      qc.invalidateQueries({ queryKey: ['push-subscriptions'] })
      toast.success('Device removed.')
    },
    onError:    (e: unknown) => toast.error(`Revoke failed: ${(e as Error).message}`),
  })

  const testMut = useMutation<{ data: { delivered: number } }, unknown, void>({
    mutationFn: () => api.post('/api/notifications/push/test'),
    onSuccess:  (r) => toast.success(`Sent test push to ${r.data?.delivered ?? '?'} device(s).`),
    onError:    (e: unknown) => toast.error(`Test failed: ${(e as Error).message}`),
  })

  if (!supported) {
    return (
      <div style={{ padding: 12, fontSize: 13, color: 'var(--text-muted)' }}>
        This browser doesn't support the Push API. Use a recent Chrome, Firefox, Edge, or
        mobile Safari (iOS 16.4+ with the app added to Home Screen) to enable notifications.
      </div>
    )
  }

  return (
    <div style={{ padding: 12, display: 'flex', flexDirection: 'column', gap: 16 }}>
      <div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          {subscribed
            ? <Bell    size={16} style={{ color: 'var(--accent)' }} />
            : <BellOff size={16} style={{ color: 'var(--text-muted)' }} />
          }
          <strong style={{ fontSize: 13 }}>
            {subscribed
              ? 'Notifications enabled in this browser'
              : 'Notifications not enabled in this browser'}
          </strong>
        </div>
        <p style={{ margin: '6px 0 10px', fontSize: 12, color: 'var(--text-muted)' }}>
          Get pushed when MIRA messages you (companion check-ins, inbound Signal/Telegram
          messages). Permission is per-browser; you can enable it on multiple devices.
        </p>
        {permission === 'denied' && (
          <div style={{ fontSize: 12, color: '#f87171', marginBottom: 8 }}>
            This browser previously denied permission. Allow notifications for this site in
            your browser settings, then click Enable again.
          </div>
        )}
        {subscribed ? (
          <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
            {/* Primary action on the left; the destructive "turn it off"
                pushed to the far right (marginLeft:auto) so it's well clear
                of the test button and harder to misclick. */}
            <button
              className={btn.btn}
              onClick={() => testMut.mutate()}
              disabled={testMut.isPending}
            >
              <Send size={13} />Send test push
            </button>
            <button
              className={btn.btn}
              onClick={disableLocal}
              disabled={busy}
              style={{ marginLeft: 'auto' }}
            >
              <BellOff size={13} />Disable in this browser
            </button>
          </div>
        ) : (
          <button
            className={btn.btn}
            onClick={enable}
            disabled={busy}
          >
            <Bell size={13} />Enable notifications
          </button>
        )}
      </div>

      <PairDeviceCard />

      <div>
        <strong style={{ fontSize: 13 }}>Registered devices</strong>
        <p style={{ margin: '4px 0 8px', fontSize: 12, color: 'var(--text-muted)' }}>
          Every browser that opted in for your account. Revoke a row to stop pushes to
          that browser even if it's offline.
        </p>
        {subsQuery.isLoading && <div style={{ fontSize: 12 }}>Loading…</div>}
        {subsQuery.data?.subscriptions?.length === 0 && (
          <div style={{ fontSize: 12, color: 'var(--text-muted)' }}>None.</div>
        )}
        <ul style={{ listStyle: 'none', padding: 0, margin: 0 }}>
          {subsQuery.data?.subscriptions?.map((s) => (
            <li key={s.id} style={{
              display: 'flex', alignItems: 'center', gap: 8,
              padding: '6px 8px', borderTop: '1px solid var(--border-subtle)',
              fontSize: 12, fontFamily: 'monospace',
            }}>
              <span style={{ flex: 1, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                {s.kind === 'fcm'
                  ? `📱 ${s.device_name || s.platform || 'Mobile app'}`
                  : (s.user_agent || '(unknown user-agent)')}
              </span>
              <span style={{ color: 'var(--text-muted)' }}>
                {new Date(s.updated_at).toLocaleDateString()}
              </span>
              <button onClick={() => revokeMut.mutate(s.id)}
                      disabled={revokeMut.isPending}
                      title="Revoke this device"
                      className={btn.btn}
                      style={{ fontSize: 11, padding: '3px 8px' }}>
                <Trash2 size={11} />
              </button>
            </li>
          ))}
        </ul>
      </div>
    </div>
  )
}
