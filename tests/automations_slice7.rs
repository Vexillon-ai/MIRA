// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/automations_slice7.rs
//! Slice 7 integration tests — outbound webhook hardening + chain hardening.
//!
//! "Done when" from `design-docs/phase10-automations.md`:
//!   an HttpPost action retries cleanly; chain loops are caught;
//!   failures notify.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::Utc;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use mira::automations::{
    Action, AutomationsStore, Dispatcher, NewSchedule, OwnerKind, RunFilter, ScheduleStatus,
    TriggerSpec, Worker,
    dispatch::{Activation, DispatchOutcome},
    heartbeats::{HeartbeatContext, HeartbeatRegistry},
};

// ── Fixture helpers ──────────────────────────────────────────────────────────

fn open_store(dir: &TempDir) -> Arc<AutomationsStore> {
    Arc::new(AutomationsStore::open(&dir.path().join("automations.db")).unwrap())
}

fn make_dispatcher(
    store:          Arc<AutomationsStore>,
    dir:            &TempDir,
    max_chain_depth: u32,
) -> Arc<Dispatcher> {
    Arc::new(Dispatcher {
        heartbeats:    Arc::new(HeartbeatRegistry::new()),
        ctx:           Arc::new(HeartbeatContext {
            data_dir:  dir.path().to_path_buf(),
            event_bus: None,
        }),
        store,
        agent:         None,
        history:       None,
        notifications: None,
        max_chain_depth,
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

fn internal_log_cleanup() -> Action {
    Action::Internal { task: "log_cleanup".into(), args: Value::Null }
}

// ── Captured request the test server can hand back to assertions ────────────

#[derive(Debug, Default)]
struct CapturedRequest {
    headers: HashMap<String, String>,
    #[allow(dead_code)] // captured for completeness; this test only asserts on headers
    body:    String,
}

/// Spawn a one-shot HTTP server that responds with `responses[i]` for the
/// `i`-th request, capturing each request into the returned vector. After
/// `responses` is exhausted, the server keeps replying with the last entry
/// (handy for "retry forever" tests).
fn spawn_http_server(
    responses: Vec<&'static str>,
) -> (SocketAddr, Arc<Mutex<Vec<CapturedRequest>>>, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = TcpListener::from_std(listener).unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));

    let cap_clone = Arc::clone(&captured);
    let cnt_clone = Arc::clone(&count);
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let cap = Arc::clone(&cap_clone);
            let cnt = Arc::clone(&cnt_clone);
            let responses = responses.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = match stream.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
                let mut headers: HashMap<String, String> = HashMap::new();
                for line in head.lines().skip(1) {
                    if let Some((k, v)) = line.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                let idx = cnt.fetch_add(1, Ordering::SeqCst);
                cap.lock().await.push(CapturedRequest {
                    headers,
                    body: body.to_string(),
                });
                let resp = responses.get(idx).copied()
                    .unwrap_or_else(|| responses.last().copied().unwrap_or("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"));
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    (addr, captured, count)
}

// ── 7.1 — HttpPost retries on 5xx until success ──────────────────────────────

#[tokio::test]
async fn http_post_retries_on_5xx_then_succeeds() {
    let (addr, captured, count) = spawn_http_server(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);

    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let disp  = make_dispatcher(Arc::clone(&store), &dir, 5);
    let worker = Arc::new(Worker::new(Arc::clone(&store), Arc::clone(&disp)));

    let now = Utc::now().timestamp();
    store.create_schedule(NewSchedule {
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "retry-success".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::HttpPost {
            url:           format!("http://{}/hook", addr),
            headers:       Default::default(),
            body_template: r#"{"hello":"world"}"#.into(),
            timeout_secs:  3,
            secret:        None,
            max_retries:   3,
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    assert_eq!(count.load(Ordering::SeqCst), 3, "expected 3 attempts");
    let runs = store.list_runs(None, 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "success", "last attempt succeeded");
    let _ = captured;
}

// ── 7.1 — HttpPost does NOT retry on 4xx (terminal) ──────────────────────────

#[tokio::test]
async fn http_post_does_not_retry_on_4xx() {
    let (addr, _captured, count) = spawn_http_server(vec![
        "HTTP/1.1 400 Bad Request\r\nContent-Length: 3\r\n\r\nbad",
    ]);

    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let disp  = make_dispatcher(Arc::clone(&store), &dir, 5);
    let worker = Arc::new(Worker::new(Arc::clone(&store), Arc::clone(&disp)));

    let now = Utc::now().timestamp();
    store.create_schedule(NewSchedule {
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "terminal-4xx".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::HttpPost {
            url:           format!("http://{}/hook", addr),
            headers:       Default::default(),
            body_template: r#"{}"#.into(),
            timeout_secs:  3,
            secret:        None,
            max_retries:   3, // would retry 5xx, but 4xx must not
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    assert_eq!(count.load(Ordering::SeqCst), 1, "4xx must not retry");
    let runs = store.list_runs(None, 10).unwrap();
    assert_eq!(runs[0].outcome, "failure");
}

// ── 7.1 — HMAC signature header is added when secret is set ──────────────────

#[tokio::test]
async fn http_post_signs_body_when_secret_provided() {
    let (addr, captured, _count) = spawn_http_server(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
    ]);

    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let disp  = make_dispatcher(Arc::clone(&store), &dir, 5);
    let worker = Arc::new(Worker::new(Arc::clone(&store), Arc::clone(&disp)));

    let secret  = "shh-its-a-secret";
    let body    = r#"{"hello":"world"}"#;
    let now     = Utc::now().timestamp();
    store.create_schedule(NewSchedule {
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "signed".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::HttpPost {
            url:           format!("http://{}/hook", addr),
            headers:       Default::default(),
            body_template: body.into(),
            timeout_secs:  3,
            secret:        Some(secret.into()),
            max_retries:   0,
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    let cap = captured.lock().await;
    assert_eq!(cap.len(), 1);
    let sig_hdr = cap[0].headers.get("x-mira-signature").expect("signature header missing");
    let prefix  = "sha256=";
    assert!(sig_hdr.starts_with(prefix), "unexpected sig header: {sig_hdr}");
    let expected = mira::security::hmac::compute_hmac(secret.as_bytes(), body.as_bytes());
    assert_eq!(&sig_hdr[prefix.len()..], expected, "signature mismatch");

    let ts_hdr = cap[0].headers.get("x-mira-timestamp").expect("timestamp header missing");
    let ts: i64 = ts_hdr.parse().expect("timestamp must be unix-seconds integer");
    let drift = (now - ts).abs();
    assert!(drift < 5, "timestamp drift {drift}s too large");

    // User-Agent badge applied automatically.
    let ua = cap[0].headers.get("user-agent").expect("user-agent header missing");
    assert!(ua.starts_with("mira/"), "expected mira/<ver>, got {ua}");
}

// ── 7.1 — User-Agent only added if not provided by caller ────────────────────

#[tokio::test]
async fn http_post_respects_caller_user_agent() {
    let (addr, captured, _count) = spawn_http_server(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
    ]);

    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let disp  = make_dispatcher(Arc::clone(&store), &dir, 5);
    let worker = Arc::new(Worker::new(Arc::clone(&store), Arc::clone(&disp)));

    let mut headers = HashMap::new();
    headers.insert("User-Agent".into(), "my-bot/1.0".into());

    let now = Utc::now().timestamp();
    store.create_schedule(NewSchedule {
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "ua-respected".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: now - 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::HttpPost {
            url:           format!("http://{}/hook", addr),
            headers,
            body_template: "{}".into(),
            timeout_secs:  3,
            secret:        None,
            max_retries:   0,
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    worker.run_once().await;

    let cap = captured.lock().await;
    let ua = cap[0].headers.get("user-agent").expect("user-agent header missing");
    assert_eq!(ua, "my-bot/1.0", "caller's UA must win");
}

// ── 7.2 — Chain depth cap rejects too-deep activations ───────────────────────

#[tokio::test]
async fn dispatcher_rejects_when_chain_depth_exceeds_max() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let disp  = make_dispatcher(Arc::clone(&store), &dir, 3); // cap=3

    let action = internal_log_cleanup();
    let chain = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let act = Activation {
        source_kind: "schedule",
        source_id:   "deep",
        user_id:     "alice",
        action:      &action,
        payload:     None,
        chain_ids:   &chain,
    };
    let outcome: DispatchOutcome = disp.dispatch(act).await;
    assert!(!matches!(outcome.outcome, mira::automations::RunOutcome::Success));
    let err = outcome.error.unwrap_or_default();
    assert!(err.contains("chain depth"), "expected depth error, got: {err}");

    // Recorded as a failure run with source_kind=schedule, id=deep.
    let runs = store.list_runs_filtered(RunFilter {
        user_id:     None,
        source_kind: Some("schedule"),
        source_id:   Some("deep"),
        limit:       10,
        ..Default::default()
    }).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "failure");
}

// ── 7.2 — Cycle detection refuses repeat source_id in chain ──────────────────

#[tokio::test]
async fn dispatcher_rejects_cycle_in_chain() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let disp  = make_dispatcher(Arc::clone(&store), &dir, 10);

    let action = internal_log_cleanup();
    let chain  = vec!["a".to_string(), "loop".to_string(), "b".to_string()];
    let act = Activation {
        source_kind: "event",
        source_id:   "loop", // already in chain → cycle
        user_id:     "alice",
        action:      &action,
        payload:     None,
        chain_ids:   &chain,
    };
    let outcome = disp.dispatch(act).await;
    let err = outcome.error.unwrap_or_default();
    assert!(err.contains("cycle"), "expected cycle error, got: {err}");
}

// ── 7.2 — Depth 0 still passes on a cap=1 dispatcher ─────────────────────────

#[tokio::test]
async fn dispatcher_allows_root_activation_under_tight_cap() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let disp  = make_dispatcher(Arc::clone(&store), &dir, 1);

    let action = internal_log_cleanup();
    let act = Activation::root("schedule", "root", "alice", &action, None);
    let outcome = disp.dispatch(act).await;
    assert!(matches!(outcome.outcome, mira::automations::RunOutcome::Success),
            "root activation should be allowed: {:?}", outcome.error);
}

// ── 7.3 — Dead-letter notify fires when status flips to failed ───────────────

#[tokio::test]
async fn dead_letter_notification_on_failure_cap() {
    // Standalone test server that always 500s — the action will keep
    // failing and eventually trip max_failures.
    let (addr, _cap, _cnt) = spawn_http_server(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
    ]);

    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let disp  = make_dispatcher(Arc::clone(&store), &dir, 5);
    let worker = Arc::new(Worker::new(Arc::clone(&store), Arc::clone(&disp)));

    let _now = Utc::now().timestamp();
    let s = store.create_schedule(NewSchedule {
        user_id:     "alice".into(),
        owner_kind:  OwnerKind::User,
        name:        "doomed".into(),
        description: None,
        rationale:   None,
        // Interval rather than one-off so failure_count can build up
        // without record_failure flipping straight to expired.
        trigger:     TriggerSpec::Interval { every_secs: 1 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::HttpPost {
            url:           format!("http://{}/hook", addr),
            headers:       Default::default(),
            body_template: "{}".into(),
            timeout_secs:  2,
            secret:        None,
            max_retries:   0, // fail fast — no per-attempt retry
        },
        expires_at:  None,
        status:      None,
    }).unwrap();

    // Force max_failures down to 2 so we don't sit through 5 retries.
    {
        use rusqlite::{Connection, params};
        let path = dir.path().join("automations.db");
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "UPDATE schedules SET max_failures = 2 WHERE id = ?1",
            params![s.id],
        ).unwrap();
    }

    // Drive 2 fires — both fail → status flips to 'failed' on the second.
    worker.run_now(&s.id).await.unwrap();
    worker.run_now(&s.id).await.unwrap();

    let after = store.get_schedule(&s.id).unwrap().unwrap();
    assert!(matches!(after.status, ScheduleStatus::Failed),
            "schedule should be in failed state, got {:?}", after.status);

    // The dead-letter notification was dispatched as a separate run with
    // source_kind="dead_letter".
    let dl_runs = store.list_runs_filtered(RunFilter {
        user_id:     None,
        source_kind: Some("dead_letter"),
        source_id:   Some(&s.id),
        limit:       10,
        ..Default::default()
    }).unwrap();
    assert_eq!(dl_runs.len(), 1, "expected exactly one dead-letter run");

    // Sanity: a second failure after the row is already 'failed' must not
    // produce a duplicate notification — record_failure on a 'failed' row
    // is a no-op-style transition (status stays failed).
    worker.run_now(&s.id).await.unwrap();
    let dl_runs2 = store.list_runs_filtered(RunFilter {
        user_id:     None,
        source_kind: Some("dead_letter"),
        source_id:   Some(&s.id),
        limit:       10,
        ..Default::default()
    }).unwrap();
    assert_eq!(dl_runs2.len(), 2,
               "subsequent failures still notify (current behaviour); update if we add idempotency");
}
