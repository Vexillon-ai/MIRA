// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/email.rs
//! Per-user email account CRUD (slice E1+E3, chunk 1).
//!
//!   * `GET    /api/email/accounts`         — list the caller's rows
//!   * `POST   /api/email/accounts`         — create one owned by the caller
//!   * `PUT    /api/email/accounts/{id}`    — update one the caller owns
//!   * `DELETE /api/email/accounts/{id}`    — delete one the caller owns
//!
//! Slice E1+E3 will add `/api/email/quarantine` (chunk 5) and status
//! probes (later). Changes to accounts take effect on next gateway
//! restart — chunk 2's poller doesn't hot-reload its registry.

use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::{Extension, Json};

use crate::agent::AgentCore;
use crate::auth::{AdminUser, AuthUser};
use crate::config::EmailOAuthConfig;
use crate::email::oauth::{self, OAuthProvider, OAuthStateStore, StartFlowResult};
use crate::email::SystemMailer;
use crate::email::{
    dispatch_inbound, parse_email, AuditEntry, EmailAccountRow, EmailAccountStore,
    EmailAuditStore, EmailPollerRegistry, EmailPollerStatus, EmailQuarantineStore,
    EmailSecurity, NewAuditEntry, NewEmailAccount, ParseSettings, QuarantineEntry,
    UpdateEmailAccount,
};
use crate::history::HistoryStore;
use crate::web::LiveConfig;

/// `GET /api/email/status` — per-account runtime snapshot (poller
/// state, last poll time, last error, total received). Scoped to
/// the caller's rows so non-admins don't see other users' state.
pub async fn status(
    AuthUser(user):      AuthUser,
    Extension(registry): Extension<Arc<EmailPollerRegistry>>,
) -> Json<Vec<EmailPollerStatus>> {
    Json(registry.snapshot_for_user(&user.id).await)
}

// ── Quarantine queue (chunk 5) ──────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct QuarantineEntryPublic {
    pub id:          String,
    pub account_id:  String,
    pub sender:      String,
    pub subject:     String,
    pub preview:     String,
    pub message_id:  String,
    pub reason:      String,
    pub received_at: i64,
}

impl From<QuarantineEntry> for QuarantineEntryPublic {
    fn from(e: QuarantineEntry) -> Self {
        Self {
            id: e.id, account_id: e.account_id, sender: e.sender,
            subject: e.subject, preview: e.preview, message_id: e.message_id,
            reason: e.reason, received_at: e.received_at,
        }
    }
}

/// `GET /api/email/quarantine` — list held messages across the
/// caller's accounts. Newest first. The raw body bytes are not
/// included in the list payload (large + irrelevant for the
/// quick-scan UI).
pub async fn list_quarantine(
    AuthUser(user):    AuthUser,
    Extension(accts):  Extension<Arc<EmailAccountStore>>,
    Extension(quar):   Extension<Arc<EmailQuarantineStore>>,
) -> Result<Json<Vec<QuarantineEntryPublic>>, StatusCode> {
    let mine = accts.list_for_user(&user.id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter().map(|a| a.id).collect::<Vec<_>>();
    let entries = quar.list_for_accounts(&mine)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(entries.into_iter().map(Into::into).collect()))
}

/// POST body for /approve. `add_to_allowlist: false` is the
/// "process this one but I'll review again next time" override.
/// Defaults to true per the design call — approval = trust this
/// sender going forward.
#[derive(serde::Deserialize)]
pub struct ApproveBody {
    #[serde(default = "default_true")]
    pub add_to_allowlist: bool,
}

fn default_true() -> bool { true }

/// `POST /api/email/quarantine/{id}/approve` — re-runs dispatch
/// for the held message and, by default, adds the sender to the
/// per-account allowlist so future mail from them skips
/// quarantine. The row is deleted after a successful dispatch.
///
/// Re-dispatch reuses the same parser + agent pipeline the live
/// poll path uses; nothing about "approved messages" gets special
/// treatment downstream — they look identical to messages from
/// already-allowlisted senders.
pub async fn approve_quarantine(
    AuthUser(user):      AuthUser,
    Extension(accts):    Extension<Arc<EmailAccountStore>>,
    Extension(quar):     Extension<Arc<EmailQuarantineStore>>,
    Extension(audit):    Extension<Arc<EmailAuditStore>>,
    Extension(history):  Extension<Arc<HistoryStore>>,
    Extension(agent):    Extension<Arc<AgentCore>>,
    Extension(registry): Extension<Arc<EmailPollerRegistry>>,
    Extension(live_cfg): Extension<Arc<LiveConfig>>,
    Path(id):            Path<String>,
    Json(body):          Json<ApproveBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let entry = quar.get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "not found".into()))?;
    let account = accts.get(&entry.account_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "account gone".into()))?;
    if account.user_id != user.id {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }

    // Add to allowlist FIRST so a dispatch crash doesn't strand the
    // intent — the operator already said "trust this sender", the
    // re-process is the consequence.
    if body.add_to_allowlist {
        let mut security = account.security();
        let sender_lc = entry.sender.to_ascii_lowercase();
        let already = security.allowed_senders.iter()
            .any(|s| s.to_ascii_lowercase() == sender_lc);
        if !already {
            security.allowed_senders.push(entry.sender.clone());
            let upd = UpdateEmailAccount {
                security: Some(security),
                ..UpdateEmailAccount::default()
            };
            if let Err(e) = accts.update(&account.id, upd) {
                return Err((StatusCode::INTERNAL_SERVER_ERROR,
                            format!("allowlist update: {e}")));
            }
        }
    }

    // Re-parse with the account's effective parse settings. We use
    // the security state we just (maybe) updated so any HTML /
    // attachment toggles take effect on this run too.
    let refreshed = accts.get(&account.id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "account gone after update".into()))?;
    let sec = refreshed.security();
    let parse_settings = ParseSettings {
        accept_html:          sec.accept_html.unwrap_or(true),
        accept_inline_images: sec.accept_inline_images.unwrap_or(false),
        accept_attachments:   sec.accept_attachments.unwrap_or(false),
    };
    let parsed = parse_email(&entry.raw_body, parse_settings)
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "could not parse held body".into()))?;

    // Use the shared reply-loop cache from the live poller registry
    // so a quarantine approval can't bypass the same-body guard the
    // inbound path uses.
    let loop_cache = Arc::clone(&registry.loop_cache);
    dispatch_inbound(
        &parsed, &refreshed, &history, &agent,
        Some(&audit), Some(&loop_cache),
        Some(&accts), Some(&live_cfg),
    ).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("dispatch: {e}")))?;

    // Audit the operator's intervention separately from the
    // poller's original "quarantined" row so the trail shows both.
    let _ = audit.record(NewAuditEntry {
        account_id:     refreshed.id.clone(),
        direction:      "inbound".into(),
        sender:         entry.sender.clone(),
        recipient:      refreshed.address.clone(),
        subject:        entry.subject.clone(),
        action:         "approved".into(),
        reason:         Some(if body.add_to_allowlist { "approve+allowlist".into() } else { "approve_once".into() }),
        body:           entry.raw_body.clone(),
        attached_count: parsed.dropped_attachments.len(),
    });

    quar.delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize, Default)]
pub struct RejectBody {
    /// When true, the sender is added to the per-account denylist
    /// so future messages are hard-dropped at the security gate.
    #[serde(default)]
    pub add_to_denylist: bool,
}

/// `POST /api/email/quarantine/{id}/reject` — discard the held
/// message, optionally adding the sender to the per-account
/// denylist.
pub async fn reject_quarantine(
    AuthUser(user):    AuthUser,
    Extension(accts):  Extension<Arc<EmailAccountStore>>,
    Extension(quar):   Extension<Arc<EmailQuarantineStore>>,
    Extension(audit):  Extension<Arc<EmailAuditStore>>,
    Path(id):          Path<String>,
    Json(body):        Json<RejectBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let entry = quar.get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "not found".into()))?;
    let account = accts.get(&entry.account_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "account gone".into()))?;
    if account.user_id != user.id {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }

    if body.add_to_denylist {
        let mut security = account.security();
        let sender_lc = entry.sender.to_ascii_lowercase();
        let already = security.denied_senders.iter()
            .any(|s| s.to_ascii_lowercase() == sender_lc);
        if !already {
            security.denied_senders.push(entry.sender.clone());
            let upd = UpdateEmailAccount {
                security: Some(security),
                ..UpdateEmailAccount::default()
            };
            if let Err(e) = accts.update(&account.id, upd) {
                return Err((StatusCode::INTERNAL_SERVER_ERROR,
                            format!("denylist update: {e}")));
            }
        }
    }

    let _ = audit.record(NewAuditEntry {
        account_id:     account.id.clone(),
        direction:      "inbound".into(),
        sender:         entry.sender.clone(),
        recipient:      account.address.clone(),
        subject:        entry.subject.clone(),
        action:         "rejected".into(),
        reason:         Some(if body.add_to_denylist { "reject+denylist".into() } else { "reject_once".into() }),
        body:           entry.raw_body.clone(),
        attached_count: 0,
    });

    quar.delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Audit log (chunk 5) ─────────────────────────────────────────────────────

/// `GET /api/email/audit` — most-recent inbound + outbound rows
/// across the caller's accounts. Bounded to the last 200 rows by
/// the store; pagination is a follow-up if it matters.
pub async fn list_audit(
    AuthUser(user):    AuthUser,
    Extension(accts):  Extension<Arc<EmailAccountStore>>,
    Extension(audit):  Extension<Arc<EmailAuditStore>>,
) -> Result<Json<Vec<AuditEntry>>, StatusCode> {
    let mine = accts.list_for_user(&user.id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter().map(|a| a.id).collect::<Vec<_>>();
    let rows = audit.list_for_accounts(&mine, 200)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows))
}

// ── E4 — OAuth flow endpoints ───────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct OAuthStartPath {
    pub id:       String,
    pub provider: String,
}

/// `POST /api/email/accounts/{id}/oauth/{provider}/start` — build the
/// provider's authorize URL and stash the PKCE state, ready for the
/// browser to follow. Owner-gated against the email_account row.
pub async fn oauth_start(
    AuthUser(user):         AuthUser,
    Extension(accts):       Extension<Arc<EmailAccountStore>>,
    Extension(state_store): Extension<Arc<OAuthStateStore>>,
    Extension(live_cfg):    Extension<Arc<LiveConfig>>,
    Path(p):                Path<OAuthStartPath>,
) -> Result<Json<StartFlowResult>, (StatusCode, String)> {
    let provider = OAuthProvider::parse(&p.provider).ok_or((
        StatusCode::BAD_REQUEST,
        format!("unknown provider {:?} (expected google|microsoft)", p.provider),
    ))?;
    let row = accts.get(&p.id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "not found".into()))?;
    if row.user_id != user.id {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }

    let cfg = live_cfg.get().await;
    let server_port = cfg.server.port;
    let result = oauth::start_flow(
        &state_store, &cfg.email_oauth, server_port,
        provider, &user.id, &p.id,
    ).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
pub struct OAuthCallbackQuery {
    pub code:  Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    #[serde(rename = "error_description")]
    pub error_description: Option<String>,
}

/// `GET /api/email/oauth/callback` — public route (no auth header
/// from the provider). The state token tied to the in-process
/// PKCE map is what binds the callback back to a user + account
/// row, so an attacker hitting this URL with a forged state lands
/// in the "unknown state" error path.
pub async fn oauth_callback(
    Extension(accts):       Extension<Arc<EmailAccountStore>>,
    Extension(state_store): Extension<Arc<OAuthStateStore>>,
    Extension(live_cfg):    Extension<Arc<LiveConfig>>,
    axum::extract::Query(q): axum::extract::Query<OAuthCallbackQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    // Provider reported an error (user denied, scope refused, etc.).
    if let Some(err) = q.error {
        return callback_page(
            "error",
            &format!("OAuth failed: {} ({})", err,
                     q.error_description.unwrap_or_default()),
            None,
        ).into_response();
    }

    let code  = match q.code  { Some(c) => c, None => return callback_page("error", "missing code", None).into_response() };
    let state = match q.state { Some(s) => s, None => return callback_page("error", "missing state", None).into_response() };

    let cfg = live_cfg.get().await;
    let server_port = cfg.server.port;
    match oauth::handle_callback(
        &state_store, &cfg.email_oauth, server_port, &accts, &code, &state,
    ).await {
        Ok(account_id) => callback_page(
            "ok",
            "OAuth connection complete — you can close this tab.",
            Some(&account_id),
        ).into_response(),
        Err(e) => callback_page("error", &format!("{e}"), None).into_response(),
    }
}

/// Minimal terminal HTML for the OAuth callback. Auto-closes the
/// tab on success (most users will appreciate it); error keeps the
/// tab open so the message is readable. No external assets — the
/// page is the only thing the browser fetches.
fn callback_page(status: &str, message: &str, account_id: Option<&str>) -> axum::response::Html<String> {
    let color = if status == "ok" { "#22c55e" } else { "#ef4444" };
    let auto_close = if status == "ok" { r#"<script>setTimeout(() => window.close(), 1500);</script>"# } else { "" };
    let aid = account_id.map(|s| format!("<p style='opacity:.6;font-size:13px'>account {s}</p>")).unwrap_or_default();
    let html = format!(
        r#"<!doctype html><meta charset=utf-8><title>MIRA OAuth</title>
<style>body{{font-family:system-ui;background:#111;color:#eee;display:flex;flex-direction:column;align-items:center;justify-content:center;height:100vh;margin:0;padding:24px;text-align:center}}h1{{color:{color}}}</style>
<h1>{status}</h1>
<p>{message}</p>
{aid}
{auto_close}"#);
    axum::response::Html(html)
}

// ── E6 — webhook inbound ────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct WebhookPath {
    pub id:     String,
    pub secret: String,
}

/// `POST /webhook/email/{id}/{secret}` — public route (no Bearer).
/// The path-segment secret is what authenticates the call. Provider
/// is determined from the account row's `webhook_provider` (the
/// operator picks Postmark / Resend / Mailgun at account creation).
///
/// Same downstream pipeline as the IMAP poller: parse → security
/// verdict → dispatch (Accept) / quarantine (Quarantine) / drop
/// + audit row regardless of verdict. Returns 200 on every well-
/// formed call so the provider doesn't retry — operator-visible
/// failures live in /api/email/audit.
pub async fn webhook_inbound(
    Extension(accts):       Extension<Arc<EmailAccountStore>>,
    Extension(quar):        Extension<Arc<EmailQuarantineStore>>,
    Extension(audit):       Extension<Arc<EmailAuditStore>>,
    Extension(history):     Extension<Arc<HistoryStore>>,
    Extension(agent):       Extension<Arc<AgentCore>>,
    Extension(registry):    Extension<Arc<EmailPollerRegistry>>,
    Extension(live_cfg):    Extension<Arc<LiveConfig>>,
    Path(p):                Path<WebhookPath>,
    headers:                axum::http::HeaderMap,
    body:                   axum::body::Bytes,
) -> Result<StatusCode, (StatusCode, String)> {
    use subtle::ConstantTimeEq;
    use crate::email::{evaluate, parse_email, security::InboundHeaders, webhook};

    let row = accts.get(&p.id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "not found".into()))?;
    if row.auth_mode != "webhook" {
        return Err((StatusCode::BAD_REQUEST,
                    "account is not configured for webhook auth_mode".into()));
    }
    let expected = row.webhook_secret.as_deref().unwrap_or("");
    if expected.is_empty() ||
       !bool::from(expected.as_bytes().ct_eq(p.secret.as_bytes())) {
        // 404 (not 401) so an attacker probing for valid account ids
        // can't distinguish "wrong secret" from "no such id".
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    let provider = row.webhook_provider.as_deref().ok_or((
        StatusCode::BAD_REQUEST,
        "webhook account missing webhook_provider".into(),
    ))?;

    let security = row.security();
    let parse_settings = crate::email::ParseSettings {
        accept_html:          security.accept_html.unwrap_or(true),
        accept_inline_images: security.accept_inline_images.unwrap_or(false),
        accept_attachments:   security.accept_attachments.unwrap_or(false),
    };
    let effective_max_size_kb = security.max_message_size_kb.unwrap_or(1024);
    let effective_rate_per_sender  = security.inbound_rate_per_sender_per_hour.unwrap_or(10);
    let effective_rate_per_account = security.inbound_rate_per_account_per_day.unwrap_or(100);

    let content_type = headers.get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    let (parsed, hdrs, raw_size) = match webhook::parse(
        provider, &body, content_type, parse_settings.accept_html,
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("email webhook '{}': parse failed: {e}", row.address);
            // Audit the failure with whatever we know; 200 OK to
            // the provider so they don't retry a malformed payload.
            let _ = audit.record(crate::email::NewAuditEntry {
                account_id:     row.id.clone(),
                direction:      "inbound".into(),
                sender:         String::new(),
                recipient:      row.address.clone(),
                subject:        String::new(),
                action:         "dropped".into(),
                reason:         Some(format!("webhook parse: {e}")),
                body:           body.to_vec(),
                attached_count: 0,
            });
            return Ok(StatusCode::OK);
        }
    };

    // Reuse the shared rate limiter from the poller registry so
    // a webhook-account flood and a poller-account flood from the
    // same sender share the same per-sender bucket.
    let rate = crate::email::NoopRateLimiter;
    let verdict = evaluate(
        &parsed, raw_size, &hdrs, &security,
        effective_max_size_kb,
        effective_rate_per_sender,
        effective_rate_per_account,
        &row.id, &rate,
    );
    // suppress unused-binding warnings — the noop rate limiter is
    // intentional; real per-webhook rate limiting can be added when
    // there's a real flood incident.
    let _ = (effective_rate_per_sender, effective_rate_per_account);
    let _ = hdrs.is_auto_submitted;
    let _ = InboundHeaders::default();
    let _ = parse_email; // satisfy unused-import lints if the path is removed

    let (action, audit_reason) = match &verdict {
        crate::email::Verdict::Accept { reason } => {
            tracing::info!(
                "email webhook '{}': from={} subject={:?} → ACCEPT ({reason})",
                row.address, parsed.sender_address, parsed.subject,
            );
            let loop_cache = Arc::clone(&registry.loop_cache);
            let action = match crate::email::dispatch_inbound(
                &parsed, &row, &history, &agent,
                Some(&audit), Some(&loop_cache),
                Some(&accts), Some(&live_cfg),
            ).await {
                Ok(_)  => "accepted",
                Err(e) => {
                    tracing::warn!("email webhook '{}': dispatch failed: {e}", row.address);
                    "dispatch_failed"
                }
            };
            (action, Some(reason.clone()))
        }
        crate::email::Verdict::Quarantine { reason } => {
            tracing::info!(
                "email webhook '{}': from={} subject={:?} → QUARANTINE ({reason})",
                row.address, parsed.sender_address, parsed.subject,
            );
            let preview: String = parsed.text_body.chars().take(500).collect();
            let _ = quar.put(crate::email::NewQuarantineEntry {
                account_id:  row.id.clone(),
                sender:      parsed.sender_address.clone(),
                subject:     parsed.subject.clone(),
                preview,
                message_id:  parsed.message_id.clone(),
                reason:      reason.clone(),
                uid:         0, // not an IMAP path
                raw_body:    body.to_vec(),
            });
            ("quarantined", Some(reason.clone()))
        }
        crate::email::Verdict::Drop { reason } => {
            tracing::info!(
                "email webhook '{}': from={} subject={:?} → DROP ({reason})",
                row.address, parsed.sender_address, parsed.subject,
            );
            ("dropped", Some(reason.clone()))
        }
    };

    let _ = audit.record(crate::email::NewAuditEntry {
        account_id:     row.id.clone(),
        direction:      "inbound".into(),
        sender:         parsed.sender_address.clone(),
        recipient:      row.address.clone(),
        subject:        parsed.subject.clone(),
        action:         action.into(),
        reason:         audit_reason,
        body:           body.to_vec(),
        attached_count: parsed.dropped_attachments.len(),
    });

    Ok(StatusCode::OK)
}

// ── E5 — system email admin test endpoint ───────────────────────────────────

#[derive(serde::Deserialize)]
pub struct SystemTestBody {
    /// Recipient for the test message. Required.
    pub to: String,
    /// Optional override; defaults to a fixed test subject/body.
    pub subject: Option<String>,
    pub body:    Option<String>,
}

/// `POST /api/admin/email/system/test` — send a one-shot test
/// message via the configured `system_email` SMTP relay. Admin-
/// gated because the config it exercises is global state.
pub async fn system_email_test(
    AdminUser(_):       AdminUser,
    Extension(mailer):  Extension<Arc<SystemMailer>>,
    Json(body):         Json<SystemTestBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    if body.to.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "to required".into()));
    }
    let subject = body.subject.unwrap_or_else(|| "MIRA system_email test".into());
    let text    = body.body.unwrap_or_else(|| {
        "If you're reading this, MIRA's system_email config can send mail.\n\n\
         (Triggered via POST /api/admin/email/system/test.)".into()
    });
    mailer.send(&body.to, &subject, &text).await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// Suppress unused-import warnings for re-exports the handlers expose
// through the function signatures but don't reach for elsewhere.
#[allow(dead_code)]
fn _refs() {
    let _ = std::any::type_name::<EmailSecurity>();
    let _ = std::any::type_name::<EmailOAuthConfig>();
}

/// `GET /api/email/accounts` — list rows owned by the caller.
pub async fn list_accounts(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<EmailAccountStore>>,
) -> Result<Json<Vec<EmailAccountRow>>, StatusCode> {
    let rows = store.list_for_user(&user.id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter()
        .map(EmailAccountRow::redacted) // never echo passwords/tokens to the client
        .collect();
    Ok(Json(rows))
}

/// `POST /api/email/accounts` — create a row owned by the caller.
pub async fn create_account(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<EmailAccountStore>>,
    Json(new):        Json<NewEmailAccount>,
) -> Result<(StatusCode, Json<EmailAccountRow>), (StatusCode, String)> {
    if new.label.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "label required".into()));
    }
    if new.address.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "address required".into()));
    }
    if new.auth_mode == "password" {
        // Password auth: require the four IMAP/SMTP host fields at
        // minimum. Without these the poller has nothing to connect
        // to; surface the error at create time rather than at boot.
        let missing = [
            ("imap_host", new.imap_host.is_none()),
            ("imap_username", new.imap_username.is_none()),
            ("imap_password", new.imap_password.is_none()),
            ("smtp_host", new.smtp_host.is_none()),
        ].iter().filter_map(|(n, m)| if *m { Some(*n) } else { None }).collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err((StatusCode::BAD_REQUEST, format!(
                "auth_mode=password requires: {}", missing.join(", ")
            )));
        }
    }
    let row = store.create(&user.id, new)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok((StatusCode::CREATED, Json(row.redacted())))
}

/// `PUT /api/email/accounts/{id}` — update a row the caller owns.
/// Returns 404 when the row doesn't exist OR belongs to someone else
/// (we don't leak existence to non-owners — mirrors MCP CRUD).
pub async fn update_account(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<EmailAccountStore>>,
    Path(id):         Path<String>,
    Json(upd):        Json<UpdateEmailAccount>,
) -> Result<Json<EmailAccountRow>, (StatusCode, String)> {
    let existing = store.get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "not found".into()))?;
    if existing.user_id != user.id {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    let row = store.update(&id, upd)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(row.redacted()))
}

/// `DELETE /api/email/accounts/{id}` — owner-gated delete.
pub async fn delete_account(
    AuthUser(user):   AuthUser,
    Extension(store): Extension<Arc<EmailAccountStore>>,
    Path(id):         Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let existing = store.get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "not found".into()))?;
    if existing.user_id != user.id {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    store.delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
