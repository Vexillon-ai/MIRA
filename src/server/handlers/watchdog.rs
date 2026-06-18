// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/watchdog.rs
//! Slice W3 — HTTP API for watchdog incidents.
//!
//! Three endpoints:
//!   * `GET  /api/watchdog/incidents/{id}`        — fetch one incident.
//!   * `POST /api/watchdog/incidents/{id}/analyze` — kick off LLM
//!     diagnosis via `Action::Prompt`. Returns the conversation id
//!     where the agent will post its reply.
//!   * `GET  /api/watchdog/incidents`              — admin-only list.
//!
//! Authorization model: an incident's `user_id` is the recipient
//! configured in `automations.watchdog.notify_user_id`. Owners can
//! always read + analyze their own incidents; admins can read +
//! analyze any. The list endpoint is admin-only because it surfaces
//! every user's alerts (a junior admin's WARN/ERROR isn't
//! necessarily for the senior admin's eyes, but the access pattern
//! is closer to "ops triage" than per-user, so we keep it gated).

use std::sync::Arc;

use axum::{
    extract::{Json, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Extension,
};
use serde::Deserialize;
use tracing::{info, warn};

use crate::agent::{AgentCore, TurnContext, StreamEvent};
use crate::auth::{AuthUser, Role};
use crate::automations::AutomationsStore;
use crate::history::{HistoryStore, NewConversation};
use crate::notifications::{Notification, NotificationBus, NotificationKind};
use crate::MiraError;

fn err(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

// ── GET /api/watchdog/incidents/{id} ─────────────────────────────────────────

pub async fn get_incident(
    AuthUser(caller):  AuthUser,
    Extension(store):  Extension<Arc<AutomationsStore>>,
    Path(id):          Path<String>,
) -> axum::response::Response {
    let inc = match store.get_watchdog_incident(&id) {
        Ok(Some(i)) => i,
        Ok(None)    => return err(StatusCode::NOT_FOUND, "incident not found"),
        Err(e)      => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };
    if inc.user_id != caller.id && caller.role != Role::Admin {
        return err(StatusCode::FORBIDDEN, "not your incident");
    }
    (StatusCode::OK, Json(inc)).into_response()
}

// ── GET /api/watchdog/incidents (admin) ──────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct ListIncidentsQuery {
    /// Limit on rows returned. Default 50, hard cap 500.
    #[serde(default)]
    pub limit:   Option<usize>,
    /// Filter by user_id. Defaults to the caller's id.
    #[serde(default)]
    pub user_id: Option<String>,
}

pub async fn list_incidents(
    AuthUser(caller):  AuthUser,
    Extension(store):  Extension<Arc<AutomationsStore>>,
    Query(q):          Query<ListIncidentsQuery>,
) -> axum::response::Response {
    if caller.role != Role::Admin {
        return err(StatusCode::FORBIDDEN, "admin only");
    }
    let user_id = q.user_id.unwrap_or_else(|| caller.id.clone());
    let limit = q.limit.unwrap_or(50).min(500);
    match store.list_watchdog_incidents(&user_id, limit) {
        Ok(rows) => (StatusCode::OK, Json(rows)).into_response(),
        Err(e)   => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

// ── POST /api/watchdog/incidents/{id}/analyze ────────────────────────────────

#[derive(serde::Serialize)]
struct AnalyzeResp {
    incident_id:     String,
    conversation_id: String,
    message:         String,
}

pub async fn analyze_incident(
    AuthUser(caller):    AuthUser,
    Extension(store):    Extension<Arc<AutomationsStore>>,
    Extension(history):  Extension<Arc<HistoryStore>>,
    Extension(agent):    Extension<Arc<AgentCore>>,
    Extension(notifs):   Extension<Arc<NotificationBus>>,
    // 0.109.0 — optional HealthStore for the trend-aware analyst.
    // axum's `Option<Extension<T>>` extractor returns None when the
    // layer isn't installed (tests / minimal builds), so the handler
    // falls back to the plain prompt without health enrichment.
    health_store_opt: Option<Extension<Arc<crate::health::store::HealthStore>>>,
    Path(id):            Path<String>,
) -> axum::response::Response {
    let inc = match store.get_watchdog_incident(&id) {
        Ok(Some(i)) => i,
        Ok(None)    => return err(StatusCode::NOT_FOUND, "incident not found"),
        Err(e)      => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };
    if inc.user_id != caller.id && caller.role != Role::Admin {
        return err(StatusCode::FORBIDDEN, "not your incident");
    }

    // Reuse an existing conversation when the incident already has
    // one (handles double-clicks without spawning another). Otherwise
    // open a fresh one titled after the incident.
    let conv_id = if let Some(existing) = inc.conversation_id.as_deref() {
        existing.to_string()
    } else {
        let title = format!(
            "Watchdog: {} {} — {}",
            inc.severity,
            inc.module,
            chrono::DateTime::<chrono::Utc>::from_timestamp(inc.created_at, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| inc.created_at.to_string()),
        );
        match history.create_conversation(NewConversation {
            user_id:          inc.user_id.clone(),
            channel:          "web".to_string(),
            title:            Some(title),
            model:            None,
            provider:         None,
            external_user_id: None,
            mode:             None,
        }) {
            Ok(c)  => c.id,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("create_conversation: {e}")),
        }
    };

    // Idempotent flip: returns false when status was already !=none.
    // Either way we proceed to surface the conversation_id; the
    // analyze worker will skip re-running on a dup.
    let is_new_run = match store.mark_incident_analysis_queued(&inc.id, &conv_id) {
        Ok(b)  => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("queue: {e}")),
    };

    if is_new_run {
        info!(
            "watchdog: analysis queued for incident {} (conv={}, requested by {})",
            inc.id, conv_id, caller.id,
        );
        let mut prompt = build_analysis_prompt(&inc);
        // 0.109.0 — append health-trend context for system_health
        // incidents. No-op (empty string) for log/db-derived alerts.
        let health_store_ref = health_store_opt.as_ref().map(|Extension(s)| s);
        let trend = crate::health::trend_context::enrich_prompt(
            health_store_ref, &store, &inc,
        );
        if !trend.is_empty() { prompt.push_str(&trend); }
        // 0.112.2 — remediation hints for ANY incident. Grounds the
        // LLM in the actual dashboard surfaces + endpoints available
        // instead of guessing at file paths.
        prompt.push_str(&crate::health::trend_context::render_remediation_hints(&inc));
        spawn_analysis_task(
            Arc::clone(&store),
            Arc::clone(&agent),
            Arc::clone(&history),
            Arc::clone(&notifs),
            inc.id.clone(),
            inc.user_id.clone(),
            conv_id.clone(),
            prompt,
        );
    } else {
        info!(
            "watchdog: analyze re-clicked for incident {} — returning existing conv {}",
            inc.id, conv_id,
        );
    }

    (StatusCode::ACCEPTED, Json(AnalyzeResp {
        incident_id: inc.id,
        conversation_id: conv_id,
        message: if is_new_run {
            "analysis queued — watch the conversation for the agent reply".into()
        } else {
            "analysis already in flight — surfacing existing conversation".into()
        },
    })).into_response()
}

/// Render the operator-facing "diagnose this" prompt. Front-loads the
/// concrete fields so the LLM has zero ambiguity about what's being
/// asked. Truncates payload_json so a multi-MB payload doesn't blow
/// the model's context window.
fn build_analysis_prompt(inc: &crate::automations::WatchdogIncident) -> String {
    let payload_excerpt: String = inc.payload_json.chars().take(2_000).collect();
    let when = chrono::DateTime::<chrono::Utc>::from_timestamp(inc.created_at, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| inc.created_at.to_string());
    format!(
        "[Watchdog incident — diagnose + remediate]\n\n\
         The MIRA watchdog detected the following anomaly. Please give a concise diagnosis \
         and a concrete remediation plan. Surface the actionable steps first.\n\n\
         - Severity: {sev}\n\
         - Source:   {src}\n\
         - Module:   {mod_}\n\
         - Time:     {when}\n\n\
         Message:\n```\n{msg}\n```\n\n\
         Detection context:\n```json\n{payload}\n```\n\n\
         Answer the operator with:\n\
         1. Most likely root cause (1-2 sentences).\n\
         2. Whether this looks transient (network blip, race) or persistent (config bug, missing dep).\n\
         3. Concrete next steps — log lines to grep for, files to edit, commands to run.\n\
         Be terse. The operator already saw the alert.",
        sev   = inc.severity,
        src   = inc.source,
        mod_  = inc.module,
        when  = when,
        msg   = inc.message,
        payload = payload_excerpt,
    )
}

/// Spawn the agent turn that produces the analysis. Runs detached
/// (tokio::spawn) so the HTTP handler can return 202 immediately;
/// the user navigates to the conversation and watches it populate
/// over the next few seconds.
///
/// `agent.process_with_context` does NOT write to conversation history
/// — the chat handler does that itself for both sides of the turn.
/// Mirror the same pattern here: persist the analysis prompt as the
/// user-side message *before* calling the agent so it's visible in
/// the chat the user lands on, then persist the streamed assistant
/// response *after*. Without this the conversation appears empty
/// even though the analysis ran.
fn spawn_analysis_task(
    store:    Arc<AutomationsStore>,
    agent:    Arc<AgentCore>,
    history:  Arc<HistoryStore>,
    notifs:   Arc<NotificationBus>,
    incident_id:     String,
    user_id:         String,
    conversation_id: String,
    prompt:          String,
) {
    tokio::spawn(async move {
        // Persist the prompt as the user message so the conversation
        // has context the user can re-read and follow up on. Failure
        // here is logged but doesn't block the analysis — better to
        // produce a diagnosis with no preserved prompt than to
        // silently abort.
        if let Err(e) = history.add_message(crate::history::NewMessage {
            conversation_id: conversation_id.clone(),
            role:            crate::history::MessageRole::User,
            content:         prompt.clone(),
            content_type:    "text".to_owned(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata:        Some(serde_json::json!({
                "watchdog_incident_id": incident_id,
                "kind":                 "watchdog_analyze_request",
            }).to_string()),
        }) {
            warn!("watchdog analyze: persist user message failed for {incident_id}: {e}");
        }
        let _ = history.touch_conversation(&conversation_id);

        let ctx = TurnContext::default();
        let mut rx = match agent.process_with_context(
            &conversation_id, &user_id, "web", &prompt, None, ctx,
        ).await {
            Ok(r)  => r,
            Err(e) => {
                warn!("watchdog analyze: agent.process failed for incident {incident_id}: {e}");
                let err_text = format!("(analysis failed: {e})");
                let _ = history.add_message(crate::history::NewMessage {
                    conversation_id: conversation_id.clone(),
                    role:            crate::history::MessageRole::Assistant,
                    content:         err_text.clone(),
                    content_type:    "text".to_owned(),
                    token_count:     None,
                    model:           None,
                    tool_calls:      None,
                    metadata:        Some(serde_json::json!({
                        "watchdog_incident_id": incident_id,
                        "kind":                 "watchdog_analyze_error",
                    }).to_string()),
                });
                let _ = store.mark_incident_analysis_completed(&incident_id, &err_text);
                notifs.send(Notification {
                    kind:            NotificationKind::ConversationUpdated,
                    conversation_id: Some(conversation_id.clone()),
                    channel:         Some("web".to_owned()),
                    user_id:         Some(user_id.clone()),
                    message:         None,
                });
                return;
            }
        };
        let mut text = String::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Token(t)     => text.push_str(&t),
                StreamEvent::Done { .. }  => break,
                StreamEvent::Error(e)     => {
                    warn!("watchdog analyze: stream error for incident {incident_id}: {e}");
                    break;
                }
                _ => {}
            }
        }
        // Persist the assistant reply so it shows up in the
        // conversation the user navigated to.
        if let Err(e) = history.add_message(crate::history::NewMessage {
            conversation_id: conversation_id.clone(),
            role:            crate::history::MessageRole::Assistant,
            content:         text.clone(),
            content_type:    "text".to_owned(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata:        Some(serde_json::json!({
                "watchdog_incident_id": incident_id,
                "kind":                 "watchdog_analyze_response",
            }).to_string()),
        }) {
            warn!("watchdog analyze: persist assistant message failed for {incident_id}: {e}");
        }
        let _ = history.touch_conversation(&conversation_id);
        if let Err(e) = store.mark_incident_analysis_completed(&incident_id, &text) {
            warn!("watchdog analyze: mark_completed failed for {incident_id}: {e}");
        }
        // Broadcast so any open ChatPage on this conversation refetches
        // the messages query — without this the page sits on its
        // initial fetch (user prompt only) until the user navigates
        // away and back.
        notifs.send(Notification {
            kind:            NotificationKind::ConversationUpdated,
            conversation_id: Some(conversation_id.clone()),
            channel:         Some("web".to_owned()),
            user_id:         Some(user_id.clone()),
            message:         None,
        });
    });
}

// Suppress unused-helper warning when MiraError isn't referenced
// directly — keeps the import for future endpoints that bubble it.
#[allow(dead_code)] fn _retain_imports(e: MiraError) -> MiraError { e }
