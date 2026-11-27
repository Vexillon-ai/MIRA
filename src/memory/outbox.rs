// SPDX-License-Identifier: AGPL-3.0-or-later

//! Durable ingest outbox.
//!
//! The harness used to hand each completed turn to a memory writer via a
//! fire-and-forget `tokio::spawn` — a transient network blip or a 5xx dropped
//! the turn silently. This persists the turn FIRST, then a background worker
//! delivers it, so nothing is lost to a transient failure:
//!
//! * **success** — the row is deleted (delivered).
//! * **5xx / transport error** — retryable; the row's `attempts` increments and
//!   it's re-tried with exponential backoff, up to `max_attempts` (then
//!   `exhausted`, with a warning).
//! * **4xx** — a client/config error (e.g. an identity the store doesn't map).
//!   Retrying the identical body won't help until the config is fixed, so the
//!   row is `held` (kept, warned) rather than retried or dropped — a later fix
//!   makes it sendable.
//!
//! Holds are bounded: any row older than `max_age_days` is pruned regardless of
//! state, so a genuinely-foreign identity (e.g. a test account a single-tenant
//! store will never map) can't accumulate held rows forever.
//!
//! Enqueue is a single local SQLite insert off the turn's critical path (the
//! reply never waits on delivery), and it degrades gracefully: if the outbox
//! isn't wired or the insert fails, the caller falls back to a direct send.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use tracing::{debug, info, warn};

use crate::memory::provider::{MemoryError, MemoryProvider, TurnRecord};

/// Tunables for the outbox worker.
#[derive(Debug, Clone, Copy)]
pub struct OutboxConfig {
    /// Give up retrying a transient failure after this many attempts.
    pub max_attempts: u32,
    /// Prune ANY row older than this (bounds held/exhausted growth).
    pub max_age_days: i64,
    /// How often the worker drains due rows.
    pub poll_interval_secs: u64,
    /// Rows processed per drain pass.
    pub batch: usize,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self { max_attempts: 8, max_age_days: 7, poll_interval_secs: 30, batch: 50 }
    }
}

/// Backoff before the next attempt of a retryable failure: 10s · 2^(attempts-1),
/// capped at 1 hour.
fn backoff(attempts: u32) -> Duration {
    let secs = 10i64.saturating_mul(1i64 << attempts.saturating_sub(1).min(12));
    Duration::seconds(secs.min(3600))
}

pub struct MemoryOutbox {
    conn:    Mutex<Connection>,
    writers: HashMap<String, Arc<dyn MemoryProvider>>,
    cfg:     OutboxConfig,
}

impl MemoryOutbox {
    /// Open (or create) the outbox DB and register the harness writers by id.
    pub fn open(
        path:    &std::path::Path,
        writers: Vec<Arc<dyn MemoryProvider>>,
        cfg:     OutboxConfig,
    ) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS ingest_outbox (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                provider_id     TEXT NOT NULL,
                payload         TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                attempts        INTEGER NOT NULL DEFAULT 0,
                next_attempt_at TEXT NOT NULL,
                state           TEXT NOT NULL DEFAULT 'pending',
                last_error      TEXT
            );
            CREATE INDEX IF NOT EXISTS ingest_outbox_due
                ON ingest_outbox(state, next_attempt_at);
            "#,
        )?;
        let writers = writers.into_iter().map(|w| (w.id().to_string(), w)).collect();
        Ok(Self { conn: Mutex::new(conn), writers, cfg })
    }

    /// Persist a turn for delivery to one writer. Cheap (one insert); the actual
    /// send is the worker's job. Errors are returned so the caller can fall back
    /// to a direct send rather than lose the turn.
    pub fn enqueue(&self, provider_id: &str, turn: &TurnRecord) -> Result<(), String> {
        let payload = serde_json::to_string(turn).map_err(|e| e.to_string())?;
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().map_err(|_| "outbox mutex poisoned".to_string())?;
        conn.execute(
            "INSERT INTO ingest_outbox (provider_id, payload, created_at, next_attempt_at, state)
             VALUES (?1, ?2, ?3, ?3, 'pending')",
            params![provider_id, payload, now],
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// One drain pass: deliver every due `pending` row, then prune aged rows.
    /// Returns (delivered, held, retrying, exhausted) for logging/tests.
    pub async fn drain_due(&self) -> (usize, usize, usize, usize) {
        let now = Utc::now();
        let due: Vec<(i64, String, String, u32)> = {
            let conn = match self.conn.lock() { Ok(c) => c, Err(_) => return (0, 0, 0, 0) };
            let mut stmt = match conn.prepare(
                "SELECT id, provider_id, payload, attempts FROM ingest_outbox
                 WHERE state = 'pending' AND next_attempt_at <= ?1
                 ORDER BY id LIMIT ?2",
            ) { Ok(s) => s, Err(_) => return (0, 0, 0, 0) };
            let rows = stmt.query_map(
                params![now.to_rfc3339(), self.cfg.batch as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as u32)),
            );
            match rows { Ok(it) => it.filter_map(Result::ok).collect(), Err(_) => return (0, 0, 0, 0) }
        };

        let (mut delivered, mut held, mut retrying, mut exhausted) = (0, 0, 0, 0);
        for (id, provider_id, payload, attempts) in due {
            let Some(writer) = self.writers.get(&provider_id) else {
                // The provider went away (config change) — nothing can deliver
                // this row; drop it rather than loop forever.
                self.delete(id);
                continue;
            };
            let turn: TurnRecord = match serde_json::from_str(&payload) {
                Ok(t) => t,
                Err(e) => { warn!("outbox: unparseable payload (row {id}), dropping: {e}"); self.delete(id); continue; }
            };
            match writer.ingest(&turn).await {
                Ok(_) => { self.delete(id); delivered += 1; }
                // 4xx → hold (a fix makes it sendable); never retried, never dropped.
                Err(MemoryError::InvalidRequest(e)) => {
                    self.mark(id, "held", attempts, None, &e);
                    warn!("outbox: HELD row {id} for '{provider_id}' — 4xx, needs a config fix, not retried: {e}");
                    held += 1;
                }
                // 5xx / transport → retry with backoff, or exhaust.
                Err(e) => {
                    let reason = match &e {
                        MemoryError::Unavailable(s) | MemoryError::Backend(s) => s.clone(),
                        other => format!("{other:?}"),
                    };
                    let n = attempts + 1;
                    if n >= self.cfg.max_attempts {
                        self.mark(id, "exhausted", n, None, &reason);
                        warn!("outbox: EXHAUSTED row {id} for '{provider_id}' after {n} attempts: {reason}");
                        exhausted += 1;
                    } else {
                        let next = now + backoff(n);
                        self.mark(id, "pending", n, Some(next), &reason);
                        debug!("outbox: retry row {id} for '{provider_id}' (attempt {n}) in {}s: {reason}",
                               backoff(n).num_seconds());
                        retrying += 1;
                    }
                }
            }
        }
        self.prune();
        if delivered + held + exhausted > 0 {
            info!("outbox drain: delivered={delivered} held={held} retrying={retrying} exhausted={exhausted}");
        }
        (delivered, held, retrying, exhausted)
    }

    fn delete(&self, id: i64) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute("DELETE FROM ingest_outbox WHERE id = ?1", params![id]);
        }
    }

    fn mark(&self, id: i64, state: &str, attempts: u32, next: Option<DateTime<Utc>>, err: &str) {
        if let Ok(conn) = self.conn.lock() {
            let next_s = next.map(|d| d.to_rfc3339())
                // held/exhausted never become due again; park next_attempt far out.
                .unwrap_or_else(|| (Utc::now() + Duration::days(3650)).to_rfc3339());
            let _ = conn.execute(
                "UPDATE ingest_outbox SET state = ?2, attempts = ?3, next_attempt_at = ?4, last_error = ?5
                 WHERE id = ?1",
                params![id, state, attempts as i64, next_s, err],
            );
        }
    }

    /// Prune any row older than `max_age_days`, whatever its state — the bound
    /// that stops held foreign-identity rows accumulating.
    fn prune(&self) {
        let cutoff = (Utc::now() - Duration::days(self.cfg.max_age_days)).to_rfc3339();
        if let Ok(conn) = self.conn.lock() {
            match conn.execute("DELETE FROM ingest_outbox WHERE created_at < ?1", params![cutoff]) {
                Ok(n) if n > 0 => info!("outbox: pruned {n} row(s) older than {} days", self.cfg.max_age_days),
                _ => {}
            }
        }
    }

    /// Spawn the background delivery loop. Runs until the process exits.
    pub fn spawn_worker(self: Arc<Self>) {
        let interval = std::time::Duration::from_secs(self.cfg.poll_interval_secs.max(1));
        tokio::spawn(async move {
            info!("ingest outbox worker started (poll={}s, max_attempts={}, max_age={}d)",
                  self.cfg.poll_interval_secs, self.cfg.max_attempts, self.cfg.max_age_days);
            loop {
                self.drain_due().await;
                tokio::time::sleep(interval).await;
            }
        });
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let conn = self.conn.lock().unwrap();
        let pending: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ingest_outbox WHERE state='pending'", [], |r| r.get(0)).unwrap();
        let held: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ingest_outbox WHERE state='held'", [], |r| r.get(0)).unwrap();
        (pending as usize, held as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::provider::{ForgetSelector, MemoryCapabilities, MemoryQuery, MemoryRetrieval};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    // A writer whose ingest returns a scripted sequence of outcomes.
    struct ScriptedWriter { id: String, calls: AtomicUsize, script: Vec<Result<usize, MemoryError>> }
    #[async_trait]
    impl MemoryProvider for ScriptedWriter {
        fn id(&self) -> &str { &self.id }
        fn capabilities(&self) -> MemoryCapabilities { MemoryCapabilities { write: true, ..MemoryCapabilities::none() } }
        fn locality(&self) -> crate::memory::provider::MemoryLocality { crate::memory::provider::MemoryLocality::Loopback }
        async fn health(&self) -> crate::memory::provider::MemoryHealth { crate::memory::provider::MemoryHealth::Ready }
        async fn retrieve(&self, _q: &MemoryQuery) -> Result<MemoryRetrieval, MemoryError> {
            Ok(MemoryRetrieval::Absent { reason: None })
        }
        async fn ingest(&self, _t: &TurnRecord) -> Result<usize, MemoryError> {
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            self.script.get(i).cloned().unwrap_or(Ok(1))
        }
        async fn forget(&self, _s: &ForgetSelector) -> Result<usize, MemoryError> { Ok(0) }
    }

    fn turn() -> TurnRecord {
        TurnRecord {
            user_id: "u".into(), username: Some("tarek".into()), email: None, agent_id: None,
            thread_id: None, channel: "web".into(), user_msg: "hi".into(),
            assistant_msg: "hey".into(), conversation_id: None, echo_refs: vec![], tool_turns: vec![], occurred_at: chrono::Utc::now(),
        }
    }

    fn outbox(w: Arc<dyn MemoryProvider>, cfg: OutboxConfig) -> (tempfile::TempDir, MemoryOutbox) {
        let dir = tempdir().unwrap();
        let ob = MemoryOutbox::open(&dir.path().join("ob.db"), vec![w], cfg).unwrap();
        (dir, ob)
    }

    #[tokio::test]
    async fn success_deletes_the_row() {
        let w = Arc::new(ScriptedWriter { id: "ext".into(), calls: AtomicUsize::new(0), script: vec![Ok(1)] });
        let (_d, ob) = outbox(w, OutboxConfig::default());
        ob.enqueue("ext", &turn()).unwrap();
        assert_eq!(ob.counts(), (1, 0));
        let (delivered, ..) = ob.drain_due().await;
        assert_eq!(delivered, 1);
        assert_eq!(ob.counts(), (0, 0), "delivered row is removed");
    }

    #[tokio::test]
    async fn four_xx_is_held_not_dropped_not_retried() {
        let w = Arc::new(ScriptedWriter { id: "ext".into(), calls: AtomicUsize::new(0),
            script: vec![Err(MemoryError::InvalidRequest("HTTP 400 user_not_configured".into()))] });
        let (_d, ob) = outbox(w.clone(), OutboxConfig::default());
        ob.enqueue("ext", &turn()).unwrap();
        let (delivered, held, ..) = ob.drain_due().await;
        assert_eq!((delivered, held), (0, 1));
        assert_eq!(ob.counts(), (0, 1), "held, not pending, not gone");
        // A second drain must NOT re-attempt a held row.
        ob.drain_due().await;
        assert_eq!(w.calls.load(Ordering::SeqCst), 1, "held row is not retried");
    }

    #[tokio::test]
    async fn five_xx_retries_then_succeeds() {
        let w = Arc::new(ScriptedWriter { id: "ext".into(), calls: AtomicUsize::new(0),
            script: vec![Err(MemoryError::Backend("HTTP 503".into())), Ok(1)] });
        // poll/backoff irrelevant — we drive drain manually, but force the retry due.
        let (_d, ob) = outbox(w.clone(), OutboxConfig { max_attempts: 8, ..OutboxConfig::default() });
        ob.enqueue("ext", &turn()).unwrap();
        ob.drain_due().await;                       // 1st: 503 → pending, backed off
        assert_eq!(ob.counts(), (1, 0));
        // Make it due again and drain → success.
        { let c = ob.conn.lock().unwrap();
          c.execute("UPDATE ingest_outbox SET next_attempt_at = ?1", params![Utc::now().to_rfc3339()]).unwrap(); }
        let (delivered, ..) = ob.drain_due().await;
        assert_eq!(delivered, 1);
        assert_eq!(ob.counts(), (0, 0));
    }

    #[tokio::test]
    async fn retryable_exhausts_after_max_attempts() {
        let w = Arc::new(ScriptedWriter { id: "ext".into(), calls: AtomicUsize::new(0),
            script: vec![Err(MemoryError::Unavailable("down".into())); 5] });
        let (_d, ob) = outbox(w, OutboxConfig { max_attempts: 3, ..OutboxConfig::default() });
        ob.enqueue("ext", &turn()).unwrap();
        for _ in 0..3 {
            { let c = ob.conn.lock().unwrap();
              c.execute("UPDATE ingest_outbox SET next_attempt_at = ?1 WHERE state='pending'",
                        params![Utc::now().to_rfc3339()]).unwrap(); }
            ob.drain_due().await;
        }
        // No longer pending (exhausted); not held.
        assert_eq!(ob.counts(), (0, 0));
        let c = ob.conn.lock().unwrap();
        let ex: i64 = c.query_row("SELECT COUNT(*) FROM ingest_outbox WHERE state='exhausted'", [], |r| r.get(0)).unwrap();
        assert_eq!(ex, 1);
    }

    #[tokio::test]
    async fn prune_removes_aged_rows_whatever_their_state() {
        let w = Arc::new(ScriptedWriter { id: "ext".into(), calls: AtomicUsize::new(0),
            script: vec![Err(MemoryError::InvalidRequest("400".into()))] });
        let (_d, ob) = outbox(w, OutboxConfig { max_age_days: 7, ..OutboxConfig::default() });
        ob.enqueue("ext", &turn()).unwrap();
        ob.drain_due().await; // → held
        assert_eq!(ob.counts(), (0, 1));
        // Backdate the row past the age bound; prune on next drain.
        { let c = ob.conn.lock().unwrap();
          let old = (Utc::now() - Duration::days(30)).to_rfc3339();
          c.execute("UPDATE ingest_outbox SET created_at = ?1", params![old]).unwrap(); }
        ob.drain_due().await;
        assert_eq!(ob.counts(), (0, 0), "aged held row is pruned");
    }
}
