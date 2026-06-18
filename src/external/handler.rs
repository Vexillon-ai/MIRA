// SPDX-License-Identifier: AGPL-3.0-or-later

// src/external/handler.rs
//
// The shared CPP inbound webhook endpoint, nested at
// `/webhook/external/{account_id}`. A provider POSTs here; we verify the
// HMAC signature over the raw body (the account's inbound_secret), then
// dispatch the message async (ack 200 fast). Account is resolved via a
// shared lookup map populated at startup by the ChannelManager.

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
    process_external_message, ExternalAccountCtx, ExternalDispatcherDeps, InboundAudioBytes,
    InboundExternal,
};
use super::types::InboundBody;

/// Max base64 length we'll accept for an inbound voice note. ~14 MB of
/// base64 ≈ ~10 MB decoded audio — generous for a voice message, but a hard
/// ceiling so a malicious/buggy provider can't push us into a huge alloc on
/// the webhook path. Oversize audio is dropped (the message still routes if
/// it also carried text).
const MAX_INBOUND_AUDIO_B64: usize = 14_000_000;

#[derive(Clone)]
pub struct ExternalState {
    pub accounts: Arc<HashMap<String, ExternalAccountCtx>>,
    pub deps:     ExternalDispatcherDeps,
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Resolve an `external` account that isn't in the startup snapshot by reading
/// the channel-account store directly. Returns the runtime ctx only for an
/// enabled External account; a disabled/deleted/non-external row yields `None`
/// (so a disabled account stops receiving, and a deleted one 404s). Requires
/// `deps.channel_store` to be wired (it always is in the server router).
fn resolve_account_live(state: &ExternalState, account_id: &str) -> Option<ExternalAccountCtx> {
    let store = state.deps.channel_store.as_ref()?;
    let acct = store.get(account_id).ok().flatten()?;
    if !acct.enabled || acct.channel != crate::channel_accounts::ChannelKind::External {
        return None;
    }
    match ExternalAccountCtx::from_account(&acct) {
        Ok(ctx) => Some(ctx),
        Err(e) => {
            warn!("External inbound: account {} present but unusable: {}", account_id, e);
            None
        }
    }
}

/// `POST /webhook/external/{account_id}` — CPP inbound.
pub async fn external_inbound(
    State(state):  State<ExternalState>,
    Path(account): Path<String>,
    headers:       HeaderMap,
    body:          Bytes,
) -> impl IntoResponse {
    // Global kill switch — 200 (no provider retry storm) when off.
    if let Some(cfg) = &state.deps.live_config {
        if !cfg.get().await.channels.external.enabled {
            info!("External webhook ignored — globally disabled (acct={})", account);
            return StatusCode::OK;
        }
    }

    // The startup snapshot covers accounts that existed at boot; an account
    // created since (a freshly-installed CPP provider, or a manually-added
    // External account) won't be there. Fall back to a live store lookup so it
    // works without a restart — and so disabling/deleting it takes effect too.
    let ctx = match state.accounts.get(&account).cloned() {
        Some(c) => c,
        None => match resolve_account_live(&state, &account) {
            Some(c) => c,
            None => {
                warn!("External inbound: unknown account id {}", account);
                return StatusCode::NOT_FOUND;
            }
        },
    };

    // Signature verification (always required — inbound_secret is mandatory).
    let timestamp = headers.get("x-mira-cpp-timestamp")
        .and_then(|v| v.to_str().ok()).unwrap_or_default();
    let signature = headers.get("x-mira-cpp-signature")
        .and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !verify_signature(&ctx.inbound_secret, timestamp, &body, signature, now_unix()) {
        warn!("External inbound: bad signature on account {}", account);
        return StatusCode::UNAUTHORIZED;
    }

    let parsed: InboundBody = match serde_json::from_slice(&body) {
        Ok(b)  => b,
        Err(e) => {
            warn!("External inbound: bad JSON on account {}: {}", account, e);
            return StatusCode::OK;
        }
    };

    // Only dispatch message events; other types ack 200 + ignore.
    if parsed.is_message() {
        let has_text = !parsed.text.trim().is_empty();
        // Decode an optional inbound voice note for server-side STT. Only
        // bother when STT is actually wired — otherwise an audio-only message
        // would just be dropped downstream, so skip the alloc. Size-capped +
        // base64-validated here at the edge; the async processor transcribes.
        let audio = if state.deps.stt.is_some() {
            decode_inbound_audio(parsed.audio, &account)
        } else {
            None
        };

        if has_text || audio.is_some() {
            let deps = state.deps.clone();
            let ctx2 = ctx.clone();
            let inbound = InboundExternal {
                conversation_id: parsed.conversation_id,
                sender_id:       parsed.sender_id,
                display_name:    parsed.sender_display_name,
                text:            parsed.text,
                audio,
            };
            tokio::spawn(async move {
                process_external_message(deps, ctx2, inbound).await;
            });
        }
    }
    StatusCode::OK
}

/// Validate + size-cap + base64-decode an optional inbound `audio` object
/// into raw bytes for STT. Returns `None` (with a warn) on oversize or bad
/// base64 — never an error, so a bad audio payload degrades to "no audio"
/// rather than dropping a message that also has text.
fn decode_inbound_audio(
    audio:   Option<super::types::InboundAudio>,
    account: &str,
) -> Option<InboundAudioBytes> {
    let a = audio?;
    if a.data_base64.is_empty() {
        return None;
    }
    if a.data_base64.len() > MAX_INBOUND_AUDIO_B64 {
        warn!("External inbound: voice note too large ({} b64 chars) on account {} — dropping audio",
              a.data_base64.len(), account);
        return None;
    }
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(a.data_base64.as_bytes()) {
        Ok(bytes) => {
            let mime = if !a.content_type.is_empty() {
                a.content_type
            } else if !a.extension.is_empty() {
                format!("audio/{}", a.extension)
            } else {
                "audio/ogg".to_string() // best-effort default
            };
            Some(InboundAudioBytes { mime, bytes })
        }
        Err(e) => {
            warn!("External inbound: bad base64 audio on account {}: {}", account, e);
            None
        }
    }
}
