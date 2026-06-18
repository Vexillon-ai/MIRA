// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/wiki.rs
//! Wiki HTTP API (Slice E).
//!
//! Per-user CRUD over wiki pages + the audit-backed review queue.
//! All endpoints require the `AuthUser` extractor; each handler
//! resolves the caller's `user_id` and operates against the
//! [`WikiRegistry`] held by [`AgentCore`]. Cross-user access is
//! impossible by construction — `wiki.for_user(<id>)` only ever sees
//! the caller's own wiki.
//!
//! **Direct user edits** (`PUT /api/wiki/page`, `POST /append-section`,
//! `DELETE /api/wiki/page`) flow through the same audit pipeline as
//! agent-tool writes, but use `Provenance::user_ui(user_id)` and call
//! `submit_and_apply` — the user is the source of truth for their own
//! wiki, so direct edits do not need review.
//!
//! **Agent / extractor writes** are the ones the user reviews here:
//! they land as `OpStatus::Pending` and only get applied when the user
//! POSTs `/api/wiki/ops/<id>/approve`.

use std::sync::Arc;

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentCore;
use crate::auth::{AdminUser, AuthUser};
use crate::wiki::{
    LogKind, PageFrontmatter, Provenance, WikiOp, WikiOpEnvelope, WikiPath,
    WikiRegistry, WikiScope, WikiSystem,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

/// Resolve the caller's wiki, or return 503 if the wiki feature is not
/// installed on this AgentCore (channel-only builds, tests).
fn user_wiki(
    agent: &AgentCore, user_id: &str,
) -> Result<Arc<WikiSystem>, (StatusCode, Json<serde_json::Value>)> {
    let registry: &Arc<WikiRegistry> = agent.wiki().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "wiki feature not enabled on this server",
    ))?;
    registry.for_user(user_id).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("failed to open wiki: {e}"),
    ))
}

fn parse_wiki_path(s: &str) -> Result<WikiPath, (StatusCode, Json<serde_json::Value>)> {
    WikiPath::parse(s).map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid path: {e}")))
}

/// Resolve the (admin-only) system wiki, or return 503.
fn system_wiki(
    agent: &AgentCore,
) -> Result<Arc<WikiSystem>, (StatusCode, Json<serde_json::Value>)> {
    let registry: &Arc<WikiRegistry> = agent.wiki().ok_or_else(|| err(
        StatusCode::SERVICE_UNAVAILABLE,
        "wiki feature not enabled on this server",
    ))?;
    registry.system().map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("failed to open system wiki: {e}"),
    ))
}

// ── Response shapes ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct PageSummary {
    pub path: String,
    pub title: Option<String>,
    pub writer: String,
    pub tags: Vec<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub is_special: bool,
}

#[derive(Debug, Serialize)]
pub struct PageDetail {
    pub path: String,
    pub title: Option<String>,
    pub writer: String,
    pub tags: Vec<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub confidence: Option<f32>,
    pub body: String,
    pub provenance: Vec<ProvenanceView>,
}

#[derive(Debug, Serialize)]
pub struct ProvenanceView {
    pub source: String,
    pub turn_id: Option<String>,
    pub conversation_id: Option<String>,
    pub extracted_at: i64,
}

#[derive(Debug, Serialize)]
pub struct NavBundle {
    pub profile: String,
    pub index: String,
    pub schema: String,
    pub log: String,
}

#[derive(Debug, Serialize)]
pub struct OpView {
    pub op_id: String,
    pub status: String,
    pub kind: String,
    pub target_path: String,
    pub scope: String,
    pub user_id: Option<String>,
    pub provenance_source: String,
    pub provenance_actor: String,
    pub conversation_id: Option<String>,
    pub turn_id: Option<String>,
    pub created_at: i64,
    pub applied_at: Option<i64>,
    pub reviewed_at: Option<i64>,
    pub reviewed_by: Option<String>,
    pub failure: Option<String>,
    /// Extractor confidence [0.0, 1.0], when the op came from the post-turn
    /// extractor. Null for direct UI/tool writes. Lets the Review tab show a
    /// confidence badge and drive "approve all ≥ X".
    pub confidence: Option<f32>,
    /// Lossy JSON view of the op payload. Useful for the UI to render
    /// previews — body text, section heading, etc. — without each
    /// caller having to know the op variant's full schema.
    pub op: serde_json::Value,
}

impl From<WikiOpEnvelope> for OpView {
    fn from(env: WikiOpEnvelope) -> Self {
        let (scope_str, user_id) = match &env.scope {
            WikiScope::User(uid) => ("user".to_string(), Some(uid.clone())),
            WikiScope::System    => ("system".to_string(), None),
        };
        Self {
            op_id: env.op_id,
            status: env.status.as_str().to_string(),
            kind: env.op.kind().to_string(),
            target_path: env.op.target_path().to_string(),
            scope: scope_str,
            user_id,
            provenance_source: env.provenance.source.clone(),
            provenance_actor: env.provenance.actor.clone(),
            conversation_id: env.provenance.conversation_id.clone(),
            turn_id: env.provenance.turn_id.clone(),
            created_at: env.created_at.timestamp_millis(),
            applied_at: env.applied_at.map(|d| d.timestamp_millis()),
            reviewed_at: env.reviewed_at.map(|d| d.timestamp_millis()),
            reviewed_by: env.reviewed_by,
            failure: env.failure,
            confidence: env.confidence,
            op: serde_json::to_value(&env.op).unwrap_or(serde_json::Value::Null),
        }
    }
}

// ── Request shapes ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct PutPageRequest {
    pub path: String,
    pub title: Option<String>,
    pub tags: Option<Vec<String>>,
    pub body: String,
    /// "user" | "agent" | "both" — defaults to "user" for direct UI edits.
    pub writer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppendSectionRequest {
    pub path: String,
    pub section: String,
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct RejectOpRequest {
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BulkApproveRequest {
    /// Only approve pending ops with confidence ≥ this. Omit to approve all.
    pub min_confidence: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct BulkRejectRequest {
    pub reason: Option<String>,
    /// Only reject pending ops with confidence < this (or unrecorded). Omit
    /// to reject all.
    pub max_confidence: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct RecentOpsQuery {
    /// Epoch ms. Defaults to 24h ago.
    pub since: Option<i64>,
    /// Hard-capped at 200.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct LogEntryRequest {
    pub kind: Option<String>,    // "ingest"|"promote"|"supersede"|"lint"|"note"
    pub summary: String,
    pub page_refs: Option<Vec<String>>,
}

// ── Page CRUD ────────────────────────────────────────────────────────────────

/// GET /api/wiki/pages — list every page in the caller's wiki.
pub async fn list_pages(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<Vec<PageSummary>>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let store = wiki.store();
    let paths = store.list_pages().map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list_pages failed: {e}"),
    ))?;
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let summary = match store.read_page(&path) {
            Ok(page) => PageSummary {
                path: path.to_string(),
                title: page.frontmatter.title.clone(),
                writer: page.frontmatter.writer.as_str().to_string(),
                tags: page.frontmatter.tags.clone(),
                valid_from: page.frontmatter.valid_from.map(|d| d.to_string()),
                valid_to:   page.frontmatter.valid_to.map(|d| d.to_string()),
                is_special: path.is_special(),
            },
            Err(_) => PageSummary {
                path: path.to_string(), title: None, writer: "both".into(),
                tags: vec![], valid_from: None, valid_to: None,
                is_special: path.is_special(),
            },
        };
        out.push(summary);
    }
    Ok(Json(out))
}

/// GET /api/wiki/page?path=<rel> — read one page.
pub async fn get_page(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Query(q): Query<PathQuery>,
) -> Result<Json<PageDetail>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let path = parse_wiki_path(&q.path)?;
    let page = wiki.store().try_read_page(&path).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("read failed: {e}"),
    ))?;
    let page = page.ok_or_else(|| err(StatusCode::NOT_FOUND, format!("page not found: {}", q.path)))?;
    let provenance = page.frontmatter.provenance.iter().map(|p| ProvenanceView {
        source: p.source.clone(),
        turn_id: p.turn_id.clone(),
        conversation_id: p.conversation_id.clone(),
        extracted_at: p.extracted_at.timestamp_millis(),
    }).collect();
    Ok(Json(PageDetail {
        path: path.to_string(),
        title: page.frontmatter.title.clone(),
        writer: page.frontmatter.writer.as_str().to_string(),
        tags: page.frontmatter.tags.clone(),
        valid_from: page.frontmatter.valid_from.map(|d| d.to_string()),
        valid_to:   page.frontmatter.valid_to.map(|d| d.to_string()),
        confidence: page.frontmatter.confidence,
        body: page.body,
        provenance,
    }))
}

/// PUT /api/wiki/page — create or replace a page (direct user edit;
/// applied immediately, not subject to review).
pub async fn put_page(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Json(body): Json<PutPageRequest>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    if body.body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "body cannot be empty"));
    }
    let path = parse_wiki_path(&body.path)?;
    let wiki = user_wiki(&agent, &me.id)?;

    let mut fm = match wiki.store().try_read_page(&path) {
        Ok(Some(existing)) => existing.frontmatter.clone(),
        _ => PageFrontmatter::default(),
    };
    if body.title.is_some() { fm.title = body.title.clone(); }
    if let Some(tags) = body.tags.clone() { fm.tags = tags; }
    if let Some(w) = body.writer.as_deref() {
        fm.writer = match w {
            "user" => crate::wiki::frontmatter::Writer::User,
            "agent" => crate::wiki::frontmatter::Writer::Agent,
            "both" => crate::wiki::frontmatter::Writer::Both,
            other => return Err(err(StatusCode::BAD_REQUEST,
                format!("invalid writer '{other}' (use user|agent|both)"))),
        };
    }

    let op = WikiOp::WritePage { path: path.clone(), frontmatter: fm, body: body.body };
    let op_id = wiki.submit_and_apply(op, Provenance::user_ui(&me.id))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("apply failed: {e}")))?;
    op_view_for(&wiki, &op_id)
}

/// POST /api/wiki/page/append-section — granular edit.
pub async fn append_section(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Json(body): Json<AppendSectionRequest>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    let path = parse_wiki_path(&body.path)?;
    if body.section.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "section is required"));
    }
    if body.body.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "body is required"));
    }
    let wiki = user_wiki(&agent, &me.id)?;
    let op = WikiOp::AppendSection { path, section: body.section, body: body.body };
    let op_id = wiki.submit_and_apply(op, Provenance::user_ui(&me.id))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("apply failed: {e}")))?;
    op_view_for(&wiki, &op_id)
}

/// DELETE /api/wiki/page?path=<rel> — archive a page (move to archive/,
/// never unlink).
pub async fn delete_page(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Query(q): Query<PathQuery>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    let path = parse_wiki_path(&q.path)?;
    if path.is_special() {
        return Err(err(StatusCode::BAD_REQUEST,
            format!("cannot delete special navigation file '{}'", q.path)));
    }
    let wiki = user_wiki(&agent, &me.id)?;
    let op = WikiOp::DeletePage { path };
    let op_id = wiki.submit_and_apply(op, Provenance::user_ui(&me.id))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("apply failed: {e}")))?;
    op_view_for(&wiki, &op_id)
}

/// POST /api/wiki/log — add a log entry from the UI.
pub async fn add_log_entry(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Json(body): Json<LogEntryRequest>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    if body.summary.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "summary is required"));
    }
    let kind = match body.kind.as_deref().unwrap_or("note") {
        "ingest"    => LogKind::Ingest,
        "promote"   => LogKind::Promote,
        "supersede" => LogKind::Supersede,
        "lint"      => LogKind::Lint,
        "note"      => LogKind::Note,
        other       => return Err(err(StatusCode::BAD_REQUEST,
            format!("invalid kind '{other}'"))),
    };
    let mut refs = Vec::new();
    for s in body.page_refs.unwrap_or_default() {
        refs.push(parse_wiki_path(&s)?);
    }
    let wiki = user_wiki(&agent, &me.id)?;
    let op = WikiOp::LogEntry { kind, summary: body.summary, page_refs: refs };
    let op_id = wiki.submit_and_apply(op, Provenance::user_ui(&me.id))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("apply failed: {e}")))?;
    op_view_for(&wiki, &op_id)
}

// ── Navigation bundle ────────────────────────────────────────────────────────

/// GET /api/wiki/nav — one-shot fetch of the four navigation files.
pub async fn get_nav(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<NavBundle>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let store = wiki.store();
    Ok(Json(NavBundle {
        profile: store.read_core_raw().unwrap_or_default(),
        index:   store.read_index_raw().unwrap_or_default(),
        schema:  store.read_schema_raw().unwrap_or_default(),
        log:     store.read_log_raw().unwrap_or_default(),
    }))
}

// ── Review queue ─────────────────────────────────────────────────────────────

/// GET /api/wiki/ops/pending — list ops awaiting review.
pub async fn list_pending_ops(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<Vec<OpView>>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let envs = wiki.list_pending_ops().map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list_pending_ops failed: {e}"),
    ))?;
    Ok(Json(envs.into_iter().map(OpView::from).collect()))
}

/// GET /api/wiki/ops?since=<ms>&limit=N — recent ops in any status.
pub async fn list_recent_ops(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Query(q): Query<RecentOpsQuery>,
) -> Result<Json<Vec<OpView>>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let since: DateTime<Utc> = match q.since {
        Some(ms) => DateTime::from_timestamp_millis(ms).unwrap_or_else(|| Utc::now() - chrono::Duration::days(1)),
        None     => Utc::now() - chrono::Duration::days(1),
    };
    let limit = q.limit.unwrap_or(50).min(200);
    let envs = wiki.list_recent_ops(since, limit).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list_recent_ops failed: {e}"),
    ))?;
    Ok(Json(envs.into_iter().map(OpView::from).collect()))
}

/// POST /api/wiki/ops/{id}/approve — approve a pending op (applies it).
pub async fn approve_op(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path(op_id): Path<String>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    // Guard against approving ops owned by a different user. The audit DB
    // is per-user already (wiki_<user>.db), so a cross-user op id can't
    // even surface — but defence in depth: only approve when the
    // envelope's scope matches.
    let env = wiki.list_pending_ops().map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list failed: {e}"),
    ))?.into_iter().find(|e| e.op_id == op_id);
    if env.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "op not found or not pending"));
    }
    wiki.approve_op(&op_id, &me.id).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("approve failed: {e}"),
    ))?;
    op_view_for(&wiki, &op_id)
}

/// POST /api/wiki/ops/{id}/reject — reject a pending op.
pub async fn reject_op(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Path(op_id): Path<String>,
    body: Option<Json<RejectOpRequest>>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let env = wiki.list_pending_ops().map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list failed: {e}"),
    ))?.into_iter().find(|e| e.op_id == op_id);
    if env.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "op not found or not pending"));
    }
    let reason = body.and_then(|Json(b)| b.reason).unwrap_or_default();
    let reason = if reason.is_empty() { "rejected by user".into() } else { reason };
    wiki.reject_op(&op_id, &me.id, &reason).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("reject failed: {e}"),
    ))?;
    op_view_for(&wiki, &op_id)
}

/// POST /api/wiki/ops/approve-all — bulk-approve pending ops (optionally only
/// those with confidence ≥ `min_confidence`). Returns `{ "approved": n }`.
pub async fn approve_all_ops(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    body: Option<Json<BulkApproveRequest>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let min_confidence = body.and_then(|Json(b)| b.min_confidence);
    let n = wiki.approve_pending_bulk(&me.id, min_confidence).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("bulk approve failed: {e}"),
    ))?;
    Ok(Json(serde_json::json!({ "approved": n })))
}

/// POST /api/wiki/ops/reject-all — bulk-reject pending ops (optionally only
/// those below `max_confidence`). Returns `{ "rejected": n }`.
pub async fn reject_all_ops(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    body: Option<Json<BulkRejectRequest>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let (reason, max_confidence) = match body {
        Some(Json(b)) => (b.reason, b.max_confidence),
        None => (None, None),
    };
    let reason = reason.filter(|r| !r.is_empty()).unwrap_or_else(|| "bulk-rejected by user".into());
    let n = wiki.reject_pending_bulk(&me.id, &reason, max_confidence).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("bulk reject failed: {e}"),
    ))?;
    Ok(Json(serde_json::json!({ "rejected": n })))
}

// ── Internals ────────────────────────────────────────────────────────────────

/// Build an `OpView` for an op id by scanning recent history. Used by
/// the write/approve/reject endpoints so the response carries the final
/// envelope state (including `applied_at`).
fn op_view_for(
    wiki: &WikiSystem, op_id: &str,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    // Recent window — generous so we always find what we just submitted.
    let since = Utc::now() - chrono::Duration::hours(1);
    let envs = wiki.list_recent_ops(since, 200).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("op lookup failed: {e}"),
    ))?;
    let env = envs.into_iter().find(|e| e.op_id == op_id).ok_or_else(|| err(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("submitted op '{op_id}' not found in recent history"),
    ))?;
    Ok(Json(OpView::from(env)))
}

// ── Admin (system wiki) endpoints — Slice F ──────────────────────────────────
//
// Mirror of the per-user wiki API, scoped to the shared system wiki. All
// admin endpoints require the `AdminUser` extractor (403 otherwise) and
// stamp writes with `Provenance::user_ui(<admin id>)` so each change is
// attributable in the audit log.
//
// Writes to `persona.md` automatically hot-reload the runtime system
// prompt — admins don't need to restart the server to see their change
// take effect.

#[derive(Debug, Serialize)]
pub struct AdminReloadResponse {
    pub reloaded: bool,
    pub message: String,
}

/// GET /api/admin/wiki/pages — list every page in the system wiki.
pub async fn admin_list_pages(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<Vec<PageSummary>>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = system_wiki(&agent)?;
    let store = wiki.store();
    let paths = store.list_pages().map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list_pages failed: {e}"),
    ))?;
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let summary = match store.read_page(&path) {
            Ok(page) => PageSummary {
                path: path.to_string(),
                title: page.frontmatter.title.clone(),
                writer: page.frontmatter.writer.as_str().to_string(),
                tags: page.frontmatter.tags.clone(),
                valid_from: page.frontmatter.valid_from.map(|d| d.to_string()),
                valid_to:   page.frontmatter.valid_to.map(|d| d.to_string()),
                is_special: path.is_special(),
            },
            Err(_) => PageSummary {
                path: path.to_string(), title: None, writer: "both".into(),
                tags: vec![], valid_from: None, valid_to: None,
                is_special: path.is_special(),
            },
        };
        out.push(summary);
    }
    Ok(Json(out))
}

/// GET /api/admin/wiki/page?path=<rel> — read one system wiki page.
pub async fn admin_get_page(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Query(q): Query<PathQuery>,
) -> Result<Json<PageDetail>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = system_wiki(&agent)?;
    let path = parse_wiki_path(&q.path)?;
    let page = wiki.store().try_read_page(&path).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("read failed: {e}"),
    ))?;
    let page = page.ok_or_else(|| err(StatusCode::NOT_FOUND, format!("page not found: {}", q.path)))?;
    let provenance = page.frontmatter.provenance.iter().map(|p| ProvenanceView {
        source: p.source.clone(),
        turn_id: p.turn_id.clone(),
        conversation_id: p.conversation_id.clone(),
        extracted_at: p.extracted_at.timestamp_millis(),
    }).collect();
    Ok(Json(PageDetail {
        path: path.to_string(),
        title: page.frontmatter.title.clone(),
        writer: page.frontmatter.writer.as_str().to_string(),
        tags: page.frontmatter.tags.clone(),
        valid_from: page.frontmatter.valid_from.map(|d| d.to_string()),
        valid_to:   page.frontmatter.valid_to.map(|d| d.to_string()),
        confidence: page.frontmatter.confidence,
        body: page.body,
        provenance,
    }))
}

/// PUT /api/admin/wiki/page — write a page in the system wiki. If the
/// path is `persona.md`, the runtime system prompt is hot-reloaded so
/// the change takes effect on the next turn without a restart.
pub async fn admin_put_page(
    AdminUser(me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Json(body): Json<PutPageRequest>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    if body.body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "body cannot be empty"));
    }
    let path = parse_wiki_path(&body.path)?;
    let wiki = system_wiki(&agent)?;

    let mut fm = match wiki.store().try_read_page(&path) {
        Ok(Some(existing)) => existing.frontmatter.clone(),
        _ => PageFrontmatter::default(),
    };
    if body.title.is_some() { fm.title = body.title.clone(); }
    if let Some(tags) = body.tags.clone() { fm.tags = tags; }
    if let Some(w) = body.writer.as_deref() {
        fm.writer = match w {
            "user" => crate::wiki::frontmatter::Writer::User,
            "agent" => crate::wiki::frontmatter::Writer::Agent,
            "both" => crate::wiki::frontmatter::Writer::Both,
            other => return Err(err(StatusCode::BAD_REQUEST,
                format!("invalid writer '{other}'"))),
        };
    }

    let is_persona = path.as_str() == "persona.md";
    let op = WikiOp::WritePage { path: path.clone(), frontmatter: fm, body: body.body };
    let op_id = wiki.submit_and_apply(op, Provenance::user_ui(&me.id))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("apply failed: {e}")))?;

    // Hot-reload the runtime prompt on persona changes.
    if is_persona {
        match agent.reload_system_prompt_from_wiki() {
            Ok(true)  => tracing::info!("system_prompt reloaded after admin edit by {}", me.id),
            Ok(false) => tracing::warn!("persona.md saved but reload returned false (empty body?)"),
            Err(e)    => tracing::warn!("persona.md saved but reload failed: {e}"),
        }
    }
    op_view_for_system(&wiki, &op_id)
}

/// POST /api/admin/wiki/page/append-section — granular admin edit.
pub async fn admin_append_section(
    AdminUser(me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Json(body): Json<AppendSectionRequest>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    let path = parse_wiki_path(&body.path)?;
    if body.section.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "section is required"));
    }
    if body.body.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "body is required"));
    }
    let wiki = system_wiki(&agent)?;
    let is_persona = path.as_str() == "persona.md";
    let op = WikiOp::AppendSection { path, section: body.section, body: body.body };
    let op_id = wiki.submit_and_apply(op, Provenance::user_ui(&me.id))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("apply failed: {e}")))?;
    if is_persona {
        let _ = agent.reload_system_prompt_from_wiki();
    }
    op_view_for_system(&wiki, &op_id)
}

/// DELETE /api/admin/wiki/page?path=<rel> — archive a system-wiki page.
pub async fn admin_delete_page(
    AdminUser(me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Query(q): Query<PathQuery>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    let path = parse_wiki_path(&q.path)?;
    if path.is_special() {
        return Err(err(StatusCode::BAD_REQUEST,
            format!("cannot delete special navigation file '{}'", q.path)));
    }
    let wiki = system_wiki(&agent)?;
    let op = WikiOp::DeletePage { path };
    let op_id = wiki.submit_and_apply(op, Provenance::user_ui(&me.id))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("apply failed: {e}")))?;
    op_view_for_system(&wiki, &op_id)
}

/// GET /api/admin/wiki/nav — bundle of the system wiki's nav files.
pub async fn admin_get_nav(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<NavBundle>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = system_wiki(&agent)?;
    let store = wiki.store();
    Ok(Json(NavBundle {
        profile: store.read_core_raw().unwrap_or_default(),
        index:   store.read_index_raw().unwrap_or_default(),
        schema:  store.read_schema_raw().unwrap_or_default(),
        log:     store.read_log_raw().unwrap_or_default(),
    }))
}

/// GET /api/admin/wiki/ops — recent admin / system-scope ops.
pub async fn admin_list_recent_ops(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Query(q): Query<RecentOpsQuery>,
) -> Result<Json<Vec<OpView>>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = system_wiki(&agent)?;
    let since: DateTime<Utc> = match q.since {
        Some(ms) => DateTime::from_timestamp_millis(ms).unwrap_or_else(|| Utc::now() - chrono::Duration::days(1)),
        None     => Utc::now() - chrono::Duration::days(1),
    };
    let limit = q.limit.unwrap_or(50).min(200);
    let envs = wiki.list_recent_ops(since, limit).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("list_recent_ops failed: {e}"),
    ))?;
    Ok(Json(envs.into_iter().map(OpView::from).collect()))
}

/// POST /api/admin/wiki/reload-prompt — re-read persona.md and swap
/// the runtime system prompt. Used after an out-of-band edit (e.g.
/// admin opened the file in vim).
pub async fn admin_reload_prompt(
    AdminUser(_me): AdminUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<AdminReloadResponse>, (StatusCode, Json<serde_json::Value>)> {
    match agent.reload_system_prompt_from_wiki() {
        Ok(true) => Ok(Json(AdminReloadResponse {
            reloaded: true,
            message: "system prompt reloaded from persona.md".into(),
        })),
        Ok(false) => Ok(Json(AdminReloadResponse {
            reloaded: false,
            message: "persona.md missing or empty; runtime prompt unchanged".into(),
        })),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("reload failed: {e}"))),
    }
}

fn op_view_for_system(
    wiki: &WikiSystem, op_id: &str,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    let since = Utc::now() - chrono::Duration::hours(1);
    let envs = wiki.list_recent_ops(since, 200).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("op lookup failed: {e}"),
    ))?;
    let env = envs.into_iter().find(|e| e.op_id == op_id).ok_or_else(|| err(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("submitted op '{op_id}' not found in recent history"),
    ))?;
    Ok(Json(OpView::from(env)))
}

// ── Save-thread endpoint (Slice H) ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SaveThreadRequest {
    pub conversation_id: String,
    /// Override the target page path. Defaults to
    /// `pages/conversations/<title-slug>-<short-id>.md`.
    pub path: Option<String>,
    /// Override the page title. Defaults to the conversation title.
    pub title: Option<String>,
    /// Cap on the number of messages included. Default 200; messages
    /// beyond the cap are dropped with a tail marker.
    pub max_messages: Option<usize>,
}

/// POST /api/wiki/save-thread — turn a conversation into a wiki page.
/// Auto-applies the resulting `WikiOp::WritePage` under
/// `Provenance::user_ui` since the caller is explicitly saving their
/// own thread.
pub async fn save_thread(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Extension(history): Extension<Arc<crate::history::HistoryStore>>,
    Json(body): Json<SaveThreadRequest>,
) -> Result<Json<OpView>, (StatusCode, Json<serde_json::Value>)> {
    let conv = history.get_conversation(&body.conversation_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("get conv: {e}")))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "conversation not found"))?;
    if conv.user_id != me.id {
        return Err(err(StatusCode::FORBIDDEN, "conversation belongs to a different user"));
    }

    let max_messages = body.max_messages.unwrap_or(200).min(1000);
    let messages = history.get_messages(&conv.id, max_messages as i64, None)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("get messages: {e}")))?;

    let title = body.title
        .or_else(|| conv.title.clone())
        .unwrap_or_else(|| format!("Conversation {}", &conv.id[..8]));
    let path_str = body.path.unwrap_or_else(|| {
        let slug = title_slug(&title);
        let short = conv.id.chars().take(8).collect::<String>();
        format!("pages/conversations/{slug}-{short}.md")
    });
    let path = parse_wiki_path(&path_str)?;

    let md = format_thread_as_markdown(&conv, &messages, &title);
    let mut fm = PageFrontmatter::default();
    fm.title = Some(title.clone());
    fm.tags = vec!["conversation".to_string(), conv.channel.clone()];
    fm.writer = crate::wiki::frontmatter::Writer::User;

    let wiki = user_wiki(&agent, &me.id)?;
    let op = WikiOp::WritePage { path: path.clone(), frontmatter: fm, body: md };
    let op_id = wiki.submit_and_apply(op, Provenance::user_ui(&me.id))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("apply failed: {e}")))?;
    op_view_for(&wiki, &op_id)
}

fn title_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() {
            for low in c.to_lowercase() { out.push(low); }
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() { "thread".to_string() } else {
        trimmed.chars().take(60).collect()
    }
}

fn format_thread_as_markdown(
    conv: &crate::history::Conversation,
    messages: &[crate::history::Message],
    title: &str,
) -> String {
    use chrono::{TimeZone, Utc};
    let mut out = String::new();
    out.push_str(&format!("# {title}\n\n"));
    out.push_str(&format!("Saved from conversation `{}` (channel: `{}`).\n\n",
                          &conv.id, conv.channel));
    out.push_str("---\n\n");
    for m in messages {
        let when = Utc.timestamp_millis_opt(m.created_at).single()
            .map(|d| d.to_rfc3339())
            .unwrap_or_default();
        let role = match m.role {
            crate::history::MessageRole::User      => "User",
            crate::history::MessageRole::Assistant => "Assistant",
            crate::history::MessageRole::System    => "System",
            crate::history::MessageRole::Tool      => "Tool",
        };
        out.push_str(&format!("### {role} · {when}\n\n"));
        out.push_str(m.content.trim());
        out.push_str("\n\n");
    }
    out
}

// ── Git endpoints (Slice G) ──────────────────────────────────────────────────
//
// Per-user wiki only — system-wiki git is admin-handled via direct
// filesystem access; we don't expose a separate `/api/admin/wiki/git`
// surface yet because admins who can edit persona.md by HTTP also
// have shell access by definition.

#[derive(Debug, Deserialize)]
pub struct GitSetRemoteRequest {
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct GitCommitRequest {
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GitOpResponse {
    pub ok: bool,
    pub output: String,
}

/// GET /api/wiki/git/status — snapshot of the wiki's git state.
pub async fn git_status(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<crate::wiki::git::GitStatus>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    wiki.git_status().map(Json).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("git status: {e}"),
    ))
}

/// POST /api/wiki/git/commit — manual commit (`message` optional).
pub async fn git_commit(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    body: Option<Json<GitCommitRequest>>,
) -> Result<Json<GitOpResponse>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let msg = body
        .and_then(|Json(b)| b.message)
        .unwrap_or_else(|| "wiki: manual commit".to_string());
    let made = wiki.git_commit_all(&msg).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("commit: {e}"),
    ))?;
    Ok(Json(GitOpResponse {
        ok: true,
        output: if made { format!("committed: {msg}") } else { "nothing to commit".into() },
    }))
}

/// POST /api/wiki/git/remote — set / replace the `origin` remote.
pub async fn git_set_remote(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    Json(body): Json<GitSetRemoteRequest>,
) -> Result<Json<GitOpResponse>, (StatusCode, Json<serde_json::Value>)> {
    if body.url.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "url is required"));
    }
    let wiki = user_wiki(&agent, &me.id)?;
    wiki.git_set_remote(body.url.trim()).map_err(|e| err(
        StatusCode::INTERNAL_SERVER_ERROR, format!("set remote: {e}"),
    ))?;
    Ok(Json(GitOpResponse { ok: true, output: format!("origin = {}", body.url.trim()) }))
}

/// POST /api/wiki/git/push — push to `origin/<current-branch>`.
pub async fn git_push(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<GitOpResponse>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let output = wiki.git_push().map_err(|e| err(
        StatusCode::BAD_GATEWAY, format!("push: {e}"),
    ))?;
    Ok(Json(GitOpResponse { ok: true, output }))
}

/// POST /api/wiki/git/pull — pull `origin/<current-branch>` with merge.
pub async fn git_pull(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<Json<GitOpResponse>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let output = wiki.git_pull().map_err(|e| err(
        StatusCode::BAD_GATEWAY, format!("pull: {e}"),
    ))?;
    Ok(Json(GitOpResponse { ok: true, output }))
}

// ── Import / export (Slice G) ────────────────────────────────────────────────

use axum::body::Body;
use axum::extract::Multipart;
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::IntoResponse;

/// GET /api/wiki/export — stream the user's wiki as a .tar.gz.
pub async fn export_tarball(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let root = wiki.root().to_path_buf();
    // We build the archive in memory then ship it. Wikis are small
    // (kilobytes to a few MB) — full streaming would be nice but isn't
    // worth the lifetime gymnastics here.
    let mut buf: Vec<u8> = Vec::new();
    let count = tokio::task::spawn_blocking(move || {
        crate::wiki::import_export::export_tar_gz(&root, &mut buf).map(|n| (n, buf))
    }).await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("export task: {e}")))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("export: {e}")))?;
    let (n, buf) = count;

    let date = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let filename = format!("wiki-{}-{}.tar.gz", me.id, date);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE,
        HeaderValue::from_static("application/gzip"));
    headers.insert(header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")));
    headers.insert("x-wiki-entries", HeaderValue::from_str(&n.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("0")));

    Ok((headers, Body::from(buf)))
}

#[derive(Debug, Serialize)]
pub struct ImportResponse {
    pub ok: bool,
    pub entries: usize,
    pub message: String,
}

/// POST /api/wiki/import — multipart upload of a .tar.gz, extracted
/// into the user's wiki root. Refuses entries that try to escape via
/// `..` or absolute paths.
pub async fn import_tarball(
    AuthUser(me): AuthUser,
    Extension(agent): Extension<Arc<AgentCore>>,
    mut multipart: Multipart,
) -> Result<Json<ImportResponse>, (StatusCode, Json<serde_json::Value>)> {
    let wiki = user_wiki(&agent, &me.id)?;
    let root = wiki.root().to_path_buf();

    // Pull the first file field; we don't care about its name.
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| err(
        StatusCode::BAD_REQUEST, format!("multipart: {e}"),
    ))? {
        let data = field.bytes().await.map_err(|e| err(
            StatusCode::BAD_REQUEST, format!("multipart body: {e}"),
        ))?;
        bytes = Some(data.to_vec());
        break;
    }
    let bytes = bytes.ok_or_else(|| err(
        StatusCode::BAD_REQUEST, "no file part in upload",
    ))?;

    let n = tokio::task::spawn_blocking(move || {
        crate::wiki::import_export::import_tar_gz(&root, bytes.as_slice())
    }).await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("import task: {e}")))?
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("import: {e}")))?;

    // After an import, commit the new tree if auto-commit is on — gives
    // the user one fat "wiki: imported" point in `git log` they can
    // revert to.
    if wiki.git_enabled() {
        let _ = wiki.git_commit_all("wiki: imported tarball");
    }
    Ok(Json(ImportResponse {
        ok: true,
        entries: n,
        message: format!("imported {n} entr{}", if n == 1 { "y" } else { "ies" }),
    }))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::{PageFrontmatter, Provenance, WikiOp, WikiPath, WikiRegistry, WikiSystem};

    fn seeded_registry() -> (tempfile::TempDir, Arc<WikiRegistry>) {
        let dir = tempfile::tempdir().unwrap();
        let wiki = WikiSystem::for_user(dir.path(), "u1").unwrap();
        let mut fm = PageFrontmatter::default();
        fm.title = Some("Pong".into());
        fm.tags = vec!["project".into()];
        wiki.submit_and_apply(WikiOp::WritePage {
            path: WikiPath::parse("pages/pong.md").unwrap(),
            frontmatter: fm,
            body: "# Pong\nNotes.\n".into(),
        }, Provenance::user_ui("u1")).unwrap();
        let reg = Arc::new(WikiRegistry::new(dir.path().to_path_buf()));
        (dir, reg)
    }

    #[test]
    fn op_view_serializes_with_useful_fields() {
        let (_dir, reg) = seeded_registry();
        let wiki = reg.for_user("u1").unwrap();
        let envs = wiki.list_recent_ops(Utc::now() - chrono::Duration::hours(1), 10).unwrap();
        assert!(!envs.is_empty());
        let v = OpView::from(envs[0].clone());
        assert_eq!(v.kind, "write_page");
        assert_eq!(v.scope, "user");
        assert_eq!(v.user_id.as_deref(), Some("u1"));
        assert!(v.created_at > 0);
        // op field carries the full op shape for the UI to inspect.
        assert!(v.op.is_object());
    }

    #[tokio::test]
    async fn pending_ops_isolated_per_user() {
        let dir = tempfile::tempdir().unwrap();
        let reg = WikiRegistry::new(dir.path().to_path_buf());
        let w1 = reg.for_user("alice").unwrap();
        let w2 = reg.for_user("bob").unwrap();

        // Alice has a pending op.
        let op = WikiOp::LogEntry {
            kind: LogKind::Note,
            summary: "private to alice".into(),
            page_refs: vec![],
        };
        w1.submit_op(op, Provenance::user_ui("alice")).unwrap();

        assert_eq!(w1.list_pending_ops().unwrap().len(), 1);
        // Bob's queue is empty — separate DB.
        assert_eq!(w2.list_pending_ops().unwrap().len(), 0);
    }

    #[test]
    fn approve_op_flips_status_to_applied() {
        let (_dir, reg) = seeded_registry();
        let wiki = reg.for_user("u1").unwrap();
        let op = WikiOp::WritePage {
            path: WikiPath::parse("pages/proposed.md").unwrap(),
            frontmatter: PageFrontmatter::default(),
            body: "proposed body\n".into(),
        };
        let op_id = wiki.submit_op(op, Provenance::from_turn("extractor", "t1", "c1")).unwrap();
        assert_eq!(wiki.list_pending_ops().unwrap().len(), 1);
        wiki.approve_op(&op_id, "u1").unwrap();
        // Page now exists.
        assert!(wiki.root().join("pages/proposed.md").exists());
        assert_eq!(wiki.list_pending_ops().unwrap().len(), 0);
    }

    #[test]
    fn system_wiki_persona_seed_matches_default_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let reg = WikiRegistry::new(dir.path().to_path_buf());
        let sys = reg.system().unwrap();
        let persona = std::fs::read_to_string(sys.root().join("persona.md")).unwrap();
        // Body — after frontmatter — equals the runtime default.
        let (_fm, body) = crate::wiki::frontmatter::parse(&persona).unwrap();
        assert_eq!(body.trim(), crate::system_prompt::DEFAULT_SYSTEM_PROMPT.trim());
    }

    #[test]
    fn reject_op_leaves_file_untouched() {
        let (_dir, reg) = seeded_registry();
        let wiki = reg.for_user("u1").unwrap();
        let op = WikiOp::WritePage {
            path: WikiPath::parse("pages/never.md").unwrap(),
            frontmatter: PageFrontmatter::default(),
            body: "would-be content\n".into(),
        };
        let op_id = wiki.submit_op(op, Provenance::from_turn("extractor", "t", "c")).unwrap();
        wiki.reject_op(&op_id, "u1", "not relevant").unwrap();
        assert!(!wiki.root().join("pages/never.md").exists());
        assert_eq!(wiki.list_pending_ops().unwrap().len(), 0);
    }
}
