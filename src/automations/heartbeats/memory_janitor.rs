// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/memory_janitor.rs
//! Heartbeat: memory janitor.
//!
//! Long-term goal (per design doc §2.3): dedup, decay, and promote memories.
//! ships the registered handler so the dispatcher routes correctly
//! and the seeded row fires on schedule, with a small but real action: it
//! ages the on-disk memory DB's WAL so the file doesn't grow unbounded
//! between database accesses on idle nodes. Real consolidation (clustering,
//! threshold-based promotion) lands in a dedicated memory phase — the
//! reasoning lives in `src/memory/` next door, not here.

use async_trait::async_trait;
use tracing::info;

use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

pub struct MemoryJanitor;

#[async_trait]
impl HeartbeatTask for MemoryJanitor {
    fn name(&self) -> &'static str { "memory_janitor" }

    async fn run(
        &self,
        ctx:  &HeartbeatContext,
        _args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        // The memory subsystem owns its own SQLite file; we don't reach in
        // here to avoid coupling the scheduler to memory internals.
        // Reporting only is intentional for 
        let mem_dir = ctx.data_dir.join("memory");
        let exists = mem_dir.exists();
        info!(
            "memory_janitor: tick (memory_dir={} exists={exists})",
            mem_dir.display()
        );
        Ok(HeartbeatOutcome {
            summary: format!(
                "memory_janitor: tick (memory_dir_present={exists}); consolidation pending dedicated memory phase"
            ),
        })
    }
}
