// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/oauth_token_refresh.rs
//! Heartbeat: OAuth token refresh.
//!
//! Per design doc §2.3: refresh OAuth tokens before expiry.  ships
//! the registered handler; actual refresh requires a per-channel token
//! store that doesn't yet exist (calendar uses its own provider hook). When
//! the OAuth subsystem lands, this handler iterates that store and calls
//! the per-provider refresh. Hourly cadence is fine for that — most
//! provider tokens have a 60–90 minute lifetime.

use async_trait::async_trait;
use tracing::info;

use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

pub struct OauthTokenRefresh;

#[async_trait]
impl HeartbeatTask for OauthTokenRefresh {
    fn name(&self) -> &'static str { "oauth_token_refresh" }

    async fn run(
        &self,
        _ctx:  &HeartbeatContext,
        _args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        info!("oauth_token_refresh: tick (no-op until generic OAuth token store exists)");
        Ok(HeartbeatOutcome {
            summary: "oauth_token_refresh: tick; nothing to refresh".to_string(),
        })
    }
}
