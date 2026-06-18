// SPDX-License-Identifier: AGPL-3.0-or-later

// src/history/indexer.rs
//! Background embedder for the `message_vectors` table.
//!
//! The indexer wakes on a fixed interval, pulls a batch of un-indexed
//! messages via [`HistoryStore::fetch_unindexed_messages`], embeds each
//! through the memory system's already-loaded provider, and inserts the
//! resulting vectors. Running against an empty queue is cheap — the LEFT
//! JOIN returns no rows, we sleep, and loop.
//!
//! Design notes:
//! - We reuse [`MemorySystem`]'s embedding provider instead of loading a
//!   second copy of the ~24 MB fastembed model. That keeps RAM predictable
//!   and means any provider change (config reload) takes effect everywhere.
//! - The indexer is fire-and-forget; any error is logged and the next tick
//!   picks up where we left off. There is no retry loop per message — if a
//!   row fails twice we'll keep re-trying on every tick, which is fine for
//!   transient errors and cheap enough to ignore for permanent ones.
//! - Embedding input is truncated to `MAX_CHARS`. fastembed's default
//!   context window is ~512 tokens; letting 10 KB messages through would
//!   silently truncate inside the provider anyway, with worse locality.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::history::storage::{HistoryStore, MessageVectorRow};
use crate::memory::MemorySystem;

/// Max chars fed into the embedding provider per message. Longer messages
/// are truncated from the start (beginnings are usually more distinctive
/// than trailing boilerplate). 2000 chars ≈ 500 tokens, the standard
/// context window for the BGE-small / MiniLM-L6 family.
const MAX_CHARS: usize = 2000;

/// Runtime configuration for [`MessageIndexer`]. Sourced from
/// `config.memory.indexer` but passed in explicitly so tests can override.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// How often to poll for new messages.
    pub interval: Duration,
    /// Max messages embedded per tick. Keeps a single pass bounded so the
    /// indexer can't starve a busy provider on first-run backfill.
    pub batch_size: i64,
    /// Roles to skip (e.g. `["tool", "system"]`). Matched case-sensitively
    /// against the stored `role` column.
    pub skip_roles: Vec<String>,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            interval:   Duration::from_secs(60),
            batch_size: 32,
            skip_roles: vec!["tool".to_owned(), "system".to_owned()],
        }
    }
}

/// Handle to a running indexer. Dropping this does *not* stop the task —
/// the indexer keeps running as long as its host process is alive. Call
/// `abort` explicitly when you need to shut it down (tests, reconfig).
pub struct MessageIndexer {
    pub handle: JoinHandle<()>,
}

impl MessageIndexer {
    /// Spawn the background indexer. Returns immediately.
    pub fn start(
        history: Arc<HistoryStore>,
        memory:  Arc<MemorySystem>,
        config:  IndexerConfig,
    ) -> Self {
        let handle = tokio::spawn(async move {
            // A short initial warmup so the model and backend have time to
            // finish loading before we hammer them. The 2s value is
            // arbitrary — it just moves the first batch out of the critical
            // startup path.
            tokio::time::sleep(Duration::from_secs(2)).await;
            run_forever(history, memory, config).await;
        });
        Self { handle }
    }

    /// Stop the running task. Returns after the current batch finishes.
    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

async fn run_forever(
    history: Arc<HistoryStore>,
    memory:  Arc<MemorySystem>,
    config:  IndexerConfig,
) {
    // Resolve the embedding label once at startup. If semantic is off, the
    // indexer has nothing to do — log and return so the spawned task is a
    // no-op instead of a spin loop.
    let Some(label) = memory.embedding_model_label().await else {
        info!("Transcript indexer: semantic embedding disabled — indexer idle");
        return;
    };
    let Some(dim) = memory.embedding_dim().await else {
        info!("Transcript indexer: embedding dim unavailable — indexer idle");
        return;
    };
    info!(
        "Transcript indexer started (interval={}s, batch={}, model='{}', dim={})",
        config.interval.as_secs(), config.batch_size, label, dim,
    );

    loop {
        let written = index_one_batch(&history, &memory, &label, dim, &config).await;
        if written == 0 {
            // Idle tick — sleep the full interval. Busy ticks still sleep;
            // this just keeps the single-pass batching loop simple.
            tokio::time::sleep(config.interval).await;
        } else {
            // Shorter pause when actively backfilling so first-run indexing
            // doesn't stretch out over hours on large histories.
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Index one batch. Exposed at the module level (not on `MessageIndexer`)
/// so tests can drive a single pass deterministically without spawning.
pub async fn index_one_batch(
    history: &Arc<HistoryStore>,
    memory:  &Arc<MemorySystem>,
    label:   &str,
    dim:     usize,
    config:  &IndexerConfig,
) -> usize {
    let batch = match history.fetch_unindexed_messages(config.batch_size, &config.skip_roles) {
        Ok(b)  => b,
        Err(e) => {
            warn!("Transcript indexer: fetch_unindexed_messages failed: {}", e);
            return 0;
        }
    };
    if batch.is_empty() { return 0; }

    debug!("Transcript indexer: processing {} messages", batch.len());
    let mut written = 0usize;

    for msg in batch {
        // Char-safe truncation — slicing by byte index can split a multibyte
        // codepoint (e.g. box-drawing `│` at 3 bytes) and panic.
        let truncated: String;
        let input: &str = if msg.content.len() > MAX_CHARS {
            truncated = msg.content.chars().take(MAX_CHARS).collect();
            &truncated
        } else {
            &msg.content
        };

        let vector = match memory.embed(input).await {
            Some(v) if v.len() == dim => v,
            Some(v) => {
                warn!(
                    "Transcript indexer: embedding dim mismatch (got {}, expected {}) — skipping message {}",
                    v.len(), dim, msg.message_id,
                );
                continue;
            }
            None => {
                warn!("Transcript indexer: embedding returned None — stopping this batch");
                break;
            }
        };

        let row = MessageVectorRow {
            message_id:      &msg.message_id,
            conversation_id: &msg.conversation_id,
            user_id:         &msg.user_id,
            role:            &msg.role,
            created_at:      msg.created_at,
            dim,
            model:           label,
            vector:          &vector,
        };
        match history.insert_message_vector(&row) {
            Ok(()) => written += 1,
            Err(e) => warn!(
                "Transcript indexer: insert failed for message {}: {}",
                msg.message_id, e,
            ),
        }
    }

    if written > 0 {
        info!("Transcript indexer: wrote {} vectors", written);
    }
    written
}
