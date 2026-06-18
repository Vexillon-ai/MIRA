// SPDX-License-Identifier: AGPL-3.0-or-later

// tests/automations_slice8.rs
//! Slice 8 — runs filter pagination + outcome filter.
//!
//! Slice 8 is mostly UI/docs polish; the only new server surface is the
//! `before` cursor and `outcome=` filter on `/api/automations/runs`. These
//! tests drive `list_runs_filtered` directly since the HTTP handler is a
//! thin wrapper over it.

use std::sync::Arc;

use tempfile::TempDir;

use mira::automations::{AutomationsStore, RunFilter, RunOutcome};

fn open_store(dir: &TempDir) -> Arc<AutomationsStore> {
    Arc::new(AutomationsStore::open(&dir.path().join("automations.db")).unwrap())
}

/// Seed N runs spaced 1 second apart, with an outcome cycle of
/// success / failure / skipped — tests can pick any subset they need.
fn seed_runs(store: &AutomationsStore, n: i64) {
    for i in 0..n {
        let outcome = match i % 3 {
            0 => RunOutcome::Success,
            1 => RunOutcome::Failure,
            _ => RunOutcome::Skipped,
        };
        store.record_run(
            "schedule",
            &format!("sched-{i}"),
            "alice",
            1_000 + i,
            Some(1_000 + i),
            outcome,
            None,
            None,
            None,
        ).unwrap();
    }
}

#[tokio::test]
async fn before_cursor_returns_only_older_runs() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    seed_runs(&store, 10);

    let page = store.list_runs_filtered(RunFilter {
        user_id:        Some("alice"),
        before_started: Some(1_005),
        limit:          100,
        ..Default::default()
    }).unwrap();

    // started_at < 1005 → ids 0..=4 (5 rows). Order is DESC.
    assert_eq!(page.len(), 5);
    assert!(page.iter().all(|r| r.started_at < 1_005));
    assert!(page[0].started_at >= page.last().unwrap().started_at);
}

#[tokio::test]
async fn keyset_pagination_walks_full_set_without_gaps() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    seed_runs(&store, 10);

    // Page size 4 → 4 + 4 + 2 = 10. Use the smallest started_at on each
    // page as the next cursor, the same way the UI's "Load more" works.
    let mut seen = Vec::new();
    let mut cursor: Option<i64> = None;
    loop {
        let page = store.list_runs_filtered(RunFilter {
            user_id:        Some("alice"),
            before_started: cursor,
            limit:          4,
            ..Default::default()
        }).unwrap();
        if page.is_empty() { break; }
        cursor = Some(page.last().unwrap().started_at);
        seen.extend(page.into_iter().map(|r| r.started_at));
    }
    assert_eq!(seen.len(), 10);
    // All distinct, all in the seeded set.
    let mut sorted = seen.clone(); sorted.sort();
    assert_eq!(sorted, (1_000..1_010).collect::<Vec<_>>());
}

#[tokio::test]
async fn outcome_filter_returns_only_matching_rows() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    seed_runs(&store, 9); // 3 success / 3 failure / 3 skipped

    let failures = store.list_runs_filtered(RunFilter {
        user_id: Some("alice"),
        outcome: Some("failure"),
        limit:   100,
        ..Default::default()
    }).unwrap();
    assert_eq!(failures.len(), 3);
    assert!(failures.iter().all(|r| r.outcome == "failure"));

    let skipped = store.list_runs_filtered(RunFilter {
        user_id: Some("alice"),
        outcome: Some("skipped"),
        limit:   100,
        ..Default::default()
    }).unwrap();
    assert_eq!(skipped.len(), 3);
    assert!(skipped.iter().all(|r| r.outcome == "skipped"));
}

#[tokio::test]
async fn outcome_and_cursor_compose() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    seed_runs(&store, 12); // 4 each outcome at started_at 1000..1011

    // success rows are at started_at 1000, 1003, 1006, 1009.
    // before=1006 + outcome=success → just 1000 and 1003.
    let page = store.list_runs_filtered(RunFilter {
        user_id:        Some("alice"),
        outcome:        Some("success"),
        before_started: Some(1_006),
        limit:          100,
        ..Default::default()
    }).unwrap();
    let ts: Vec<_> = page.iter().map(|r| r.started_at).collect();
    assert_eq!(ts, vec![1_003, 1_000]);
}

#[tokio::test]
async fn limit_caps_page_size() {
    let dir   = TempDir::new().unwrap();
    let store = open_store(&dir);
    seed_runs(&store, 10);

    let page = store.list_runs_filtered(RunFilter {
        user_id: Some("alice"),
        limit:   3,
        ..Default::default()
    }).unwrap();
    assert_eq!(page.len(), 3);
    // DESC ordering — newest first.
    assert_eq!(page[0].started_at, 1_009);
    assert_eq!(page[2].started_at, 1_007);
}
