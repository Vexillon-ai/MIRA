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
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
) -> axum::response::Response {
    let config   = live_cfg.get().await;
    let log_path = config.log_file_path();

    // Initial lines cap — bytes to seek back from EOF.
    const TAIL_BYTES: u64 = 32_768; // ~32 KB covers ~100 typical log lines

    type LogState = Option<(tokio::fs::File, u64)>;
    type LogItem  = Result<Event, std::convert::Infallible>;

    let s = stream::unfold(None::<(tokio::fs::File, u64)>, move |state: LogState| {
        let log_path = log_path.clone();
        async move {
            let result: Option<(LogItem, LogState)> = match state {
                // ── First call: open file, send tail ─────────────────────────
                None => {
                    let mut file = match tokio::fs::File::open(&log_path).await {
                        Ok(f)  => f,
                        Err(e) => {
                            warn!("logs_stream: cannot open {:?}: {}", log_path, e);
                            let ev = Event::default()
                                .event("error")
                                .data(format!("Cannot open log file: {}", e));
                            return Some((Ok(ev), None));
                        }
                    };

                    let file_len = file.seek(tokio::io::SeekFrom::End(0)).await.unwrap_or(0);
                    let start    = file_len.saturating_sub(TAIL_BYTES);
                    if start > 0 {
                        let _ = file.seek(tokio::io::SeekFrom::Start(start)).await;
                    } else {
                        let _ = file.seek(tokio::io::SeekFrom::Start(0)).await;
                    }

                    let mut buf = Vec::with_capacity(TAIL_BYTES as usize);
                    let _ = file.read_to_end(&mut buf).await;
                    let pos = file.seek(tokio::io::SeekFrom::Current(0)).await.unwrap_or(file_len);

                    let text = String::from_utf8_lossy(&buf);
                    let lines: Vec<&str> = text.lines().collect();
                    let skip = if start > 0 && !lines.is_empty() { 1 } else { 0 };
                    let batch = lines[skip..].join("\n");

                    let ev = Event::default().event("init").data(batch);
                    Some((Ok(ev), Some((file, pos))))
                }

                // ── Subsequent calls: poll for new content ────────────────────
                Some((mut file, pos)) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;

                    let _ = file.seek(tokio::io::SeekFrom::Start(pos)).await;
                    let mut buf = Vec::new();
                    let _ = file.read_to_end(&mut buf).await;

                    if buf.is_empty() {
                        let ev = Event::default().comment("ping");
                        return Some((Ok(ev), Some((file, pos))));
                    }

                    let new_pos = file.seek(tokio::io::SeekFrom::Current(0)).await.unwrap_or(pos);
                    let text    = String::from_utf8_lossy(&buf).into_owned();
                    let ev      = Event::default().event("lines").data(text);
                    Some((Ok(ev), Some((file, new_pos))))
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
