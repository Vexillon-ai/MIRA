// SPDX-License-Identifier: AGPL-3.0-or-later

// src/web/config_watcher.rs
//! LiveConfig — thread-safe, hot-reloadable config wrapper.

use std::sync::Arc;

use tokio::sync::{watch, RwLock};

use crate::config::MiraConfig;
use crate::MiraError;

// ── LiveConfig ────────────────────────────────────────────────────────────────

pub struct LiveConfig {
    inner: Arc<RwLock<MiraConfig>>,
    tx:    watch::Sender<Arc<MiraConfig>>,
    /// Kept alive so the sender never becomes disconnected.
    _rx:   watch::Receiver<Arc<MiraConfig>>,
}

impl LiveConfig {
    pub fn new(config: MiraConfig) -> Self {
        let arc = Arc::new(config);
        let (tx, _rx) = watch::channel(Arc::clone(&arc));
        Self {
            inner: Arc::new(RwLock::new((*arc).clone())),
            tx,
            _rx,
        }
    }

    /// Read a snapshot of the current config.
    pub async fn get(&self) -> Arc<MiraConfig> {
        Arc::new(self.inner.read().await.clone())
    }

    /// Subscribe to config changes.
    pub fn subscribe(&self) -> watch::Receiver<Arc<MiraConfig>> {
        self.tx.subscribe()
    }

    /// Validate, persist to disk, and broadcast the new config.
    pub async fn update(&self, new_config: MiraConfig) -> Result<(), MiraError> {
        new_config
            .save()
            .map_err(|e| MiraError::ConfigError(e.to_string()))?;

        let arc = Arc::new(new_config.clone());
        *self.inner.write().await = new_config;
        let _ = self.tx.send(arc);
        Ok(())
    }
}
