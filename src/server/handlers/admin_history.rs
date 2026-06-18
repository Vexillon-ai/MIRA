// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/admin_history.rs
//! Admin-only cross-user history views.
//!
//! The regular `/api/conversations` and `/api/conversations/stats` endpoints
//! filter to the caller's own rows for every role — the sidebar, chat dropdown,
//! and "your totals" stats bar use those. This module exposes the
//! all-users view the admin History page needs, grouped by owner, with a
//! per-user stats breakdown plus a global totals block.

use std::sync::Arc;

use axum::{http::StatusCode, response::IntoResponse, Extension};
use serde::Serialize;

use crate::auth::{AdminUser, LocalAuthService};
use crate::history::{Conversation, HistoryStats, HistoryStore};
use crate::MiraError;

// ── Shared user summary ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct UserSummary {
    pub id:           String,
    pub username:     String,
    pub display_name: Option<String>,
}

// ── GET /api/admin/conversations/grouped ──────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ConversationGroup {
    pub owner:         UserSummary,
    pub conversations: Vec<Conversation>,
    /// `updated_at` of the most recent conversation in the group, or 0 when
    /// the group is empty. Used on the frontend to sort groups.
    pub last_activity: i64,
}

fn err_resp(e: MiraError) -> axum::response::Response {
    match e {
        MiraError::NotFound(m) => (StatusCode::NOT_FOUND, m).into_response(),
        _                      => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn list_grouped_conversations(
    AdminUser(_):     AdminUser,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Extension(store): Extension<Arc<HistoryStore>>,
) -> impl IntoResponse {
    let users = match auth.list_users() {
        Ok(u)  => u,
        Err(e) => return err_resp(e),
    };

    let mut groups: Vec<ConversationGroup> = Vec::with_capacity(users.len());
    for u in users {
        // 1000-row ceiling: matches the "show me everything" admin expectation
        // without blowing the response size. Tune if users hit the cap.
        let convs = match store.list_visible_conversations(&u.id, None, 1000, 0) {
            Ok(c)  => c,
            Err(e) => return err_resp(e),
        };
        let last_activity = convs.first().map(|c| c.updated_at).unwrap_or(0);
        groups.push(ConversationGroup {
            owner: UserSummary {
                id:           u.id,
                username:     u.username,
                display_name: u.display_name,
            },
            conversations: convs,
            last_activity,
        });
    }

    // Most-recently-active groups first; empty groups sink to the bottom.
    groups.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));

    axum::Json(groups).into_response()
}

// ── GET /api/admin/conversations/stats ────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct PerUserStats {
    #[serde(flatten)]
    pub owner: UserSummary,
    pub stats: HistoryStats,
}

#[derive(Debug, Serialize)]
pub struct AdminStatsResponse {
    pub per_user: Vec<PerUserStats>,
    pub totals:   HistoryStats,
}

pub async fn admin_conversations_stats(
    AdminUser(_):     AdminUser,
    Extension(auth):  Extension<Arc<LocalAuthService>>,
    Extension(store): Extension<Arc<HistoryStore>>,
) -> impl IntoResponse {
    let users = match auth.list_users() {
        Ok(u)  => u,
        Err(e) => return err_resp(e),
    };

    let mut per_user = Vec::with_capacity(users.len());
    for u in users {
        let stats = match store.history_stats(Some(&u.id)) {
            Ok(s)  => s,
            Err(e) => return err_resp(e),
        };
        per_user.push(PerUserStats {
            owner: UserSummary {
                id:           u.id,
                username:     u.username,
                display_name: u.display_name,
            },
            stats,
        });
    }

    // Sort by most active (total messages) descending so the History page's
    // per-user breakdown shows the busiest users first.
    per_user.sort_by(|a, b| b.stats.total_messages.cmp(&a.stats.total_messages));

    let totals = match store.history_stats(None) {
        Ok(s)  => s,
        Err(e) => return err_resp(e),
    };

    axum::Json(AdminStatsResponse { per_user, totals }).into_response()
}
