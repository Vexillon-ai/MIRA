// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/sessions.rs

use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Serialize;

use crate::agent::AgentCore;
use crate::auth::AdminUser;

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub session_id:    String,
    pub user_id:       String,
    pub channel:       String,
    pub created_at:    u64,
    pub last_active:   u64,
    pub message_count: usize,
}

/// GET /api/sessions — list all active in-memory sessions.
/// ADMIN ONLY: this lists EVERY user's sessions (user_id, channel, activity),
/// so a non-admin must not see it.
pub async fn list_sessions(
    _admin: AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Json<Vec<SessionResponse>> {
    let mut sessions: Vec<SessionResponse> = agent.sessions.list_all().await
        .into_iter()
        .map(|s| SessionResponse {
            session_id:    s.session_id,
            user_id:       s.user_id,
            channel:       s.channel,
            created_at:    s.created_at,
            last_active:   s.last_active,
            message_count: s.conversation_history.len(),
        })
        .collect();

    sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    Json(sessions)
}

/// DELETE /api/sessions/{id} — evict a session. ADMIN ONLY (was ungated — a
/// non-admin could evict any user's live session).
pub async fn evict_session(
    _admin: AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path(id): Path<String>,
) -> StatusCode {
    if agent.sessions.evict(&id).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
