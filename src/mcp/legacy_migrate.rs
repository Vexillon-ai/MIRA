// SPDX-License-Identifier: AGPL-3.0-or-later

// src/mcp/legacy_migrate.rs
//! One-shot migration from the legacy `[mcp.servers]` config block to
//! per-user [`mcp_servers`] rows. Runs at gateway startup after the
//! store opens and the admin user is known. No-op when the store is
//! already populated, so it's safe to call on every boot.
//!
//! Same posture as `channel_accounts::legacy_migrate` — the goal is
//! to let pre-Slice-4 installs keep working without the operator
//! re-entering their MCP entries by hand.

use tracing::{info, warn};

use crate::MiraError;
use crate::config::McpConfig;
use crate::mcp::store::{McpServerStore, NewMcpServer};

/// Seed `mcp_servers` from `config.mcp.servers` when the store is
/// empty. Existing deployments that already use the per-user API are
/// a no-op (the count check short-circuits).
pub fn migrate_if_empty(
    store:    &McpServerStore,
    config:   &McpConfig,
    admin_id: &str,
) -> Result<(), MiraError> {
    if store.count_all()? > 0 {
        return Ok(());
    }
    if config.servers.is_empty() {
        info!("mcp legacy migrate: nothing to seed");
        return Ok(());
    }

    let mut seeded = 0usize;
    for entry in &config.servers {
        let new = NewMcpServer {
            name:      entry.name.clone(),
            transport: entry.transport.clone(),
            command:   entry.command.clone(),
            args:      entry.args.clone(),
            env:       entry.env.clone(),
            url:       entry.url.clone(),
            enabled:   entry.enabled,
            sampling_enabled: entry.sampling_enabled,
        };
        match store.create(admin_id, new) {
            Ok(_) => {
                seeded += 1;
                info!("mcp legacy migrate: seeded '{}' for admin", entry.name);
            }
            Err(e) => warn!("mcp legacy migrate: '{}' failed: {e}", entry.name),
        }
    }
    info!("mcp legacy migrate: seeded {seeded} entries from config.mcp.servers");
    Ok(())
}
