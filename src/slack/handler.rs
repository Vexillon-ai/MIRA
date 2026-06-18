// SPDX-License-Identifier: AGPL-3.0-or-later

// src/slack/handler.rs
//
// The shared Slack Events API webhook endpoint, nested at
// `/webhook/slack/{account_id}`. Unlike WhatsApp (GET verify + POST
// events), Slack sends everything as POST:
//
//   * url_verification — a one-time `{type, challenge}` body during setup.
//     We verify the signature, then echo `challenge` back as plain text.
//   * event_callback   — actual events. Verify signature, ack 200 FAST
//     (Slack retries if we take >3s), and dispatch each message async.
//
// Account is resolved via a shared lookup map (`SlackState.accounts`),
// populated at startup by the ChannelManager. Webhook-driven — no task.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use tracing::{info, warn};

use super::api::verify_signature;
use super::dispatch::{
    process_slack_message, InboundSlack, SlackAccountCtx, SlackDispatcherDeps,
};
use super::types::WebhookEnvelope;

#[derive(Clone)]
pub struct SlackState {
    pub accounts: Arc<HashMap<String, SlackAccountCtx>>,
    pub deps:     SlackDispatcherDeps,
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `POST /webhook/slack/{account_id}` — handles both url_verification and
/// event_callback. We take the RAW body (`Bytes`) because the signature is
/// computed over the exact bytes Slack sent.
pub async fn slack_inbound(
    State(state):  State<SlackState>,
    Path(account): Path<String>,
    headers:       HeaderMap,
    body:          Bytes,
) -> impl IntoResponse {
    // Global kill switch — 200 (no retry storm) when off.
    if let Some(cfg) = &state.deps.live_config {
        if !cfg.get().await.channels.slack.enabled {
            info!("Slack webhook ignored — globally disabled (acct={})", account);
            return (StatusCode::OK, String::new());
        }
    }

    let Some(ctx) = state.accounts.get(&account).cloned() else {
        warn!("Slack inbound: unknown account id {}", account);
        return (StatusCode::NOT_FOUND, String::new());
    };

    // Signature verification — always required for Slack (the signing
    // secret is mandatory config; there's no "unverified" mode).
    let timestamp = headers.get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok()).unwrap_or_default();
    let signature = headers.get("x-slack-signature")
        .and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !verify_signature(&ctx.signing_secret, timestamp, &body, signature, now_unix()) {
        warn!("Slack inbound: bad signature on account {}", account);
        return (StatusCode::UNAUTHORIZED, String::new());
    }

    let envelope: WebhookEnvelope = match serde_json::from_slice(&body) {
        Ok(e)  => e,
        Err(e) => {
            warn!("Slack inbound: bad JSON on account {}: {}", account, e);
            return (StatusCode::OK, String::new());
        }
    };

    // url_verification handshake: echo the challenge back as plain text.
    if envelope.is_url_verification() {
        let challenge = envelope.challenge.unwrap_or_default();
        info!("Slack webhook verified for account {}", account);
        return (StatusCode::OK, challenge);
    }

    // event_callback: dispatch the message (if any) async + ack immediately
    // so we stay under Slack's 3-second retry threshold.
    if let Some((channel, user, text)) = envelope.inbound_message() {
        let deps = state.deps.clone();
        let ctx2 = ctx.clone();
        let inbound = InboundSlack { channel, user, text };
        tokio::spawn(async move {
            process_slack_message(deps, ctx2, inbound).await;
        });
    }
    (StatusCode::OK, String::new())
}
