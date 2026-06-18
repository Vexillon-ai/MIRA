// SPDX-License-Identifier: AGPL-3.0-or-later

//! In-process transport for manager↔worker `Envelope`s (slice B2).
//!
//! For v1 every Agent runs in the same process, so the wire is a pair
//! of Tokio mpsc channels — one for each direction. The `AgentChannel`
//! abstracts request/response correlation so callers can `await` a
//! reply without hand-managing pending tables.
//!
//! Cross-process / cross-host transport (NATS, Redis, gRPC) would
//! re-implement `AgentChannel` over a different wire without changing
//! anything in `agent/protocol.rs` or in the agent loop. The
//! `Envelope` is already JSON-serialisable for that future move.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::agent::protocol::{Envelope, EnvelopeId, Event, Request, Response};

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("the peer dropped its end of the channel")]
    PeerDropped,
    #[error("the peer never replied to request {id}")]
    NoReply { id: EnvelopeId },
}

/// Send-side half of an `AgentChannel`. Cloneable, sharable across
/// tasks, supports `request()` / `reply()` / `send_event()`. Held by
/// the supervisor (per worker, for issuing Interrupt/Pause/Resume
/// asynchronously) and by anyone who needs to talk to a peer without
/// holding the unique receiver.
#[derive(Clone)]
pub struct ChannelSender {
    out:        mpsc::UnboundedSender<Envelope>,
    pending:    Arc<Mutex<HashMap<EnvelopeId, oneshot::Sender<Response>>>>,
    next_id:    Arc<AtomicU64>,
    /// Set to false when the router task observes the peer has dropped.
    peer_alive: Arc<AtomicBool>,
}

impl ChannelSender {
    /// Send a request and await the peer's response. The future
    /// resolves with `ChannelError::PeerDropped` if the peer goes away
    /// before replying.
    pub async fn request(&self, req: Request) -> Result<Response, ChannelError> {
        if !self.peer_alive.load(Ordering::Acquire) {
            return Err(ChannelError::PeerDropped);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);

        if self.out.send(Envelope::Request { id, payload: req }).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(ChannelError::PeerDropped);
        }

        rx.await.map_err(|_| ChannelError::PeerDropped)
    }

    /// Reply to a Request the caller previously received via `recv`.
    /// `id` must be the same id `recv` returned with the Request.
    pub fn reply(&self, id: EnvelopeId, response: Response) -> Result<(), ChannelError> {
        self.out
            .send(Envelope::Response { id, payload: response })
            .map_err(|_| ChannelError::PeerDropped)
    }

    /// Send a fire-and-forget event to the peer.
    pub fn send_event(&self, event: Event) -> Result<(), ChannelError> {
        self.out
            .send(Envelope::Event { payload: event })
            .map_err(|_| ChannelError::PeerDropped)
    }

    pub fn is_peer_alive(&self) -> bool {
        self.peer_alive.load(Ordering::Acquire)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().expect("pending lock").len()
    }
}

/// One half of a manager↔worker connection. Holds the unique receiver
/// + a `ChannelSender` for outbound traffic. Cloning the sender via
/// `sender()` lets other tasks issue requests concurrently with the
/// channel's owner calling `recv()`.
pub struct AgentChannel {
    sender:     ChannelSender,
    incoming:   mpsc::UnboundedReceiver<Incoming>,
}

/// What the caller pulls off the channel via `recv()`. Pure responses
/// to outstanding requests are routed to their oneshot waiters and
/// never appear here — the caller only sees inbound work (Requests
/// they need to answer) and Events.
#[derive(Debug)]
pub enum Incoming {
    Request { id: EnvelopeId, payload: Request },
    Event   { payload: Event },
}

impl AgentChannel {
    /// Build a connected pair. Either handle can issue requests and
    /// receive incoming work — the labels "manager" / "worker" are
    /// just convention; the transport itself is symmetric.
    pub fn pair() -> (AgentChannel, AgentChannel) {
        let (a_to_b_tx, a_to_b_rx) = mpsc::unbounded_channel();
        let (b_to_a_tx, b_to_a_rx) = mpsc::unbounded_channel();
        let a = AgentChannel::new(a_to_b_tx, b_to_a_rx);
        let b = AgentChannel::new(b_to_a_tx, a_to_b_rx);
        (a, b)
    }

    fn new(
        out: mpsc::UnboundedSender<Envelope>,
        mut inbox: mpsc::UnboundedReceiver<Envelope>,
    ) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let pending = Arc::new(Mutex::new(HashMap::<EnvelopeId, oneshot::Sender<Response>>::new()));
        let peer_alive = Arc::new(AtomicBool::new(true));

        let pending_router    = pending.clone();
        let peer_alive_router = peer_alive.clone();

        // Router task: drains the inbox, fulfils pending response
        // promises, forwards Requests + Events to the user-visible
        // `incoming_rx`. Exits when the inbox closes (peer dropped) OR
        // when the user-facing receiver is dropped (no one to forward
        // requests/events to).
        tokio::spawn(async move {
            while let Some(env) = inbox.recv().await {
                match env {
                    Envelope::Response { id, payload } => {
                        if let Some(tx) = pending_router.lock().expect("pending lock").remove(&id) {
                            // Drop is fine if the awaiter has already gone away.
                            let _ = tx.send(payload);
                        }
                    }
                    Envelope::Request { id, payload } => {
                        if incoming_tx.send(Incoming::Request { id, payload }).is_err() {
                            break;
                        }
                    }
                    Envelope::Event { payload } => {
                        if incoming_tx.send(Incoming::Event { payload }).is_err() {
                            break;
                        }
                    }
                }
            }
            // Peer dropped (or local handle gone). Mark dead and fail
            // all in-flight requests so awaiters don't hang forever.
            peer_alive_router.store(false, Ordering::Release);
            pending_router.lock().expect("pending lock").clear();
        });

        Self {
            sender: ChannelSender {
                out,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
                peer_alive,
            },
            incoming: incoming_rx,
        }
    }

    /// Convenience proxies — these forward to the inner sender.
    pub async fn request(&self, req: Request) -> Result<Response, ChannelError> {
        self.sender.request(req).await
    }
    pub fn reply(&self, id: EnvelopeId, response: Response) -> Result<(), ChannelError> {
        self.sender.reply(id, response)
    }
    pub fn send_event(&self, event: Event) -> Result<(), ChannelError> {
        self.sender.send_event(event)
    }
    pub fn pending_count(&self) -> usize { self.sender.pending_count() }

    /// Get a cheaply-clonable, send-only handle for asynchronous
    /// callers (supervisor's interrupt path, executors needing
    /// bidirectional comms, etc).
    pub fn sender(&self) -> ChannelSender { self.sender.clone() }

    /// Block until the next inbound `Incoming` (Request or Event)
    /// arrives. `None` when the peer drops.
    pub async fn recv(&mut self) -> Option<Incoming> {
        self.incoming.recv().await
    }

    /// Get a cheaply-clonable handle that can only emit fire-and-forget
    /// `Event`s. Used by the worker runtime to give an executor a way
    /// to publish progress without handing it the full channel (which
    /// would let it issue requests and conflict with the runtime's
    /// own request/response flow).
    pub fn event_sender(&self) -> EventSender {
        EventSender(self.sender.out.clone())
    }
}

/// Send-only handle for worker executors. Cloneable; safe to share
/// across tasks; cannot issue requests or replies.
#[derive(Clone)]
pub struct EventSender(mpsc::UnboundedSender<Envelope>);

impl EventSender {
    pub fn send(&self, event: Event) -> Result<(), ChannelError> {
        self.0
            .send(Envelope::Event { payload: event })
            .map_err(|_| ChannelError::PeerDropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::protocol::{InterruptReason, Response};
    use std::time::Duration;
    use tokio::time::timeout;

    fn t(ms: u64) -> Duration { Duration::from_millis(ms) }

    #[tokio::test]
    async fn request_and_response_correlate() {
        let (mgr, mut worker) = AgentChannel::pair();

        // Worker side: answer the first request.
        let worker_task = tokio::spawn(async move {
            let inc = worker.recv().await.unwrap();
            match inc {
                Incoming::Request { id, payload: Request::Pause } => {
                    worker.reply(id, Response::Ack).unwrap();
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let resp = timeout(t(500), mgr.request(Request::Pause)).await.unwrap().unwrap();
        assert!(matches!(resp, Response::Ack));
        worker_task.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_requests_correlate_correctly() {
        let (mgr, mut worker) = AgentChannel::pair();

        // Worker side: receive several requests, reply OUT OF ORDER on
        // purpose — the manager must still match each reply to its
        // original request via the envelope id.
        tokio::spawn(async move {
            let mut buffered = Vec::new();
            while buffered.len() < 3 {
                if let Some(Incoming::Request { id, payload }) = worker.recv().await {
                    buffered.push((id, payload));
                }
            }
            // reply order: 2, 0, 1
            for idx in [2usize, 0, 1] {
                let (id, _payload) = &buffered[idx];
                worker.reply(*id, Response::Ack).unwrap();
            }
        });

        let r1 = mgr.request(Request::Pause);
        let r2 = mgr.request(Request::Resume);
        let r3 = mgr.request(Request::Interrupt { reason: InterruptReason::User });
        let (a, b, c) = tokio::join!(r1, r2, r3);
        assert!(matches!(a, Ok(Response::Ack)));
        assert!(matches!(b, Ok(Response::Ack)));
        assert!(matches!(c, Ok(Response::Ack)));
    }

    #[tokio::test]
    async fn events_reach_the_other_side() {
        let (mgr, mut worker) = AgentChannel::pair();

        let mgr_task = tokio::spawn(async move {
            mgr.send_event(Event::Progress {
                step_summary: "step 1".into(),
                percent_done: Some(0.25),
                llm_spend_usd: 0.05,
            }).unwrap();
            // Manager intentionally never sends a request, so the
            // worker should see an Event and nothing else.
        });

        let inc = timeout(t(500), worker.recv()).await.unwrap().unwrap();
        assert!(matches!(inc, Incoming::Event { payload: Event::Progress { .. } }));
        mgr_task.await.unwrap();
    }

    #[tokio::test]
    async fn peer_drop_fails_outstanding_request() {
        let (mgr, worker) = AgentChannel::pair();
        drop(worker); // peer goes away before answering

        // Give the router task a moment to notice.
        tokio::task::yield_now().await;

        let err = timeout(t(500), mgr.request(Request::Pause)).await.unwrap();
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn unsolicited_response_is_silently_dropped() {
        // Reply with no matching pending request → no panic, no leak,
        // peer just keeps running.
        let (mgr, worker) = AgentChannel::pair();
        worker.reply(999, Response::Ack).unwrap();

        // mgr.recv() is for Requests/Events — bare Responses get
        // routed (or silently dropped). Confirm by sending a real
        // Event after and seeing it come through promptly.
        worker.send_event(Event::Progress {
            step_summary: "ping".into(),
            percent_done: None,
            llm_spend_usd: 0.0,
        }).unwrap();

        let mut mgr = mgr;
        let inc = timeout(t(500), mgr.recv()).await.unwrap().unwrap();
        assert!(matches!(inc, Incoming::Event { .. }));
    }

    #[tokio::test]
    async fn pending_count_decrements_after_reply() {
        let (mgr, mut worker) = AgentChannel::pair();

        let mgr_clone_pending = mgr.sender.pending.clone();
        tokio::spawn(async move {
            // Worker that answers everything Ack.
            while let Some(Incoming::Request { id, .. }) = worker.recv().await {
                worker.reply(id, Response::Ack).unwrap();
            }
        });

        assert_eq!(mgr.pending_count(), 0);
        let _ = mgr.request(Request::Pause).await.unwrap();
        // After the reply has been delivered the pending entry is gone.
        assert_eq!(mgr_clone_pending.lock().unwrap().len(), 0);
    }
}
