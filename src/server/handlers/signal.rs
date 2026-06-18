// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/signal.rs
//! POST /webhook/signal — inbound Signal message handler.
//!
//! Authentication is handled upstream by [`crate::security::HmacLayer`].
//!
//! Signal now has session history (like Telegram):
//! `session_id = format!("signal-{}", sender_phone_number)`
//!
//! Flow:
//! 1. Parse the Signal webhook JSON body.
//! 2. Extract sender phone number and message text.
//! 3. Call `AgentCore::process()` — sessions are managed inside AgentCore.
//! 4. Collect full response.
//! 5. Send back via signal-cli REST API (if configured).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::agent::{AgentCore, TurnContext};
use crate::auth::LocalAuthService;
use crate::channel::parse_signal_webhook;
use crate::history::{HistoryStore, NewConversation};
use crate::providers::signal_cli::SignalCliClient;

// ─────────────────────────────────────────────────────────────────────────────

// State injected alongside `Arc<AgentCore>` for outbound Signal API calls.
#[derive(Clone)]
pub struct SignalState {
    pub agent_core:    Arc<AgentCore>,
    pub signal_port:   u16,
    pub signal_number: Option<String>,
    pub history:       Option<Arc<HistoryStore>>,
    // Maps the inbound sender phone to a MIRA user UUID so memory and
    // profile context follow the user across channels. `None` falls back
    // to using the raw sender phone as the user_id (legacy behaviour).
    pub auth:          Option<Arc<LocalAuthService>>,
    // MCP host registry for the per-user `allowed_tool_names`
    // filter. `None` leaves the turn unrestricted.
    pub mcp_servers:   Option<Arc<crate::mcp::McpServerRegistry>>,
}

// ─────────────────────────────────────────────────────────────────────────────

pub async fn signal_handler(
    State(state): State<SignalState>,
    body: String,
) -> impl IntoResponse {
    let msg = match parse_signal_webhook(&body) {
        Some(m) => m,
        None => {
            error!("Failed to parse Signal webhook body");
            return (StatusCode::BAD_REQUEST, "invalid payload");
        }
    };

    info!("Signal message from {}: {}", msg.sender, &msg.content[..msg.content.len().min(80)]);

    // Kick off a "typing" indicator as soon as we receive the inbound — the
    // user sees the MIRA-is-typing bubble in their Signal app immediately,
    // not only when we finally send the reply. The loop stops as soon as
    // `cancel_tx` fires (right before we send the real response) so the
    // bubble doesn't linger.
    let typing_cancel = state.signal_number.as_ref().map(|phone| {
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        tokio::spawn(run_typing_loop(
            state.signal_port,
            phone.clone(),
            msg.sender.clone(),
            cancel_rx,
        ));
        cancel_tx
    });

    let session_id = format!("signal-{}", msg.sender);

    // Cross-channel identity: prefer the MIRA user UUID resolved from
    // `users.phone` so memory and profile context match the web UI. Falls
    // back to the raw sender phone for unclaimed numbers.
    let resolved_user_id = state.auth
        .as_ref()
        .and_then(|a| a.find_by_phone(&msg.sender).ok().flatten())
        .map(|u| u.id)
        .unwrap_or_else(|| msg.sender.clone());

    if let Some(bus) = state.agent_core.event_bus() {
        bus.emit_named(
            crate::events::names::MESSAGE_RECEIVED,
            Some(resolved_user_id.clone()),
            serde_json::json!({
                "user_id":         resolved_user_id,
                "channel":         "signal",
                "conversation_id": session_id,
                "text":            msg.content,
            }),
        );
    }

    // Trusted identity injection: user-tier tools (recall_history,
    // automations.*, etc.) need `_user_id` to scope results / authorise
    // create-on-behalf-of-user. The web chat handler does the same thing
    // this keeps Signal at parity so the agent can schedule follow-ups
    // when asked from a phone.
    let mut inject = serde_json::Map::new();
    inject.insert(
        "_user_id".to_string(),
        serde_json::Value::String(resolved_user_id.clone()),
    );
    let mut turn_ctx = TurnContext { inject_tool_args: inject, ..TurnContext::default() };
    if let Some(reg) = state.mcp_servers.as_ref() {
        let all = state.agent_core.tools.list_tools();
        if let Some(allow) = reg.allowed_tools_for(&resolved_user_id, &all) {
            turn_ctx.allowed_tool_names = Some(allow);
        }
    }

    let rx = match state.agent_core
        .process_with_context(
            &session_id, &resolved_user_id, "signal", &msg.content,
            None, turn_ctx,
        )
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            if let Some(tx) = typing_cancel { let _ = tx.send(()); }
            error!("AgentCore failed for Signal sender {}: {}", msg.sender, e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "agent error");
        }
    };

    let (response_text, _events) = AgentCore::collect_response(rx).await;

    // Stop the typing indicator right before we dispatch the real message
    // so Signal clients don't briefly show both "typing…" and the reply.
    if let Some(tx) = typing_cancel { let _ = tx.send(()); }

    // Record turn in history store if available.
    if let Some(ref hist) = state.history {
        let conv_id = {
            let existing = hist.list_conversations(&msg.sender, Some("signal"), 1, 0)
                .ok()
                .and_then(|v| v.into_iter().next());
            if let Some(c) = existing {
                c.id
            } else {
                hist.create_conversation(NewConversation {
                    user_id:          msg.sender.clone(),
                    channel:          "signal".to_owned(),
                    title:            None,
                    model:            None,
                    provider:         None,
                    external_user_id: None,
                    mode:             None,
                }).map(|c| c.id).unwrap_or_default()
            }
        };
        if !conv_id.is_empty() {
            let _ = hist.record_turn(&conv_id, &msg.content, &response_text, None, None);
        }
    }

    // Send back via signal-cli REST API.
    if let Some(ref phone) = state.signal_number {
        let client = SignalCliClient::new(state.signal_port, phone.clone());
        match client.send(vec![msg.sender.clone()], &response_text).await {
            Ok(())  => info!("Signal response sent to {}", msg.sender),
            Err(e)  => warn!("Failed to send Signal response to {}: {}", msg.sender, e),
        }
    } else {
        warn!("SIGNAL_PHONE_NUMBER not configured — Signal response not sent");
    }

    (StatusCode::OK, "ok")
}

// Keep the "MIRA is typing…" indicator alive for the length of one turn.
// // Signal's typing indicator auto-expires after ~15s of no refresh, so we
// re-send every 10s. `cancel_rx` is awaited in parallel with the sleep so
// the caller can stop the loop the moment the response is ready — on
// cancel we send one final `stop = true` to clear the bubble immediately.
async fn run_typing_loop(
    signal_port:   u16,
    phone_number:  String,
    recipient:     String,
    mut cancel_rx: oneshot::Receiver<()>,
) {
    let client = SignalCliClient::new(signal_port, phone_number);

    loop {
        if let Err(e) = client.send_typing(vec![recipient.clone()], false).await {
            // Typing failures shouldn't abort the turn — log and keep going
            // so a transient daemon hiccup doesn't lose the indicator for
            // the rest of the response.
            debug!("Signal typing indicator send failed: {}", e);
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = &mut cancel_rx => break,
        }
    }

    // Final stop so the client clears the bubble right away.
    if let Err(e) = client.send_typing(vec![recipient], true).await {
        debug!("Signal typing stop failed: {}", e);
    }
}
