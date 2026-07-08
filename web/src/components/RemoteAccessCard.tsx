// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/RemoteAccessCard.tsx
//
// Admin-only "Remote access" status + guided setup (Tailscale-first). Shows
// whether Tailscale is detected / up / serving HTTPS, the effective remote URL
// baked into the pairing QR, and copy-paste setup commands. Read-only status;
// setting `remote_url` goes through the normal Server settings save alongside
// this card. Backed by GET /api/admin/remote-access (admin-gated).

import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { RefreshCw, Check, X, Copy } from 'lucide-react'
import toast from 'react-hot-toast'
import { api } from '@/api/client'
import styles from './RemoteAccessCard.module.css'

interface TailscaleStatus {
  installed:     boolean
  up:            boolean
  dns_name:      string | null
  tailscale_ips: string[]
  serving_https: boolean
  derived_url:   string | null
  hint:          string | null
}
interface RemoteStatus {
  effective_url:      string | null
  source:             'config' | 'tailscale' | 'none'
  configured_url:     string | null
  configured_invalid: boolean
  tailscale:          TailscaleStatus
  mira_port:          number
}
interface SetupGuide {
  install:       string[]
  enable:        string[]
  console_notes: string[]
  windows_note:  string
  docs:          string
}
interface RemoteAccessResponse {
  status: RemoteStatus
  setup:  SetupGuide
}

function copy(text: string) {
  navigator.clipboard?.writeText(text).then(
    () => toast.success('Copied'),
    () => toast.error('Copy failed'),
  )
}

function Yn({ v, label }: { v: boolean; label: string }) {
  return (
    <span className={v ? styles.ok : styles.no}>
      {v ? <Check size={12} /> : <X size={12} />} {label}
    </span>
  )
}

export default function RemoteAccessCard() {
  // Bumping `probe` re-keys the query with ?redetect=true for a fresh scan.
  const [probe, setProbe] = useState(0)
  const q = useQuery<RemoteAccessResponse>({
    queryKey: ['remote-access', probe],
    queryFn:  () => api.get(`/api/admin/remote-access${probe ? '?redetect=true' : ''}`).then((r) => r.data),
    refetchOnWindowFocus: false,
  })

  const d  = q.data
  const ts = d?.status.tailscale

  return (
    <div className={styles.card}>
      {q.isLoading ? (
        <div className={styles.dim}>Checking remote access…</div>
      ) : (
        <>
          <div className={styles.row}>
            {d?.status.effective_url ? (
              <>Pairing hands out remote URL <b>{d.status.effective_url}</b>{' '}
                <span className={styles.dim}>· from {d.status.source}</span></>
            ) : (
              <>No remote URL yet — set one below, or enable Tailscale to auto-detect it.</>
            )}
          </div>

          {d?.status.configured_invalid && (
            <div className={styles.warn}>
              The configured remote URL isn't a valid http/https URL — it's being ignored.
            </div>
          )}

          <div className={styles.badges}>
            <Yn v={!!ts?.installed} label="Tailscale installed" />
            <Yn v={!!ts?.up}        label="up" />
            <Yn v={!!ts?.serving_https} label="serving HTTPS" />
          </div>

          {ts?.dns_name && <div className={styles.dim}>MagicDNS name: {ts.dns_name}</div>}
          {ts?.tailscale_ips?.length ? (
            <div className={styles.dim}>Tailscale IPs: {ts.tailscale_ips.join(', ')}</div>
          ) : null}
          {ts?.hint && <div className={styles.warn}>{ts.hint}</div>}

          {d && !ts?.serving_https && (
            <div className={styles.setup}>
              <div className={styles.dim}>Set up Tailscale (run on the server, then reload):</div>
              {[...d.setup.install, ...d.setup.enable].map((cmd) => (
                <div key={cmd} className={styles.codeRow}>
                  <code className={styles.code}>{cmd}</code>
                  <button className={styles.copy} onClick={() => copy(cmd)} title="Copy">
                    <Copy size={12} />
                  </button>
                </div>
              ))}
              {d.setup.console_notes.map((n) => (
                <div key={n} className={styles.note}>{n}</div>
              ))}
              <a className={styles.link} href={d.setup.docs} target="_blank" rel="noreferrer">
                Tailscale serve docs ↗
              </a>
            </div>
          )}

          <button className={styles.btn} onClick={() => setProbe((n) => n + 1)} disabled={q.isFetching}>
            <RefreshCw size={13} /> {q.isFetching ? 'Detecting…' : 'Re-detect'}
          </button>
        </>
      )}
    </div>
  )
}
