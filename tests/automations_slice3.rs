// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/automations_slice3.rs
//! Slice 3 integration tests: store CRUD + lifecycle + worker.run_now +
//! visibility helpers.
//!
//! The HTTP handlers are thin wrappers over these store methods + the
//! AuthUser extractor. End-to-end HTTP coverage is easier once auth is
//! wired into a test fixture (Slice 4 territory); here we prove the
//! underlying machinery the routes call into.

use std::sync::Arc;

use chrono::Utc;
use tempfile::TempDir;

use mira::automations::{
    Action, AutomationsStore, Dispatcher, NewSchedule, OwnerKind, QuietHours,
    RunFilter, ScheduleStatus, TriggerSpec, UpdateSchedule, Worker,
};
use mira::automations::heartbeats::{HeartbeatContext, HeartbeatRegistry};

fn open_store(dir: &TempDir) -> Arc<AutomationsStore> {
    Arc::new(AutomationsStore::open(&dir.path().join("automations.db")).unwrap())
}

fn make_worker(store: Arc<AutomationsStore>, dir: &TempDir) -> Arc<Worker> {
    let dispatcher = Arc::new(Dispatcher {
        heartbeats:    Arc::new(HeartbeatRegistry::new()),
        ctx:           Arc::new(HeartbeatContext { data_dir: dir.path().to_path_buf(), event_bus: None }),
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

fn user_schedule(user: &str, name: &str) -> NewSchedule {
    NewSchedule {
        user_id:     user.into(),
        owner_kind:  OwnerKind::User,
        name:        name.into(),
        description: None,
        rationale:   None,
        // Far enough in the future the worker won't pick it up during the
        // test, but valid so create_schedule succeeds.
        trigger:     TriggerSpec::OneOff { at: Utc::now().timestamp() + 3600 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::Internal { task: "log_cleanup".into(), args: serde_json::Value::Null },
        expires_at:  None,
        status:      None,
    }
}

// ── Visibility ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_visible_returns_own_plus_system_for_non_admin() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);

    // Two real users + one system row + one heartbeat (also system).
    let alice  = store.create_schedule(user_schedule("alice", "alice-job")).unwrap();
    let _bob   = store.create_schedule(user_schedule("bob",   "bob-job")).unwrap();
    let system = store.create_schedule(NewSchedule {
        owner_kind: OwnerKind::System,
        ..user_schedule("system", "heartbeat-x")
    }).unwrap();

    let visible = store.list_schedules_visible_to("alice", false).unwrap();
    let names: Vec<_> = visible.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&alice.name.as_str()),  "alice sees own row");
    assert!(names.contains(&system.name.as_str()), "alice sees system row");
    assert!(!names.contains(&"bob-job"),           "alice must not see bob's row");
}

#[tokio::test]
async fn list_visible_admin_sees_everything() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);

    store.create_schedule(user_schedule("alice", "a")).unwrap();
    store.create_schedule(user_schedule("bob",   "b")).unwrap();

    let admin_view = store.list_schedules_visible_to("alice", true).unwrap();
    let names: Vec<_> = admin_view.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"a"));
    assert!(names.contains(&"b"));
}

// ── Update ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn update_changes_trigger_and_recomputes_next_run() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);

    let s = store.create_schedule(user_schedule("alice", "j")).unwrap();
    let original_next = s.next_run_at;

    // Switch to interval that fires in ~10 min from now — distinct from the
    // 1-hour OneOff, so we can detect the recompute.
    let upd = UpdateSchedule {
        name:        "j-renamed".into(),
        description: Some("now with a description".into()),
        rationale:   None,
        trigger:     TriggerSpec::Interval { every_secs: 600 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::Internal {
            task: "tmp_cleanup".into(),
            args: serde_json::Value::Null,
        },
        expires_at:  None,
    };
    let updated = store.update_schedule(&s.id, upd).unwrap();
    assert_eq!(updated.name, "j-renamed");
    assert_eq!(updated.description.as_deref(), Some("now with a description"));
    assert!(matches!(updated.trigger, TriggerSpec::Interval { every_secs: 600 }));
    // Counters preserved.
    assert_eq!(updated.run_count,     s.run_count);
    assert_eq!(updated.failure_count, s.failure_count);
    // next_run_at recomputed from new trigger (interval = +600s vs 1h OneOff).
    assert_ne!(updated.next_run_at, original_next);
}

#[tokio::test]
async fn update_unknown_id_errors() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let upd = UpdateSchedule {
        name:        "x".into(),
        description: None,
        rationale:   None,
        trigger:     TriggerSpec::OneOff { at: Utc::now().timestamp() + 60 },
        timezone:    "UTC".into(),
        quiet_hours: None,
        action:      Action::Internal { task: "log_cleanup".into(), args: serde_json::Value::Null },
        expires_at:  None,
    };
    let res = store.update_schedule("nope", upd);
    assert!(res.is_err(), "updating an unknown id must return an error");
}

// ── Pause / Resume / Snooze ──────────────────────────────────────────────────

#[tokio::test]
async fn pause_then_resume_recomputes_next_run() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);

    let s = store.create_schedule(NewSchedule {
        // Use cron so resume produces a deterministic next fire — interval
        // resume would just bump by every_secs which is also fine.
        trigger: TriggerSpec::Cron { expr: "0 0 9 * * *".into() },
        ..user_schedule("alice", "morning")
    }).unwrap();
    assert!(matches!(s.status, ScheduleStatus::Active));

    let paused = store.pause_schedule(&s.id).unwrap();
    assert!(matches!(paused.status, ScheduleStatus::Paused));

    let resumed = store.resume_schedule(&s.id).unwrap();
    assert!(matches!(resumed.status, ScheduleStatus::Active));
    assert!(resumed.next_run_at.is_some(), "resume must recompute next_run_at");
}

#[tokio::test]
async fn resume_rejects_active_or_terminal() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let s = store.create_schedule(user_schedule("alice", "j")).unwrap();
    // Active row: resume should error since there's nothing to resume.
    assert!(store.resume_schedule(&s.id).is_err());
}

#[tokio::test]
async fn snooze_clamps_past_until_to_now_and_clears_pause() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let s = store.create_schedule(user_schedule("alice", "j")).unwrap();
    let _ = store.pause_schedule(&s.id).unwrap();

    let now = Utc::now().timestamp();
    let snoozed = store.snooze_schedule(&s.id, now - 3600).unwrap();
    // Status flipped back to active and next_run_at >= now.
    assert!(matches!(snoozed.status, ScheduleStatus::Active));
    let next = snoozed.next_run_at.expect("snooze must set next_run_at");
    assert!(next >= now, "snooze must clamp past targets to now");
}

#[tokio::test]
async fn snooze_to_future_target_is_honored() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let s = store.create_schedule(user_schedule("alice", "j")).unwrap();
    let target = Utc::now().timestamp() + 7200;
    let snoozed = store.snooze_schedule(&s.id, target).unwrap();
    assert_eq!(snoozed.next_run_at, Some(target));
}

// ── Delete ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_removes_user_row() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let s = store.create_schedule(user_schedule("alice", "j")).unwrap();
    assert!(store.delete_schedule(&s.id).unwrap());
    assert!(store.get_schedule(&s.id).unwrap().is_none());
}

#[tokio::test]
async fn delete_refuses_to_remove_system_row() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    let s = store.create_schedule(NewSchedule {
        owner_kind: OwnerKind::System,
        ..user_schedule("system", "heartbeat-y")
    }).unwrap();
    // Returns false (no rows affected) — the handler maps this to a 4xx.
    assert!(!store.delete_schedule(&s.id).unwrap());
    // Row still exists.
    assert!(store.get_schedule(&s.id).unwrap().is_some());
}

// ── Worker::run_now ──────────────────────────────────────────────────────────

#[tokio::test]
async fn run_now_fires_an_active_schedule_and_bumps_run_count() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    // Far-future trigger so the worker timer wouldn't pick it up.
    let s = store.create_schedule(NewSchedule {
        trigger: TriggerSpec::Interval { every_secs: 86_400 },
        ..user_schedule("system", "manual-fire")
    }).unwrap();
    assert_eq!(s.run_count, 0);

    worker.run_now(&s.id).await.unwrap();
    let after = store.get_schedule(&s.id).unwrap().unwrap();
    assert_eq!(after.run_count, 1, "run_now must fire the schedule");
    assert!(after.last_run_at.is_some());
    // The interval recompute leaves status active for the next tick.
    assert!(matches!(after.status, ScheduleStatus::Active));
}

#[tokio::test]
async fn run_now_fires_a_paused_schedule_too() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    let s = store.create_schedule(user_schedule("alice", "j")).unwrap();
    let _ = store.pause_schedule(&s.id).unwrap();
    // run_now is an explicit user override — pause must not block it.
    worker.run_now(&s.id).await.unwrap();
    let after = store.get_schedule(&s.id).unwrap().unwrap();
    assert_eq!(after.run_count, 1);
}

#[tokio::test]
async fn run_now_bypasses_quiet_hours() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    // Quiet 24/7 — a normal worker tick would skip a Prompt/ChannelMessage,
    // but Internal is exempt anyway, so we verify run_now fires irrespective
    // of the gate setting (the gate isn't even applied in run_now).
    let s = store.create_schedule(NewSchedule {
        quiet_hours: Some(QuietHours { start: "00:00".into(), end: "23:59".into() }),
        ..user_schedule("system", "j")
    }).unwrap();
    worker.run_now(&s.id).await.unwrap();
    let after = store.get_schedule(&s.id).unwrap().unwrap();
    assert_eq!(after.run_count, 1);
}

#[tokio::test]
async fn run_now_unknown_id_errors() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);
    assert!(worker.run_now("nope").await.is_err());
}

// ── list_runs_filtered ───────────────────────────────────────────────────────

#[tokio::test]
async fn list_runs_filtered_by_source_and_id() {
    let dir    = TempDir::new().unwrap();
    let store  = open_store(&dir);
    let worker = make_worker(Arc::clone(&store), &dir);

    let s1 = store.create_schedule(user_schedule("alice", "a")).unwrap();
    let s2 = store.create_schedule(user_schedule("alice", "b")).unwrap();
    worker.run_now(&s1.id).await.unwrap();
    worker.run_now(&s2.id).await.unwrap();

    let only_s1 = store.list_runs_filtered(RunFilter {
        user_id:     None,
        source_kind: Some("schedule"),
        source_id:   Some(&s1.id),
        limit:       100,
        ..Default::default()
    }).unwrap();
    assert_eq!(only_s1.len(), 1);
    assert_eq!(only_s1[0].source_id, s1.id);

    let by_user = store.list_runs_filtered(RunFilter {
        user_id:     Some("alice"),
        source_kind: None,
        source_id:   None,
        limit:       100,
        ..Default::default()
    }).unwrap();
    assert_eq!(by_user.len(), 2);
}
