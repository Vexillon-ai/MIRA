// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/PairDeviceCard.tsx
//
// "Pair mobile device" affordance (Settings → Notifications). Calls
// POST /api/auth/pairing/start, renders the returned payload as a QR code
// the MIRA mobile app scans to configure the server URL *and* sign in with
// no password re-entry. Polls /status to flip to "✓ Paired" once the phone
// claims it, with a live countdown to expiry and a regenerate action.
//
// The QR encodes the JSON the app expects (see MIRA-SERVER-CHANGES §2.3):
//   { v, type:"mira_pairing", base_url, remote_url?, server_name,
//     pairing_id, pairing_secret, expires_at }
// `base_url` is the LAN / currently-browsed address; `remote_url` is the
// optional "away" endpoint (Tailscale / tunnel). It's emitted only when the
// server knows one, and `v` is bumped to 2 only in that case — so a server
// with no remote URL produces the exact v1 payload as before. A v2-aware app
// stores both endpoints and auto-selects whichever is reachable; older builds
// ignore the new field.
// The pairing secret is single-use + short-lived; it only ever lives in
// this authenticated browser and the QR it renders.

import { useEffect, useRef, useState, useCallback } from 'react'
import QRCode from 'qrcode'
import { Smartphone, RefreshCw, CheckCircle2 } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import btn from './actionButton.module.css'

interface PairingStart {
  pairing_id:     string
  pairing_secret: string
  base_url:       string
  /** Optional "away" endpoint (Tailscale / tunnel), present only when known. */
  remote_url?:    string
  server_name:    string
  expires_at:     number
}

interface PairingStatus {
  status:      'pending' | 'claimed' | 'expired'
  device_name: string | null
}

export default function PairDeviceCard() {
  const [pairing, setPairing] = useState<PairingStart | null>(null)
  const [status, setStatus]   = useState<PairingStatus['status']>('pending')
  const [claimedName, setClaimedName] = useState<string | null>(null)
  const [remaining, setRemaining] = useState<number>(0)
  const [busy, setBusy] = useState(false)
  const canvasRef = useRef<HTMLCanvasElement | null>(null)

  const start = useCallback(async () => {
    setBusy(true)
    try {
      const r = await api.post<PairingStart>('/api/auth/pairing/start', {
        device_name: 'Mobile device',
      })
      setPairing(r.data)
      setStatus('pending')
      setClaimedName(null)
    } catch (e) {
      toast.error(`Couldn't start pairing: ${(e as Error).message}`)
    } finally {
      setBusy(false)
    }
  }, [])

  // Render the QR whenever a fresh pairing arrives.
  useEffect(() => {
    if (!pairing || !canvasRef.current) return
    // Only the presence of an away endpoint changes the payload: emit
    // `remote_url` + bump to v2 when we have one, else keep the exact v1 shape
    // so a server with no remote URL produces a byte-identical QR to before.
    const payload = JSON.stringify({
      v: pairing.remote_url ? 2 : 1,
      type: 'mira_pairing',
      base_url:       pairing.base_url,
      ...(pairing.remote_url ? { remote_url: pairing.remote_url } : {}),
      server_name:    pairing.server_name,
      pairing_id:     pairing.pairing_id,
      pairing_secret: pairing.pairing_secret,
      expires_at:     pairing.expires_at,
    })
    QRCode.toCanvas(canvasRef.current, payload, { width: 220, margin: 1 }, (err) => {
      if (err) toast.error('Could not render QR code.')
    })
  }, [pairing])

  // Countdown + poll status while a pairing is live and unclaimed.
  useEffect(() => {
    if (!pairing || status !== 'pending') return
    let alive = true

    const tick = () => {
      const secs = Math.max(0, Math.round((pairing.expires_at - Date.now()) / 1000))
      if (alive) setRemaining(secs)
      if (secs === 0 && alive) setStatus('expired')
    }
    tick()
    const countdown = setInterval(tick, 1000)

    const poll = setInterval(async () => {
      try {
        const r = await api.get<PairingStatus>(`/api/auth/pairing/${pairing.pairing_id}/status`)
        if (!alive) return
        if (r.data.status === 'claimed') {
          setStatus('claimed')
          setClaimedName(r.data.device_name)
        } else if (r.data.status === 'expired') {
          setStatus('expired')
        }
      } catch { /* transient — keep polling until expiry */ }
    }, 2500)

    return () => { alive = false; clearInterval(countdown); clearInterval(poll) }
  }, [pairing, status])

  return (
    <div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <Smartphone size={16} style={{ color: 'var(--accent)' }} />
        <strong style={{ fontSize: 13 }}>Pair a mobile device</strong>
      </div>
      <p style={{ margin: '4px 0 8px', fontSize: 12, color: 'var(--text-muted)' }}>
        Open the MIRA app on your phone, scan this code, and it configures the server and
        signs you in — no password to type. The code is single-use and expires.
      </p>

      {!pairing && (
        <button className={btn.btn} onClick={start} disabled={busy}>
          <Smartphone size={13} />Show pairing code
        </button>
      )}

      {pairing && (
        <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-start', gap: 8 }}>
          {status === 'claimed' ? (
            <div style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 13, color: 'var(--accent)' }}>
              <CheckCircle2 size={18} />
              Paired{claimedName ? ` with ${claimedName}` : ''}.
            </div>
          ) : (
            <>
              {/* Hide the canvas once expired so a stale (useless) code isn't scannable. */}
              <canvas
                ref={canvasRef}
                style={{ borderRadius: 8, background: '#fff', padding: 8,
                         opacity: status === 'expired' ? 0.25 : 1 }}
              />
              <div style={{ fontSize: 12, color: 'var(--text-muted)' }}>
                {status === 'expired'
                  ? 'This code expired.'
                  : `Expires in ${remaining}s. Scan it in the MIRA app.`}
              </div>
            </>
          )}
          <button className={btn.btn} onClick={start} disabled={busy}>
            <RefreshCw size={13} />{status === 'claimed' ? 'Pair another device' : 'Regenerate code'}
          </button>
        </div>
      )}
    </div>
  )
}
