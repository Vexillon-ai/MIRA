// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/weekly_reflection.rs
//! Heartbeat: weekly reflection.
//!
//! Design doc §2.3 originally describes this as a `Prompt` action ("review
//! the last 7 days…"). It is also seeded as an `Internal` task so the
//! handler is callable from the standard heartbeat flow — useful when
//! reflection should happen without burning the agent loop, e.g. during
//! quiet hours or for users who haven't enabled prompted reflection.
//!
//! ships a no-prompt placeholder: it logs the tick and reports a
//! summary. A user-facing reflection turn lands when an admin schedules a
//! `Prompt` action with cadence `0 0 18 ? * SUN` (already seeded by
//! default; the prompt action is now wired in this slice).

use async_trait::async_trait;
use tracing::info;

use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

pub struct WeeklyReflection;

#[async_trait]
impl HeartbeatTask for WeeklyReflection {
    fn name(&self) -> &'static str { "weekly_reflection" }

    async fn run(
        &self,
        _ctx:  &HeartbeatContext,
        _args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        info!("weekly_reflection: tick (placeholder — switch action_kind to prompt for live reflections)");
        Ok(HeartbeatOutcome {
            summary: "weekly_reflection: tick".to_string(),
        })
    }
}
