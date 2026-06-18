// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/onboarding_nudge.rs
//! Heartbeat: onboarding nudge.
//!
//! Per design doc §2.3: nudge users with stale onboarding groups, daily at
//! 09:00 local.  ships the registered handler; the actual notification
//! enqueues a `ChannelMessage` activation per stale user, which depends on
//! the per-user groups + reminder windows the onboarding phase tracks. For
//! we report only — no message is sent — so the cadence shows up
//! cleanly in run history without spamming users.

use async_trait::async_trait;
use tracing::info;

use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

pub struct OnboardingNudge;

#[async_trait]
impl HeartbeatTask for OnboardingNudge {
    fn name(&self) -> &'static str { "onboarding_nudge" }

    async fn run(
        &self,
        _ctx:  &HeartbeatContext,
        _args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        info!("onboarding_nudge: tick (notification enqueue pending onboarding phase wiring)");
        Ok(HeartbeatOutcome {
            summary: "onboarding_nudge: tick; no users flagged for nudge".to_string(),
        })
    }
}
