// SPDX-License-Identifier: AGPL-3.0-or-later

//! Agents HTTP surface (slice B7).
//!
//! - **GET /api/agents** — list every agent currently in the registry
//!   plus the multi-agent runtime config the UI cares about
//!   (depth cap, default session budget). Empty when nothing is
//!   running.
//! - **POST /api/agents/{id}/interrupt** — propagate Stop to one agent.
//!   When the agent is the root of a tree, every active descendant is
//!   signalled too (so the user's "Stop" button cleans up an entire
//!   user-request tree in one call).
//! - **POST /api/agents/{id}/pause** — flip status to Paused.
//! - **POST /api/agents/{id}/resume** — flip status back to Running.
//!
//! Status snapshot is computed on each request — registry size is
//! small (single-host trees, hundreds of agents max) and trees are
//! short-lived enough that stale-cache headaches aren't worth the
//! complexity.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::agent::{
    Agent, AgentId, AgentRegistry, AgentStatus, AuditEvent, AuditFilter, AuditRecord,
    InterruptReason, Supervisor, MAX_RECURSION_DEPTH,
};
use crate::agent::instance::LlmChoice;
use crate::agent::supervisor::DEFAULT_SESSION_BUDGET_USD;
use crate::auth::middleware::AuthUser;

// ─── DTOs ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AgentsResponse {
    pub agents:                Vec<AgentDto>,
    pub max_recursion_depth:   u8,
    pub default_session_usd:   f64,
    /// Fleet-wide rollup (Phase A3) so the dashboard shows "what's happening
    /// right now" without the client re-deriving it every render.
    pub aggregate:             FleetAggregate,
}

/// Live rollup across every agent currently in the registry.
#[derive(Debug, Serialize, Default)]
pub struct FleetAggregate {
    pub total:        usize,
    pub running:      usize,
    pub paused:       usize,
    pub completed:    usize,
    pub failed:       usize,
    pub interrupted:  usize,
    /// Sum of `spent_usd` across all agents (the live burn for the fleet).
    pub total_spent_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentDto {
    pub id:             String,
    /// `None` for root agents.
    pub parent:         Option<String>,
    pub skill_id:       Option<String>,
    pub status:         &'static str,
    pub depth:          u8,
    pub created_at_ms:  i64,
    /// Free-form one-liner the worker last reported via Progress.
    pub current_step:   Option<String>,
    /// Last self-reported progress fraction (0.0–1.0), if any — drives the
    /// dashboard progress bar (Phase A3).
    pub percent_done:   Option<f32>,
    /// Set when status == Completed.
    pub result_summary: Option<String>,
    /// Set when status == Failed / Interrupted (human one-liner).
    pub failure_reason: Option<String>,
    /// Structured fault (Phase A1): `{ code, … }` — a precise machine-readable
    /// cause (budget_exceeded / timeout / policy_denied / …). Null on success.
    pub fault:          Option<crate::agent::instance::AgentFault>,
    pub spent_usd:      f64,
    /// `null` when the agent has unlimited budget (root only).
    pub max_usd:        Option<f64>,
    /// Direct children, sorted by spawn time.
    pub child_ids:      Vec<String>,
    /// Resolved (provider, model) the agent is using (slice B8).
    /// Null for agents that didn't have a choice assigned at spawn.
    pub llm_choice:     Option<LlmChoiceDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmChoiceDto {
    pub alias:    String,
    pub provider: String,
    pub model:    Option<String>,
}

impl From<&LlmChoice> for LlmChoiceDto {
    fn from(c: &LlmChoice) -> Self {
        Self { alias: c.alias.clone(), provider: c.provider.clone(), model: c.model.clone() }
    }
}

impl AgentDto {
    fn from_agent(a: &Agent, child_ids: Vec<String>) -> Self {
        let max_usd = if a.budget.max_usd.is_finite() { Some(a.budget.max_usd) } else { None };
        Self {
            id:             a.id.to_string(),
            parent:         a.parent.map(|p| p.to_string()),
            skill_id:       a.skill_id.clone(),
            status:         status_str(a.status),
            depth:          a.depth,
            created_at_ms:  a.created_at,
            current_step:   a.current_step.clone(),
            percent_done:   a.percent_done,
            result_summary: a.result_summary.clone(),
            failure_reason: a.failure_reason.clone(),
            fault:          a.fault.clone(),
            spent_usd:      a.budget.spent_usd,
            max_usd,
            child_ids,
            llm_choice:     a.llm_choice.as_ref().map(LlmChoiceDto::from),
        }
    }
}

fn status_str(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Pending     => "pending",
        AgentStatus::Running     => "running",
        AgentStatus::Paused      => "paused",
        AgentStatus::Completed   => "completed",
        AgentStatus::Failed      => "failed",
        AgentStatus::Interrupted => "interrupted",
    }
}

// ─── Handlers ──────────────────────────────────────────────────────────

pub async fn list_agents(
    AuthUser(_user):              AuthUser,
    Extension(registry):          Extension<Arc<AgentRegistry>>,
) -> Json<AgentsResponse> {
    Json(build_agents_response(&registry))
}

/// Build the full fleet snapshot (agents sorted by id + aggregate rollup).
/// Shared by `list_agents` and the live `agents_stream`.
fn build_agents_response(registry: &AgentRegistry) -> AgentsResponse {
    // Snapshot then sort by id so the JSON is stable across calls.
    let mut handles = registry.list();
    handles.sort_by_key(|h| h.read().map(|a| a.id.0).unwrap_or_default());

    let mut agents = Vec::with_capacity(handles.len());
    let mut agg = FleetAggregate::default();
    for h in &handles {
        let a = match h.read() { Ok(a) => a, Err(_) => continue };
        let child_ids: Vec<String> = registry.children_of(a.id).iter()
            .filter_map(|c| c.read().ok().map(|c| c.id.to_string()))
            .collect();
        let dto = AgentDto::from_agent(&a, child_ids);
        agg.total += 1;
        agg.total_spent_usd += dto.spent_usd;
        match dto.status {
            "running"     => agg.running += 1,
            "paused"      => agg.paused += 1,
            "completed"   => agg.completed += 1,
            "failed"      => agg.failed += 1,
            "interrupted" => agg.interrupted += 1,
            _             => {}
        }
        agents.push(dto);
    }

    AgentsResponse {
        agents,
        max_recursion_depth: MAX_RECURSION_DEPTH,
        default_session_usd: DEFAULT_SESSION_BUDGET_USD,
        aggregate: agg,
    }
}

/// `GET /api/agents/stream` — fleet-wide SSE. Server polls the registry ~1s and
/// pushes a fresh full snapshot whenever anything changes (status, spend, step,
/// or the agent set), so the dashboard is live without a client poll. First
/// snapshot is sent immediately; the stream stays open until the client leaves.
pub async fn agents_stream(
    AuthUser(_user):     AuthUser,
    Extension(registry): Extension<Arc<AgentRegistry>>,
) -> axum::response::Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use tokio_stream::wrappers::ReceiverStream;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(8);
    tokio::spawn(async move {
        // Sentinel that no real signature equals, so the first iteration always
        // pushes an initial snapshot (even an empty fleet) to the client.
        let mut last_sig = "\u{0}init".to_string();
        loop {
            let resp = build_agents_response(&registry);
            // Cheap change signature: id:status:step:spent per agent.
            let sig: String = resp.agents.iter()
                .map(|a| format!("{}:{}:{}:{:.4}:{:.3}",
                    a.id, a.status, a.current_step.as_deref().unwrap_or(""), a.spent_usd,
                    a.percent_done.unwrap_or(-1.0)))
                .collect::<Vec<_>>()
                .join("|");
            if sig != last_sig {
                last_sig = sig;
                if let Ok(json) = serde_json::to_string(&resp) {
                    if tx.send(Ok(SseEvent::default().data(json))).await.is_err() {
                        break; // client disconnected
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct InterruptBody {
    /// One of "user", "timeout", "budget", "policy". Defaults to "user"
    /// when absent — the most common case (the Stop button click).
    #[serde(default)]
    pub reason: Option<String>,
    /// When true, propagate the interrupt to every active descendant of
    /// this agent (the design doc's "Stop" button mechanic). When false,
    /// only the named agent is signalled.
    #[serde(default = "default_propagate")]
    pub propagate: bool,
}
fn default_propagate() -> bool { true }

#[derive(Debug, Serialize)]
pub struct InterruptResponse {
    /// How many agents were actually signalled. 0 when the named agent
    /// has already finished.
    pub signalled: usize,
}

pub async fn interrupt_agent(
    AuthUser(_user):       AuthUser,
    Extension(supervisor): Extension<Arc<Supervisor>>,
    Path(agent_id):        Path<String>,
    Json(body):            Json<InterruptBody>,
) -> impl IntoResponse {
    let agent_id = match parse_agent_id(&agent_id) {
        Ok(id) => id,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error(&msg))).into_response(),
    };
    let reason = parse_reason(body.reason.as_deref());

    let signalled = if body.propagate {
        supervisor.interrupt_tree(agent_id, reason).await
    } else {
        match supervisor.interrupt(agent_id, reason).await {
            Ok(()) => 1,
            Err(_) => 0,
        }
    };

    (StatusCode::OK, Json(InterruptResponse { signalled })).into_response()
}

pub async fn pause_agent(
    AuthUser(_user):       AuthUser,
    Extension(supervisor): Extension<Arc<Supervisor>>,
    Path(agent_id):        Path<String>,
) -> impl IntoResponse {
    let agent_id = match parse_agent_id(&agent_id) {
        Ok(id) => id,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error(&msg))).into_response(),
    };
    match supervisor.pause(agent_id).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(error(&e.to_string()))).into_response(),
    }
}

pub async fn resume_agent(
    AuthUser(_user):       AuthUser,
    Extension(supervisor): Extension<Arc<Supervisor>>,
    Path(agent_id):        Path<String>,
) -> impl IntoResponse {
    let agent_id = match parse_agent_id(&agent_id) {
        Ok(id) => id,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error(&msg))).into_response(),
    };
    match supervisor.resume(agent_id).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(error(&e.to_string()))).into_response(),
    }
}

// ─── Audit log (slice B9) ──────────────────────────────────────────────

/// Query string for `GET /api/agents/audit`. All filters optional —
/// `?limit=` defaults to 200 to keep payloads bounded.
#[derive(Debug, Deserialize, Default)]
pub struct AuditQuery {
    /// UUID of an agent to filter to. Omit to see every agent's events.
    pub agent_id: Option<String>,
    /// Comma-separated event_kinds (e.g. `spawn_denied,interrupted`).
    pub kinds:    Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub limit:    Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct AuditRowDto {
    pub id:        i64,
    pub ts_ms:     i64,
    pub agent_id:  String,
    pub kind:      &'static str,
    pub event:     AuditEvent,
    pub prev_hmac: String,
    pub hmac:      String,
}

impl From<AuditRecord> for AuditRowDto {
    fn from(r: AuditRecord) -> Self {
        Self {
            id:        r.id,
            ts_ms:     r.ts_ms,
            agent_id:  r.agent_id.to_string(),
            kind:      r.event.kind(),
            event:     r.event,
            prev_hmac: r.prev_hmac,
            hmac:      r.hmac,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuditResponse {
    pub rows:  Vec<AuditRowDto>,
    /// True iff the audit chain verified clean — false (with `chain_break`
    /// populated) when verification stops at a tampered or deleted row.
    pub chain_ok:    bool,
    pub chain_break: Option<String>,
}

pub async fn list_audit(
    AuthUser(_user):       AuthUser,
    Extension(supervisor): Extension<Arc<Supervisor>>,
    Query(q):              Query<AuditQuery>,
) -> impl IntoResponse {
    let store = match supervisor.audit_store() {
        Some(s) => s,
        None    => return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error("audit store not initialised on this server")),
        ).into_response(),
    };

    // Parse filters.
    let agent_filter = match q.agent_id.as_deref() {
        Some(s) => match parse_agent_id(s) {
            Ok(id) => Some(id),
            Err(e) => return (StatusCode::BAD_REQUEST, Json(error(&e))).into_response(),
        },
        None => None,
    };
    let kinds_owned: Vec<String> = q.kinds.as_deref().map(|raw| {
        raw.split(',').map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()).collect()
    }).unwrap_or_default();
    let kinds_static: Vec<&'static str> = kinds_owned.iter()
        .filter_map(|k| kind_to_static(k)).collect();

    let filter = AuditFilter {
        agent_id: agent_filter,
        kinds:    kinds_static,
        since_ms: q.since_ms,
        until_ms: q.until_ms,
        limit:    q.limit,
    };
    let rows = match store.query(&filter) {
        Ok(r)  => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR,
                          Json(error(&format!("audit query: {e}")))).into_response(),
    };

    // Verify the chain so the UI can flag tampering. Cheap relative to
    // a normal request — single full table scan with HMAC recompute.
    let (chain_ok, chain_break) = match store.verify_chain() {
        Ok(())  => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };

    let resp = AuditResponse {
        rows: rows.into_iter().map(AuditRowDto::from).collect(),
        chain_ok,
        chain_break,
    };
    (StatusCode::OK, Json(resp)).into_response()
}

/// Map a user-supplied kind string to the `&'static str` used by the
/// `AuditFilter::kinds` slot. Returns `None` for unknown kinds, which
/// the caller silently drops — better than 400-erroring on a typo
/// since the user can still see other kinds in the result.
fn kind_to_static(s: &str) -> Option<&'static str> {
    Some(match s {
        "spawn_requested"          => "spawn_requested",
        "spawn_approved"           => "spawn_approved",
        "spawn_denied"             => "spawn_denied",
        "status_change"            => "status_change",
        "agent_budget_exceeded"    => "agent_budget_exceeded",
        "session_budget_exceeded"  => "session_budget_exceeded",
        "interrupted"              => "interrupted",
        "policy_decision"          => "policy_decision",
        _ => return None,
    })
}

// ─── helpers ───────────────────────────────────────────────────────────

fn parse_agent_id(raw: &str) -> Result<AgentId, String> {
    uuid::Uuid::parse_str(raw)
        .map(AgentId)
        .map_err(|e| format!("invalid agent id {raw:?}: {e}"))
}

fn parse_reason(raw: Option<&str>) -> InterruptReason {
    match raw.unwrap_or("user").to_lowercase().as_str() {
        "timeout" => InterruptReason::Timeout,
        "budget"  => InterruptReason::Budget,
        "policy"  => InterruptReason::Policy,
        _         => InterruptReason::User,
    }
}

fn error(msg: &str) -> serde_json::Value {
    serde_json::json!({"error": msg})
}

// ── 0.113.0 — agent detail (activity + stdout) ──────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct AgentActivityResponse {
    /// Snapshot of the agent's current registry state. None when the
    /// agent isn't (or no longer is) in the live registry — happens
    /// when rewatching a completed task whose row already aged out.
    pub agent: Option<AgentDto>,
    /// Audit events in chronological order. Pulled from `agent_audit`
    /// by agent_id filter.
    pub audit: Vec<AuditEntry>,
    /// Per-Progress event lines parsed from `progress.jsonl` in the
    /// artifact dir. Empty for legacy tasks pre-0.113.0.
    pub progress: Vec<ProgressEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub ts_ms: i64,
    pub kind:  String,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEntry {
    pub ts_ms:         i64,
    pub summary:       String,
    #[serde(default)]
    pub percent_done:  Option<f32>,
    #[serde(default)]
    pub llm_spend_usd: f64,
}

pub async fn agent_activity(
    AuthUser(_user):       AuthUser,
    Extension(registry):   Extension<Arc<AgentRegistry>>,
    audit_store:           Option<Extension<Arc<crate::agent::AuditStore>>>,
    task_artifacts:        Option<Extension<Arc<crate::task_artifacts::TaskArtifactsStore>>>,
    Path(agent_id):        Path<String>,
) -> axum::response::Response {
    let aid = match parse_agent_id(&agent_id) {
        Ok(id) => id,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error(&msg))).into_response(),
    };

    // Live agent (may be None for rewatch of completed tasks that
    // dropped from the registry).
    let agent = registry.get(aid).and_then(|h| {
        h.read().ok().map(|a| {
            let kids = registry.children_of(a.id).iter()
                .filter_map(|c| c.read().ok().map(|c| c.id.to_string())).collect();
            AgentDto::from_agent(&a, kids)
        })
    });

    // Audit history for this agent (oldest first; agent_audit returns
    // newest first, so we reverse).
    let audit: Vec<AuditEntry> = match audit_store {
        Some(Extension(store)) => store.query(&crate::agent::audit::AuditFilter {
            agent_id: Some(aid),
            limit:    Some(500),
            ..Default::default()
        }).unwrap_or_default().into_iter().rev().map(|r| AuditEntry {
            ts_ms:  r.ts_ms,
            kind:   r.event.kind().to_string(),
            detail: serde_json::to_value(&r.event).unwrap_or(serde_json::Value::Null),
        }).collect(),
        None => Vec::new(),
    };

    // Progress events from the artifact dir's progress.jsonl.
    let progress: Vec<ProgressEntry> = match task_artifacts {
        Some(Extension(arts)) => {
            arts.find_dir_by_task_id(&aid.0.to_string())
                .and_then(|dir| std::fs::read_to_string(dir.join("logs/progress.jsonl")).ok())
                .map(|s| s.lines()
                    .filter_map(|l| serde_json::from_str::<ProgressEntry>(l).ok())
                    .collect())
                .unwrap_or_default()
        }
        None => Vec::new(),
    };

    (StatusCode::OK, Json(AgentActivityResponse { agent, audit, progress })).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct StdoutQuery {
    /// Number of trailing bytes to return. Default 64 KB. Hard cap 1 MB.
    #[serde(default)]
    pub tail: Option<usize>,
    /// Byte offset to read FROM (instead of tail). Used by the SSE
    /// stream + the polling-with-cursor frontend. Mutually exclusive
    /// with tail; offset wins when both present.
    #[serde(default)]
    pub offset: Option<u64>,
    /// "stdout" (default) or "stderr".
    #[serde(default)]
    pub which: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StdoutResponse {
    pub content:   String,
    /// Current size of the file (= next valid offset for incremental
    /// reads). 0 when the file doesn't exist.
    pub size:      u64,
    /// Byte offset this response starts at.
    pub offset:    u64,
    /// True when the agent is still running (and the file may grow).
    pub running:   bool,
    pub truncated: bool,
}

pub async fn agent_stdout(
    AuthUser(_user):       AuthUser,
    Extension(registry):   Extension<Arc<AgentRegistry>>,
    task_artifacts:        Option<Extension<Arc<crate::task_artifacts::TaskArtifactsStore>>>,
    Path(agent_id):        Path<String>,
    axum::extract::Query(q): axum::extract::Query<StdoutQuery>,
) -> axum::response::Response {
    let aid = match parse_agent_id(&agent_id) {
        Ok(id) => id,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error(&msg))).into_response(),
    };
    let Some(Extension(arts)) = task_artifacts else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(error("task artifacts not wired"))).into_response();
    };
    let Some(dir) = arts.find_dir_by_task_id(&aid.0.to_string()) else {
        return (StatusCode::NOT_FOUND, Json(error("no artifact dir for that agent"))).into_response();
    };
    let which = q.which.as_deref().unwrap_or("stdout");
    let filename = match which {
        "stdout" => "logs/stdout.log",
        "stderr" => "logs/stderr.log",
        _ => return (StatusCode::BAD_REQUEST, Json(error("which must be stdout or stderr"))).into_response(),
    };
    let path = dir.join(filename);
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let running = registry.get(aid)
        .and_then(|h| h.read().ok().map(|a| matches!(a.status,
            crate::agent::AgentStatus::Running | crate::agent::AgentStatus::Pending)))
        .unwrap_or(false);

    // Cap user-supplied tail at 1 MB; offset reads everything from
    // that point forward (also capped at 1 MB chunk for SSE friendliness).
    const MAX_CHUNK: u64 = 1024 * 1024;
    let (content, start_offset) = if let Some(off) = q.offset {
        let off = off.min(size);
        let read_end = (off + MAX_CHUNK).min(size);
        let bytes = read_range(&path, off, read_end - off).unwrap_or_default();
        (bytes, off)
    } else {
        let tail = q.tail.unwrap_or(64 * 1024).min(MAX_CHUNK as usize) as u64;
        let off = size.saturating_sub(tail);
        let bytes = read_range(&path, off, size - off).unwrap_or_default();
        (bytes, off)
    };
    let truncated = q.offset.is_none() && size > content.len() as u64;

    (StatusCode::OK, Json(StdoutResponse {
        content,
        size,
        offset:    start_offset,
        running,
        truncated,
    })).into_response()
}

/// SSE variant of `agent_activity`. Polls the underlying sources
/// every 1s on the server side and emits a fresh full snapshot when
/// the audit/progress counts change. Closes when the agent reaches a
/// terminal state AND no new events arrive for ~3s.
pub async fn agent_activity_stream(
    AuthUser(_user):       AuthUser,
    Extension(registry):   Extension<Arc<AgentRegistry>>,
    audit_store:           Option<Extension<Arc<crate::agent::AuditStore>>>,
    task_artifacts:        Option<Extension<Arc<crate::task_artifacts::TaskArtifactsStore>>>,
    Path(agent_id):        Path<String>,
) -> axum::response::Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use tokio_stream::wrappers::ReceiverStream;
    let aid = match parse_agent_id(&agent_id) {
        Ok(id) => id,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error(&msg))).into_response(),
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(8);
    let audit = audit_store.map(|Extension(s)| s);
    let arts = task_artifacts.map(|Extension(s)| s);

    tokio::spawn(async move {
        let mut last_audit_len = 0usize;
        let mut last_progress_len = 0usize;
        let mut idle_after_terminal_ticks = 0u32;
        loop {
            // Build a fresh snapshot.
            let agent = registry.get(aid).and_then(|h| {
                h.read().ok().map(|a| {
                    let kids = registry.children_of(a.id).iter()
                        .filter_map(|c| c.read().ok().map(|c| c.id.to_string())).collect();
                    AgentDto::from_agent(&a, kids)
                })
            });
            let audit_rows: Vec<AuditEntry> = match audit.as_ref() {
                Some(s) => s.query(&crate::agent::audit::AuditFilter {
                    agent_id: Some(aid), limit: Some(500), ..Default::default()
                }).unwrap_or_default().into_iter().rev().map(|r| AuditEntry {
                    ts_ms: r.ts_ms, kind: r.event.kind().to_string(),
                    detail: serde_json::to_value(&r.event).unwrap_or(serde_json::Value::Null),
                }).collect(),
                None => Vec::new(),
            };
            let progress_rows: Vec<ProgressEntry> = match arts.as_ref() {
                Some(a) => a.find_dir_by_task_id(&aid.0.to_string())
                    .and_then(|d| std::fs::read_to_string(d.join("logs/progress.jsonl")).ok())
                    .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            // Only send on change (or every 5s anyway, via SSE keepalive).
            let changed = audit_rows.len() != last_audit_len
                || progress_rows.len() != last_progress_len;
            last_audit_len = audit_rows.len();
            last_progress_len = progress_rows.len();

            let resp = AgentActivityResponse {
                agent: agent.clone(),
                audit: audit_rows, progress: progress_rows,
            };
            if changed {
                if let Ok(json) = serde_json::to_string(&resp) {
                    if tx.send(Ok(SseEvent::default().data(json))).await.is_err() {
                        break;  // client disconnected
                    }
                }
            }

            // Termination: agent is in a terminal state AND we've
            // had a few consecutive idle ticks.
            let terminal = agent.as_ref().map(|a| matches!(a.status,
                "completed" | "failed" | "interrupted")).unwrap_or(true);
            if terminal && !changed {
                idle_after_terminal_ticks += 1;
                if idle_after_terminal_ticks >= 3 { break; }
            } else {
                idle_after_terminal_ticks = 0;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

/// SSE variant of `agent_stdout`. Tracks a byte offset and pushes new
/// content as the log file grows. Closes when the agent reaches
/// terminal AND no new bytes arrive for 3s.
pub async fn agent_stdout_stream(
    AuthUser(_user):       AuthUser,
    Extension(registry):   Extension<Arc<AgentRegistry>>,
    task_artifacts:        Option<Extension<Arc<crate::task_artifacts::TaskArtifactsStore>>>,
    Path(agent_id):        Path<String>,
    axum::extract::Query(q): axum::extract::Query<StdoutQuery>,
) -> axum::response::Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use tokio_stream::wrappers::ReceiverStream;
    let aid = match parse_agent_id(&agent_id) {
        Ok(id) => id,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(error(&msg))).into_response(),
    };
    let Some(Extension(arts)) = task_artifacts else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(error("task artifacts not wired"))).into_response();
    };
    let Some(dir) = arts.find_dir_by_task_id(&aid.0.to_string()) else {
        return (StatusCode::NOT_FOUND, Json(error("no artifact dir"))).into_response();
    };
    let which = q.which.unwrap_or_else(|| "stdout".to_string());
    let path = dir.join(match which.as_str() {
        "stderr" => "logs/stderr.log",
        _ => "logs/stdout.log",
    });
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(8);

    tokio::spawn(async move {
        // Start from tail if no offset given (consistent with polling
        // endpoint), then poll the file every 500ms.
        let starting_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut offset: u64 = match q.offset {
            Some(o) => o.min(starting_size),
            None    => starting_size.saturating_sub(q.tail.unwrap_or(64*1024) as u64),
        };
        let mut idle_after_terminal_ticks = 0u32;
        loop {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size > offset {
                let chunk_size = (size - offset).min(256 * 1024);  // 256K per push
                if let Some(content) = read_range(&path, offset, chunk_size) {
                    let payload = serde_json::json!({
                        "content": content, "offset": offset, "size": size,
                    }).to_string();
                    if tx.send(Ok(SseEvent::default().data(payload))).await.is_err() {
                        break;
                    }
                    offset += chunk_size;
                }
                idle_after_terminal_ticks = 0;
            } else {
                // No new bytes. Check if agent is done.
                let terminal = registry.get(aid)
                    .and_then(|h| h.read().ok().map(|a| matches!(a.status,
                        crate::agent::AgentStatus::Completed
                        | crate::agent::AgentStatus::Failed
                        | crate::agent::AgentStatus::Interrupted)))
                    .unwrap_or(true);
                if terminal {
                    idle_after_terminal_ticks += 1;
                    if idle_after_terminal_ticks >= 6 { break; }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

fn read_range(path: &std::path::Path, offset: u64, len: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0u8; len as usize];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    // Lossy is fine — these files are bytes streamed from an LLM
    // tool and any byte that's not valid UTF-8 in the middle of a
    // chunk is best rendered as � rather than refused.
    Some(String::from_utf8_lossy(&buf).into_owned())
}
