// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/guardian.rs
//! MIRA-Guardian action approval (P4a-2). Admin-only HTTP surface over the
//! pending action proposals the Guardian recorded (P4a-1):
//!
//!   GET    /api/guardian/actions            — list (optional ?status=pending)
//!   POST   /api/guardian/actions/{id}/approve  — execute + record outcome
//!   POST   /api/guardian/actions/{id}/decline  — reject, never execute
//!
//! The LLM only ever *proposes*; execution lives here, in deterministic
//! server code, gated by an explicit human approval. Each decision is persisted
//! on the action row (status + decided_at + outcome) and logged. The action set
//! is bounded + reversible (re-run/requeue/trim go through the automations
//! scheduler; restart-bridge through the ChannelManager) — never shell, config,
//! data-delete, or a MIRA self-restart.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use tracing::{info, warn};

use crate::agent::audit::{AuditEvent, AuditStore, guardian_agent_id};
use crate::agent::guardian_actions::{GuardianActionStatus, GuardianActionStore};
use crate::auth::{AuthUser, Role};
use crate::automations::AutomationsStore;
use crate::server::handlers::channel_accounts::ChannelManagerExt;

fn admin_only(caller: &AuthUser) -> Option<Response> {
    if caller.0.role != Role::Admin {
        Some((StatusCode::FORBIDDEN, "admin only").into_response())
    } else { None }
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Append a tamper-evident HMAC-chain record for a Guardian action decision.
/// Best-effort: a missing/failed audit store never blocks the operation.
fn audit_decision(
    audit: &Option<Extension<Arc<AuditStore>>>,
    action_id: &str, action_kind: &str, decision: &str, detail: Option<String>,
) {
    if let Some(Extension(store)) = audit {
        let _ = store.record(guardian_agent_id(), AuditEvent::GuardianAction {
            action_id:   action_id.to_string(),
            action_kind: action_kind.to_string(),
            decision:    decision.to_string(),
            detail,
        });
    }
}

#[derive(Debug, Deserialize)]
pub struct ListQuery { pub status: Option<String> }

/// GET /api/guardian/actions[?status=pending|executed|declined|failed]
pub async fn list_actions(
    caller: AuthUser,
    Extension(store): Extension<Arc<GuardianActionStore>>,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Some(r) = admin_only(&caller) { return r; }
    let status = match q.status.as_deref() {
        Some("pending")  => Some(GuardianActionStatus::Pending),
        Some("executed") => Some(GuardianActionStatus::Executed),
        Some("declined") => Some(GuardianActionStatus::Declined),
        Some("failed")   => Some(GuardianActionStatus::Failed),
        _                => None,
    };
    match store.list(status, 100) {
        Ok(rows) => (StatusCode::OK, Json(rows)).into_response(),
        Err(e)   => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

/// POST /api/guardian/actions/{id}/approve — execute the bounded action.
pub async fn approve_action(
    caller: AuthUser,
    Extension(store): Extension<Arc<GuardianActionStore>>,
    automations: Option<Extension<Arc<AutomationsStore>>>,
    channel_mgr: Option<Extension<ChannelManagerExt>>,
    audit: Option<Extension<Arc<AuditStore>>>,
    Path(id): Path<String>,
) -> Response {
    if let Some(r) = admin_only(&caller) { return r; }

    let action = match store.get(&id) {
        Ok(Some(a)) => a,
        Ok(None)    => return err(StatusCode::NOT_FOUND, "action not found"),
        Err(e)      => return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    };
    if action.status != GuardianActionStatus::Pending {
        return err(StatusCode::CONFLICT,
            &format!("action already {} — not pending", action.status.as_str()));
    }

    info!("guardian action APPROVED by {}: {} {:?} [id={id}]",
          caller.0.username, action.kind.as_str(), action.target);
    audit_decision(&audit, &id, action.kind.as_str(), "approved",
                   Some(format!("approved by {}", caller.0.username)));

    let outcome = crate::agent::guardian_actions::execute_action(
        action.kind,
        action.target.as_deref(),
        automations.as_ref().map(|e| &e.0),
        channel_mgr.as_ref().map(|e| &e.0.0),
    ).await;

    let kind = action.kind.as_str();
    match outcome {
        Ok(msg) => {
            let _ = store.decide(&id, GuardianActionStatus::Executed, &msg);
            audit_decision(&audit, &id, kind, "executed", Some(msg.clone()));
            info!("guardian action EXECUTED [id={id}]: {msg}");
            (StatusCode::OK, Json(serde_json::json!({
                "id": id, "status": "executed", "result": msg
            }))).into_response()
        }
        Err(e) => {
            let _ = store.decide(&id, GuardianActionStatus::Failed, &e);
            audit_decision(&audit, &id, kind, "failed", Some(e.clone()));
            warn!("guardian action FAILED [id={id}]: {e}");
            (StatusCode::OK, Json(serde_json::json!({
                "id": id, "status": "failed", "error": e
            }))).into_response()
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct DeclineBody { pub note: Option<String> }

/// POST /api/guardian/actions/{id}/decline — reject without executing.
pub async fn decline_action(
    caller: AuthUser,
    Extension(store): Extension<Arc<GuardianActionStore>>,
    audit: Option<Extension<Arc<AuditStore>>>,
    Path(id): Path<String>,
    body: Option<Json<DeclineBody>>,
) -> Response {
    if let Some(r) = admin_only(&caller) { return r; }
    let note = body.and_then(|b| b.0.note).unwrap_or_else(|| "declined by operator".to_string());
    let kind = store.get(&id).ok().flatten().map(|a| a.kind.as_str().to_string()).unwrap_or_default();
    match store.decide(&id, GuardianActionStatus::Declined, &note) {
        Ok(true)  => {
            audit_decision(&audit, &id, &kind, "declined",
                           Some(format!("{note} (by {})", caller.0.username)));
            info!("guardian action DECLINED by {} [id={id}]: {note}", caller.0.username);
            (StatusCode::OK, Json(serde_json::json!({ "id": id, "status": "declined" }))).into_response()
        }
        Ok(false) => err(StatusCode::CONFLICT, "action not found or already decided"),
        Err(e)    => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")),
    }
}

// execute_action now lives in agent::guardian_actions (shared with the watch
// loop's isolation-autonomy path); the approve handler calls it above.

// ── Always-on status ─────────────────────────────────────────────────────────

/// GET /api/guardian/status — the always-on operator view of the Guardian. Unlike
/// the provision/actions panels (which only render when there's something to *do*),
/// this surfaces the Guardian's state whenever it exists: its mode, the fail-closed
/// local-model verdict + alias binding, the proactive watch loop's liveness (last
/// tick + last alert), and recent action history. So the operator can always see
/// *that it's running and what it's been doing*, even when idle + healthy.
pub async fn status(
    caller: AuthUser,
    Extension(live_cfg): Extension<Arc<crate::web::LiveConfig>>,
    actions: Option<Extension<Arc<GuardianActionStore>>>,
) -> Response {
    if let Some(r) = admin_only(&caller) { return r; }
    let cfg = live_cfg.get().await;
    let check = crate::agent::guardian::model_check(&cfg);
    let alias_set = cfg.agent.llm_aliases.contains_key(crate::agent::guardian::GUARDIAN_ALIAS);
    let recent = match &actions {
        Some(Extension(s)) => s.list(None, 10).unwrap_or_default(),
        None               => Vec::new(),
    };
    let watch = crate::agent::guardian::watch_status().read().await.clone();

    (StatusCode::OK, Json(serde_json::json!({
        "mode":                format!("{:?}", crate::agent::guardian::mode(&cfg)),
        "local_model_ok":      check.allowed,
        "model_check":         check.reason,
        "guardian_alias_set":  alias_set,
        "watch_interval_secs": cfg.guardian.watch_interval_secs,
        "isolation_dry_run":   cfg.guardian.isolation_dry_run,
        "watch":               watch,
        "recent_actions":      recent,
    }))).into_response()
}

// ── P2b — provisioning status ────────────────────────────────────────────────

/// GET /api/guardian/provision/status — tells the operator (and the UI) what's
/// needed to give the Guardian a local model: is one already resolvable, is
/// Ollama reachable, is the recommended model pulled, is the alias set.
pub async fn provision_status(
    caller: AuthUser,
    Extension(live_cfg): Extension<Arc<crate::web::LiveConfig>>,
) -> Response {
    if let Some(r) = admin_only(&caller) { return r; }
    let cfg = live_cfg.get().await;
    let check = crate::agent::guardian::model_check(&cfg);
    let alias_set = cfg.agent.llm_aliases.contains_key(crate::agent::guardian::GUARDIAN_ALIAS);
    let model = cfg.guardian.provision_model.clone();
    let ourl  = cfg.providers.ollama.url.clone();
    let (reachable, version, present) = probe_ollama(&ourl, &model).await;

    let next_step = if check.allowed {
        "Guardian already has a local model — nothing to provision.".to_string()
    } else if !reachable {
        format!("Install/start Ollama (expected at {ourl}), then provision the Guardian model.")
    } else if !present {
        format!("Ollama is up — pull '{model}' and bind it (provision).")
    } else {
        format!("Ollama has '{model}' — bind it to the Guardian (provision sets the alias).")
    };

    (StatusCode::OK, Json(serde_json::json!({
        "guardian_mode":      format!("{:?}", crate::agent::guardian::mode(&cfg)),
        "local_model_ok":     check.allowed,
        "model_check":        check.reason,
        "guardian_alias_set": alias_set,
        "ollama": {
            "url":               ourl,
            "reachable":         reachable,
            "version":           version,
            "recommended_model": model,
            "model_present":     present,
        },
        "next_step": next_step,
    }))).into_response()
}

/// POST /api/guardian/provision — pull `guardian.provision_model` via Ollama (if
/// not already present) and bind the Guardian to it (`guardian` llm-alias →
/// ollama/<model>) via the safe `LiveConfig::update`. The pull can take minutes,
/// so it runs in the background; poll `/provision/status`. A restart is needed
/// afterward for the (startup-snapshot) Guardian resolver to use the new alias.
pub async fn provision(
    caller: AuthUser,
    Extension(live_cfg): Extension<Arc<crate::web::LiveConfig>>,
) -> Response {
    if let Some(r) = admin_only(&caller) { return r; }
    let cfg   = live_cfg.get().await;
    let model = cfg.guardian.provision_model.clone();
    let ourl  = cfg.providers.ollama.url.clone();
    let base  = ourl.trim_end_matches('/').trim_end_matches("/v1").to_string();

    let (reachable, _, present) = probe_ollama(&ourl, &model).await;
    if !reachable {
        return err(StatusCode::BAD_REQUEST,
            &format!("Ollama not reachable at {base} — install/start it, then retry."));
    }

    let lc = Arc::clone(&live_cfg);
    let (m, b) = (model.clone(), base.clone());
    tokio::spawn(async move {
        if !present {
            info!("guardian provision: pulling '{m}' from Ollama…");
            if let Err(e) = ollama_pull(&b, &m).await {
                warn!("guardian provision: pull '{m}' failed: {e}");
                return;
            }
            info!("guardian provision: pulled '{m}'");
        }
        match wire_guardian_alias(&lc, &m).await {
            Ok(()) => info!("guardian provision: bound guardian alias → ollama/{m}. \
                             Restart MIRA to apply."),
            Err(e) => warn!("guardian provision: alias wiring failed: {e}"),
        }
    });

    (StatusCode::ACCEPTED, Json(serde_json::json!({
        "status": "provisioning_started",
        "model":  model,
        "note":   "Pulling (if needed) + binding in the background — poll /api/guardian/provision/status. \
                   Restart MIRA once done to bind the Guardian to the new model.",
    }))).into_response()
}

/// Pull a model via the Ollama native `/api/pull` (non-streaming; long timeout).
async fn ollama_pull(base: &str, model: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(900)).build().map_err(|e| e.to_string())?;
    let resp = client.post(format!("{base}/api/pull"))
        .json(&serde_json::json!({ "name": model, "stream": false }))
        .send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("pull HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.unwrap_or_default();
    if v.get("error").and_then(|e| e.as_str()).is_some() {
        Err(format!("ollama: {}", v["error"]))
    } else {
        Ok(()) // {"status":"success"} (or an empty/ok body)
    }
}

/// Bind the Guardian to a local Ollama model by setting the `guardian` llm-alias
/// + enabling the ollama provider, persisted via the safe `LiveConfig::update`
/// (validate → persist → broadcast). Never a raw config write.
async fn wire_guardian_alias(live_cfg: &crate::web::LiveConfig, model: &str) -> Result<(), String> {
    let mut cfg = (*live_cfg.get().await).clone();
    cfg.agent.llm_aliases.insert(
        crate::agent::guardian::GUARDIAN_ALIAS.to_string(),
        crate::config::LlmAlias { provider: "ollama".to_string(), model: Some(model.to_string()) },
    );
    cfg.providers.ollama.enabled = true;
    live_cfg.update(cfg).await.map_err(|e| e.to_string())
}

/// Probe the local Ollama native API: reachable? version? is `model` pulled?
/// The Ollama native endpoints live at the root (not the `/v1` OpenAI-compat
/// path the chat provider uses), so strip a trailing `/v1`.
async fn probe_ollama(url: &str, model: &str) -> (bool, Option<String>, bool) {
    let base = url.trim_end_matches('/').trim_end_matches("/v1").to_string();
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4)).build() else { return (false, None, false); };

    let version = match client.get(format!("{base}/api/version")).send().await {
        Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.ok()
            .and_then(|v| v.get("version").and_then(|s| s.as_str()).map(String::from)),
        _ => return (false, None, false),
    };
    let present = match client.get(format!("{base}/api/tags")).send().await {
        Ok(r) => r.json::<serde_json::Value>().await.ok()
            .and_then(|v| v.get("models").and_then(|m| m.as_array().cloned()))
            .map(|arr| arr.iter().any(|m| m.get("name").and_then(|n| n.as_str())
                .map(|n| n == model || n.starts_with(&format!("{model}:"))).unwrap_or(false)))
            .unwrap_or(false),
        Err(_) => false,
    };
    (true, version, present)
}
