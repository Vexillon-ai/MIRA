// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/logs.rs
//! GET /api/logs/stream — SSE tail of the MIRA log file.
//! GET/PUT /api/logs/level — runtime log level toggle (admin only).

use std::sync::Arc;
use std::time::Duration;

use axum::response::{IntoResponse, Sse};
use axum::response::sse::{Event, KeepAlive};
use axum::{http::StatusCode, Extension, Json};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{info, warn};

use crate::auth::AdminUser;
use crate::log_filter;
use crate::web::LiveConfig;

/// GET /api/logs/stream
///
/// Sends the last ~100 lines of the log file as individual SSE events, then
/// continuously polls for new content every 500 ms until the client disconnects.
pub async fn logs_stream(
    // ADMIN ONLY. The log file contains every user's messages (incl. Telegram
    // chats) in plaintext plus system internals, so a non-admin must never read
    // it. This endpoint previously had no auth extractor at all — it ignored the
    // `?token=` the client sent and was effectively open. `AdminUser` supports
    // the SSE query-token (middleware extract_bearer_token), so EventSource auth
    // still works; non-admins now get 403.
    _admin: AdminUser,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
) -> axum::response::Response {
    let config   = live_cfg.get().await;
    let log_path = config.log_file_path();

    // Initial lines cap — bytes to seek back from EOF.
    const TAIL_BYTES: u64 = 32_768; // ~32 KB covers ~100 typical log lines

    type LogItem = Result<Event, std::convert::Infallible>;

    // State is just the byte offset already delivered (`None` = first call).
    //
    // The file is re-opened by path on every poll rather than a handle being
    // held across iterations. This is what lets the tail follow log **rotation**
    // (`src/log_rotate.rs`): when the active file rolls, the fixed path points
    // at a fresh, shorter file. We detect that as `current_len < pos` and reset
    // to the start of the new file. Holding a handle would instead keep reading
    // the rolled-out inode, which never grows again. Re-opening at a 500 ms
    // cadence for an admin-only stream is negligible.
    let s = stream::unfold(None::<u64>, move |state: Option<u64>| {
        let log_path = log_path.clone();
        async move {
            let result: Option<(LogItem, Option<u64>)> = match state {
                // ── First call: open file, send the tail ─────────────────────
                None => {
                    let mut file = match tokio::fs::File::open(&log_path).await {
                        Ok(f)  => f,
                        Err(e) => {
                            warn!("logs_stream: cannot open {:?}: {}", log_path, e);
                            let ev = Event::default()
                                .event("error")
                                .data(format!("Cannot open log file: {}", e));
                            // Retry from offset 0 on the next poll so the stream
                            // self-heals once the writer creates the file, rather
                            // than ending or hot-looping without a sleep.
                            return Some((Ok(ev), Some(0)));
                        }
                    };

                    let file_len = file.seek(tokio::io::SeekFrom::End(0)).await.unwrap_or(0);
                    let start    = file_len.saturating_sub(TAIL_BYTES);
                    let _ = file.seek(tokio::io::SeekFrom::Start(start)).await;

                    let mut buf = Vec::with_capacity(TAIL_BYTES as usize);
                    let _ = file.read_to_end(&mut buf).await;

                    let text = String::from_utf8_lossy(&buf);
                    let lines: Vec<&str> = text.lines().collect();
                    // Drop the (likely partial) first line when we seeked mid-file.
                    let skip = if start > 0 && !lines.is_empty() { 1 } else { 0 };
                    let batch = lines[skip..].join("\n");

                    let ev = Event::default().event("init").data(batch);
                    Some((Ok(ev), Some(file_len)))
                }

                // ── Subsequent calls: poll for new content ────────────────────
                Some(pos) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;

                    let mut file = match tokio::fs::File::open(&log_path).await {
                        Ok(f)  => f,
                        Err(_) => {
                            // File briefly absent between the rename and reopen of
                            // a rotation — ping and retry at the same offset.
                            let ev = Event::default().comment("ping");
                            return Some((Ok(ev), Some(pos)));
                        }
                    };

                    let len = file.seek(tokio::io::SeekFrom::End(0)).await.unwrap_or(0);
                    // Shrink ⇒ the file rolled; restart from the top of the new one.
                    let read_from = if len < pos { 0 } else { pos };
                    let _ = file.seek(tokio::io::SeekFrom::Start(read_from)).await;

                    let mut buf = Vec::new();
                    let _ = file.read_to_end(&mut buf).await;

                    if buf.is_empty() {
                        let ev = Event::default().comment("ping");
                        return Some((Ok(ev), Some(len)));
                    }

                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let ev   = Event::default().event("lines").data(text);
                    Some((Ok(ev), Some(len)))
                }
            };
            result
        }
    });

    Sse::new(s)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ── /api/logs/level ──────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct LogLevelResponse {
    pub level:  String,
    pub levels: Vec<String>,
}

#[derive(Deserialize)]
pub struct SetLogLevelRequest {
    pub level: String,
}

/// GET /api/logs/level — current effective level + the set of accepted values.
pub async fn get_log_level(_admin: AdminUser) -> Json<LogLevelResponse> {
    Json(LogLevelResponse {
        level:  log_filter::current_level(),
        levels: log_filter::LEVELS.iter().map(|s| (*s).to_string()).collect(),
    })
}

/// PUT /api/logs/level — swap the active filter. Lives only for the lifetime
/// of the current process; restart restores `config.logging.level`.
pub async fn set_log_level(
    AdminUser(caller): AdminUser,
    Json(req):         Json<SetLogLevelRequest>,
) -> Result<Json<LogLevelResponse>, (StatusCode, String)> {
    log_filter::set_level(&req.level)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    info!(user = %caller.username, level = %req.level, "log level changed via API");
    Ok(Json(LogLevelResponse {
        level:  log_filter::current_level(),
        levels: log_filter::LEVELS.iter().map(|s| (*s).to_string()).collect(),
    }))
}
