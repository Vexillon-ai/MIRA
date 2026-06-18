// SPDX-License-Identifier: AGPL-3.0-or-later

// src/whatsapp/handler.rs
//
// The shared WhatsApp webhook endpoints, both nested under
// `/webhook/whatsapp/{account_id}`:
//
//   GET  — Meta's subscription verification. Echo `hub.challenge` back as
//          plain text iff `hub.verify_token` matches the account's token.
//   POST — inbound message delivery. Verify the X-Hub-Signature-256 HMAC
//          over the RAW body (app secret), then flatten + dispatch each
//          text message.
//
// Like the Telegram webhook, this resolves the account via a shared
// lookup map (`WhatsAppState.accounts`) populated at startup by the
// ChannelManager. No long-lived task — Meta pushes to us.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use subtle::ConstantTimeEq;
use tracing::{info, warn};

use super::api::verify_signature;
use super::dispatch::{
    process_whatsapp_message, InboundWhatsApp, WhatsAppAccountCtx, WhatsAppDispatcherDeps,
};
use super::types::{VerifyQuery, WebhookPayload};

/// State injected into both handlers: the per-account lookup map + the
/// dispatcher deps. Cheap to clone (Arcs inside).
#[derive(Clone)]
pub struct WhatsAppState {
    pub accounts: Arc<HashMap<String, WhatsAppAccountCtx>>,
    pub deps:     WhatsAppDispatcherDeps,
}

/// `GET /webhook/whatsapp/{account_id}` — subscription verification.
pub async fn whatsapp_verify(
    State(state):  State<WhatsAppState>,
    Path(account): Path<String>,
    Query(q):      Query<VerifyQuery>,
) -> impl IntoResponse {
    let Some(ctx) = state.accounts.get(&account) else {
        warn!("WhatsApp verify: unknown account id {}", account);
        return (StatusCode::NOT_FOUND, String::new());
    };
    // Constant-time compare of the verify token (matches the Telegram
    // webhook-secret convention). Lower-risk than the per-message HMAC —
    // it only gates the one-time subscription handshake — but cheap to do
    // right.
    let token_ok = q.verify_token.as_deref()
        .map(|t| bool::from(t.as_bytes().ct_eq(ctx.verify_token.as_bytes())))
        .unwrap_or(false);
    let is_subscribe = q.mode.as_deref() == Some("subscribe");
    if is_subscribe && token_ok {
        if let Some(challenge) = q.challenge {
            info!("WhatsApp webhook verified for account {}", account);
            // Meta expects the bare challenge string echoed back, 200.
            return (StatusCode::OK, challenge);
        }
    }
    warn!("WhatsApp verify failed for account {} (token_ok={}, subscribe={})",
          account, token_ok, is_subscribe);
    (StatusCode::FORBIDDEN, String::new())
}

/// `POST /webhook/whatsapp/{account_id}` — inbound message delivery.
///
/// We take the RAW body (`Bytes`) because the signature is computed over
/// the exact bytes Meta sent; re-serialising a parsed Json would change
/// whitespace and break the HMAC.
pub async fn whatsapp_inbound(
    State(state):  State<WhatsAppState>,
    Path(account): Path<String>,
    headers:       HeaderMap,
    body:          Bytes,
) -> impl IntoResponse {
    // Global kill switch.
    if let Some(cfg) = &state.deps.live_config {
        if !cfg.get().await.channels.whatsapp.enabled {
            info!("WhatsApp webhook ignored — globally disabled (acct={})", account);
            // 200 so Meta doesn't retry-storm; we just drop it.
            return StatusCode::OK;
        }
    }

    let Some(ctx) = state.accounts.get(&account).cloned() else {
        warn!("WhatsApp inbound: unknown account id {}", account);
        return StatusCode::NOT_FOUND;
    };

    // Signature verification over the raw body. When an app_secret is
    // configured we REQUIRE a valid signature; without one we accept
    // unverified (operator's explicit choice, warned at startup).
    if let Some(secret) = ctx.app_secret.as_deref() {
        let provided = headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !verify_signature(secret, &body, provided) {
            warn!("WhatsApp inbound: bad signature on account {}", account);
            return StatusCode::UNAUTHORIZED;
        }
    }

    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p)  => p,
        Err(e) => {
            warn!("WhatsApp inbound: bad JSON on account {}: {}", account, e);
            // 200 — a malformed body won't get better on retry.
            return StatusCode::OK;
        }
    };

    for (from, text, display_name) in payload.text_messages() {
        let deps = state.deps.clone();
        let ctx2 = ctx.clone();
        let inbound = InboundWhatsApp { from, body: text, display_name };
        tokio::spawn(async move {
            process_whatsapp_message(deps, ctx2, inbound).await;
        });
    }
    StatusCode::OK
}
