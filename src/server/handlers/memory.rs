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
use axum::response::IntoResponse;
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
    /// `"saved"` (non-rollup) | `"summary"` (rollup) | absent (all) — applied in
    /// SQL with an accurate `X-Total-Count`, so tab counts are right past the page.
    pub kind:     Option<String>,
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

/// Strict parse for WRITE paths: an unknown category is a client error, not
/// silently coerced to `fact` (the lenient `parse_category` is kept for the
/// list FILTER, where an unknown value simply matches nothing).
fn parse_category_strict(s: &str) -> Option<Category> {
    match s {
        "fact"         => Some(Category::Fact),
        "preference"   => Some(Category::Preference),
        "skill"        => Some(Category::Skill),
        "relationship" => Some(Category::Relationship),
        "project"      => Some(Category::Project),
        _              => None,
    }
}
const ALLOWED_CATEGORIES_MSG: &str =
    "unknown category; allowed: fact, preference, skill, relationship, project";

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
) -> axum::response::Response {
    let limit  = q.limit.unwrap_or(50).min(500) as u64;
    let offset = q.offset.unwrap_or(0) as u64;
    let groups = group_ids_for(&auth, &me.id);
    let sort   = q.sort.as_deref().map(ListSort::parse).unwrap_or(ListSort::Strength);
    let kind   = q.kind.as_deref().filter(|s| !s.is_empty() && *s != "all");

    // Strict category validation (L6): an unknown `?category=` is a 400 with the
    // allowed list, not a silent coerce-to-`fact` that returns facts.
    if let Some(c) = q.category.as_deref().filter(|s| !s.is_empty() && *s != "all") {
        if parse_category_strict(c).is_none() {
            return err(StatusCode::BAD_REQUEST, ALLOWED_CATEGORIES_MSG).into_response();
        }
    }

    // Normalise the filters to the DB's own values so they can be applied IN SQL
    // (M5) — a correct `total` under a filter needs the filter in the query, not
    // after paging. `category` is validated above; `scope` is normalised; `source`
    // maps to the stored `source_type`.
    let cat_f   = q.category.as_deref().filter(|s| !s.is_empty() && *s != "all");
    let scope_f = q.scope.as_deref().filter(|s| !s.is_empty() && *s != "all").map(|s| parse_scope(s).as_str());
    let src_f   = q.source.as_deref().filter(|s| !s.is_empty() && *s != "all");
    let tag_f   = q.tag.as_deref().filter(|s| !s.is_empty());

    // `total` is the full count for every filter (server-side), so the UI's tab
    // count is correct regardless of the page size. The search path has no cheap
    // SQL total, so it still filters in memory and reports the filtered count.
    let (items, total): (Vec<MemoryItem>, u64) = if let Some(query) = q.q.filter(|s| !s.is_empty()) {
        let mut items = agent.memory.search_visible(&query, &me.id, &groups).unwrap_or_default();
        match kind {
            Some("summary") => items.retain(|m| m.tags.iter().any(|t| t == "rollup" || t.starts_with("rollup:"))),
            Some("saved")   => items.retain(|m| !m.tags.iter().any(|t| t == "rollup" || t.starts_with("rollup:"))),
            _ => {}
        }
        if let Some(c) = cat_f   { let cat = parse_category(c); items.retain(|m| m.category == cat); }
        if let Some(s) = scope_f { items.retain(|m| m.scope.as_str() == s); }
        if let Some(src) = src_f {
            items.retain(|m| matches!((&m.source, src),
                (Some(MemorySource::UserExplicit(_)), "user_explicit")
              | (Some(MemorySource::AutoExtracted),   "auto_extracted")
              | (Some(MemorySource::Imported(_)),     "imported")));
        }
        if let Some(t) = tag_f { items.retain(|m| m.tags.iter().any(|x| x == t)); }
        let n = items.len() as u64;
        (items, n)
    } else {
        agent.memory.list_visible_filtered(
            &me.id, &groups, kind, cat_f, scope_f, src_f, tag_f, limit, offset, sort,
        ).unwrap_or_default()
    };

    let body: Vec<MemoryResponse> = items.into_iter().map(MemoryResponse::from).collect();
    ([(axum::http::header::HeaderName::from_static("x-total-count"), total.to_string())], Json(body))
        .into_response()
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
    // W3: refuse credential-like content on this write path too (not only the
    // extractor/tool paths).
    if crate::memory::auto_extract::looks_like_secret(&body.content) {
        return Err(err(StatusCode::BAD_REQUEST, "refusing to store what looks like a credential (password/API key/token/card number) — don't save secrets to memory"));
    }

    let category = match body.category.as_deref() {
        Some(c) => parse_category_strict(c).ok_or_else(|| err(StatusCode::BAD_REQUEST, ALLOWED_CATEGORIES_MSG))?,
        None    => Category::Fact,
    };
    // A `key:`/`entity:` conflict key is derived server-side; strip any a client
    // tried to set so it can't hijack another fact's key.
    let tags: Vec<String> = body.tags.unwrap_or_default().into_iter()
        .filter(|t| !t.starts_with("key:") && !t.starts_with("entity:"))
        .collect();
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
    if crate::memory::auto_extract::looks_like_secret(&body.content) {
        return Err(err(StatusCode::BAD_REQUEST, "refusing to store what looks like a credential — don't save secrets to memory"));
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

    let category = match body.category.as_deref() {
        Some(c) => parse_category_strict(c).ok_or_else(|| err(StatusCode::BAD_REQUEST, ALLOWED_CATEGORIES_MSG))?,
        None    => existing.category,
    };
    // An omitted `tags` inherits the superseded memory's tags rather than
    // wiping them. (Pass an explicit empty array to clear.)
    let tags     = body.tags.unwrap_or_else(|| existing.tags.clone());

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

    // Over-fetch, then filter by visibility: semantic_search ranks GLOBALLY across
    // all users, so on a multi-user install another user's vectors would otherwise
    // starve the caller's top-k.
    let overfetch = (top_k * 10).min(500);
    let sem = agent.memory.semantic_search(&body.query, overfetch).await.unwrap_or_default();
    // NB: this is a BROWSE surface — it must NOT reinforce (that would let merely
    // viewing the Memory page change recall ranking). Reinforcement happens on
    // agent recall only (the builtin provider's retrieve).
    let sem_items: Vec<MemoryResponse> = sem.into_iter()
        .filter_map(|(id, _content, score)| {
            agent.memory.get_visible(id, &me.id, &groups).ok().flatten().map(|mut m| {
                m.relevance_score = score;
                MemoryResponse::from(m)
            })
        })
        .take(top_k)
        .collect();

    if !sem_items.is_empty() {
        return Json(sem_items);
    }

    // Keyword fallback (also non-reinforcing).
    let items: Vec<MemoryResponse> = agent.memory
        .search_visible(&body.query, &me.id, &groups)
        .unwrap_or_default()
        .into_iter()
        .take(top_k)
        .map(MemoryResponse::from)
        .collect();
    Json(items)
}

/// DELETE /api/memory/{id} — delete a memory the caller is allowed to remove.
/// Policy (shared with the `memory_forget` tool): a user may delete their OWN
/// user-scoped memory; a group memory may be removed by its creator or an admin;
/// a system memory by an admin. Soft delete (audit-preserving).
pub async fn delete_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<u64>,
) -> StatusCode {
    let groups = group_ids_for(&auth, &me.id);
    let existing = match agent.memory.get_visible(id, &me.id, &groups) {
        Ok(Some(m)) => m,
        Ok(None)    => return StatusCode::NOT_FOUND,
        Err(_)      => return StatusCode::INTERNAL_SERVER_ERROR,
    };
    // `get_visible` already guarantees a user-scoped row is the caller's own.
    let allowed = match existing.scope {
        Scope::User   => true,
        Scope::Group  => me.role == Role::Admin
            || existing.created_by.as_deref() == Some(me.id.as_str()),
        Scope::System => me.role == Role::Admin,
    };
    if !allowed {
        return StatusCode::FORBIDDEN;
    }
    // A user-scoped delete removes the whole supersede chain so older
    // versions don't linger; a group/system moderation delete removes the one row.
    let res = if existing.scope == Scope::User {
        agent.memory.soft_delete_owned(id, &me.id).await
    } else {
        agent.memory.soft_delete(id, &me.id).await
    };
    match res {
        Ok(true)  => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_)    => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// POST /api/memory/{id}/restore — undo a soft-delete of the caller's OWN
/// user-scoped memory (the Memory page's "Undo" toast). Owner-scoped in storage:
/// a user can only restore a row they own; others 404 (no existence leak).
pub async fn restore_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path(id): Path<u64>,
) -> StatusCode {
    match agent.memory.restore(id, &me.id).await {
        Ok(true)  => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_)    => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// ── Memory catalog (per-user browse of an external source) ───────────────────

#[derive(Debug, Deserialize)]
pub struct CatalogQuery {
    /// A subject/person/title to fetch held facts (with tier labels) for.
    pub subject:  Option<String>,
    /// List only subjects whose name/alias/title starts with this.
    pub prefix:   Option<String>,
    /// List only subjects whose name/alias/title contains this (not with prefix).
    pub contains: Option<String>,
    pub page:     Option<u32>,
    pub per_page: Option<u32>,
}

/// GET /api/memory/catalog — the caller's OWN memory catalog (what is held) from
/// a configured catalog-capable source (e.g. a remote HTTP memory provider). Always
/// owner-scoped: it only ever returns the authenticated user's holdings. Reports
/// `{available:false}` when no catalog source is configured, so the UI can hide
/// the browser rather than show an error.
pub async fn memory_catalog(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Query(q): Query<CatalogQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // `prefix` and `contains` are mutually exclusive — reject client-side with a
    // real 400 rather than passing both and surfacing the service's error as 200.
    let has = |o: &Option<String>| o.as_deref().is_some_and(|s| !s.trim().is_empty());
    if has(&q.prefix) && has(&q.contains) {
        return Err(err(StatusCode::BAD_REQUEST, "prefix and contains are mutually exclusive — pass only one"));
    }
    let page     = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(50).clamp(1, 500);
    let source = agent.memory_catalog_source();
    Ok(match agent.memory_catalog(&me.id, q.subject, q.prefix, q.contains, page, per_page).await {
        None => Json(serde_json::json!({ "available": false })),
        Some(Ok(res)) => Json(serde_json::json!({
            "available": true, "state": res.state, "catalog": res.catalog, "source": source,
        })),
        // A 404 / "not available" from an older service degrades to unavailable,
        // not a hard error; any other error is surfaced with its reason.
        Some(Err(crate::memory::provider::MemoryError::InvalidRequest(reason)))
            if reason.contains("not available") =>
            Json(serde_json::json!({ "available": false, "reason": reason })),
        Some(Err(e)) => Json(serde_json::json!({ "available": true, "error": e.to_string() })),
    })
}

/// GET /api/memory/{id}/history — the supersede chain through this memory
/// (oldest → newest, including superseded versions), so the UI can show "View
/// history" even though the older versions are hidden from normal reads. Visible
/// only to a caller who owns/sees the chain.
pub async fn memory_history(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Path(id): Path<u64>,
) -> Result<Json<Vec<MemoryResponse>>, (StatusCode, Json<serde_json::Value>)> {
    let groups = group_ids_for(&auth, &me.id);
    let chain = agent.memory.memory_history(id, me.role == Role::Admin)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    if chain.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "no such memory"));
    }
    // Authorize against the chain (all rows share one owner/scope). Don't leak
    // existence to a caller who can't see it.
    let can_see = |m: &MemoryItem| match m.scope {
        Scope::User   => m.scope_id.as_deref() == Some(me.id.as_str()),
        Scope::Group  => m.scope_id.as_deref().is_some_and(|g| groups.iter().any(|x| x == g)),
        Scope::System => true,
    };
    if !chain.iter().all(can_see) {
        return Err(err(StatusCode::NOT_FOUND, "no such memory"));
    }
    Ok(Json(chain.into_iter().map(MemoryResponse::from).collect()))
}

/// GET /api/admin/memory/providers/status — per-source load state, capabilities,
/// and a live health probe. Admin-only; drives the Settings status pills.
pub async fn memory_providers_status(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if me.role != Role::Admin {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(serde_json::json!({ "providers": agent.memory_provider_status().await })))
}

// ── Delete-all + export (per-user privacy controls) ──────────────────────────

#[derive(Debug, Deserialize)]
pub struct DeleteMineQuery {
    /// `true` = permanently remove (hard delete) instead of soft-delete.
    pub hard: Option<bool>,
}

/// DELETE /api/memory/mine — delete ALL of the caller's own (user-scoped)
/// memories. Soft by default (retained per `memory.retention_days`, then purged);
/// `?hard=true` removes them permanently now. Shared group/system memories are
/// not touched. Returns 5xx on failure so the UI's error path fires.
pub async fn delete_my_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Query(q): Query<DeleteMineQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let hard = q.hard.unwrap_or(false);
    let res = if hard {
        agent.memory.hard_delete_all_user(&me.id).await
    } else {
        agent.memory.soft_delete_all_user(&me.id, &me.id).await
    };
    match res {
        Ok(n)  => Ok(Json(serde_json::json!({ "deleted": n, "hard": hard }))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    /// `"json"` (default) or `"md"`.
    pub format: Option<String>,
}

/// GET /api/memory/export — export everything MIRA remembers that's visible to
/// the caller, as JSON (default) or Markdown (`?format=md`). A privacy/portability
/// control so a user can take their data with them.
pub async fn export_memory(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(auth): Extension<Arc<LocalAuthService>>,
    Query(q): Query<ExportQuery>,
) -> axum::response::Response {
    let groups = group_ids_for(&auth, &me.id);
    let items = agent.memory
        .list_visible_sorted(&me.id, &groups, 100_000, 0, ListSort::Recent)
        .unwrap_or_default();

    if q.format.as_deref() == Some("md") {
        let mut md = String::from("# My MIRA memory\n\n");
        md.push_str(&format!("_{} memories, exported {}_\n", items.len(), chrono::Utc::now().format("%Y-%m-%d")));
        for m in &items {
            md.push_str(&format!("\n- **{}** — {}", m.category.as_str(), m.content));
            if m.scope != Scope::User {
                md.push_str(&format!(" _(scope: {})_", m.scope.as_str()));
            }
        }
        md.push('\n');
        (
            [
                (axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
                (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"mira-memory.md\""),
            ],
            md,
        ).into_response()
    } else {
        let out: Vec<MemoryResponse> = items.into_iter().map(MemoryResponse::from).collect();
        Json(out).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_category_rejects_unknown_but_accepts_the_five() {
        for c in ["fact", "preference", "skill", "relationship", "project"] {
            assert!(parse_category_strict(c).is_some(), "{c} should parse");
        }
        assert!(parse_category_strict("banana").is_none());
        assert!(parse_category_strict("").is_none());
        // The lenient filter parser still coerces (unknown filter → matches nothing-ish).
        assert_eq!(parse_category("banana").as_str(), "fact");
    }
}
