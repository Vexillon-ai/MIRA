// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/memory.rs
//!
//! Memory endpoints gated by the visibility chokepoint.
//!
//! Every read resolves the caller's (user_id, group_ids) and filters through
//! `MemorySystem::list_visible` / `get_visible` / `search_visible`. Writes
//! enforce the scope policy:
//!   - `user` scope:   scope_id must equal the caller's id.
//!   - `group` scope:  caller must be a member of scope_id.
//!   - `system` scope: caller must be admin.
//!
//! Non-admin users cannot delete or directly mutate memories. To change a
//! fact, they POST `/api/memory/{id}/supersede`, which appends a newer row
//! linked to the old one — preserving historical weight.

use std::sync::Arc;

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use crate::agent::AgentCore;
use crate::auth::{AuthUser, LocalAuthService, Role};
use crate::memory::{Category, ListSort, MemoryItem, MemorySource, Scope};

// ── Request / response types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub q:        Option<String>,
    pub category: Option<String>,
    pub scope:    Option<String>, // "user" | "group" | "system" | "all" (default)
    /// "strength" (default, decay-aware) | "recent" (created_at desc).
    pub sort:     Option<String>,
    pub limit:    Option<usize>,
    pub offset:   Option<usize>,
    /// Filter by `MemorySource` kind — `"user_explicit" | "auto_extracted" | "imported"`.
    pub source:   Option<String>,
    /// Filter to memories carrying this exact tag (e.g. `"rollup"`).
    pub tag:      Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateMemoryRequest {
    pub content:  String,
    pub category: Option<String>,
    pub tags:     Option<Vec<String>>,
    /// "user" (default), "group", or "system".
    pub scope:    Option<String>,
    /// Required for `group`. Ignored for `user` (auto-filled with caller id)
    /// and `system` (stored as NULL).
    pub scope_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SupersedeRequest {
    pub content:  String,
    pub category: Option<String>,
    pub tags:     Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct MemoryResponse {
    pub id:              u64,
    pub content:         String,
    pub category:        String,
    pub tags:            Vec<String>,
    pub created_at:      i64,
    pub relevance_score: f32,
    pub scope:           String,
    pub scope_id:        Option<String>,
    pub created_by:      Option<String>,
    pub supersedes:      Option<u64>,
    pub superseded_by:   Option<u64>,
    /// Persisted baseline strength (0.0..=1.0).
    pub strength:           f32,
    /// Decay-adjusted strength as of this response.
    pub effective_strength: f32,
    /// Times this memory has been surfaced in retrieval.
    pub access_count:       u32,
    /// Epoch ms of last reinforcement.
    pub last_reinforced:    i64,
    /// Decay class: `permanent` | `stable` | `episodic` | `ephemeral`.
    pub stability:          String,

    // ── Provenance (review surface) ──
    /// `MemorySource` discriminant — `"user_explicit" | "auto_extracted" | "imported"`.
    pub source_kind:            Option<String>,
    /// Free-form detail attached to the source (e.g. importer name).
    pub source_detail:          Option<String>,
    /// Channel that produced the triggering turn.
    pub source_channel:         Option<String>,
    /// Conversation id that produced this memory — deep-link back to the transcript.
    pub source_conversation_id: Option<String>,
    /// Message id that produced this memory, when applicable.
    pub source_message_id:      Option<String>,
}

impl From<MemoryItem> for MemoryResponse {
    fn from(m: MemoryItem) -> Self {
        let (source_kind, source_detail) = match &m.source {
            Some(MemorySource::UserExplicit(d)) => (Some("user_explicit".to_owned()), Some(d.clone())),
            Some(MemorySource::AutoExtracted)   => (Some("auto_extracted".to_owned()), None),
            Some(MemorySource::Imported(d))     => (Some("imported".to_owned()),       Some(d.clone())),
            None                                => (None, None),
        };
        Self {
            id:              m.id,
            content:         m.content,
            category:        m.category.to_string(),
            tags:            m.tags,
            created_at:      m.created_at.timestamp_millis(),
            relevance_score: m.relevance_score,
            scope:           m.scope.as_str().to_owned(),
            scope_id:        m.scope_id,
            created_by:      m.created_by,
            supersedes:      m.supersedes,
            superseded_by:   m.superseded_by,
            strength:            m.strength,
            effective_strength:  m.effective_strength,
            access_count:        m.access_count,
            last_reinforced:     m.last_reinforced,
            stability:           m.stability,
            source_kind,
            source_detail,
            source_channel:         m.source_channel,
            source_conversation_id: m.source_conversation_id,
            source_message_id:      m.source_message_id,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchMemoryRequest {
    pub query:     String,
    pub limit:     Option<usize>,
    pub threshold: Option<f32>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_category(s: &str) -> Category {
    match s {
        "preference"   => Category::Preference,
        "skill"        => Category::Skill,
        "relationship" => Category::Relationship,
        "project"      => Category::Project,
        _              => Category::Fact,
    }
}

fn parse_scope(s: &str) -> Scope {
    match s {
        "group"  => Scope::Group,
        "system" => Scope::System,
        _        => Scope::User,
    }
}

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

/// Resolve the caller's group ids from the auth store. Logs & swallows errors
/// so a transient auth-db hiccup degrades to "no groups" rather than 500s.
fn group_ids_for(auth: &LocalAuthService, user_id: &str) -> Vec<String> {
    auth.list_user_group_ids(user_id).unwrap_or_default()
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// GET /api/memory — list/search memories visible to the caller.
pub async fn list_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Query(q): Query<ListQuery>,
) -> Json<Vec<MemoryResponse>> {
    let limit  = q.limit.unwrap_or(50).min(200) as u64;
    let offset = q.offset.unwrap_or(0) as u64;
    let groups = group_ids_for(&auth, &me.id);
    let sort   = q.sort.as_deref().map(ListSort::parse).unwrap_or(ListSort::Strength);

    let mut items: Vec<MemoryItem> = if let Some(query) = q.q.filter(|s| !s.is_empty()) {
        agent.memory.search_visible(&query, &me.id, &groups).unwrap_or_default()
    } else {
        agent.memory.list_visible_sorted(&me.id, &groups, limit, offset, sort).unwrap_or_default()
    };

    // Optional post-filters — cheaper to apply in-memory than to template the SQL.
    if let Some(cat_str) = q.category.as_deref().filter(|s| !s.is_empty() && *s != "all") {
        let cat = parse_category(cat_str);
        items.retain(|m| m.category == cat);
    }
    if let Some(scope_str) = q.scope.as_deref().filter(|s| !s.is_empty() && *s != "all") {
        let want = parse_scope(scope_str);
        items.retain(|m| m.scope == want);
    }
    if let Some(src) = q.source.as_deref().filter(|s| !s.is_empty() && *s != "all") {
        items.retain(|m| match (&m.source, src) {
            (Some(MemorySource::UserExplicit(_)), "user_explicit") => true,
            (Some(MemorySource::AutoExtracted),   "auto_extracted") => true,
            (Some(MemorySource::Imported(_)),     "imported")       => true,
            _ => false,
        });
    }
    if let Some(tag) = q.tag.as_deref().filter(|s| !s.is_empty()) {
        items.retain(|m| m.tags.iter().any(|t| t == tag));
    }

    Json(items.into_iter().map(MemoryResponse::from).collect())
}

/// GET /api/memory/{id} — only returns memories the caller can see.
pub async fn get_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<u64>,
) -> Result<Json<MemoryResponse>, StatusCode> {
    let groups = group_ids_for(&auth, &me.id);
    agent.memory.get_visible(id, &me.id, &groups)
        .unwrap_or(None)
        .map(|m| Json(MemoryResponse::from(m)))
        .ok_or(StatusCode::NOT_FOUND)
}

/// POST /api/memory — write a new memory under an explicit scope.
pub async fn create_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Json(body): Json<CreateMemoryRequest>,
) -> Result<Json<MemoryResponse>, (StatusCode, Json<serde_json::Value>)> {
    if body.content.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "content is required"));
    }

    let category = body.category.as_deref().map(parse_category).unwrap_or(Category::Fact);
    let tags     = body.tags.unwrap_or_default();
    let scope    = body.scope.as_deref().map(parse_scope).unwrap_or(Scope::User);

    // Resolve + authorize scope_id per policy.
    let scope_id: Option<String> = match scope {
        Scope::User => Some(me.id.clone()),                    // caller's own memory
        Scope::Group => {
            let gid = body.scope_id.clone()
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, "scope_id (group_id) is required for group scope"))?;
            let ok = auth.is_group_member(&gid, &me.id)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
            if !ok {
                return Err(err(StatusCode::FORBIDDEN, "not a member of this group"));
            }
            Some(gid)
        }
        Scope::System => {
            if me.role != Role::Admin {
                return Err(err(StatusCode::FORBIDDEN, "system-scope memories are admin-only"));
            }
            None
        }
    };

    let id = agent.memory.store_scoped(
        body.content,
        category,
        tags,
        None,
        scope,
        scope_id.as_deref(),
        &me.id,
        &[],
        Some("web"),
        None, None,
    ).await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    let groups = group_ids_for(&auth, &me.id);
    let item = agent.memory.get_visible(id, &me.id, &groups)
        .unwrap_or(None)
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "failed to retrieve created memory"))?;

    Ok(Json(MemoryResponse::from(item)))
}

/// POST /api/memory/{id}/supersede — append a newer memory that replaces an
/// existing one. Caller must be able to see the old memory.
pub async fn supersede_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(old_id): Path<u64>,
    Json(body): Json<SupersedeRequest>,
) -> Result<Json<MemoryResponse>, (StatusCode, Json<serde_json::Value>)> {
    if body.content.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "content is required"));
    }

    let groups = group_ids_for(&auth, &me.id);
    let existing = agent.memory.get_visible(old_id, &me.id, &groups)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "memory not visible to caller"))?;

    // For group-scoped supersessions, confirm the caller is still a member —
    // `get_visible` guards reads, but membership may change during a session.
    if let (Scope::Group, Some(gid)) = (&existing.scope, &existing.scope_id) {
        let ok = auth.is_group_member(gid, &me.id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
        if !ok {
            return Err(err(StatusCode::FORBIDDEN, "not a member of this group"));
        }
    }
    if existing.scope == Scope::System && me.role != Role::Admin {
        return Err(err(StatusCode::FORBIDDEN, "system-scope memories are admin-only"));
    }

    let category = body.category.as_deref().map(parse_category).unwrap_or(existing.category);
    let tags     = body.tags.unwrap_or_default();

    let new_id = agent.memory.supersede(old_id, body.content, category, tags, None, &me.id).await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    let item = agent.memory.get_visible(new_id, &me.id, &groups)
        .unwrap_or(None)
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "failed to retrieve new memory"))?;

    Ok(Json(MemoryResponse::from(item)))
}

/// POST /api/memory/search — semantic (or keyword fallback), scoped.
pub async fn search_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Json(body): Json<SearchMemoryRequest>,
) -> Json<Vec<MemoryResponse>> {
    let top_k  = body.limit.unwrap_or(10).min(50);
    let groups = group_ids_for(&auth, &me.id);

    // Try semantic first — fall back to keyword-visibility when it yields nothing.
    let sem = agent.memory.semantic_search(&body.query, top_k).await.unwrap_or_default();
    let sem_items: Vec<MemoryResponse> = sem.into_iter()
        .filter_map(|(id, _content, score)| {
            agent.memory.get_visible(id, &me.id, &groups).ok().flatten().map(|mut m| {
                m.relevance_score = score;
                agent.memory.reinforce(m.id, &me.id);
                MemoryResponse::from(m)
            })
        })
        .collect();

    if !sem_items.is_empty() {
        return Json(sem_items);
    }

    // Keyword fallback — also reinforces each surfaced hit.
    let items: Vec<MemoryResponse> = agent.memory
        .search_visible(&body.query, &me.id, &groups)
        .unwrap_or_default()
        .into_iter()
        .take(top_k)
        .map(|m| { agent.memory.reinforce(m.id, &me.id); MemoryResponse::from(m) })
        .collect();
    Json(items)
}

/// DELETE /api/memory/{id} — admin-only soft delete.
pub async fn delete_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path(id): Path<u64>,
) -> StatusCode {
    if me.role != Role::Admin {
        return StatusCode::FORBIDDEN;
    }
    match agent.memory.soft_delete(id, &me.id) {
        Ok(true)  => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_)    => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
