// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/admin.rs
//! Admin-only server control endpoints.
//!
//! `restart_handler` triggers a graceful shutdown; a supervisor (systemd,
//! forever, a shell `while` loop, etc.) is expected to relaunch the binary.
//! This is the mechanism the frontend's "Restart server" button uses after
//! editing channel accounts — daemons are spawned at boot so a restart is
//! required for changes to take effect.
//!
//! `consolidator_run_now` runs the sleep-like memory consolidator (Phases C,
//! A, D — see design-docs/memory-research-2026.md §5) on-demand for every user,
//! independently of the per-phase config flags that gate the nightly job.
//! Lets operators eyeball what the consolidator would do without waiting for
//! the hourly rollup tick or flipping flags on a quiet user.

use std::sync::Arc;

use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use tokio::sync::Notify;
use tracing::info;

use crate::agent::AgentCore;
use crate::auth::{AdminUser, LocalAuthService};
use crate::web::config_watcher::LiveConfig;

/// Signal the Gateway to stop serving. The server completes in-flight
/// requests, then exits — the process must be restarted externally.
pub async fn restart_handler(
    AdminUser(caller):    AdminUser,
    Extension(shutdown):  Extension<Arc<Notify>>,
) -> impl IntoResponse {
    info!(user = %caller.username, "Restart requested via API — signalling shutdown");
    // Delay briefly so the 202 response flushes before axum tears down.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        shutdown.notify_waiters();
    });
    (StatusCode::ACCEPTED, "restart scheduled")
}

/// Per-phase counts returned by `consolidator_run_now`. Fields stay zero when
/// nothing fired in that phase (so the UI can show "no work to do" honestly).
#[derive(Debug, serde::Serialize)]
pub struct ConsolidatorRunResult {
    /// Users iterated. Always = total user count when triggered manually.
    pub users_processed: usize,
    /// Phase C — contradiction resolution.
    pub contradictions_groups: usize,
    pub contradictions_edges_closed: usize,
    /// Phase A — entity dedup.
    pub entities_merged: usize,
    pub entity_edges_repointed: usize,
    /// Phase D — importance scoring (count of edges scored).
    pub importance_edges_scored: usize,
    /// Thresholds used (echoed back for the UI to confirm).
    pub entity_dedup_ratio: f64,
    pub importance_half_life_days: f64,
}

/// Run all three consolidator phases (C → A → D) on every user, regardless of
/// the per-phase config flags. Same order as the nightly tick (`memory.rollup`)
/// so the flag-on production behaviour matches what this manual trigger does.
/// Admin-only — iterates every user and writes to their graph DB.
///
/// Synchronous: returns when all users have been processed. For a few thousand
/// memories per user this completes in well under a second per user (pure SQL,
/// no LLM calls). Returns the per-phase totals as JSON so the UI can show
/// "merged X, resolved Y, scored Z" without a follow-up call.
pub async fn consolidator_run_now(
    AdminUser(caller):       AdminUser,
    Extension(agent_core):   Extension<Arc<AgentCore>>,
    Extension(auth):         Extension<Arc<LocalAuthService>>,
    Extension(live_config):  Extension<Arc<LiveConfig>>,
) -> Result<Json<ConsolidatorRunResult>, (StatusCode, String)> {
    let cfg = live_config.get().await;
    let ratio = cfg.memory.consolidation.entity_dedup_ratio;
    let half_life = cfg.memory.consolidation.importance_half_life_days;

    let users = auth.list_users()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("list_users: {}", e)))?;

    info!(
        user = %caller.username,
        "consolidator: manual run triggered by admin (will process {} user(s))",
        users.len(),
    );

    let mut result = ConsolidatorRunResult {
        users_processed: 0,
        contradictions_groups: 0,
        contradictions_edges_closed: 0,
        entities_merged: 0,
        entity_edges_repointed: 0,
        importance_edges_scored: 0,
        entity_dedup_ratio: ratio,
        importance_half_life_days: half_life,
    };

    for user in &users {
        // Same order as the nightly tick: contradictions → dedup → importance.
        // (C first so dedup's "more-edges-wins" tiebreak sees post-resolution
        // counts; D last so it scores the cleaned graph.)
        let (cg, cc) = agent_core.memory.consolidate_contradictions(&user.id);
        let (em, er) = agent_core.memory.consolidate_entities(&user.id, ratio);
        let scored   = agent_core.memory.consolidate_importance(&user.id, half_life);

        result.contradictions_groups       += cg;
        result.contradictions_edges_closed += cc;
        result.entities_merged             += em;
        result.entity_edges_repointed      += er;
        result.importance_edges_scored     += scored;
        result.users_processed             += 1;
    }

    info!(
        user = %caller.username,
        "consolidator: manual run complete — users={} contradictions(groups/edges)={}/{} entities(merged/repointed)={}/{} importance(scored)={}",
        result.users_processed,
        result.contradictions_groups, result.contradictions_edges_closed,
        result.entities_merged, result.entity_edges_repointed,
        result.importance_edges_scored,
    );

    Ok(Json(result))
}
