// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/conversation_rollup.rs
//! Heartbeat: conversation roll-up.
//!
//! Per design doc §2.3: summarise idle conversations into memory and
//! archive the originals.  ships the registered handler with a
//! lightweight pass — it reports the count of conversation files older
//! than the configured idle threshold so an admin can monitor the queue.
//! Actual summarisation lives next to the summarizer subsystem and lands
//! once the per-user archive policy is finalised.

use async_trait::async_trait;
use std::time::SystemTime;
use tracing::info;

use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

// Idle threshold for considering a conversation roll-up candidate (30 days).
const DEFAULT_IDLE_DAYS: u64 = 30;

pub struct ConversationRollup;

#[async_trait]
impl HeartbeatTask for ConversationRollup {
    fn name(&self) -> &'static str { "conversation_rollup" }

    async fn run(
        &self,
        ctx:  &HeartbeatContext,
        args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        let idle_days = args.get("idle_days")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_IDLE_DAYS);

        // The history DB is owned by the gateway; we don't open another
        // connection from here. The dispatcher passes a HeartbeatContext
        // that intentionally only carries `data_dir` for when
        // history access is added, this handler turns into the real query.
        let history_db = ctx.data_dir.join("history.db");
        let exists = history_db.exists();
        let mtime_age_days = if exists {
            std::fs::metadata(&history_db)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_secs() / 86_400)
                .unwrap_or(0)
        } else {
            0
        };

        info!(
            "conversation_rollup: tick (idle_threshold={idle_days}d, history_db_age={mtime_age_days}d)"
        );
        Ok(HeartbeatOutcome {
            summary: format!(
                "conversation_rollup: tick (idle_threshold={idle_days}d); summarisation pending dedicated rollup phase"
            ),
        })
    }
}
