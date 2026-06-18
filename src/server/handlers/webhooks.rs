// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/webhooks.rs
//! Webhook HTTP surface.
//!
//! Two distinct mounts:
//!
//! - **Public ingest**: `POST /webhook/incoming/{token}`. Anonymous; the
//! token + HMAC signature is the auth. Lives outside the Bearer-protected
//! `/api/*` tree so the AuthLayer doesn't reject inbound calls from
//! GitHub / Stripe / cron-by-curl. The handler runs through:
//!   1. token lookup (404 if unknown / paused / expired);
//!   2. HMAC-SHA256 verification (`X-Webhook-Signature: sha256=<hex>`);
//!   3. optional replay-window check (`X-Webhook-Timestamp` within ±5min);
//!   4. per-webhook rate limit (`rate_limit_per_min`);
//!   5. predicate evaluation against `{payload, headers, now}`;
//!   6. payload appended to ring buffer (always, for debugging);
//!   7. on match → render `payload_template` (if any) → dispatch action.
//!
//! - **Authenticated CRUD**: `/api/webhooks/...`. Standard owner-scoped
//! list / get / create / update / delete plus token-rotate, secret-rotate,
//! pause / resume, payload audit, and a "test replay" endpoint that
//! re-runs a stored payload without round-tripping the public POST.
//!
//! Authorization mirrors the schedules surface: a user sees their own rows;
//! admins see everything; the seeded `system` rows are read-only listed.

use std::sync::{Arc, LazyLock};

use axum::{
    body::Bytes,
    extract::{Path, Query},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tracing::{debug, warn};

use crate::auth::{AuthUser, models::Role};
use crate::automations::{
    Action, AutomationsStore, AutomationStatus, NewWebhook, OwnerKind, UpdateWebhook,
    Webhook, WebhookPayload, Worker,
    agent_gate::{gate_create_webhook, GateError},
    dispatch::Activation,
    predicate,
};
use crate::config::MiraConfig;
use crate::security::hmac::compute_hmac;
use crate::MiraError;

fn gate_err_response(e: GateError) -> axum::response::Response {
    use GateError::*;
    match e {
        QuotaExceeded { .. } | RationaleRequired => {
            (StatusCode::FORBIDDEN, e.to_string()).into_response()
        }
        Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Public ingest helpers ────────────────────────────────────────────────────

// Acceptable clock skew for the optional replay window. Five minutes is the
// same window most third-party providers (GitHub, Stripe) use, so signed
// payloads from those services pass without bespoke tuning.
const REPLAY_WINDOW_SECS: i64 = 300;

// Per-webhook rate-limit buckets keyed by webhook id. A simple per-minute
// counter — when the wall-clock minute rolls over we reset, so the worst
// case is one webhook getting up to `2 × rate_limit_per_min` requests
// across a minute boundary. That's fine: the predicate / dispatcher are
// the primary cost gate; rate limit is a coarse safety net against a
// runaway sender.
struct RateBucket {
    minute: i64,
    count:  u32,
}

static RATE_BUCKETS: LazyLock<DashMap<String, RateBucket>> = LazyLock::new(DashMap::new);

fn rate_check(webhook_id: &str, limit: i64) -> bool {
    if limit <= 0 { return true; } // 0 / negative = unlimited
    let now_min = chrono::Utc::now().timestamp() / 60;
    let mut entry = RATE_BUCKETS.entry(webhook_id.to_string())
        .or_insert(RateBucket { minute: now_min, count: 0 });
    if entry.minute != now_min {
        entry.minute = now_min;
        entry.count = 0;
    }
    entry.count = entry.count.saturating_add(1);
    (entry.count as i64) <= limit
}

// Pull the signature from `X-Webhook-Signature: sha256=<hex>` (also accept
// the bare hex form for tooling that strips prefixes).
fn extract_signature(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("x-webhook-signature")?.to_str().ok()?;
    Some(raw.strip_prefix("sha256=").unwrap_or(raw).to_string())
}

// Convert headers to a JSON object for the predicate context. Skips the
// signature/timestamp headers because they're not interesting to predicates
// and may leak through audit if the user serialises the context.
fn headers_to_json(headers: &HeaderMap) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in headers.iter() {
        let name = k.as_str().to_lowercase();
        if name == "x-webhook-signature" || name == "x-webhook-timestamp" {
            continue;
        }
        if let Ok(s) = v.to_str() {
            map.insert(name, Value::String(s.to_string()));
        }
    }
    Value::Object(map)
}

// ── Public ingest ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct IngestResponse {
    matched:    bool,
    dispatched: bool,
}

// `POST /webhook/incoming/{token}`. Public. Body is the raw payload (we
// JSON-parse if possible, otherwise treat it as a string under
// `payload._raw` so predicates and templates can still reach it).
pub async fn ingest_webhook(
    Extension(store):  Extension<Arc<AutomationsStore>>,
    Extension(worker): Extension<Arc<Worker>>,
    Path(token):       Path<String>,
    headers:           HeaderMap,
    body:              Bytes,
) -> impl IntoResponse {
    let now = chrono::Utc::now().timestamp();

    // 1. Token lookup. Unknown tokens get a generic 404 — we deliberately
    //  don't disambiguate "no such token" from "paused" so a probe can't
    //  enumerate live webhooks.
    let (webhook, secret) = match store.get_webhook_by_token(&token) {
        Ok(Some(t)) => t,
        Ok(None)    => return (StatusCode::NOT_FOUND, "no such webhook").into_response(),
        Err(e)      => {
            warn!("ingest_webhook lookup failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "lookup failed").into_response();
        }
    };
    if !matches!(webhook.status, AutomationStatus::Active) {
        return (StatusCode::NOT_FOUND, "no such webhook").into_response();
    }
    if let Some(exp) = webhook.expires_at {
        if exp <= now {
            return (StatusCode::NOT_FOUND, "no such webhook").into_response();
        }
    }

    // 2. HMAC verification. Always required — a webhook without a secret
    //  can't be created (the store fills one in at create time).
    let Some(provided) = extract_signature(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing X-Webhook-Signature").into_response();
    };
    let expected = compute_hmac(secret.as_bytes(), &body);
    let ok = bool::from(expected.as_bytes().ct_eq(provided.as_bytes()));
    if !ok {
        let _ = store.touch_webhook(&webhook.id, now, Some("hmac mismatch"));
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    // 3. Optional replay window. Only enforced when the sender chose to
    //  include a timestamp — we don't want to reject senders that don't
    //  bother (most ad-hoc curl users won't).
    if let Some(ts_hdr) = headers.get("x-webhook-timestamp")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
    {
        let drift = (now - ts_hdr).abs();
        if drift > REPLAY_WINDOW_SECS {
            let _ = store.touch_webhook(&webhook.id, now, Some("timestamp outside replay window"));
            return (StatusCode::UNAUTHORIZED, "stale signature").into_response();
        }
    }

    // 4. Rate limit.
    if !rate_check(&webhook.id, webhook.rate_limit_per_min) {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }

    // 5. Parse body into JSON; fall back to a wrapping `_raw` so predicates
    //  and templates can always find *something* under `payload`.
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v)  => v,
        Err(_) => {
            let s = String::from_utf8_lossy(&body).into_owned();
            json!({ "_raw": s })
        }
    };

    // 6. Predicate evaluation.
    let pred_ctx = json!({
        "payload": payload,
        "headers": headers_to_json(&headers),
        "now":     now,
    });
    let matched = match webhook.predicate.as_ref() {
        Some(p) => match predicate::eval(p, &pred_ctx) {
            Ok(b)  => b,
            Err(e) => {
                warn!("webhook predicate eval failed: {e}");
                let _ = store.touch_webhook(&webhook.id, now, Some(&format!("predicate: {e}")));
                false
            }
        }
        None => true, // No predicate → fire on every authenticated payload.
    };

    // 7. Always append to the ring buffer (matched flag tells the user why
    //  a payload didn't fire). Best-effort — never block dispatch on a
    //  storage hiccup.
    let headers_json = serde_json::to_string(&headers_to_json(&headers))
        .unwrap_or_else(|_| "{}".into());
    let body_str = String::from_utf8_lossy(&body).into_owned();
    if let Err(e) = store.append_webhook_payload(
        &webhook.id, now, &headers_json, &body_str, matched,
    ) {
        warn!("webhook append_payload failed: {e}");
    }

    // 8. Touch the row so the UI shows freshness, even when filtered out.
    let _ = store.touch_webhook(&webhook.id, now, None);

    if !matched {
        return Json(IngestResponse { matched: false, dispatched: false }).into_response();
    }

    // 9. Dispatch — payload feeds {{payload.…}} templating in the action.
    let activation = Activation {
        source_kind: "webhook",
        source_id:   &webhook.id,
        user_id:     &webhook.user_id,
        action:      &webhook.action,
        payload:     Some(&payload),
        chain_ids:   &[],
    };
    let outcome = worker.dispatcher().dispatch(activation).await;
    if let Some(err) = outcome.error.as_ref() {
        let _ = store.touch_webhook(&webhook.id, now, Some(err));
    }

    Json(IngestResponse { matched: true, dispatched: true }).into_response()
}

// ── Authenticated CRUD ───────────────────────────────────────────────────────

fn err_response(e: MiraError) -> axum::response::Response {
    let msg = e.to_string();
    if msg.contains("not found") {
        return (StatusCode::NOT_FOUND, msg).into_response();
    }
    (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
}

fn is_admin(role: &Role) -> bool { matches!(role, Role::Admin) }

fn can_read(user_id: &str, role: &Role, w: &Webhook) -> bool {
    is_admin(role) || w.user_id == user_id || matches!(w.owner_kind, OwnerKind::System)
}

fn can_write(user_id: &str, role: &Role, w: &Webhook) -> bool {
    is_admin(role) || w.user_id == user_id
}

#[derive(Debug, Deserialize)]
pub struct ListWebhooksQuery {
    pub user_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WebhooksListResponse { pub webhooks: Vec<Webhook> }

#[derive(Debug, Deserialize)]
pub struct CreateWebhookRequest {
    pub name:                String,
    #[serde(default)]
    pub description:         Option<String>,
    #[serde(default)]
    pub rationale:           Option<String>,
    #[serde(default)]
    pub predicate:           Option<Value>,
    #[serde(default)]
    pub payload_template:    Option<String>,
    pub action:              Action,
    #[serde(default)]
    pub rate_limit_per_min:  Option<i64>,
    #[serde(default)]
    pub debounce_secs:       Option<i64>,
    #[serde(default)]
    pub expires_at:          Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateWebhookRequest {
    pub name:                String,
    #[serde(default)]
    pub description:         Option<String>,
    #[serde(default)]
    pub rationale:           Option<String>,
    #[serde(default)]
    pub predicate:           Option<Value>,
    #[serde(default)]
    pub payload_template:    Option<String>,
    pub action:              Action,
    #[serde(default)]
    pub rate_limit_per_min:  Option<i64>,
    #[serde(default)]
    pub debounce_secs:       Option<i64>,
    #[serde(default)]
    pub expires_at:          Option<i64>,
}

fn validate_predicate(p: &Option<Value>) -> Result<(), String> {
    let Some(pred) = p else { return Ok(()); };
    let probe = json!({"payload": {}, "headers": {}, "now": 0});
    predicate::eval(pred, &probe).map(|_| ()).map_err(|e| format!("invalid predicate: {e}"))
}

pub async fn list_webhooks(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Query(q):         Query<ListWebhooksQuery>,
) -> impl IntoResponse {
    let result = if is_admin(&user.role) {
        store.list_webhooks(q.user_id.as_deref())
    } else {
        store.list_webhooks(Some(user.id.as_str()))
    };
    match result {
        Ok(rows) => Json(WebhooksListResponse { webhooks: rows }).into_response(),
        Err(e)   => err_response(e),
    }
}

pub async fn create_webhook(
    AuthUser(user):    AuthUser,
    Extension(store):  Extension<Arc<AutomationsStore>>,
    Extension(config): Extension<Arc<MiraConfig>>,
    Json(req):         Json<CreateWebhookRequest>,
) -> impl IntoResponse {
    if req.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    if let Err(e) = validate_predicate(&req.predicate) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    let status_override = match gate_create_webhook(
        &store, &config.automations, &user.id,
        OwnerKind::User, req.rationale.as_deref(),
    ) {
        Ok(v)  => v,
        Err(e) => return gate_err_response(e),
    };
    let new = NewWebhook {
        user_id:            user.id.clone(),
        owner_kind:         OwnerKind::User,
        name:               req.name,
        description:        req.description,
        rationale:          req.rationale,
        predicate:          req.predicate,
        payload_template:   req.payload_template,
        action:             req.action,
        rate_limit_per_min: req.rate_limit_per_min,
        debounce_secs:      req.debounce_secs,
        expires_at:         req.expires_at,
        status:             status_override,
    };
    match store.create_webhook(new) {
        Ok(w)  => (StatusCode::CREATED, Json(w)).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn get_webhook(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    match store.get_webhook(&id) {
        Ok(Some(w)) if can_read(&user.id, &user.role, &w) => Json(w).into_response(),
        Ok(Some(_)) => (StatusCode::FORBIDDEN, "not your webhook").into_response(),
        Ok(None)    => (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => err_response(e),
    }
}

pub async fn update_webhook(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
    Json(req):        Json<UpdateWebhookRequest>,
) -> impl IntoResponse {
    if req.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    if let Err(e) = validate_predicate(&req.predicate) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    let existing = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }
    let upd = UpdateWebhook {
        name:               req.name,
        description:        req.description,
        rationale:          req.rationale,
        predicate:          req.predicate,
        payload_template:   req.payload_template,
        action:             req.action,
        rate_limit_per_min: req.rate_limit_per_min,
        debounce_secs:      req.debounce_secs,
        expires_at:         req.expires_at,
    };
    match store.update_webhook(&id, upd) {
        Ok(w)  => Json(w).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn delete_webhook(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }
    match store.delete_webhook(&id) {
        Ok(true)  => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)    => err_response(e),
    }
}

pub async fn pause_webhook(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }
    match store.pause_webhook(&id) {
        Ok(w)  => Json(w).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn resume_webhook(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }
    match store.resume_webhook(&id) {
        Ok(w)  => Json(w).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn approve_webhook(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }
    if !matches!(existing.status, AutomationStatus::PendingApproval) {
        return (StatusCode::CONFLICT, "webhook is not pending approval").into_response();
    }
    match store.approve_webhook(&id) {
        Ok(w)  => Json(w).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn reject_webhook(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }
    if !matches!(existing.status, AutomationStatus::PendingApproval) {
        return (StatusCode::CONFLICT, "webhook is not pending approval").into_response();
    }
    match store.delete_webhook(&id) {
        Ok(true)  => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)    => err_response(e),
    }
}

#[derive(Debug, Serialize)]
pub struct RotateResponse {
    // Field name reflects what was rotated (`token` or `secret`).
    pub value: String,
}

pub async fn rotate_token(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }
    match store.rotate_webhook_token(&id) {
        Ok(t)  => Json(RotateResponse { value: t }).into_response(),
        Err(e) => err_response(e),
    }
}

pub async fn rotate_secret(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }
    match store.rotate_webhook_secret(&id) {
        Ok(s)  => Json(RotateResponse { value: s }).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Serialize)]
pub struct PayloadsResponse { pub payloads: Vec<WebhookPayload> }

pub async fn list_payloads(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    let existing = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_read(&user.id, &user.role, &existing) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }
    match store.list_webhook_payloads(&id) {
        Ok(rows) => Json(PayloadsResponse { payloads: rows }).into_response(),
        Err(e)   => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct TestReplayRequest {
    // Either `payload_id` (re-run a stored payload) or `body` (replay
    // arbitrary JSON without round-tripping HMAC). At least one required.
    #[serde(default)]
    pub payload_id: Option<i64>,
    #[serde(default)]
    pub body:       Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct TestReplayResponse {
    pub matched:    bool,
    pub dispatched: bool,
    pub error:      Option<String>,
}

// `POST /api/webhooks/{id}/test`. Owner-only. Runs the predicate +
// dispatcher against an explicit body or a previously-stored payload —
// useful for iterating on the predicate / template without convincing the
// upstream sender to re-deliver. Skips HMAC, rate limit, replay window;
// the user is already authenticated.
pub async fn test_replay(
    AuthUser(user):    AuthUser,
    Extension(store):  Extension<Arc<AutomationsStore>>,
    Extension(worker): Extension<Arc<Worker>>,
    Path(id):          Path<String>,
    Json(req):         Json<TestReplayRequest>,
) -> impl IntoResponse {
    let webhook = match store.get_webhook(&id) {
        Ok(Some(w)) => w,
        Ok(None)    => return (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => return err_response(e),
    };
    if !can_write(&user.id, &user.role, &webhook) {
        return (StatusCode::FORBIDDEN, "not your webhook").into_response();
    }

    let payload = match (req.body, req.payload_id) {
        (Some(b), _)    => b,
        (None, Some(pid)) => {
            let stored = match store.list_webhook_payloads(&id) {
                Ok(v)  => v,
                Err(e) => return err_response(e),
            };
            let Some(p) = stored.into_iter().find(|p| p.id == pid) else {
                return (StatusCode::NOT_FOUND, "payload not found").into_response();
            };
            serde_json::from_str(&p.body).unwrap_or_else(|_| json!({"_raw": p.body}))
        }
        (None, None) => return (StatusCode::BAD_REQUEST, "body or payload_id required").into_response(),
    };

    let now = chrono::Utc::now().timestamp();
    let pred_ctx = json!({
        "payload": payload,
        "headers": {},
        "now":     now,
    });
    let matched = match webhook.predicate.as_ref() {
        Some(p) => match predicate::eval(p, &pred_ctx) {
            Ok(b)  => b,
            Err(e) => {
                return Json(TestReplayResponse {
                    matched: false, dispatched: false,
                    error: Some(format!("predicate: {e}")),
                }).into_response();
            }
        }
        None => true,
    };

    if !matched {
        return Json(TestReplayResponse { matched: false, dispatched: false, error: None })
            .into_response();
    }

    let activation = Activation {
        source_kind: "webhook",
        source_id:   &webhook.id,
        user_id:     &webhook.user_id,
        action:      &webhook.action,
        payload:     Some(&payload),
        chain_ids:   &[],
    };
    let outcome = worker.dispatcher().dispatch(activation).await;
    debug!("webhook test_replay outcome: {:?}", outcome.error);
    Json(TestReplayResponse {
        matched:    true,
        dispatched: outcome.error.is_none(),
        error:      outcome.error,
    }).into_response()
}

// Convenience: get the public POST URL for the webhook so the UI can copy
// it without composing the path itself. Returns relative path; the UI
// joins against the configured base origin.
#[derive(Debug, Serialize)]
pub struct WebhookUrlResponse { pub path: String }

pub async fn webhook_url(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<AutomationsStore>>,
    Path(id):         Path<String>,
) -> impl IntoResponse {
    match store.get_webhook(&id) {
        Ok(Some(w)) if can_read(&user.id, &user.role, &w) => {
            Json(WebhookUrlResponse { path: format!("/webhook/incoming/{}", w.token) }).into_response()
        }
        Ok(Some(_)) => (StatusCode::FORBIDDEN, "not your webhook").into_response(),
        Ok(None)    => (StatusCode::NOT_FOUND, "webhook not found").into_response(),
        Err(e)      => err_response(e),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_id(tag: &str) -> String {
        format!("{tag}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
    }

    #[test]
    fn rate_check_allows_under_limit() {
        let id = unique_id("rate");
        for _ in 0..5 {
            assert!(rate_check(&id, 10));
        }
    }

    #[test]
    fn rate_check_blocks_over_limit() {
        let id = unique_id("rate-block");
        for _ in 0..3 {
            assert!(rate_check(&id, 3));
        }
        // Fourth call exceeds the cap.
        assert!(!rate_check(&id, 3));
    }

    #[test]
    fn rate_check_zero_means_unlimited() {
        let id = unique_id("rate-zero");
        for _ in 0..1000 {
            assert!(rate_check(&id, 0));
        }
    }

    #[test]
    fn extract_signature_strips_prefix() {
        let mut h = HeaderMap::new();
        h.insert("x-webhook-signature", "sha256=deadbeef".parse().unwrap());
        assert_eq!(extract_signature(&h).as_deref(), Some("deadbeef"));

        let mut h2 = HeaderMap::new();
        h2.insert("x-webhook-signature", "deadbeef".parse().unwrap());
        assert_eq!(extract_signature(&h2).as_deref(), Some("deadbeef"));
    }

    #[test]
    fn headers_to_json_omits_signature_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-webhook-signature", "sha256=x".parse().unwrap());
        h.insert("x-webhook-timestamp", "12345".parse().unwrap());
        h.insert("x-other", "ok".parse().unwrap());
        let v = headers_to_json(&h);
        assert!(v.get("x-webhook-signature").is_none());
        assert!(v.get("x-webhook-timestamp").is_none());
        assert_eq!(v.get("x-other"), Some(&Value::String("ok".into())));
    }
}
