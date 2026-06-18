// SPDX-License-Identifier: AGPL-3.0-or-later

// src/pages/IncidentPage.tsx
//
// Tiny landing page that the watchdog "🔍 Analyze with LLM" link
// points at. Auto-fires the analyze POST on mount, navigates to the
// resulting chat conversation as soon as the backend returns the
// conversation_id. While we're waiting, show a spinner + the
// incident summary so the user has context for what's about to land
// in their chat.
import { useEffect, useMemo } from 'react'
import { useParams, useNavigate, Link } from 'react-router-dom'
import { useQuery, useMutation } from '@tanstack/react-query'
import toast from 'react-hot-toast'
import { Loader2, AlertTriangle } from 'lucide-react'
import { watchdogApi } from '@/api/watchdog'

export default function IncidentPage() {
  const { id } = useParams<{ id: string }>()
  const navigate = useNavigate()
  const incidentId = id ?? ''

  const { data: incident, isLoading, error } = useQuery({
    queryKey: ['watchdog-incident', incidentId],
    queryFn:  () => watchdogApi.get(incidentId),
    enabled:  !!incidentId,
    staleTime: 0,
  })

  // The analyze mutation auto-fires on mount unless the incident
  // already has an analysis_status != 'none' (we just go straight
  // to the existing conversation in that case).
  const analyzeMut = useMutation({
    mutationFn: () => watchdogApi.analyze(incidentId),
    onSuccess:  (r) => {
      // Conversation may take a beat to register in the React Query
      // cache; navigate immediately, the chat page handles the
      // race by polling the message stream.
      navigate(`/chat/${r.conversation_id}`, { replace: true })
    },
    onError: (e: unknown) => {
      const m = (e as { response?: { data?: { error?: string } } }).response?.data?.error
              ?? (e as Error).message
      toast.error(m ?? 'Analyze failed')
    },
  })

  // Auto-trigger once incident loads.
  const shouldAutoFire = useMemo(() => {
    if (!incident) return false
    if (analyzeMut.isPending) return false
    if (analyzeMut.isSuccess) return false
    if (analyzeMut.isError)   return false
    // If the incident already has a conversation, jump straight there.
    if (incident.conversation_id) {
      navigate(`/chat/${incident.conversation_id}`, { replace: true })
      return false
    }
    return true
    // navigate is stable; analyzeMut.* and incident drive the decision.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [incident, analyzeMut.isPending, analyzeMut.isSuccess, analyzeMut.isError])

  useEffect(() => {
    if (shouldAutoFire) analyzeMut.mutate()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shouldAutoFire])

  if (!incidentId) {
    return <div style={pageStyle}>Missing incident id.</div>
  }
  if (isLoading) {
    return <div style={pageStyle}><Loader2 size={20} className="spin" /> Loading incident…</div>
  }
  if (error || !incident) {
    return (
      <div style={pageStyle}>
        <AlertTriangle size={20} color="var(--error, #ef4444)" />
        {' '}Couldn't load incident <code>{incidentId.slice(0, 8)}</code>.
        {' '}<Link to="/">Back home</Link>
      </div>
    )
  }

  return (
    <div style={pageStyle}>
      <h2 style={{ marginTop: 0 }}>Watchdog incident</h2>
      <dl style={dlStyle}>
        <dt>Severity</dt><dd>{incident.severity}</dd>
        <dt>Source</dt>  <dd><code>{incident.source}</code></dd>
        <dt>Module</dt>  <dd><code>{incident.module}</code></dd>
        <dt>Time</dt>    <dd>{new Date(incident.created_at * 1000).toLocaleString()}</dd>
        <dt>Message</dt> <dd><pre style={preStyle}>{incident.message}</pre></dd>
      </dl>

      <div style={{ marginTop: 16, display: 'flex', alignItems: 'center', gap: 8 }}>
        <Loader2 size={16} className="spin" />
        <span>
          {analyzeMut.isPending && 'Asking the agent to diagnose…'}
          {analyzeMut.isSuccess && 'Redirecting to conversation…'}
          {!analyzeMut.isPending && !analyzeMut.isSuccess && 'Preparing analysis…'}
        </span>
      </div>
    </div>
  )
}

const pageStyle: React.CSSProperties = {
  maxWidth: 720,
  margin:   '32px auto',
  padding:  '0 16px',
  fontSize: 14,
}

const dlStyle: React.CSSProperties = {
  display: 'grid',
  gridTemplateColumns: '120px 1fr',
  gap: '4px 12px',
  marginTop: 12,
}

const preStyle: React.CSSProperties = {
  background: 'var(--code-bg, rgba(127,127,127,0.08))',
  padding: '8px 10px',
  borderRadius: 6,
  margin: 0,
  whiteSpace: 'pre-wrap',
  wordBreak: 'break-word',
  fontFamily: 'var(--font-mono, monospace)',
  fontSize: 12,
}
