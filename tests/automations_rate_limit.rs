// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/automations_rate_limit.rs
//! Integration tests for the channel-message rate limiter.
//!
//! These tests drive the dispatcher end-to-end (real store, real
//! HistoryStore, real NotificationBus) so we cover both the in-memory
//! limiter and the way it's wired into `Dispatcher::run_channel_message`.

use std::sync::Arc;

use chrono::Utc;
use mira::automations::{
    Action, AutomationsStore, ChannelRateLimiter, Dispatcher, RateDecision, RunOutcome,
};
use mira::automations::dispatch::Activation;
use mira::automations::heartbeats::{HeartbeatContext, HeartbeatRegistry};
use mira::history::HistoryStore;
use mira::notifications::NotificationBus;
use tempfile::TempDir;

// ── Fixture helpers ──────────────────────────────────────────────────────────

fn open_store(dir: &TempDir) -> Arc<AutomationsStore> {
    Arc::new(AutomationsStore::open(&dir.path().join("automations.db")).unwrap())
}

fn make_dispatcher(
    dir:     &TempDir,
    store:   Arc<AutomationsStore>,
    limiter: Option<Arc<ChannelRateLimiter>>,
) -> Arc<Dispatcher> {
    let history = Arc::new(
        HistoryStore::open(&dir.path().join("history.db")).unwrap()
    );
    let bus = Arc::new(NotificationBus::new());
    Arc::new(Dispatcher {
        heartbeats:    Arc::new(HeartbeatRegistry::new()),
        ctx:           Arc::new(HeartbeatContext {
            data_dir:  dir.path().to_path_buf(),
            event_bus: None,
        }),
        store,
        agent:         None,
        history:       Some(history),
        notifications: Some(bus),
        max_chain_depth: 0,
        rate_limiter:    limiter,
        auth: None,
        signal_port: None,
        signal_bot_number: None,
        channel_accounts: None,
        email_accounts: None,
        email_loop_cache: None,
        tts: None,
        live_config: None,
    })
}

fn limits(pairs: &[(&str, u32)]) -> std::collections::HashMap<String, u32> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

fn web_msg(text: &str) -> Action {
    Action::ChannelMessage {
        channel:       "web".into(),
        to:            None,
        conversation_id: None, text_template: text.into(),
    }
}

// ── Direct limiter unit-style tests (cover the public API) ──────────────────

#[test]
fn direct_limiter_blocks_above_cap() {
    let lim = ChannelRateLimiter::new(limits(&[("web", 2)]));
    let now = Utc::now().timestamp();
    assert!(matches!(lim.check_and_record("u1", "web", now), RateDecision::Allowed { .. }));
    assert!(matches!(lim.check_and_record("u1", "web", now), RateDecision::Allowed { .. }));
    let denied = lim.check_and_record("u1", "web", now);
    assert!(matches!(denied, RateDecision::Denied { cap: 2, retry_after_secs }
        if retry_after_secs >= 1 && retry_after_secs <= 60));
}

// ── End-to-end: dispatch path respects the limiter ──────────────────────────

#[tokio::test]
async fn dispatch_first_n_succeed_then_rate_limit() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let limiter = Arc::new(ChannelRateLimiter::new(limits(&[("web", 2)])));
    let dispatcher = make_dispatcher(&dir, Arc::clone(&store), Some(limiter));

    let action = web_msg("hi");
    for _ in 0..2 {
        let out = dispatcher.dispatch(Activation::root(
            "schedule", "sched-A", "user-X", &action, None,
        )).await;
        assert!(matches!(out.outcome, RunOutcome::Success), "expected success, got {:?}", out.error);
    }
    let blocked = dispatcher.dispatch(Activation::root(
        "schedule", "sched-A", "user-X", &action, None,
    )).await;
    assert!(matches!(blocked.outcome, RunOutcome::Failure));
    let err = blocked.error.expect("blocked dispatch should record an error");
    assert!(err.contains("rate limit exceeded"), "unexpected error: {err}");
    assert!(err.contains("cap=2"));
}

#[tokio::test]
async fn dispatch_isolates_users() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let limiter = Arc::new(ChannelRateLimiter::new(limits(&[("web", 1)])));
    let dispatcher = make_dispatcher(&dir, Arc::clone(&store), Some(limiter));

    let action = web_msg("hi");
    let a = dispatcher.dispatch(Activation::root("schedule", "s1", "alice", &action, None)).await;
    let b = dispatcher.dispatch(Activation::root("schedule", "s2", "bob",   &action, None)).await;
    assert!(matches!(a.outcome, RunOutcome::Success));
    assert!(matches!(b.outcome, RunOutcome::Success), "bob's first message should not be blocked by alice");

    // Alice's second hits the cap; bob's second should also hit (his own bucket).
    let a2 = dispatcher.dispatch(Activation::root("schedule", "s1", "alice", &action, None)).await;
    let b2 = dispatcher.dispatch(Activation::root("schedule", "s2", "bob",   &action, None)).await;
    assert!(matches!(a2.outcome, RunOutcome::Failure));
    assert!(matches!(b2.outcome, RunOutcome::Failure));
}

#[tokio::test]
async fn dispatch_isolates_channels() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    // signal is stubbed for outbound but goes through the limiter just like
    // web does — that's the whole point of putting the gate before the
    // channel match.
    let limiter = Arc::new(ChannelRateLimiter::new(
        limits(&[("web", 1), ("signal", 1)])
    ));
    let dispatcher = make_dispatcher(&dir, Arc::clone(&store), Some(limiter));

    let web    = web_msg("web message");
    let signal = Action::ChannelMessage {
        channel: "signal".into(), to: None, conversation_id: None, text_template: "phone ping".into(),
    };

    let w1 = dispatcher.dispatch(Activation::root("schedule", "s1", "u1", &web,    None)).await;
    let s1 = dispatcher.dispatch(Activation::root("schedule", "s2", "u1", &signal, None)).await;
    assert!(matches!(w1.outcome, RunOutcome::Success));
    assert!(matches!(s1.outcome, RunOutcome::Success));

    let w2 = dispatcher.dispatch(Activation::root("schedule", "s1", "u1", &web,    None)).await;
    assert!(matches!(w2.outcome, RunOutcome::Failure));
}

#[tokio::test]
async fn no_limiter_means_no_throttling() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let dispatcher = make_dispatcher(&dir, Arc::clone(&store), None);

    let action = web_msg("flood me");
    for _ in 0..20 {
        let out = dispatcher.dispatch(Activation::root(
            "schedule", "s", "u1", &action, None,
        )).await;
        assert!(matches!(out.outcome, RunOutcome::Success));
    }
}

#[tokio::test]
async fn unknown_channel_falls_back_to_star() {
    // A dispatcher whose limit map has only `*` should still gate every
    // channel (signal, telegram, future ones).
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let limiter = Arc::new(ChannelRateLimiter::new(limits(&[("*", 1)])));
    let dispatcher = make_dispatcher(&dir, Arc::clone(&store), Some(limiter));

    let action = Action::ChannelMessage {
        channel: "telegram".into(), to: None, conversation_id: None, text_template: "hi".into(),
    };
    let first  = dispatcher.dispatch(Activation::root("schedule", "s", "u1", &action, None)).await;
    let second = dispatcher.dispatch(Activation::root("schedule", "s", "u1", &action, None)).await;
    assert!(matches!(first.outcome,  RunOutcome::Success));
    assert!(matches!(second.outcome, RunOutcome::Failure));
}
