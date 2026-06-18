// SPDX-License-Identifier: AGPL-3.0-or-later

// src/automations/heartbeats/embedding_refresh.rs
//! Heartbeat: embedding refresh.
//!
//! Per design doc §2.3: refresh stored embeddings against the current model
//! when the embedder identity changes (different provider, different model
//! name, different output dimension).  ships the registered handler;
//! the actual diff/recompute pass lives alongside the embedding subsystem
//! and runs only when an embedding-config rotation is detected — which
//! requires the model-identity bookkeeping the memory phase will introduce.

use async_trait::async_trait;
use tracing::info;

use crate::MiraError;

use super::{HeartbeatContext, HeartbeatOutcome, HeartbeatTask};

pub struct EmbeddingRefresh;

#[async_trait]
impl HeartbeatTask for EmbeddingRefresh {
    fn name(&self) -> &'static str { "embedding_refresh" }

    async fn run(
        &self,
        _ctx:  &HeartbeatContext,
        _args: &serde_json::Value,
    ) -> Result<HeartbeatOutcome, MiraError> {
        info!("embedding_refresh: tick (no-op until model-identity bookkeeping lands)");
        Ok(HeartbeatOutcome {
            summary: "embedding_refresh: tick; no rotation detected".to_string(),
        })
    }
}
