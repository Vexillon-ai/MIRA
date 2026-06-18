// SPDX-License-Identifier: AGPL-3.0-or-later

// src/mcp/handler.rs
//! Local `ClientHandler` impl that fulfils MCP **sampling** requests
//!.
//!
//! When a server sends `sampling/createMessage`, this handler:
//! 1. Refuses unless the server was configured with
//!    `sampling_enabled = true` (so the owner has explicitly opted
//!    this server in — third-party MCP servers can otherwise burn
//!    provider quota at will).
//! 2. Translates the MCP `SamplingMessage` array into MIRA's
//!    `ChatMessage` shape, prepending `system_prompt` if present.
//! 3. Hands the messages to the gateway's primary provider and
//!    returns the assistant text as a `CreateMessageResult`.
//!
//! The owner_user_id is recorded for log + audit but **not** yet
//! used to pick a per-user provider —  routes every sampling
//! call through the gateway's single primary provider. Per-user
//! provider routing is a follow-up when multi-user MIRA installs
//! with heterogeneous provider configurations become a real
//! workflow.

use std::sync::Arc;

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientInfo, CreateMessageRequestMethod,
    CreateMessageRequestParams, CreateMessageResult, ErrorData as McpError,
    Implementation, Role, SamplingContent, SamplingMessage, SamplingMessageContent,
};
use rmcp::service::{RequestContext, RoleClient};
use tracing::{info, warn};

use crate::providers::ModelProvider;
use crate::types::{Attachment, AttachmentKind, ChatMessage, GenerationOptions, MessageRole};

// Per-server local handler attached to the rmcp client. Built fresh
// for each `McpClient::connect` (incl. reconnect) so the captured
// `sampling_enabled` reflects the row's latest config.
pub struct McpClientHandler {
    pub server_name:      String,
    pub owner_user_id:    String,
    pub sampling_enabled: bool,
    // Provider routed to by every sampling call. Cloned out of the
    // gateway's `Arc<dyn ModelProvider>` at registry-build time so
    // the handler can fulfil without taking another lock.
    pub provider:         Arc<dyn ModelProvider>,
}

impl ClientHandler for McpClientHandler {
    fn get_info(&self) -> ClientInfo {
        // Only declare the sampling capability when the server is
        // opted-in. Well-behaved MCP servers won't issue
        // sampling/createMessage to a client that didn't advertise
        // it, so this is the first line of defence against
        // quota-burning servers.
        let capabilities = if self.sampling_enabled {
            ClientCapabilities::builder().enable_sampling().build()
        } else {
            ClientCapabilities::default()
        };
        ClientInfo::new(
            capabilities,
            Implementation::new("MIRA", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _ctx:   RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, McpError> {
        if !self.sampling_enabled {
            warn!(
                "mcp '{}': server attempted sampling but sampling_enabled=false — refusing",
                self.server_name,
            );
            return Err(McpError::method_not_found::<CreateMessageRequestMethod>());
        }

        // ── Translate MCP messages → MIRA ChatMessage. ──────────────────
        // System prompt is prepended (MCP carries it as a sibling
        // field, MIRA wants it inline). Image blocks pass through as
        // real `Attachment`s — every provider wire layer already
        // knows how to format `AttachmentKind::Image`. Audio and
        // SEP-1577 tool-use/tool-result blocks remain placeholdered
        // until there's a real MCP server using them.
        let mut messages: Vec<ChatMessage> = Vec::new();
        if let Some(sys) = &params.system_prompt {
            messages.push(ChatMessage::system(sys.clone()));
        }
        let mut total_images = 0usize;
        for m in &params.messages {
            let (text, images) = extract_text_and_images(&m.content);
            total_images += images.len();
            let role = match m.role {
                Role::User      => MessageRole::User,
                Role::Assistant => MessageRole::Assistant,
            };
            messages.push(ChatMessage {
                role,
                content: text,
                tool_calls:   None,
                tool_call_id: None,
                attachments:  if images.is_empty() { None } else { Some(images) },
            });
        }

        // Refuse when neither text nor images survived. Refusing here
        // instead of letting the provider see an empty turn keeps the
        // error message at the MCP layer where the operator can act
        // on it.
        let any_payload = messages.iter()
            .any(|m| !m.content.trim().is_empty() || m.attachments.as_ref().is_some_and(|a| !a.is_empty()));
        if !any_payload {
            return Err(McpError::invalid_params(
                "sampling: messages produced empty prompt after content extraction",
                None,
            ));
        }

        let opts = GenerationOptions {
            temperature: params.temperature.unwrap_or(0.7),
            // MCP and our provider both use u32 here. Bound
            // defensively — a 0 max_tokens from a misbehaving server
            // shouldn't trip the provider's "no limit" path.
            max_tokens:  Some(params.max_tokens.max(1)),
            ..Default::default()
        };

        info!(
            "mcp '{}': sampling for user '{}' — {} message{}{}, max_tokens={}",
            self.server_name, self.owner_user_id,
            messages.len(), if messages.len() == 1 { "" } else { "s" },
            if total_images > 0 {
                format!(" ({} image{})", total_images, if total_images == 1 { "" } else { "s" })
            } else { String::new() },
            opts.max_tokens.unwrap_or(0),
        );

        let resp = self.provider.generate(&messages, &opts).await
            .map_err(|e| McpError::internal_error(
                format!("provider sampling failed: {e}"), None,
            ))?;

        Ok(CreateMessageResult::new(
            SamplingMessage::assistant_text(resp.content),
            // The MCP spec wants the model id that produced the
            // response. We don't currently surface a stable id from
            // the provider response; "mira-routed" tells the server
            // honestly that it doesn't get to know which model we
            // used. Future: thread the model id back through the
            // provider's GenerateResponse if/when that's needed.
            "mira-routed".to_string(),
        )
        .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN))
    }
}

// Split one [`SamplingContent`] payload into (joined text, image
// attachments). Text blocks are concatenated with newlines; image
// blocks become real `Attachment`s the provider wire layer can
// embed inline. Audio and SEP-1577 tool-use / tool-result blocks
// fall back to placeholder text so they're not silently dropped —
// they remain a future-slice item until there's a real MCP server
// using them.
fn extract_text_and_images(
    c: &SamplingContent<SamplingMessageContent>,
) -> (String, Vec<Attachment>) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut images:     Vec<Attachment> = Vec::new();

    let visit = |entry: &SamplingMessageContent,
                 text_parts: &mut Vec<String>,
                 images: &mut Vec<Attachment>| {
        use SamplingMessageContent::*;
        match entry {
            Text(t)  => text_parts.push(t.text.clone()),
            Image(i) => images.push(Attachment {
                kind:      AttachmentKind::Image,
                mime_type: i.mime_type.clone(),
                data_b64:  i.data.clone(),
            }),
            Audio(_) => text_parts.push("[audio content — not yet passed through]".into()),
            other    => text_parts.push(format!("[{}]", std::any::type_name_of_val(other))),
        }
    };

    match c {
        SamplingContent::Single(s)    => visit(s, &mut text_parts, &mut images),
        SamplingContent::Multiple(ms) => for entry in ms { visit(entry, &mut text_parts, &mut images); },
    }
    (text_parts.join("\n"), images)
}
