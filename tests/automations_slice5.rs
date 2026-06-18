// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/automations_slice5.rs
//! Slice 5 integration tests: webhook ingest end-to-end + event subscriber loop.
//!
//! Two "Done when" conditions from `design-docs/phase10-automations.md`:
//!   1. A POST with valid HMAC signature triggers a prompt (here: an Internal
//!      action — easier to assert on without standing up the agent stack).
//!   2. An event subscription to `tool.failed` reliably notifies via the
//!      dispatcher (again: Internal action stand-in for ChannelMessage).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Extension, Router};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

use mira::automations::{
    Action, AutomationsStore, Dispatcher, NewEventSubscription, NewWebhook, OwnerKind, RunFilter,
    Worker,
};
use mira::automations::heartbeats::{HeartbeatContext, HeartbeatRegistry};
use mira::events::{names as event_names, EventBus};
use mira::events::subscriber as event_subscriber;
use mira::security::hmac::compute_hmac;
use mira::server::handlers::webhooks::ingest_webhook;

// ── Fixture helpers ──────────────────────────────────────────────────────────

fn open_store(dir: &TempDir) -> Arc<AutomationsStore> {
    Arc::new(AutomationsStore::open(&dir.path().join("automations.db")).unwrap())
}

fn make_worker(store: Arc<AutomationsStore>, dir: &TempDir) -> Arc<Worker> {
    let dispatcher = Arc::new(Dispatcher {
        heartbeats:    Arc::new(HeartbeatRegistry::new()),
        ctx:           Arc::new(HeartbeatContext {
            data_dir:  dir.path().to_path_buf(),
            event_bus: None,
        }),
        store:         Arc::clone(&store),
        agent:         None,
        history:       None,
        notifications: None,
        max_chain_depth: 0,
        rate_limiter: None,
        auth: None,
        signal_port: None,
        signal_bot_number: None,
        channel_accounts: None,
        email_accounts: None,
        email_loop_cache: None,
        tts: None,
        live_config: None,
    });
    Arc::new(Worker::new(store, dispatcher))
}

fn ingest_router(store: Arc<AutomationsStore>, worker: Arc<Worker>) -> Router {
    Router::new()
        .route("/webhook/incoming/{token}", post(ingest_webhook))
        .layer(Extension(store))
        .layer(Extension(worker))
}

fn internal_log_cleanup() -> Action {
    Action::Internal { task: "log_cleanup".into(), args: serde_json::Value::Null }
}

fn new_webhook(name: &str, predicate: Option<serde_json::Value>) -> NewWebhook {
    NewWebhook {
        user_id:            "alice".into(),
        owner_kind:         OwnerKind::User,
        name:               name.into(),
        description:        None,
        rationale:          None,
        predicate,
        payload_template:   None,
        action:             internal_log_cleanup(),
        rate_limit_per_min: Some(120),
        debounce_secs:      None,
        expires_at:         None,
        status:             None,
    }
}

// ── Webhook ingest ───────────────────────────────────────────────────────────

#[tokio::test]
async fn signed_payload_matches_predicate_and_dispatches() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    let webhook = store.create_webhook(new_webhook(
        "ci-fire",
        Some(json!({ "eq": ["payload.action", "fire"] })),
    )).unwrap();
    let secret = webhook.secret.clone().expect("secret returned on create");
    let token  = webhook.token.clone();

    let body = br#"{"action":"fire","branch":"main"}"#.as_slice();
    let sig  = compute_hmac(secret.as_bytes(), body);

    let app  = ingest_router(Arc::clone(&store), Arc::clone(&worker));
    let req  = Request::builder()
        .method("POST")
        .uri(format!("/webhook/incoming/{token}"))
        .header("content-type", "application/json")
        .header("x-webhook-signature", format!("sha256={sig}"))
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The dispatch records a row in automation_runs with source_kind="webhook".
    let runs = store.list_runs_filtered(RunFilter {
        user_id:     None,
        source_kind: Some("webhook"),
        source_id:   Some(&webhook.id),
        limit:       10,
        ..Default::default()
    }).unwrap();
    assert_eq!(runs.len(), 1, "matched payload must produce exactly one run");
    assert_eq!(runs[0].user_id, "alice");
}

#[tokio::test]
async fn signed_payload_failing_predicate_appends_payload_but_does_not_dispatch() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    let webhook = store.create_webhook(new_webhook(
        "no-match",
        Some(json!({ "eq": ["payload.action", "fire"] })),
    )).unwrap();
    let secret = webhook.secret.clone().unwrap();
    let token  = webhook.token.clone();

    let body = br#"{"action":"ignore"}"#.as_slice();
    let sig  = compute_hmac(secret.as_bytes(), body);

    let app  = ingest_router(Arc::clone(&store), worker);
    let req  = Request::builder()
        .method("POST")
        .uri(format!("/webhook/incoming/{token}"))
        .header("content-type", "application/json")
        .header("x-webhook-signature", sig)
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // No dispatch → no run.
    let runs = store.list_runs_filtered(RunFilter {
        user_id:     None,
        source_kind: Some("webhook"),
        source_id:   Some(&webhook.id),
        limit:       10,
        ..Default::default()
    }).unwrap();
    assert!(runs.is_empty(), "predicate-rejected payload must not run");

    // But the ring buffer still records it (matched=false) so the UI can show
    // the failed match.
    let payloads = store.list_webhook_payloads(&webhook.id).unwrap();
    assert_eq!(payloads.len(), 1);
    assert!(!payloads[0].matched);
}

#[tokio::test]
async fn invalid_signature_returns_401() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    let webhook = store.create_webhook(new_webhook("bad-sig", None)).unwrap();
    let token   = webhook.token.clone();

    let app  = ingest_router(Arc::clone(&store), worker);
    let req  = Request::builder()
        .method("POST")
        .uri(format!("/webhook/incoming/{token}"))
        .header("content-type", "application/json")
        .header("x-webhook-signature", "sha256=deadbeef")
        .body(Body::from(r#"{"hello":"world"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // No run, no payload — the handler bails before either step.
    let runs = store.list_runs_filtered(RunFilter {
        user_id:     None,
        source_kind: Some("webhook"),
        source_id:   Some(&webhook.id),
        limit:       10,
        ..Default::default()
    }).unwrap();
    assert!(runs.is_empty());
}

#[tokio::test]
async fn unknown_token_returns_404() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    let app  = ingest_router(store, worker);
    let req  = Request::builder()
        .method("POST")
        .uri("/webhook/incoming/no-such-token")
        .header("content-type", "application/json")
        .header("x-webhook-signature", "sha256=abc123")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Event subscriber loop ────────────────────────────────────────────────────

#[tokio::test]
async fn tool_failed_event_triggers_subscription() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    // Subscription owned by alice, listens for tool.failed with no predicate
    // (fires on every emission).
    let sub = store.create_event_subscription(NewEventSubscription {
            delete_after_fire: false,
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "alert-on-tool-failure".into(),
        description: None,
        rationale:   None,
        event_name:  event_names::TOOL_FAILED.into(),
        predicate:   None,
        action:      internal_log_cleanup(),
        expires_at:  None,
        status:      None,
    }).unwrap();

    let bus = Arc::new(EventBus::new());
    let _h  = event_subscriber::spawn(
        Arc::clone(&bus),
        Arc::clone(&store),
        Arc::clone(&worker),
    );

    // Wait for the subscriber to register before emitting — broadcast has no
    // replay, so events emitted before subscribe() are lost.
    for _ in 0..50 {
        if bus.receiver_count() > 0 { break; }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(bus.receiver_count() > 0, "subscriber must be wired before emit");

    bus.emit_named(
        event_names::TOOL_FAILED,
        Some("alice".into()),
        json!({"tool": "search", "error": "timeout"}),
    );

    // Poll for the run row — handler is async on a separate task.
    let mut attempts = 0;
    loop {
        let runs = store.list_runs_filtered(RunFilter {
            user_id:     None,
            source_kind: Some("event"),
            source_id:   Some(&sub.id),
            limit:       10,
            ..Default::default()
        }).unwrap();
        if !runs.is_empty() {
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].user_id, "alice");
            break;
        }
        attempts += 1;
        assert!(attempts < 100, "subscription never produced a run");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn event_with_failing_predicate_does_not_dispatch() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    let sub = store.create_event_subscription(NewEventSubscription {
            delete_after_fire: false,
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "only-search-failures".into(),
        description: None,
        rationale:   None,
        event_name:  event_names::TOOL_FAILED.into(),
        predicate:   Some(json!({ "eq": ["payload.tool", "search"] })),
        action:      internal_log_cleanup(),
        expires_at:  None,
        status:      None,
    }).unwrap();

    let bus = Arc::new(EventBus::new());
    let _h  = event_subscriber::spawn(
        Arc::clone(&bus),
        Arc::clone(&store),
        Arc::clone(&worker),
    );
    for _ in 0..50 {
        if bus.receiver_count() > 0 { break; }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Different tool — predicate should reject.
    bus.emit_named(
        event_names::TOOL_FAILED,
        Some("alice".into()),
        json!({"tool": "calendar", "error": "auth"}),
    );

    // Give the loop a moment to process and reject. We can't wait for "no run"
    // forever, so settle on a short ceiling.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let runs = store.list_runs_filtered(RunFilter {
        user_id:     None,
        source_kind: Some("event"),
        source_id:   Some(&sub.id),
        limit:       10,
        ..Default::default()
    }).unwrap();
    assert!(runs.is_empty(), "predicate-rejected event must not run");
}

#[tokio::test]
async fn event_for_other_user_is_ignored_by_user_owned_sub() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    let sub = store.create_event_subscription(NewEventSubscription {
            delete_after_fire: false,
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "alice-only".into(),
        description: None,
        rationale:   None,
        event_name:  event_names::TOOL_FAILED.into(),
        predicate:   None,
        action:      internal_log_cleanup(),
        expires_at:  None,
        status:      None,
    }).unwrap();

    let bus = Arc::new(EventBus::new());
    let _h  = event_subscriber::spawn(
        Arc::clone(&bus),
        Arc::clone(&store),
        Arc::clone(&worker),
    );
    for _ in 0..50 {
        if bus.receiver_count() > 0 { break; }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Event emitted on bob's behalf — alice's user-owned sub must not fire.
    bus.emit_named(
        event_names::TOOL_FAILED,
        Some("bob".into()),
        json!({"tool": "search", "error": "timeout"}),
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let runs = store.list_runs_filtered(RunFilter {
        user_id:     None,
        source_kind: Some("event"),
        source_id:   Some(&sub.id),
        limit:       10,
        ..Default::default()
    }).unwrap();
    assert!(runs.is_empty(), "user-owned sub must not see other users' events");
}
