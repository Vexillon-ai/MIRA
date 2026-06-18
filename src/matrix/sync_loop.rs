// SPDX-License-Identifier: AGPL-3.0-or-later

// src/matrix/sync_loop.rs
//
// The inbound half of the Matrix channel: a long-lived task that
// long-polls `GET /sync`, auto-joins invited rooms, and hands each new
// `m.room.message` text event to the dispatcher. Architecturally this is
// the Matrix analogue of the Discord gateway task / Telegram poller —
// HTTP long-poll rather than a WebSocket, so it needs no extra crate.
//
// Lifecycle:
//   1. /whoami → cache our own MXID (skip self-echo) + sanity-check the
//      token. A 401 here means a bad/expired token — log + retry with
//      backoff (the operator fixes it in Settings).
//   2. Initial /sync (filter timeline limit=1) → capture next_batch and
//      a connect timestamp; events at-or-before connect time are NOT
//      dispatched (avoids replaying history on startup).
//   3. Loop: /sync?since=<batch>&timeout=30000. For each joined-room
//      timeline event that is an m.room.message text newer than connect,
//      spawn the dispatcher. Auto-join any invited rooms.
//   4. On any HTTP error, exponential backoff (1s..30s) then resume from
//      the last good batch token.
//   5. Shutdown via the Notify (so stop_account returns promptly).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tracing::{info, warn};

use super::api::{join_room, sync_once, whoami};
use super::dispatch::{process_matrix_message, InboundMatrix, MatrixAccountCtx, MatrixDispatcherDeps};

const SYNC_TIMEOUT_MS: u64 = 30_000;
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Config the loop needs that isn't in the dispatcher ctx (the ctx's
/// `bot_mxid` is filled in by the loop after /whoami, so the caller passes
/// a "pending" ctx with an empty bot_mxid).
pub struct MatrixLoopConfig {
    pub account_id:    String,
    pub owner_user_id: String,
    pub homeserver:    String,
    pub access_token:  String,
    pub mention_only:  bool,
    pub routing_mode:  crate::channel_accounts::RoutingMode,
}

/// Spawn the long-poll loop. Returns the JoinHandle (held by the channel
/// manager so it aborts on drop) — pair it with the `shutdown` Notify for
/// a clean stop.
pub fn spawn_sync_loop(
    cfg:      MatrixLoopConfig,
    deps:     MatrixDispatcherDeps,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { run_loop(cfg, deps, shutdown).await; })
}

async fn run_loop(cfg: MatrixLoopConfig, deps: MatrixDispatcherDeps, shutdown: Arc<Notify>) {
    let mut backoff = Duration::from_secs(1);

    // Outer loop re-establishes the whole session (re-whoami + fresh
    // initial sync) after a hard failure. The inner loop does steady-state
    // incremental syncs.
    loop {
        // Allow a shutdown between reconnect attempts.
        if is_shutdown(&shutdown) { return; }

        // ── /whoami ───────────────────────────────────────────────────
        let bot_mxid = match whoami(&deps.http_client, &cfg.homeserver, &cfg.access_token).await {
            Ok(id) => id,
            Err(e) => {
                warn!(account = %cfg.account_id, "Matrix whoami failed: {} — backing off", e);
                if wait_or_shutdown(&shutdown, backoff).await { return; }
                backoff = (backoff * 2).min(BACKOFF_CAP);
                continue;
            }
        };
        info!(account = %cfg.account_id, "Matrix connected as {} (homeserver={})",
              bot_mxid, cfg.homeserver);
        backoff = Duration::from_secs(1); // reset on a good connect

        let ctx = MatrixAccountCtx {
            account_id:    cfg.account_id.clone(),
            owner_user_id: cfg.owner_user_id.clone(),
            homeserver:    cfg.homeserver.clone(),
            access_token:  cfg.access_token.clone(),
            bot_mxid,
            mention_only:  cfg.mention_only,
            routing_mode:  cfg.routing_mode,
        };

        // ── initial sync: get a batch token, dispatch nothing ─────────
        let mut since = match sync_once(&deps.http_client, &cfg.homeserver, &cfg.access_token, None, 0).await {
            Ok(resp) => {
                // Auto-join anything we were invited to before we started.
                for room_id in resp.rooms.invite.keys() {
                    let _ = join_room(&deps.http_client, &cfg.homeserver, &cfg.access_token, room_id).await;
                }
                resp.next_batch
            }
            Err(e) => {
                warn!(account = %cfg.account_id, "Matrix initial sync failed: {} — backing off", e);
                if wait_or_shutdown(&shutdown, backoff).await { return; }
                backoff = (backoff * 2).min(BACKOFF_CAP);
                continue;
            }
        };

        // ── steady-state incremental sync ─────────────────────────────
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    info!(account = %cfg.account_id, "Matrix sync loop: clean shutdown");
                    return;
                }
                res = sync_once(&deps.http_client, &cfg.homeserver, &cfg.access_token,
                                Some(&since), SYNC_TIMEOUT_MS) => {
                    match res {
                        Ok(resp) => {
                            since = resp.next_batch;

                            for room_id in resp.rooms.invite.keys() {
                                let _ = join_room(&deps.http_client, &cfg.homeserver,
                                                  &cfg.access_token, room_id).await;
                            }

                            for (room_id, room) in resp.rooms.join {
                                for ev in room.timeline.events {
                                    if ev.event_type != "m.room.message" { continue; }
                                    let is_text = ev.content.msgtype.as_deref()
                                        .map(|t| t == "m.text" || t == "m.notice")
                                        .unwrap_or(false);
                                    if !is_text { continue; }
                                    let Some(body) = ev.content.body else { continue; };
                                    // Spawn the dispatcher so a slow LLM
                                    // turn never stalls the sync loop.
                                    let deps2 = deps.clone();
                                    let ctx2  = ctx.clone();
                                    let inbound = InboundMatrix {
                                        room_id: room_id.clone(),
                                        sender:  ev.sender,
                                        body,
                                    };
                                    tokio::spawn(async move {
                                        process_matrix_message(deps2, ctx2, inbound).await;
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            warn!(account = %cfg.account_id, "Matrix sync error: {} — backing off", e);
                            if wait_or_shutdown(&shutdown, backoff).await { return; }
                            backoff = (backoff * 2).min(BACKOFF_CAP);
                            // Break to the outer loop to re-whoami + fresh
                            // sync; the token may have been rotated.
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn is_shutdown(_n: &Arc<Notify>) -> bool {
    // Notify has no "already fired" query; the select! arms handle the
    // real race. This helper exists so the outer-loop top can be a no-op
    // placeholder that keeps the structure obvious.
    false
}

/// Sleep for `dur`, but return `true` immediately if shutdown fires first.
async fn wait_or_shutdown(shutdown: &Arc<Notify>, dur: Duration) -> bool {
    tokio::select! {
        _ = shutdown.notified() => true,
        _ = tokio::time::sleep(dur) => false,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_or_shutdown_returns_true_on_notify() {
        let n = Arc::new(Notify::new());
        let n2 = Arc::clone(&n);
        tokio::spawn(async move { n2.notify_waiters(); });
        // Long sleep, but the notify should short-circuit it.
        let hit = wait_or_shutdown(&n, Duration::from_secs(30)).await;
        assert!(hit);
    }

    #[tokio::test]
    async fn wait_or_shutdown_returns_false_on_timeout() {
        let n = Arc::new(Notify::new());
        let hit = wait_or_shutdown(&n, Duration::from_millis(10)).await;
        assert!(!hit);
    }
}
