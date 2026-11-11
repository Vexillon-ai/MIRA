// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/turn_registry.rs
//! In-flight turn registry.
//!
//! Every `/api/chat` turn is given a stable `turn_id` and registered here for
//! its lifetime, so a second request can reach it mid-flight to **cancel** it
//! (barge-in, item 1) or **commit/discard** a speculative turn (item 6). The
//! handle carries a [`CancellationToken`] the turn's task cooperatively checks,
//! plus the owning `user_id` and a `created_at` stamp for the ownership (403)
//! and stale-window (409) guards on the cancel route.
//!
//! The registry is process-wide (`Arc<TurnRegistry>` in app state). Entries are
//! removed when the turn's stream ends (success, error, or cancellation), so a
//! lookup miss means "already finished or never existed" → the cancel route
//! treats it as a no-op 404. Nothing here ever blocks the turn itself.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// A commit barrier for a speculative turn. The turn's task
/// awaits [`CommitGate::wait`] before it executes any tool and before it
/// finalises; the commit route releases it by notifying. Idempotent after the
/// first release (so later tools in the same, now-committed turn don't wait),
/// and cancellation-safe (dropping the future when the turn is discarded or
/// restarted is fine — those paths fire the cancel token instead).
pub struct CommitGate {
    notify:   Arc<Notify>,
    released: AtomicBool,
}

impl CommitGate {
    pub fn new(notify: Arc<Notify>) -> Self {
        Self { notify, released: AtomicBool::new(false) }
    }
    /// Block until the turn is committed. Returns at once once released.
    pub async fn wait(&self) {
        if self.released.load(Ordering::Relaxed) { return; }
        self.notify.notified().await;
        self.released.store(true, Ordering::Relaxed);
    }
    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::Relaxed)
    }
}

/// Result of a commit request against the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// The commit (proceed) or restart was accepted.
    Accepted,
    /// No such live turn → 404.
    NotFound,
    /// The turn belongs to another user → 403.
    Forbidden,
    /// The turn is not speculative, so there is nothing to commit → 409.
    NotSpeculative,
}

/// Opaque per-turn identifier. Also sent to the client as the `X-Mira-Turn-Id`
/// response header and the `turn_started` SSE event.
pub type TurnId = String;

/// The payload a cancel request carries. The turn task reads
/// it when it observes cancellation, to decide what (if anything) to persist.
#[derive(Debug, Clone, Default)]
pub struct CancelRequest {
    /// What the client actually delivered to the user before barging in (e.g.
    /// the text TTS had spoken). Persisted as the assistant turn when present;
    /// `None` falls back to whatever the turn had streamed so far.
    pub spoken_text: Option<String>,
    /// Drop the turn entirely — persist nothing (e.g. the user replaced it).
    pub discard: bool,
}

/// Result of a cancel request against the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The turn was found, owned by the caller, within the window, and fired.
    Accepted,
    /// No such live turn (already finished or never existed) → 404.
    NotFound,
    /// The turn belongs to another user → 403.
    Forbidden,
    /// The turn is older than the cancel window → 409.
    Stale,
}

/// Lifecycle state of a registered turn. `Running` is the normal path; the
/// speculative states (item 6) are set once that path lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// A normal turn, streaming to the client.
    Running,
    /// A speculative turn holding its side effects, awaiting commit/discard.
    Speculating,
    /// Committed — side effects released, finishing normally.
    Committed,
    /// Cancelled (barge-in) or discarded.
    Cancelled,
}

/// A handle to one in-flight turn.
pub struct TurnHandle {
    /// Cancellation signal the turn's task checks cooperatively. Firing it asks
    /// the turn to stop at its next checkpoint (wired into the turn).
    pub cancel: CancellationToken,
    /// The authenticated user who owns this turn — the ownership (403) guard.
    pub user_id: String,
    /// The resolved channel for the turn (e.g. `"web"`, `"voice"`).
    pub channel: String,
    /// When the turn was registered — the stale-window (409) guard.
    pub created_at: DateTime<Utc>,
    /// Current lifecycle state.
    pub state: TurnState,
    /// Set by a cancel request, read once by the turn task when it observes
    /// cancellation (so the spoken text / discard flag reach the persistence
    /// site). An implicit disconnect-cancel leaves this `None`.
    pub cancel_request: Option<CancelRequest>,

    // ── speculative ──
    /// Whether this turn was started speculatively.
    pub speculative: bool,
    /// The turn's original user message (to compare against a commit's message).
    pub original_message: String,
    /// Notified on a commit (proceed) to release the turn's [`CommitGate`].
    pub commit_notify: Arc<Notify>,
    /// Set true once committed, so the timeout timer stands down.
    pub committed: Arc<AtomicBool>,
    /// A commit with a changed message drops the new transcript here and fires
    /// [`Self::restart_notify`]; the turn task reads it to restart under the
    /// same id. Separate from the cancel token so the restarted turn keeps a
    /// live token (a fired cancel token can't be reused).
    pub restart_slot: Arc<Mutex<Option<String>>>,
    /// Notified on a changed-message commit to interrupt the turn for a restart.
    pub restart_notify: Arc<Notify>,
    /// The message a commit carried. The handler reads it
    /// at Done to persist the FINAL user message — the corrected transcript on a
    /// restart, else the original. `None` = proceed with no override.
    pub committed_message: Option<String>,
}

impl TurnHandle {
    fn new(user_id: String, channel: String, speculative: bool, original_message: String) -> Self {
        Self {
            cancel: CancellationToken::new(),
            user_id,
            channel,
            created_at: Utc::now(),
            state: if speculative { TurnState::Speculating } else { TurnState::Running },
            cancel_request: None,
            speculative,
            original_message,
            commit_notify: Arc::new(Notify::new()),
            committed: Arc::new(AtomicBool::new(false)),
            restart_slot: Arc::new(Mutex::new(None)),
            restart_notify: Arc::new(Notify::new()),
            committed_message: None,
        }
    }
}

/// The shared handles a speculative turn's task needs: the commit notifier (for
/// its [`CommitGate`]), the restart slot + notifier, and the committed flag
/// (checked by the timeout timer).
pub struct SpeculativeHandles {
    pub commit_notify:  Arc<Notify>,
    pub restart_slot:   Arc<Mutex<Option<String>>>,
    pub restart_notify: Arc<Notify>,
    pub committed:      Arc<AtomicBool>,
}

/// A turn that finished normally, kept briefly so a cancel arriving AFTER
/// `done` can still truncate the stored reply (the
/// voice engine speaks a reply after generation, and the user interrupts
/// mid-speech).
#[derive(Clone)]
pub struct CompletedTurn {
    pub user_id: String,
    pub conversation_id: String,
    pub message_id: String,
    pub completed_at: DateTime<Utc>,
}

/// Process-wide map of live turns + a short-lived map of recently-completed ones.
#[derive(Default)]
pub struct TurnRegistry {
    inner: Mutex<HashMap<TurnId, TurnHandle>>,
    completed: Mutex<HashMap<TurnId, CompletedTurn>>,
}

impl TurnRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new turn and return a clone of its cancellation token (the
    /// turn task holds the clone; firing either clone cancels the turn).
    pub fn register(
        &self,
        turn_id: &str,
        user_id: &str,
        channel: &str,
        speculative: bool,
        original_message: &str,
    ) -> CancellationToken {
        let handle = TurnHandle::new(
            user_id.to_string(), channel.to_string(), speculative, original_message.to_string());
        let token = handle.cancel.clone();
        self.inner
            .lock()
            .unwrap()
            .insert(turn_id.to_string(), handle);
        token
    }

    /// The shared speculative handles for a live turn. `None`
    /// if the turn isn't registered.
    pub fn speculative_handles(&self, turn_id: &str) -> Option<SpeculativeHandles> {
        self.inner.lock().unwrap().get(turn_id).map(|h| SpeculativeHandles {
            commit_notify:  Arc::clone(&h.commit_notify),
            restart_slot:   Arc::clone(&h.restart_slot),
            restart_notify: Arc::clone(&h.restart_notify),
            committed:      Arc::clone(&h.committed),
        })
    }

    /// Commit a speculative turn. Runs the ownership (403) +
    /// speculative (409) guards. An unchanged (or absent) message proceeds:
    /// marks committed and notifies, releasing the turn's gate. A changed
    /// message records the new transcript and fires the cancel token so the turn
    /// restarts under the same id. Idempotent-ish: a repeat proceed re-notifies
    /// harmlessly.
    pub fn commit(&self, turn_id: &str, caller_id: &str, message: Option<String>) -> CommitOutcome {
        let mut map = self.inner.lock().unwrap();
        let Some(h) = map.get_mut(turn_id) else { return CommitOutcome::NotFound };
        if h.user_id != caller_id {
            return CommitOutcome::Forbidden;
        }
        if !h.speculative {
            return CommitOutcome::NotSpeculative;
        }
        // Remember the committed message so the handler persists the FINAL user
        // message at Done (the corrected transcript on a restart, else original).
        h.committed_message = message.clone();
        let changed = message.as_deref().map(|m| m != h.original_message).unwrap_or(false);
        if changed {
            // Restart: hand the turn the new transcript and signal it — WITHOUT
            // firing the cancel token, so the restarted turn keeps a live token.
            *h.restart_slot.lock().unwrap() = message;
            h.restart_notify.notify_one();
        } else {
            h.committed.store(true, Ordering::Relaxed);
            h.state = TurnState::Committed;
            h.commit_notify.notify_one();
        }
        CommitOutcome::Accepted
    }

    /// Auto-discard a speculative turn on timeout, unless it has already been
    /// committed. Fires the cancel token with a discard payload.
    pub fn discard_on_timeout(&self, turn_id: &str) {
        let mut map = self.inner.lock().unwrap();
        if let Some(h) = map.get_mut(turn_id) {
            if h.committed.load(Ordering::Relaxed) { return; }
            h.cancel_request = Some(CancelRequest { spoken_text: None, discard: true });
            h.state = TurnState::Cancelled;
            h.cancel.cancel();
        }
    }

    /// Remove a turn (called when its stream ends, for any reason). Idempotent.
    pub fn remove(&self, turn_id: &str) {
        self.inner.lock().unwrap().remove(turn_id);
    }

    /// Number of live turns — used by tests and diagnostics.
    pub fn active_count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Is this turn currently registered?
    pub fn contains(&self, turn_id: &str) -> bool {
        self.inner.lock().unwrap().contains_key(turn_id)
    }

    /// The owning user + age of a live turn, if present. Lets the cancel route
    /// run its 403/409 guards without exposing the handle's internals.
    pub fn owner_and_age(&self, turn_id: &str) -> Option<(String, DateTime<Utc>)> {
        self.inner
            .lock()
            .unwrap()
            .get(turn_id)
            .map(|h| (h.user_id.clone(), h.created_at))
    }

    /// Request cancellation of a live turn. Runs the ownership
    /// (403) and stale-window (409) guards, stashes the payload for the turn
    /// task to read, and fires the cancellation token. Idempotent: a repeat
    /// request on a still-live turn re-fires harmlessly and returns `Accepted`;
    /// once the turn has finished and been removed, it returns `NotFound`.
    pub fn request_cancel(
        &self,
        turn_id: &str,
        caller_id: &str,
        window_secs: u64,
        req: CancelRequest,
    ) -> CancelOutcome {
        let mut map = self.inner.lock().unwrap();
        let Some(h) = map.get_mut(turn_id) else { return CancelOutcome::NotFound };
        if h.user_id != caller_id {
            return CancelOutcome::Forbidden;
        }
        // Age guard — a cancel for a turn older than the window is refused; the
        // turn has almost certainly already finished.
        let age_secs = (Utc::now() - h.created_at).num_seconds();
        if age_secs > window_secs as i64 {
            return CancelOutcome::Stale;
        }
        h.cancel_request = Some(req);
        h.state = TurnState::Cancelled;
        h.cancel.cancel();
        CancelOutcome::Accepted
    }

    /// Take the cancel payload for a turn, if one was set (consumes it). Called
    /// by the turn task at the persistence site. `None` for an implicit
    /// disconnect-cancel (no payload) or a turn that wasn't cancelled.
    pub fn take_cancel_request(&self, turn_id: &str) -> Option<CancelRequest> {
        self.inner
            .lock()
            .unwrap()
            .get_mut(turn_id)
            .and_then(|h| h.cancel_request.take())
    }

    /// The message a commit carried for a turn, if any.
    /// Read by the handler at Done to persist the final user message.
    pub fn committed_message(&self, turn_id: &str) -> Option<String> {
        self.inner.lock().unwrap().get(turn_id).and_then(|h| h.committed_message.clone())
    }

    /// Record a turn that finished normally, so a cancel arriving after `done`
    /// can still truncate its stored reply. Prunes
    /// entries older than `retain_secs` on each insert so the map can't grow.
    pub fn record_completed(
        &self,
        turn_id: &str,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
        retain_secs: u64,
    ) {
        let now = Utc::now();
        let mut map = self.completed.lock().unwrap();
        map.retain(|_, c| (now - c.completed_at).num_seconds() <= retain_secs as i64);
        map.insert(turn_id.to_string(), CompletedTurn {
            user_id: user_id.to_string(),
            conversation_id: conversation_id.to_string(),
            message_id: message_id.to_string(),
            completed_at: now,
        });
    }

    /// Take a recently-completed turn for a post-`done` cancel, if it exists, is
    /// owned by `caller_id`, and finished within `window_secs`. Consumes it (so a
    /// repeat cancel is a no-op 404).
    pub fn take_completed_for_cancel(
        &self,
        turn_id: &str,
        caller_id: &str,
        window_secs: u64,
    ) -> Option<CompletedTurn> {
        let mut map = self.completed.lock().unwrap();
        let c = map.get(turn_id)?;
        if c.user_id != caller_id { return None; }
        if (Utc::now() - c.completed_at).num_seconds() > window_secs as i64 { return None; }
        map.remove(turn_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_remove_roundtrip() {
        let reg = TurnRegistry::new();
        assert_eq!(reg.active_count(), 0);
        let tok = reg.register("t1", "alice", "voice", false, "hi");
        assert!(reg.contains("t1"));
        assert_eq!(reg.active_count(), 1);
        assert!(!tok.is_cancelled());

        // Owner + age readable for the 403/409 guards.
        let (owner, _age) = reg.owner_and_age("t1").expect("live turn");
        assert_eq!(owner, "alice");

        reg.remove("t1");
        assert!(!reg.contains("t1"));
        assert_eq!(reg.active_count(), 0);
        // Remove is idempotent.
        reg.remove("t1");
    }

    #[test]
    fn firing_the_returned_token_cancels() {
        let reg = TurnRegistry::new();
        let tok = reg.register("t2", "bob", "web", false, "hi");
        tok.cancel();
        assert!(tok.is_cancelled());
    }

    #[test]
    fn unknown_turn_has_no_owner() {
        let reg = TurnRegistry::new();
        assert!(reg.owner_and_age("nope").is_none());
    }

    #[test]
    fn request_cancel_runs_guards_and_fires() {
        let reg = TurnRegistry::new();
        let tok = reg.register("t", "alice", "voice", false, "hi");

        // Wrong user → 403, nothing fired.
        assert_eq!(
            reg.request_cancel("t", "mallory", 600, CancelRequest::default()),
            CancelOutcome::Forbidden,
        );
        assert!(!tok.is_cancelled());

        // Unknown turn → 404.
        assert_eq!(
            reg.request_cancel("ghost", "alice", 600, CancelRequest::default()),
            CancelOutcome::NotFound,
        );

        // Owner within window → accepted, token fired, payload readable once.
        let req = CancelRequest { spoken_text: Some("hi the".into()), discard: false };
        assert_eq!(reg.request_cancel("t", "alice", 600, req), CancelOutcome::Accepted);
        assert!(tok.is_cancelled());
        let taken = reg.take_cancel_request("t").expect("payload set");
        assert_eq!(taken.spoken_text.as_deref(), Some("hi the"));
        // Consumed — a second take is empty.
        assert!(reg.take_cancel_request("t").is_none());

        // Idempotent: a repeat cancel on the still-live turn is accepted.
        assert_eq!(
            reg.request_cancel("t", "alice", 600, CancelRequest::default()),
            CancelOutcome::Accepted,
        );
    }

    #[test]
    fn request_cancel_rejects_a_stale_turn() {
        let reg = TurnRegistry::new();
        let tok = reg.register("old", "alice", "voice", false, "hi");
        // Backdate the turn past the window.
        {
            let mut map = reg.inner.lock().unwrap();
            map.get_mut("old").unwrap().created_at =
                Utc::now() - chrono::Duration::seconds(1000);
        }
        assert_eq!(
            reg.request_cancel("old", "alice", 600, CancelRequest::default()),
            CancelOutcome::Stale,
        );
        assert!(!tok.is_cancelled(), "a stale turn is not cancelled");
    }

    // ── speculative commit ──

    #[test]
    fn commit_proceed_marks_committed() {
        let reg = TurnRegistry::new();
        reg.register("t", "alice", "voice", true, "hello");
        let h = reg.speculative_handles("t").unwrap();
        assert!(!h.committed.load(Ordering::Relaxed));
        assert_eq!(reg.commit("t", "alice", None), CommitOutcome::Accepted);
        assert!(h.committed.load(Ordering::Relaxed), "proceed marks committed");
        // Unchanged message is also a proceed.
        assert_eq!(reg.commit("t", "alice", Some("hello".into())), CommitOutcome::Accepted);
    }

    #[test]
    fn commit_changed_message_restarts() {
        let reg = TurnRegistry::new();
        let tok = reg.register("t", "alice", "voice", true, "hello");
        let h = reg.speculative_handles("t").unwrap();
        assert_eq!(reg.commit("t", "alice", Some("goodbye".into())), CommitOutcome::Accepted);
        // Restart does NOT fire the cancel token (the restarted turn reuses it).
        assert!(!tok.is_cancelled(), "a changed-message commit must not fire cancel");
        assert_eq!(h.restart_slot.lock().unwrap().as_deref(), Some("goodbye"));
    }

    #[test]
    fn commit_guards() {
        let reg = TurnRegistry::new();
        reg.register("t", "alice", "voice", true, "hi");
        assert_eq!(reg.commit("t", "mallory", None), CommitOutcome::Forbidden);
        assert_eq!(reg.commit("ghost", "alice", None), CommitOutcome::NotFound);
        reg.register("n", "alice", "voice", false, "hi");
        assert_eq!(reg.commit("n", "alice", None), CommitOutcome::NotSpeculative);
    }

    #[test]
    fn discard_on_timeout_fires_cancel_unless_committed() {
        let reg = TurnRegistry::new();
        let tok = reg.register("t", "alice", "voice", true, "hi");
        reg.discard_on_timeout("t");
        assert!(tok.is_cancelled());

        // A committed turn is NOT discarded by a late timeout.
        let tok2 = reg.register("c", "alice", "voice", true, "hi");
        reg.commit("c", "alice", None);
        reg.discard_on_timeout("c");
        assert!(!tok2.is_cancelled(), "a committed turn survives the timeout");
    }

    // a recently-completed turn can be found by a
    // post-`done` cancel (owner + window), and is consumed once.
    #[test]
    fn completed_turn_found_by_owner_within_window() {
        let reg = TurnRegistry::new();
        reg.record_completed("t", "alice", "conv1", "msg1", 600);
        // Wrong user → None.
        assert!(reg.take_completed_for_cancel("t", "mallory", 600).is_none());
        // Owner within window → Some, carrying the message id to truncate.
        let c = reg.take_completed_for_cancel("t", "alice", 600).expect("found");
        assert_eq!(c.message_id, "msg1");
        assert_eq!(c.conversation_id, "conv1");
        // Consumed — a second take is None.
        assert!(reg.take_completed_for_cancel("t", "alice", 600).is_none());
    }

    #[test]
    fn completed_turn_outside_window_is_not_cancellable() {
        let reg = TurnRegistry::new();
        reg.record_completed("t", "alice", "conv1", "msg1", 100_000);
        // Backdate past the window → the post-done cancel is refused.
        {
            let mut m = reg.completed.lock().unwrap();
            m.get_mut("t").unwrap().completed_at = Utc::now() - chrono::Duration::seconds(1000);
        }
        assert!(reg.take_completed_for_cancel("t", "alice", 600).is_none());
    }

    #[tokio::test]
    async fn commit_gate_blocks_until_notified() {
        let notify = Arc::new(Notify::new());
        let gate = Arc::new(CommitGate::new(Arc::clone(&notify)));
        assert!(!gate.is_released());
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move { g2.wait().await; });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!handle.is_finished(), "gate holds until committed");
        notify.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), handle).await.unwrap().unwrap();
        assert!(gate.is_released(), "released after commit");
        // Idempotent: a released gate returns at once.
        gate.wait().await;
    }
}
