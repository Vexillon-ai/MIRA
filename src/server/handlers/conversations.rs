// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/conversations.rs
//! Conversation history API handlers.

use std::sync::Arc;

use axum::{
    extract::{Json, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::Deserialize;

use crate::auth::{AuthUser, Role};
use crate::history::{HistoryStore, NewConversation};
use crate::MiraError;

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct HistoryState {
    pub store: Arc<HistoryStore>,
}

// ── DTOs ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ListConversationsQuery {
    pub channel: Option<String>,
    pub limit:   Option<i64>,
    pub offset:  Option<i64>,
}

#[derive(Deserialize)]
pub struct CreateConversationRequest {
    pub channel:  Option<String>,
    pub title:    Option<String>,
    pub model:    Option<String>,
    pub provider: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateConversationRequest {
    pub title: Option<String>,
    /// Slice H — flip the per-conversation wiki context-injection
    /// toggle. `true` = skip the wiki hook for future turns in this
    /// thread; `false` = re-enable.
    pub skip_wiki: Option<bool>,
}

#[derive(Deserialize)]
pub struct GetMessagesQuery {
    pub limit:     Option<i64>,
    pub before_id: Option<String>,
}

// ── Helper: map MiraError to HTTP response ────────────────────────────────────

fn err_response(e: MiraError) -> axum::response::Response {
    match e {
        MiraError::NotFound(msg) =>
            (StatusCode::NOT_FOUND, msg).into_response(),
        _ =>
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── GET /api/conversations ────────────────────────────────────────────────────

pub async fn list_conversations(
    AuthUser(user): AuthUser,
    Extension(store): Extension<Arc<HistoryStore>>,
    Query(q): Query<ListConversationsQuery>,
) -> impl IntoResponse {
    let limit  = q.limit.unwrap_or(50).min(200);
    let offset = q.offset.unwrap_or(0);

    // Everyone — admins included — sees strictly their own conversations in the
    // sidebar and chat-dropdown. The cross-user view lives at
    // `/api/admin/conversations/grouped` and is admin-gated.
    match store.list_visible_conversations(&user.id, q.channel.as_deref(), limit, offset) {
        Ok(convs) => axum::Json(convs).into_response(),
        Err(e)    => err_response(e),
    }
}

// ── POST /api/conversations ───────────────────────────────────────────────────

pub async fn create_conversation(
    AuthUser(user): AuthUser,
    Extension(store): Extension<Arc<HistoryStore>>,
    Json(req): Json<CreateConversationRequest>,
) -> impl IntoResponse {
    let new = NewConversation {
        user_id:          user.id,
        channel:          req.channel.unwrap_or_else(|| "web".to_owned()),
        title:            req.title,
        model:            req.model,
        provider:         req.provider,
        external_user_id: None,
        mode:             None,
    };
    match store.create_conversation(new) {
        Ok(conv) => (StatusCode::CREATED, axum::Json(conv)).into_response(),
        Err(e)   => err_response(e),
    }
}

// ── GET /api/conversations/:id ────────────────────────────────────────────────

pub async fn get_conversation(
    AuthUser(user): AuthUser,
    Extension(store): Extension<Arc<HistoryStore>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let is_admin = user.role == Role::Admin;
    match store.get_conversation(&id) {
        Ok(Some(conv)) if is_admin || conv.user_id == user.id =>
            axum::Json(conv).into_response(),
        Ok(Some(_)) => StatusCode::FORBIDDEN.into_response(),
        Ok(None)    => StatusCode::NOT_FOUND.into_response(),
        Err(e)      => err_response(e),
    }
}

// ── PATCH /api/conversations/:id ──────────────────────────────────────────────

pub async fn update_conversation(
    AuthUser(user): AuthUser,
    Extension(store): Extension<Arc<HistoryStore>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateConversationRequest>,
) -> impl IntoResponse {
    // Admins bypass ownership; regular users must own the conversation.
    let is_admin = user.role == Role::Admin;
    match store.get_conversation(&id) {
        Ok(Some(conv)) if !is_admin && conv.user_id != user.id =>
            return StatusCode::FORBIDDEN.into_response(),
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e)   => return err_response(e),
        Ok(_)    => {}
    }

    if let Some(title) = req.title {
        if let Err(e) = store.update_conversation_title(&id, &title) {
            return err_response(e);
        }
    }
    if let Some(skip) = req.skip_wiki {
        if let Err(e) = store.update_conversation_skip_wiki(&id, skip) {
            return err_response(e);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

// ── DELETE /api/conversations/:id ─────────────────────────────────────────────

pub async fn delete_conversation(
    AuthUser(user): AuthUser,
    Extension(store): Extension<Arc<HistoryStore>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let is_admin = user.role == Role::Admin;
    match store.get_conversation(&id) {
        Ok(Some(conv)) if !is_admin && conv.user_id != user.id =>
            return StatusCode::FORBIDDEN.into_response(),
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e)   => return err_response(e),
        Ok(_)    => {}
    }

    match store.delete_conversation(&id) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_response(e),
    }
}

// ── GET /api/conversations/:id/messages ───────────────────────────────────────

pub async fn get_messages(
    AuthUser(user): AuthUser,
    Extension(store): Extension<Arc<HistoryStore>>,
    Path(id): Path<String>,
    Query(q): Query<GetMessagesQuery>,
) -> impl IntoResponse {
    // Admins can read any conversation's messages; regular users can read
    // strictly their own.
    let is_admin = user.role == Role::Admin;
    match store.get_conversation(&id) {
        Ok(Some(conv)) if !is_admin && conv.user_id != user.id =>
            return StatusCode::FORBIDDEN.into_response(),
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e)   => return err_response(e),
        Ok(_)    => {}
    }

    let limit = q.limit.unwrap_or(100).min(500);
    match store.get_messages(&id, limit, q.before_id.as_deref()) {
        Ok(msgs) => axum::Json(msgs).into_response(),
        Err(e)   => err_response(e),
    }
}

// ── GET /api/conversations/stats ──────────────────────────────────────────────

pub async fn conversations_stats(
    AuthUser(user): AuthUser,
    Extension(store): Extension<Arc<HistoryStore>>,
) -> impl IntoResponse {
    // Admins get "your totals" from this endpoint now — the cross-user breakdown
    // has moved to `/api/admin/conversations/stats`.
    match store.history_stats(Some(user.id.as_str())) {
        Ok(s)  => axum::Json(s).into_response(),
        Err(e) => err_response(e),
    }
}

// ── DELETE /api/messages/:id ──────────────────────────────────────────────────

pub async fn delete_message(
    AuthUser(_user): AuthUser,
    Extension(store): Extension<Arc<HistoryStore>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match store.delete_message(&id) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_response(e),
    }
}
