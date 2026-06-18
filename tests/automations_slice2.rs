// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/automations_slice2.rs
//! Slice 2 integration tests: action variants, quiet hours, templates.
//!
//! These tests exercise the dispatcher with `agent`/`history`/`notifications`
//! set to `None` — so `Prompt`/`ToolCall`/`ChannelMessage(channel=web)`
//! return clean failures. The remaining variants (`HttpPost`, stubbed
//! channel kinds, `Internal`) and the worker-side quiet-hours gate are
//! exercised end-to-end.

use std::sync::Arc;

use chrono::Utc;
use tempfile::TempDir;

use mira::automations::{
    Action, AutomationsStore, Dispatcher, NewSchedule, OwnerKind, QuietHours,
    ScheduleStatus, TriggerSpec, Worker,
};
use mira::automations::heartbeats::{HeartbeatContext, HeartbeatRegistry};

fn open_store(dir: &TempDir) -> Arc<AutomationsStore> {
    Arc::new(
        AutomationsStore::open(&dir.path().join("automations.db")).unwrap()
    )
}

fn make_dispatcher(store: Arc<AutomationsStore>, data_dir: &TempDir) -> Arc<Dispatcher> {
    Arc::new(Dispatcher {
        heartbeats:    Arc::new(HeartbeatRegistry::new()),
        ctx:           Arc::new(HeartbeatContext { data_dir: data_dir.path().to_path_buf(), event_bus: None }),
        store,
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
    })
}

// ── Quiet hours ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn quiet_hours_skip_pushes_next_run_past_window() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let dispatcher = make_dispatcher(Arc::clone(&store), &dir);
    let worker = Arc::new(Worker::new(Arc::clone(&store), dispatcher));

    // Quiet window covers nearly the whole day so any wall-clock the test
    // happens to run at is inside it.
    let now = Utc::now().timestamp();
    let qh  = QuietHours { start: "00:00".into(), end: "23:59".into() };

    let s = store.create_schedule(NewSchedule {
        user_id:     "system".into(),
        owner_kind:  OwnerKind::System,
        name:        "test_quiet_skip".into(),
        description: None,
        rationale:   None,
        // One-off, already due.
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: Some(qh),
        action:      Action::ChannelMessage {
            channel:       "signal".into(),
            to:            None,
            conversation_id: None, text_template: "hello".into(),
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    // Skipped: run_count untouched, failure_count untouched, last_run_at
    // bumped, next_run_at pushed forward, status still active (not expired
    // — quiet hours skip is not the same as a one-off firing).
    let after = store.get_schedule(&s.id).unwrap().unwrap();
    assert_eq!(after.run_count, 0, "skipped runs don't bump run_count");
    assert_eq!(after.failure_count, 0);
    assert_eq!(after.status, ScheduleStatus::Active);
    assert!(after.last_run_at.is_some());
    assert!(after.next_run_at.is_some(), "next_run_at should advance to quiet end");

    let runs = store.list_runs(None, 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "skipped");
    assert_eq!(runs[0].error.as_deref(), Some("quiet_hours"));
}

#[tokio::test]
async fn quiet_hours_does_not_gate_internal_actions() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let dispatcher = make_dispatcher(Arc::clone(&store), &dir);
    let worker = Arc::new(Worker::new(Arc::clone(&store), dispatcher));

    // Internal heartbeats run regardless of quiet hours — those are
    // user-set windows for *user-visible* actions. Set an "always quiet"
    // window and verify log_cleanup still fires.
    let now = Utc::now().timestamp();
    let s = store.create_schedule(NewSchedule {
        user_id:     "system".into(),
        owner_kind:  OwnerKind::System,
        name:        "test_quiet_no_gate_internal".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: Some(QuietHours { start: "00:00".into(), end: "23:59".into() }),
        action:      Action::Internal {
            task: "log_cleanup".into(),
            args: serde_json::Value::Null,
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    let after = store.get_schedule(&s.id).unwrap().unwrap();
    assert_eq!(after.run_count, 1);
    assert_eq!(after.status, ScheduleStatus::Expired);

    let runs = store.list_runs(None, 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "success");
}

// ── ChannelMessage stub paths ────────────────────────────────────────────────

#[tokio::test]
async fn channel_message_signal_warn_and_noop_without_config() {
    // Outbound Signal delivery requires the dispatcher to be built with
    // `signal_port` + `signal_bot_number` (production wiring populates
    // both from config). When unconfigured — as in this test — the
    // dispatcher logs a warning and treats the run as a success so the
    // schedule still progresses (the message lives in history and the
    // operator sees the missing-config warning in the tail). The
    // channel_message handler echoes the rendered text back as its
    // snippet so the audit row shows what would have been sent.
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let dispatcher = make_dispatcher(Arc::clone(&store), &dir);
    let worker = Arc::new(Worker::new(Arc::clone(&store), dispatcher));

    let now = Utc::now().timestamp();
    let s = store.create_schedule(NewSchedule {
        user_id:     "system".into(),
        owner_kind:  OwnerKind::System,
        name:        "test_signal_unconfigured".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::ChannelMessage {
            channel:       "signal".into(),
            to:            Some("+15555555".into()),
            conversation_id: None, text_template: "hi".into(),
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    let after = store.get_schedule(&s.id).unwrap().unwrap();
    assert_eq!(after.run_count, 1);
    assert_eq!(after.status, ScheduleStatus::Expired);

    let runs = store.list_runs(None, 10).unwrap();
    assert_eq!(runs[0].outcome, "success");
    assert!(
        runs[0].output_snippet.as_deref().unwrap_or_default().contains("signal"),
        "snippet should reflect the channel that handled the action"
    );
}

#[tokio::test]
async fn channel_message_unknown_channel_fails() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let dispatcher = make_dispatcher(Arc::clone(&store), &dir);
    let worker = Arc::new(Worker::new(Arc::clone(&store), dispatcher));

    let now = Utc::now().timestamp();
    let _ = store.create_schedule(NewSchedule {
        user_id:     "system".into(),
        owner_kind:  OwnerKind::System,
        name:        "test_unknown_channel".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::ChannelMessage {
            channel:       "fax".into(),
            to:            None,
            conversation_id: None, text_template: "hi".into(),
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    let runs = store.list_runs(None, 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "failure");
    assert!(
        runs[0].error.as_deref().unwrap_or_default().contains("unknown channel"),
        "error should call out the unknown channel"
    );
}

#[tokio::test]
async fn channel_message_web_without_history_fails() {
    // No HistoryStore wired → web path returns ConfigError.
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let dispatcher = make_dispatcher(Arc::clone(&store), &dir);
    let worker = Arc::new(Worker::new(Arc::clone(&store), dispatcher));

    let now = Utc::now().timestamp();
    let _ = store.create_schedule(NewSchedule {
        user_id:     "system".into(),
        owner_kind:  OwnerKind::System,
        name:        "test_web_no_history".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::ChannelMessage {
            channel:       "web".into(),
            to:            None,
            conversation_id: None, text_template: "hi".into(),
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    let runs = store.list_runs(None, 10).unwrap();
    assert_eq!(runs[0].outcome, "failure");
    assert!(
        runs[0].error.as_deref().unwrap_or_default().contains("HistoryStore"),
        "error should call out the missing HistoryStore"
    );
}

// ── HttpPost ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_post_invalid_url_records_failure() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let dispatcher = make_dispatcher(Arc::clone(&store), &dir);
    let worker = Arc::new(Worker::new(Arc::clone(&store), dispatcher));

    let now = Utc::now().timestamp();
    let _ = store.create_schedule(NewSchedule {
        user_id:     "system".into(),
        owner_kind:  OwnerKind::System,
        name:        "test_http_bad".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::HttpPost {
            url:           "http://127.0.0.1:1/never-listening".into(), // port 1 = unbindable
            headers:       Default::default(),
            body_template: r#"{"x":"{{ now }}"}"#.into(),
            timeout_secs:  1,
            secret:        None,
            max_retries:   0,
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;
    let runs = store.list_runs(None, 10).unwrap();
    assert_eq!(runs[0].outcome, "failure");
    assert!(runs[0].error.as_deref().unwrap_or_default().contains("http_post"));
}

#[tokio::test]
async fn http_post_to_local_server_records_success_with_template_body() {
    use std::net::SocketAddr;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    // Spin up a tiny axum receiver that captures the body.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();

    let (tx, rx) = oneshot::channel::<String>();
    let server = tokio::spawn(async move {
        let mut tx_opt = Some(tx);
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            // Hand-rolled minimal HTTP read: enough for the test's POST.
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            // Find the body after the blank line.
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            // Reply 200.
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
            if let Some(t) = tx_opt.take() {
                let _ = t.send(body);
                return;
            }
        }
    });

    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let dispatcher = make_dispatcher(Arc::clone(&store), &dir);
    let worker = Arc::new(Worker::new(Arc::clone(&store), dispatcher));

    let now = Utc::now().timestamp();
    let _ = store.create_schedule(NewSchedule {
        user_id:     "system".into(),
        owner_kind:  OwnerKind::System,
        name:        "test_http_ok".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::HttpPost {
            url:           format!("http://{}/hook", addr),
            headers:       std::collections::HashMap::from([
                ("Content-Type".to_string(), "application/json".to_string()),
            ]),
            // Includes a template substitution against the dispatcher's
            // implicit context (`source.kind`).
            body_template: r#"{"src":"{{source.kind}}","u":"{{user.id}}"}"#.into(),
            timeout_secs:  3,
            secret:        None,
            max_retries:   0,
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    let runs = store.list_runs(None, 10).unwrap();
    assert_eq!(runs[0].outcome, "success");
    assert!(runs[0].output_snippet.as_deref().unwrap_or_default().contains("200"));

    // Verify the template was actually rendered before sending.
    let body = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await.unwrap().unwrap();
    assert!(body.contains(r#""src":"schedule""#), "template should render source.kind: got {body}");
    assert!(body.contains(r#""u":"system""#),     "template should render user.id: got {body}");
    server.abort();
}

// ── Sanity: quiet hours window math wrt timezone parsing ─────────────────────

#[tokio::test]
async fn quiet_hours_unknown_timezone_falls_through() {
    // A typo'd timezone shouldn't permanently silence a schedule. The
    // worker's gate returns `is_quiet=false`, so the action runs normally.
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let dispatcher = make_dispatcher(Arc::clone(&store), &dir);
    let worker = Arc::new(Worker::new(Arc::clone(&store), dispatcher));

    let now = Utc::now().timestamp();
    let s = store.create_schedule(NewSchedule {
        user_id:     "system".into(),
        owner_kind:  OwnerKind::System,
        name:        "test_qh_bad_tz".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "Mars/Olympus".into(),
        quiet_hours: Some(QuietHours { start: "00:00".into(), end: "23:59".into() }),
        action:      Action::ChannelMessage {
            channel:       "signal".into(),
            to:            None,
            conversation_id: None, text_template: "hi".into(),
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    // The worker's create_schedule path also computes next_run_at; cron
    // would fail on a bad tz but OneOff doesn't touch tz, so creation
    // succeeds. The gate then sees a parse failure and treats the schedule
    // as "not quiet", letting the action run.
    worker.run_once().await;

    let after = store.get_schedule(&s.id).unwrap().unwrap();
    assert_eq!(after.run_count, 1, "bad tz must not silently gate the schedule");
}
